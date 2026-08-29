//! The library's health, as the Doctor last reported it.
//!
//! [`rec_core::Doctor`] decides what is wrong with a track; this module holds what it
//! found and keeps that answer current as tracks change. Every result arrives through
//! [`Diagnostics::record`], so the list the Doctor view reads and the per-track index
//! the Library view badges from can never disagree.

use crate::state::projection::CatalogProjection;
use dioxus::prelude::*;
use rec_core::{Diagnosis, Doctor};
use std::collections::HashMap;
use std::sync::Arc;
use ui::TrackIssueInfo;
use uuid::Uuid;

/// What the Doctor last found, indexed for the views that render it.
#[derive(Clone, Copy)]
pub struct Diagnostics {
    projection: CatalogProjection,
    doctor: Signal<Arc<Doctor>>,
    diagnoses: Signal<Vec<Diagnosis>>,
    by_track: Signal<HashMap<Uuid, Vec<Diagnosis>>>,
    scanning: Signal<bool>,
    scanned_count: Signal<usize>,
}

impl Diagnostics {
    /// Diagnostics over a projection, before anything has been scanned.
    pub fn new(projection: CatalogProjection) -> Self {
        Self {
            projection,
            doctor: Signal::new(Arc::new(Doctor::new())),
            diagnoses: Signal::new(Vec::new()),
            by_track: Signal::new(HashMap::new()),
            scanning: Signal::new(false),
            scanned_count: Signal::new(0),
        }
    }

    /// The Doctor running the checks, for views that describe what it looks for.
    pub fn doctor(&self) -> Arc<Doctor> {
        self.doctor.read().clone()
    }

    /// Everything wrong with the library, as of the last scan.
    pub fn diagnoses(&self) -> Vec<Diagnosis> {
        self.diagnoses.read().clone()
    }

    /// How many issues the library has.
    pub fn issue_count(&self) -> usize {
        self.diagnoses.read().len()
    }

    /// How many issues can be fixed automatically.
    pub fn fixable_count(&self) -> usize {
        self.diagnoses.read().iter().filter(|d| d.fixable).count()
    }

    /// Whether a scan is running.
    pub fn scanning(&self) -> bool {
        *self.scanning.read()
    }

    /// How many tracks the last scan covered.
    pub fn scanned_count(&self) -> usize {
        *self.scanned_count.read()
    }

