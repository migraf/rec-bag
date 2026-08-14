//! Rekordbox XML export format.
//!
//! Generates XML files compatible with Pioneer Rekordbox.

use crate::{ExportError, PlaylistExporter};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::Writer;
use rec_core::{Playlist, Track};
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

/// Exporter for Rekordbox XML format.
pub struct RekordboxExporter {
    /// Product name to include in XML
    product_name: String,
    /// Product version
    product_version: String,
}

impl Default for RekordboxExporter {
    fn default() -> Self {
        Self::new()
    }
}

impl RekordboxExporter {
    /// Create a new Rekordbox exporter.
    pub fn new() -> Self {
        Self {
            product_name: "rec_bag".to_string(),
            product_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Write a track element.
    fn write_track<W: std::io::Write>(
        &self,
        writer: &mut Writer<W>,
        track: &Track,
        track_id: u32,
    ) -> Result<(), ExportError> {
        let mut elem = BytesStart::new("TRACK");

        elem.push_attribute(("TrackID", track_id.to_string().as_str()));
        elem.push_attribute(("Name", track.title.as_str()));

        if let Some(ref artist) = track.artist {
            elem.push_attribute(("Artist", artist.as_str()));
        }

        if let Some(ref album) = track.album {
            elem.push_attribute(("Album", album.as_str()));
        }

        if let Some(ref genre) = track.genre {
            elem.push_attribute(("Genre", genre.as_str()));
        }

        if let Some(bpm) = track.bpm {
            elem.push_attribute(("AverageBpm", format!("{:.2}", bpm).as_str()));
        }

        if let Some(ref key) = track.key {
            elem.push_attribute(("Tonality", key.as_str()));
        }

        if let Some(duration) = track.duration {
            elem.push_attribute(("TotalTime", duration.as_secs().to_string().as_str()));
        }

        if let Some(rating) = track.rating {
            // Rekordbox uses 0-255 scale, we use 0-5
            let rb_rating = (rating as u32 * 51).min(255);
            elem.push_attribute(("Rating", rb_rating.to_string().as_str()));
        }

        // File location
        let location = track.path.to_string_lossy();
        elem.push_attribute(("Location", format!("file://localhost{}", location).as_str()));

        writer.write_event(Event::Empty(elem))?;

        Ok(())
    }

    /// Write a playlist node element.
    fn write_playlist_node<W: std::io::Write>(
        &self,
        writer: &mut Writer<W>,
        playlist: &Playlist,
        track_ids: &[u32],
    ) -> Result<(), ExportError> {
        let mut elem = BytesStart::new("NODE");
        elem.push_attribute(("Type", "1")); // Type 1 = Playlist
        elem.push_attribute(("Name", playlist.name.as_str()));
        elem.push_attribute(("KeyType", "0"));
        elem.push_attribute(("Entries", track_ids.len().to_string().as_str()));

        writer.write_event(Event::Start(elem))?;

        // Write track references
        for track_id in track_ids {
            let mut track_elem = BytesStart::new("TRACK");
            track_elem.push_attribute(("Key", track_id.to_string().as_str()));
            writer.write_event(Event::Empty(track_elem))?;
        }

        writer.write_event(Event::End(BytesEnd::new("NODE")))?;

        Ok(())
    }

    /// Export playlists to Rekordbox XML format.
    fn export_to_writer<W: std::io::Write>(
        &self,
        playlists: &[Playlist],
        writer: W,
    ) -> Result<(), ExportError> {
        let mut xml_writer = Writer::new_with_indent(writer, b' ', 2);

        // XML declaration
        xml_writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

        // Root element
        let mut root = BytesStart::new("DJ_PLAYLISTS");
        root.push_attribute(("Version", "1.0.0"));
        xml_writer.write_event(Event::Start(root))?;

        // Product info
        let mut product = BytesStart::new("PRODUCT");
        product.push_attribute(("Name", self.product_name.as_str()));
        product.push_attribute(("Version", self.product_version.as_str()));
        product.push_attribute(("Company", "rec_bag"));
        xml_writer.write_event(Event::Empty(product))?;

        // Collection - all tracks
        let mut collection = BytesStart::new("COLLECTION");

        // Count total tracks
        let total_tracks: usize = playlists.iter().map(|p| p.tracks.len()).sum();
        collection.push_attribute(("Entries", total_tracks.to_string().as_str()));
        xml_writer.write_event(Event::Start(collection))?;

        // Build track ID mapping and write tracks
        let mut track_id_map: std::collections::HashMap<uuid::Uuid, u32> =
            std::collections::HashMap::new();
        let mut current_id = 1u32;

        for playlist in playlists {
            for track in &playlist.tracks {
                if !track_id_map.contains_key(&track.id) {
                    self.write_track(&mut xml_writer, track, current_id)?;
                    track_id_map.insert(track.id, current_id);
                    current_id += 1;
                }
            }
        }

        xml_writer.write_event(Event::End(BytesEnd::new("COLLECTION")))?;

        // Playlists section
        let playlists_elem = BytesStart::new("PLAYLISTS");
        xml_writer.write_event(Event::Start(playlists_elem))?;

        // Root folder node
        let mut root_node = BytesStart::new("NODE");
        root_node.push_attribute(("Type", "0")); // Type 0 = Folder
        root_node.push_attribute(("Name", "ROOT"));
        root_node.push_attribute(("Count", playlists.len().to_string().as_str()));
        xml_writer.write_event(Event::Start(root_node))?;

        // Write each playlist
        for playlist in playlists {
            let track_ids: Vec<u32> = playlist
                .tracks
                .iter()
                .filter_map(|t| track_id_map.get(&t.id).copied())
                .collect();
            self.write_playlist_node(&mut xml_writer, playlist, &track_ids)?;
        }

        xml_writer.write_event(Event::End(BytesEnd::new("NODE")))?;
        xml_writer.write_event(Event::End(BytesEnd::new("PLAYLISTS")))?;

        // Close root
        xml_writer.write_event(Event::End(BytesEnd::new("DJ_PLAYLISTS")))?;

        Ok(())
    }
}

impl PlaylistExporter for RekordboxExporter {
    fn name(&self) -> &str {
        "Rekordbox XML"
    }

    fn extension(&self) -> &str {
        "xml"
    }

    fn export_playlist(&self, playlist: &Playlist, path: &Path) -> Result<(), ExportError> {
        self.export_playlists(&[playlist.clone()], path)
    }

    fn export_playlists(&self, playlists: &[Playlist], path: &Path) -> Result<(), ExportError> {
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        self.export_to_writer(playlists, writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_rekordbox_export() {
        let exporter = RekordboxExporter::new();

        let mut playlist = Playlist::new("Test Playlist");
        let mut track = Track::new(PathBuf::from("/music/song.mp3"));
        track.artist = Some("Test Artist".to_string());
        track.bpm = Some(128.0);
        playlist.add_track(track);

        let mut output = Vec::new();
        exporter.export_to_writer(&[playlist], &mut output).unwrap();

        let xml = String::from_utf8(output).unwrap();
        assert!(xml.contains("DJ_PLAYLISTS"));
        assert!(xml.contains("Test Playlist"));
        assert!(xml.contains("Test Artist"));
        assert!(xml.contains("128.00"));
    }
}
