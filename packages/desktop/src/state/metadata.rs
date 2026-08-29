//! One metadata-editor submission, as a value.
//!
//! A submission has to survive three destinations without losing what the user meant:
//! the projected track on screen, the canonical metadata candidates in the Catalog, and
//! the user's own library entry. Carrying it as a value keeps those three in step and
//! keeps "the editor did not touch this" distinct from "the user cleared this".

use chrono::{DateTime, Utc};
use rec_core::{LibraryEntry, MetadataField, MetadataValue, Track};
use ui::MetadataFields;

/// What one metadata-editor submission changes.
///
/// Every field is `None` when the editor did not touch it and `Some(None)` when the
/// user cleared it. Collapsing the two would make editing a rating erase a comment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetadataEdit {
    pub title: Option<String>,
    pub artist: Option<Option<String>>,
    pub album: Option<Option<String>>,
    pub genre: Option<Option<String>>,
    pub bpm: Option<Option<f32>>,
    pub key: Option<Option<String>>,
    pub year: Option<Option<u32>>,
    pub track_number: Option<Option<u32>>,
    pub comment: Option<Option<String>>,
    pub rating: Option<Option<u8>>,
    /// Whether the audio file's own tags are rewritten too.
    pub write_to_file: bool,
}

impl MetadataEdit {
    /// Read one submission from the metadata editor.
    pub fn from_editor(fields: MetadataFields, write_to_file: bool) -> Self {
        Self {
            title: fields.title,
            artist: fields.artist.map(Some),
            album: fields.album.map(Some),
            genre: fields.genre.map(Some),
            bpm: fields.bpm.map(Some),
            key: fields.key.map(Some),
            year: fields.year.map(Some),
            track_number: fields.track_number.map(Some),
            comment: fields.comment.map(Some),
            rating: fields.rating.map(Some),
            write_to_file,
        }
    }

    /// Apply the submission to a projected track.
    pub fn apply_to(&self, track: &mut Track) {
        track.update_metadata(
            self.title.clone(),
            self.artist.clone(),
            self.album.clone(),
            self.genre.clone(),
            self.bpm,
            self.key.clone(),
            self.year,
            self.track_number,
            self.comment.clone(),
            self.rating,
        );
    }

    /// Whether the submission touched the user's own record about the track.
    ///
    /// Rating and comment are authored locally and never reconciled against a provider,
    /// so they go to the library entry rather than to canonical metadata.
    pub fn touches_library_entry(&self) -> bool {
        self.comment.is_some() || self.rating.is_some()
    }

    /// Apply the submission to the user's own record about the track.
    pub fn apply_to_entry(&self, entry: &mut LibraryEntry, edited_at: DateTime<Utc>) {
        if let Some(comment) = self.comment.clone() {
            entry.comment = comment;
        }
        if let Some(rating) = self.rating {
            entry.rating = rating;
        }
        entry.updated_at = edited_at;
    }

    /// The canonical metadata candidates the submission authored.
    ///
    /// Only touched fields are recorded, and only the fields that describe the audio:
    /// rating and comment belong to the library entry, not to canonical metadata.
    pub fn candidates(&self) -> Vec<(MetadataField, Option<MetadataValue>)> {
        [
            text(MetadataField::Title, self.title.clone().map(Some)),
            text(MetadataField::Artist, self.artist.clone()),
            text(MetadataField::Album, self.album.clone()),
            text(MetadataField::Genre, self.genre.clone()),
            text(MetadataField::MusicalKey, self.key.clone()),
            self.bpm.map(|bpm| {
                (
                    MetadataField::Bpm,
                    bpm.map(|bpm| MetadataValue::Decimal(bpm.to_string())),
                )
            }),
            count(MetadataField::ReleaseYear, self.year),
            count(MetadataField::TrackNumber, self.track_number),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

fn text(
    field: MetadataField,
    value: Option<Option<String>>,
) -> Option<(MetadataField, Option<MetadataValue>)> {
    Some((
        field,
        value?
            .filter(|value| !value.trim().is_empty())
            .map(MetadataValue::Text),
    ))
}

fn count(
    field: MetadataField,
    value: Option<Option<u32>>,
) -> Option<(MetadataField, Option<MetadataValue>)> {
    Some((
        field,
        value?.map(|value| MetadataValue::Integer(i64::from(value))),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn an_entry() -> LibraryEntry {
        let mut entry = LibraryEntry::new(Uuid::new_v4());
        entry.comment = Some("keeper".into());
        entry.rating = Some(4);
        entry
    }

    #[test]
    fn editing_only_the_rating_leaves_the_comment_alone() {
        let mut entry = an_entry();
        let edit = MetadataEdit {
            rating: Some(Some(5)),
            ..MetadataEdit::default()
        };

        edit.apply_to_entry(&mut entry, Utc::now());

        assert_eq!(entry.rating, Some(5));
        assert_eq!(entry.comment.as_deref(), Some("keeper"));
    }

    #[test]
    fn editing_only_the_comment_leaves_the_rating_alone() {
        let mut entry = an_entry();
        let edit = MetadataEdit {
            comment: Some(Some("revised".into())),
            ..MetadataEdit::default()
        };

        edit.apply_to_entry(&mut entry, Utc::now());

        assert_eq!(entry.comment.as_deref(), Some("revised"));
        assert_eq!(entry.rating, Some(4));
    }

    /// Clearing is a real edit and must be distinguishable from not editing.
    #[test]
    fn clearing_a_field_removes_only_that_field() {
        let mut entry = an_entry();
        let edit = MetadataEdit {
            comment: Some(None),
            ..MetadataEdit::default()
        };

        edit.apply_to_entry(&mut entry, Utc::now());

        assert_eq!(entry.comment, None);
        assert_eq!(entry.rating, Some(4));
    }

    #[test]
    fn an_edit_only_records_candidates_for_the_fields_the_editor_touched() {
        let edit = MetadataEdit {
            title: Some("New title".into()),
            album: Some(None),
            ..MetadataEdit::default()
        };

        assert_eq!(
            edit.candidates(),
            vec![
                (
                    MetadataField::Title,
                    Some(MetadataValue::Text("New title".into()))
                ),
                (MetadataField::Album, None),
            ]
        );
    }

    /// The user's own record is not canonical metadata, so it never becomes a
    /// candidate for reconciliation.
    #[test]
    fn a_rating_or_comment_never_becomes_a_metadata_candidate() {
        let edit = MetadataEdit {
            comment: Some(Some("keeper".into())),
            rating: Some(Some(5)),
            ..MetadataEdit::default()
        };

        assert!(edit.candidates().is_empty());
        assert!(edit.touches_library_entry());
    }

    #[test]
    fn an_untouched_editor_submission_changes_nothing() {
        let edit = MetadataEdit::from_editor(MetadataFields::default(), false);

        assert!(edit.candidates().is_empty());
        assert!(!edit.touches_library_entry());
    }
}