    /// What is wrong with one track.
    pub fn for_track(&self, track_id: Uuid) -> Vec<Diagnosis> {
        self.by_track
            .read()
            .get(&track_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Every track's issues, summarised for the track list's badges.
    pub fn issues_by_track(&self) -> HashMap<Uuid, TrackIssueInfo> {
        self.by_track
            .read()
            .iter()
            .map(|(track_id, diagnoses)| {
                (
                    *track_id,
                    TrackIssueInfo {
                        count: diagnoses.len(),
                        fixable_count: diagnoses.iter().filter(|d| d.fixable).count(),
                        summary: diagnoses
                            .iter()
                            .map(|d| d.message.as_str())
                            .collect::<Vec<_>>()
                            .join("; "),
                    },
                )
            })
            .collect()
    }

    /// Run every check over the whole library.
    pub fn scan(mut self) {
        self.scanning.set(true);
        self.scanned_count.set(0);

        let tracks = self.projection.tracks();
        let found = self.doctor().check_tracks(&tracks);

        let mut by_track: HashMap<Uuid, Vec<Diagnosis>> = HashMap::new();
        for diagnosis in &found {
            by_track
                .entry(diagnosis.track_id)
                .or_default()
                .push(diagnosis.clone());
        }

        self.diagnoses.set(found);
        self.by_track.set(by_track);
        self.scanned_count.set(tracks.len());
        self.scanning.set(false);
    }

    /// Check one track again, after something about it changed.
    pub fn recheck(self, track_id: Uuid) {
        let Some(track) = self.projection.track(track_id) else {
            return;
        };
        let found = self.doctor().check_track(&track);
        self.record(track_id, found);
    }

    /// Fix one diagnosis and re-check the track it was about.
    pub fn fix(self, track_id: Uuid, check_name: &str) -> Result<(), String> {
        let Some(track) = self.projection.track(track_id) else {
            return Err("Track not found".to_string());
        };
        let diagnosis = self
            .diagnoses
            .read()
            .iter()
            .find(|d| d.track_id == track_id && d.check_name == check_name)
            .cloned();
        let Some(diagnosis) = diagnosis else {
            return Err("Diagnosis not found".to_string());
        };

        let doctor = self.doctor();
        doctor
            .fix_diagnosis(&diagnosis, &track)
            .map_err(|error| error.to_string())?;
        self.record(track_id, doctor.check_track(&track));
        Ok(())
    }

    /// Fix every fixable issue one check found.
    pub fn fix_all_by_check(self, check_name: &str) {
        let to_fix: Vec<(Uuid, String)> = self
            .diagnoses
            .read()
            .iter()
            .filter(|d| d.check_name == check_name && d.fixable)
            .map(|d| (d.track_id, d.check_name.clone()))
            .collect();
        self.fix_each(to_fix)
    }

    /// Fix every fixable issue one track has.
    pub fn fix_all_for_track(self, track_id: Uuid) {
        let to_fix: Vec<(Uuid, String)> = self
            .for_track(track_id)
            .into_iter()
            .filter(|d| d.fixable)
            .map(|d| (d.track_id, d.check_name))
            .collect();
        self.fix_each(to_fix)
    }

    /// Fix each named issue, leaving in place any the check refuses.
    ///
    /// A failure stays visible as the diagnosis it already is, so there is nothing for
    /// a caller to do with a count it would only discard.
    fn fix_each(self, to_fix: Vec<(Uuid, String)>) {
        for (track_id, check_name) in to_fix {
            if let Err(error) = self.fix(track_id, &check_name) {
                tracing::warn!(%track_id, check = %check_name, %error, "could not fix a diagnosis");
            }
        }
    }

    /// Replace what is known about one track's health.
    ///
    /// The flat list and the per-track index are two readings of one answer, so they
    /// are only ever written together — including the case where a track becomes clean
    /// and has to leave the index rather than linger with an empty entry.
    fn record(mut self, track_id: Uuid, found: Vec<Diagnosis>) {
        let mut diagnoses = self.diagnoses.write();
        diagnoses.retain(|d| d.track_id != track_id);
        diagnoses.extend(found.iter().cloned());
        drop(diagnoses);

        let mut by_track = self.by_track.write();
        if found.is_empty() {
            by_track.remove(&track_id);
        } else {
            by_track.insert(track_id, found);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::with_runtime;
    use rec_core::{Diagnosis, DiagnosticCheck, DoctorError, Severity, Track};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// A check that flags the tracks it was told to, and stops flagging one once it has
    /// been asked to fix it.
    struct FlagCheck {
        name: String,
        fixable: bool,
        flagged: Mutex<HashSet<Uuid>>,
        fixes: Mutex<Vec<Uuid>>,
        refuse_fix: bool,
    }

    impl FlagCheck {
        fn new(name: &str, flagged: impl IntoIterator<Item = Uuid>) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_string(),
                fixable: true,
                flagged: Mutex::new(flagged.into_iter().collect()),
                fixes: Mutex::new(Vec::new()),
                refuse_fix: false,
            })
        }

        fn unfixable(name: &str, flagged: impl IntoIterator<Item = Uuid>) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_string(),
                fixable: false,
                flagged: Mutex::new(flagged.into_iter().collect()),
                fixes: Mutex::new(Vec::new()),
                refuse_fix: false,
            })
        }

        fn stubborn(name: &str, flagged: impl IntoIterator<Item = Uuid>) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_string(),
                fixable: true,
                flagged: Mutex::new(flagged.into_iter().collect()),
                fixes: Mutex::new(Vec::new()),
                refuse_fix: true,
            })
        }

        fn fixes(&self) -> Vec<Uuid> {
            self.fixes.lock().unwrap().clone()
        }
    }

    impl DiagnosticCheck for FlagCheck {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "flags whatever it was told to"
        }

        fn check(&self, track: &Track) -> Option<Diagnosis> {
            if !self.flagged.lock().unwrap().contains(&track.id) {
                return None;
            }
            Some(Diagnosis {
                check_name: self.name.clone(),
                track_id: track.id,
                track_path: track.path.clone(),
                severity: Severity::Warning,
                message: format!("{} is unhappy", self.name),
                fixable: self.fixable,
            })
        }

        fn fix(&self, track: &Track) -> Result<(), DoctorError> {
            self.fixes.lock().unwrap().push(track.id);
            if self.refuse_fix {
                return Err(DoctorError::FixNotSupported);
            }
            self.flagged.lock().unwrap().remove(&track.id);
            Ok(())
        }

        fn can_fix(&self) -> bool {
            self.fixable
        }
    }

    fn a_track(title: &str) -> Track {
        let mut track = Track::new_without_metadata(PathBuf::from(format!("/music/{title}.mp3")));
        track.title = title.to_string();
        track
    }

    /// Diagnostics over `tracks`, running only `checks` — the built-in duplicate check
    /// is library-wide and would otherwise flag tracks these tests never mention.
    fn diagnosing<T>(
        tracks: Vec<Track>,
        checks: Vec<Arc<dyn DiagnosticCheck>>,
        test: impl FnOnce(Diagnostics) -> T,
    ) -> T {
        with_runtime(|| {
            let projection = CatalogProjection::new_in_scope(tracks);
            let mut doctor = Doctor::with_checks(checks);
            doctor.set_check_duplicates(false);
            test(Diagnostics {
                projection,
                doctor: Signal::new_in_scope(Arc::new(doctor), ScopeId::ROOT),
                diagnoses: Signal::new_in_scope(Vec::new(), ScopeId::ROOT),
                by_track: Signal::new_in_scope(HashMap::new(), ScopeId::ROOT),
                scanning: Signal::new_in_scope(false, ScopeId::ROOT),
                scanned_count: Signal::new_in_scope(0, ScopeId::ROOT),
            })
        })
    }

    #[test]
    fn a_scan_reports_every_flagged_track_and_how_many_it_covered() {
        let tracks = vec![a_track("one"), a_track("two")];
        let flagged = tracks[0].id;
        diagnosing(
            tracks,
            vec![FlagCheck::new("tags", [flagged])],
            |diagnostics| {
                diagnostics.scan();

                assert_eq!(diagnostics.issue_count(), 1);
                assert_eq!(diagnostics.scanned_count(), 2);
                assert!(!diagnostics.scanning());
                assert_eq!(diagnostics.for_track(flagged).len(), 1);
            },
        )
    }

    /// The flat list and the per-track index are two readings of one answer.
    #[test]
    fn fixing_a_track_clears_it_from_both_the_list_and_the_index() {
        let tracks = vec![a_track("one"), a_track("two")];
        let (fixed, still_broken) = (tracks[0].id, tracks[1].id);
        let check = FlagCheck::new("tags", [fixed, still_broken]);
        diagnosing(tracks, vec![check.clone()], |diagnostics| {
            diagnostics.scan();

            diagnostics.fix(fixed, "tags").unwrap();

            assert_eq!(check.fixes(), vec![fixed]);
            assert!(diagnostics.for_track(fixed).is_empty());
            assert!(!diagnostics.issues_by_track().contains_key(&fixed));
            assert_eq!(diagnostics.issue_count(), 1);
            assert_eq!(diagnostics.diagnoses()[0].track_id, still_broken);
        })
    }

    #[test]
    fn rechecking_a_track_replaces_only_that_tracks_findings() {
        let tracks = vec![a_track("one"), a_track("two")];
        let (changed, untouched) = (tracks[0].id, tracks[1].id);
        let check = FlagCheck::new("tags", [changed, untouched]);
        diagnosing(tracks, vec![check.clone()], |diagnostics| {
            diagnostics.scan();
            check.flagged.lock().unwrap().remove(&changed);

            diagnostics.recheck(changed);

            assert!(diagnostics.for_track(changed).is_empty());
            assert_eq!(diagnostics.for_track(untouched).len(), 1);
        })
    }

    #[test]
    fn fixing_everything_one_check_found_skips_the_issues_it_cannot_fix() {
        let tracks = vec![a_track("one"), a_track("two")];
        let (fixable_track, unfixable_track) = (tracks[0].id, tracks[1].id);
        let fixable = FlagCheck::new("tags", [fixable_track]);
        let unfixable = FlagCheck::unfixable("duplicates", [unfixable_track]);
        let checks = vec![
            fixable.clone() as Arc<dyn DiagnosticCheck>,
            unfixable.clone(),
        ];
        diagnosing(tracks, checks, |diagnostics| {
            diagnostics.scan();

            diagnostics.fix_all_by_check("tags");
            diagnostics.fix_all_by_check("duplicates");

            assert_eq!(fixable.fixes(), vec![fixable_track]);
            assert!(unfixable.fixes().is_empty());
            assert_eq!(diagnostics.issue_count(), 1);
            assert_eq!(diagnostics.diagnoses()[0].track_id, unfixable_track);
        })
    }

    #[test]
    fn a_fix_that_fails_leaves_the_issue_in_place() {
        let tracks = vec![a_track("one")];
        let track_id = tracks[0].id;
        diagnosing(
            tracks,
            vec![FlagCheck::stubborn("tags", [track_id])],
            |diagnostics| {
                diagnostics.scan();

                diagnostics.fix_all_for_track(track_id);

                assert_eq!(diagnostics.for_track(track_id).len(), 1);
            },
        )
    }

    #[test]
    fn a_tracks_badge_summarises_its_issues_and_how_many_are_fixable() {
        let tracks = vec![a_track("one")];
        let track_id = tracks[0].id;
        let checks = vec![
            FlagCheck::new("tags", [track_id]) as Arc<dyn DiagnosticCheck>,
            FlagCheck::unfixable("duplicates", [track_id]),
        ];
        diagnosing(tracks, checks, |diagnostics| {
            diagnostics.scan();

            let badge = diagnostics.issues_by_track().remove(&track_id).unwrap();

            assert_eq!(badge.count, 2);
            assert_eq!(badge.fixable_count, 1);
            assert_eq!(badge.summary, "tags is unhappy; duplicates is unhappy");
        })
    }

    #[test]
    fn fixing_a_track_the_projection_no_longer_holds_is_reported() {
        diagnosing(Vec::new(), Vec::new(), |diagnostics| {
            assert_eq!(
                diagnostics.fix(Uuid::new_v4(), "tags"),
                Err("Track not found".to_string())
            );
        })
    }
}
