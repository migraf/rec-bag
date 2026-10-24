//! Track playback through `GET /tracks/{urn}/streams`.
//!
//! SoundCloud serves full tracks only as HLS. A deck has to seek anywhere in a track, so the
//! segments are assembled in memory into one continuous buffer. Nothing here touches the disk: the
//! API Terms allow session-based caching only and forbid file-save functionality.

use super::*;
use futures::{StreamExt, TryStreamExt};

/// Largest playlist, segment, or progressive preview fetched in one request.
const MAX_PART_BYTES: usize = 32 * 1024 * 1024;
/// About four hours at 160 kbps. A longer stream is refused rather than exhausting memory.
const MAX_STREAM_BYTES: usize = 320 * 1024 * 1024;
/// Segments fetched at once. Enough to load a track in a few seconds without hammering the CDN.
const SEGMENT_CONCURRENCY: usize = 4;

/// The encoding SoundCloud delivered a stream in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundCloudStreamFormat {
    /// Full track, MP3 segments.
    Mp3,
    /// Full track, AAC segments.
    Aac,
    /// A short preview clip, offered when the full track is not playable for this account.
    PreviewMp3,
}

/// A track's audio, held in memory for one playback session.
#[derive(Clone)]
pub struct SoundCloudAudio {
    pub format: SoundCloudStreamFormat,
    pub bytes: Vec<u8>,
}

impl fmt::Debug for SoundCloudAudio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SoundCloudAudio")
            .field("format", &self.format)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct StreamsDto {
    #[serde(default)]
    hls_mp3_128_url: Option<String>,
    #[serde(default)]
    hls_aac_160_url: Option<String>,
    #[serde(default)]
    preview_mp3_128_url: Option<String>,
}

impl StreamsDto {
    /// MP3 first: its segments decode as one stream when simply concatenated. AAC next, and the
    /// preview only when no full stream is offered.
    fn choices(self) -> Vec<(SoundCloudStreamFormat, String)> {
        [
            (SoundCloudStreamFormat::Mp3, self.hls_mp3_128_url),
            (SoundCloudStreamFormat::Aac, self.hls_aac_160_url),
            (SoundCloudStreamFormat::PreviewMp3, self.preview_mp3_128_url),
        ]
        .into_iter()
        .filter_map(|(format, url)| url.filter(|url| !url.is_empty()).map(|url| (format, url)))
        .collect()
    }
}

impl SoundCloudClient {
    /// Fetches a track's audio for playback. Only call this for a deliberate user action, such
    /// as loading the track onto a deck; the API Terms forbid playing sounds otherwise.
    pub async fn stream_audio(&self, track_urn: &str) -> Result<SoundCloudAudio, SourceError> {
        let streams: StreamsDto = self
            .get(self.url_for_path(&format!("tracks/{track_urn}/streams"))?)
            .await?;
        let mut last_error = None;
        for (format, url) in streams.choices() {
            match self.assemble_stream(&url).await {
                Ok(bytes) => return Ok(SoundCloudAudio { format, bytes }),
                // A format this client cannot assemble (say, an encrypted playlist) may still
                // leave the next format playable; access and network failures will not.
                Err(error) if error.kind == SourceErrorKind::InvalidData => {
                    last_error = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            SourceError::new(
                SourceErrorKind::PermissionDenied,
                "SoundCloud offers no stream for this track",
            )
        }))
    }

    async fn assemble_stream(&self, url: &str) -> Result<Vec<u8>, SourceError> {
        let mut url = stream_url(url)?;
        // One redirection from a master playlist to a media playlist, never a chain.
        for _ in 0..2 {
            let body = self.fetch_stream_part(&url).await?;
            if !body.starts_with(b"#EXTM3U") {
                // A progressive file, such as the preview.
                return non_empty(body);
            }
            let text = std::str::from_utf8(&body)
                .map_err(|_| invalid_data("SoundCloud stream playlist is not UTF-8"))?;
            match parse_hls(text, &url)? {
                HlsPlaylist::Master(variant) => url = variant,
                HlsPlaylist::Media(parts) => return self.fetch_segments(parts).await,
            }
        }
        Err(invalid_data("SoundCloud stream playlist nests too deeply"))
    }

    async fn fetch_segments(&self, parts: Vec<Url>) -> Result<Vec<u8>, SourceError> {
        let mut audio = Vec::new();
        let mut segments = futures::stream::iter(parts)
            .map(|part| async move { self.fetch_stream_part(&part).await })
            .buffered(SEGMENT_CONCURRENCY);
        while let Some(segment) = segments.try_next().await? {
            if audio.len().saturating_add(segment.len()) > MAX_STREAM_BYTES {
                return Err(invalid_data(
                    "SoundCloud stream exceeds the playback memory limit",
                ));
            }
            audio.extend_from_slice(&segment);
        }
        non_empty(audio)
    }

    /// Fetches one playlist or segment. Only a request to the API's own origin carries the
    /// access token; signed CDN URLs are fetched without it so the token never leaves SoundCloud.
    async fn fetch_stream_part(&self, url: &Url) -> Result<Vec<u8>, SourceError> {
        let authorize = url.origin() == self.api_url.origin();
        let mut token = if authorize {
            Some(
                self.tokens
                    .access_token()
                    .await
                    .map_err(sanitize_token_provider_error)?,
            )
        } else {
            None
        };
        let mut refreshed = false;
        let mut retries = 0_u8;
        loop {
            let mut headers = BTreeMap::new();
            if let Some(token) = &token {
                headers.insert("authorization".to_string(), format!("OAuth {token}"));
            }
            let response = self
                .transport
                .execute_bytes(
                    SoundCloudHttpRequest {
                        url: url.to_string(),
                        headers,
                    },
                    MAX_PART_BYTES,
                )
                .await;
            match response {
                Ok(response) if (200..300).contains(&response.status) => return Ok(response.body),
                Ok(response) if response.status == 401 && authorize && !refreshed => {
                    token = Some(
                        self.tokens
                            .refresh_access_token()
                            .await
                            .map_err(sanitize_token_provider_error)?,
                    );
                    refreshed = true;
                }
                Ok(response) => {
                    let response = SoundCloudHttpResponse {
                        status: response.status,
                        headers: response.headers,
                        body: String::new(),
                    };
                    let error = response_error(&response, self.retry_policy.max_retry_after);
                    if error.is_retryable() && retries < self.retry_policy.max_retries {
                        let delay = retry_delay(
                            &response,
                            retries,
                            self.retry_policy,
                            self.jitter.as_ref(),
                        );
                        retries += 1;
                        self.sleeper.sleep(delay).await;
                    } else {
                        return Err(error);
                    }
                }
                Err(error) if error.kind == SoundCloudTransportErrorKind::ResponseTooLarge => {
                    return Err(invalid_data(
                        "SoundCloud stream part exceeds the playback memory limit",
                    ));
                }
                Err(_error) if retries < self.retry_policy.max_retries => {
                    let delay = jittered_exponential_delay(
                        retries,
                        self.retry_policy,
                        self.jitter.as_ref(),
                    );
                    retries += 1;
                    self.sleeper.sleep(delay).await;
                }
                Err(_error) => {
                    return Err(SourceError::new(
                        SourceErrorKind::TemporarilyUnavailable,
                        "SoundCloud stream request failed",
                    ));
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HlsPlaylist {
    /// A master playlist; the variant to fetch next.
    Master(Url),
    /// A media playlist: the initialization section, if any, then every segment in order.
    Media(Vec<Url>),
}

/// Reads the subset of HLS (RFC 8216) SoundCloud uses: plain or fMP4 segments, unencrypted.
fn parse_hls(text: &str, base: &Url) -> Result<HlsPlaylist, SourceError> {
    let mut parts = Vec::new();
    let mut expect_variant = false;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(attributes) = line.strip_prefix("#EXT-X-KEY:") {
            if attribute(attributes, "METHOD").is_some_and(|method| method != "NONE") {
                return Err(invalid_data("SoundCloud stream is encrypted"));
            }
        } else if let Some(attributes) = line.strip_prefix("#EXT-X-MAP:") {
            let uri = attribute(attributes, "URI")
                .ok_or_else(|| invalid_data("SoundCloud stream map has no URI"))?;
            parts.push(resolve(base, uri)?);
        } else if line.starts_with("#EXT-X-STREAM-INF") {
            expect_variant = true;
        } else if !line.starts_with('#') {
            let url = resolve(base, line)?;
            if expect_variant {
                return Ok(HlsPlaylist::Master(url));
            }
            parts.push(url);
        }
    }
    if parts.is_empty() {
        return Err(invalid_data("SoundCloud stream playlist has no segments"));
    }
    Ok(HlsPlaylist::Media(parts))
}

/// One attribute of an HLS attribute list, unquoted.
fn attribute<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = attributes;
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let (value, after) = if let Some(quoted) = after.strip_prefix('"') {
            let (value, after) = quoted.split_once('"')?;
            (value, after.strip_prefix(',').unwrap_or(after))
        } else {
            after.split_once(',').unwrap_or((after, ""))
        };
        if key.trim() == name {
            return Some(value);
        }
        rest = after;
    }
    None
}

fn resolve(base: &Url, reference: &str) -> Result<Url, SourceError> {
    stream_url(
        base.join(reference)
            .map_err(|_| invalid_data("SoundCloud stream playlist has an invalid URI"))?
            .as_str(),
    )
}

fn stream_url(value: &str) -> Result<Url, SourceError> {
    let url =
        Url::parse(value).map_err(|_| invalid_data("SoundCloud returned an invalid stream URL"))?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        return Err(invalid_data("SoundCloud returned an invalid stream URL"));
    }
    Ok(url)
}

fn non_empty(audio: Vec<u8>) -> Result<Vec<u8>, SourceError> {
    if audio.is_empty() {
        return Err(invalid_data("SoundCloud stream is empty"));
    }
    Ok(audio)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cf-hls-media.sndcdn.com/playlist/abc/playlist.m3u8?Policy=x").unwrap()
    }

    #[test]
    fn a_media_playlist_lists_every_segment_in_order_resolved_against_it() {
        let text = "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:10.0,\nhttps://cf-hls-media.sndcdn.com/media/0/1/a.mp3?s=1\n#EXTINF:10.0,\nseg2.mp3\n#EXT-X-ENDLIST\n";

        let HlsPlaylist::Media(parts) = parse_hls(text, &base()).unwrap() else {
            panic!("expected a media playlist");
        };

        assert_eq!(
            parts.iter().map(Url::as_str).collect::<Vec<_>>(),
            [
                "https://cf-hls-media.sndcdn.com/media/0/1/a.mp3?s=1",
                "https://cf-hls-media.sndcdn.com/playlist/abc/seg2.mp3",
            ]
        );
    }

    #[test]
    fn an_fmp4_playlist_starts_with_its_initialization_section() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"0@0\"\n#EXTINF:6,\n1.m4s\n";

        let HlsPlaylist::Media(parts) = parse_hls(text, &base()).unwrap() else {
            panic!("expected a media playlist");
        };

        assert!(parts[0].as_str().ends_with("/init.mp4"));
        assert!(parts[1].as_str().ends_with("/1.m4s"));
    }

    #[test]
    fn a_master_playlist_names_its_first_variant() {
        let text =
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"\nvariant.m3u8\n";

        assert_eq!(
            parse_hls(text, &base()).unwrap(),
            HlsPlaylist::Master(base().join("variant.m3u8").unwrap())
        );
    }

    #[test]
    fn encrypted_and_empty_playlists_are_refused() {
        let encrypted = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:1,\na.ts\n";
        let unencrypted = "#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:1,\na.ts\n";

        assert_eq!(
            parse_hls(encrypted, &base()).unwrap_err().kind,
            SourceErrorKind::InvalidData
        );
        assert!(parse_hls(unencrypted, &base()).is_ok());
        assert!(parse_hls("#EXTM3U\n#EXT-X-ENDLIST\n", &base()).is_err());
    }

    #[test]
    fn segments_must_be_web_urls() {
        assert!(parse_hls("#EXTM3U\nfile:///etc/passwd\n", &base()).is_err());
    }

    #[test]
    fn streams_are_tried_mp3_first_and_preview_last() {
        let streams = StreamsDto {
            hls_mp3_128_url: Some(String::new()),
            hls_aac_160_url: Some("aac".into()),
            preview_mp3_128_url: Some("preview".into()),
        };

        assert_eq!(
            streams.choices(),
            [
                (SoundCloudStreamFormat::Aac, "aac".to_string()),
                (SoundCloudStreamFormat::PreviewMp3, "preview".to_string()),
            ]
        );
    }
}
