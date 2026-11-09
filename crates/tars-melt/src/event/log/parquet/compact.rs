//! Pure core of a partition's manifest: which files merge next, and what the
//! manifest becomes when a file is published, a merge lands, a past day is
//! frozen for the bucket, an upload is confirmed, or a retired file outlives
//! `grace`. Time arrives as an argument; nothing here does IO, and nothing
//! here knows the bucket.
//!
//! A level is explicit data in the manifest, never read off a file name.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Tiered compaction: once a level holds `fan_in` files, the oldest `fan_in` of them
/// merge into one file of the next level. Injected by whoever opens the
/// sink; the call log's is `CALL_LOG_COMPACTION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactRule {
    fan_in: usize,
}

impl CompactRule {
    /// Builds the rule; `at < 2`, which would merge a file into itself forever, fails the build
    /// in a `const`.
    pub const fn new(fan_in: usize) -> Self {
        assert!(fan_in >= 2, "a compaction fan-in must be at least 2");
        Self { fan_in }
    }

    /// Returns how many files one merge takes in, at least 2.
    #[cfg(test)]
    pub(super) const fn fan_in(self) -> usize {
        self.fan_in
    }
}

/// The files readers may see, and the merged-away files kept for `grace` so a
/// reader holding an older manifest can still open them. Adapter-private: the on-disk DTO of `_live.json`.
const DATA_EXT: &str = "parquet";
pub(super) const RETIRED_SUFFIX: &str = ".retired";

/// The name of a data file in a partition directory, as the manifest lists it: `<stem>.parquet`.
/// A sealed batch's stem is `<writer>-<seq>`, a merge's `L<n>-<sha16>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(super) struct DataFile(String);

impl DataFile {
    /// Returns the data file a `stem` is published as.
    pub(super) fn of_stem(stem: impl fmt::Display) -> Self {
        Self(format!("{stem}.{DATA_EXT}"))
    }

    /// Reads `name` as a data file's name; `None` unless it ends `.parquet`.
    pub(super) fn parse(name: &str) -> Option<Self> {
        let is_data = Path::new(name).extension().is_some_and(|x| x == DATA_EXT);
        is_data.then(|| Self(name.to_owned()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the name without `.parquet`.
    pub(super) fn stem(&self) -> &str {
        let ext = DATA_EXT.len() + 1;
        &self.0[..self.0.len() - ext]
    }

    /// Returns the name the file has once a merge retires it, which a glob of `*.parquet` does
    /// not match.
    pub(super) fn retired_name(&self) -> String {
        format!("{}{RETIRED_SUFFIX}", self.0)
    }

    /// Reads a directory entry's name as a data file at its retired name.
    pub(super) fn parse_retired(name: &str) -> Option<Self> {
        Self::parse(name.strip_suffix(RETIRED_SUFFIX)?)
    }
}

impl TryFrom<String> for DataFile {
    type Error = NotADataFile;

    fn try_from(name: String) -> Result<Self, NotADataFile> {
        Self::parse(&name).ok_or(NotADataFile(name))
    }
}

impl From<DataFile> for String {
    fn from(file: DataFile) -> Self {
        file.0
    }
}

/// A manifest names a file that does not end `.parquet`.
#[derive(Debug, thiserror::Error)]
#[error("{0:?} is not a `.parquet` data file name")]
pub(super) struct NotADataFile(String);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Manifest {
    pub(super) live: Vec<LiveFile>,
    pub(super) retired: Vec<RetiredFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct LiveFile {
    pub(super) file: DataFile,
    /// 0 for a sealed batch; a merge of level-n files is level n + 1.
    pub(super) level: u8,
    /// `None` while the file may still be merged; set once it is frozen for the bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) upload: Option<Upload>,
}

impl LiveFile {
    /// Returns whether a merge may take this file in: a frozen file never is, so the copy in the
    /// bucket stays the only copy of its rows there.
    fn merges(&self) -> bool {
        self.upload.is_none()
    }
}

/// Where a frozen file is on its way to the bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Upload {
    /// Frozen; the bucket has not confirmed it holds the file.
    Pending,
    /// The bucket holds the file.
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RetiredFile {
    pub(super) file: DataFile,
    pub(super) retired_at: SystemTime,
}

/// One merge: these live files become one file of `level`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompactionPlan {
    pub(super) inputs: Vec<DataFile>,
    pub(super) level: u8,
}

impl CompactionPlan {
    /// Returns `L<level>-<sha16 of the input names>.parquet`, so the same inputs always name
    /// the same output. Each merger writes it under its own `.tmp` name first
    /// (`WriterId::merge_tmp`), so a retry by that merger overwrites its own crash residue.
    pub(super) fn output(&self) -> DataFile {
        let names: Vec<&str> = self.inputs.iter().map(DataFile::as_str).collect();
        let digest = Sha256::digest(names.join("\n"));
        let sha16: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        DataFile::of_stem(format_args!("L{}-{sha16}", self.level))
    }
}

impl Manifest {
    /// Adds a freshly published level-0 file, visible from now on.
    pub(super) fn register(mut self, file: DataFile) -> Self {
        self.live.push(LiveFile {
            file,
            level: 0,
            upload: None,
        });
        self
    }

    /// Lands `plan`, written out as `output`: readers switch to it, its inputs are retired as of
    /// `now`. It returns `None` when an input is no longer
    /// live: the plan was made outside the lock, another merge has taken that input in since, and
    /// landing this output would repeat its rows.
    pub(super) fn retire(
        self,
        plan: CompactionPlan,
        output: DataFile,
        now: SystemTime,
    ) -> Option<Self> {
        if self.is_stale(&plan) {
            return None;
        }
        let merged: BTreeSet<&DataFile> = plan.inputs.iter().collect();
        let (gone, mut live): (Vec<LiveFile>, Vec<LiveFile>) = self
            .live
            .into_iter()
            .partition(|f| merged.contains(&f.file));

        live.push(LiveFile {
            file: output,
            level: plan.level,
            upload: None,
        });
        let retired = self
            .retired
            .into_iter()
            .chain(gone.into_iter().map(|f| RetiredFile {
                file: f.file,
                retired_at: now,
            }))
            .collect();
        Some(Self { live, retired })
    }

    /// Returns whether an input of `plan` may no longer be merged because, after the plan was made
    /// outside the lock, another merge took it in or a writer froze it.
    pub(super) fn is_stale(&self, plan: &CompactionPlan) -> bool {
        !plan
            .inputs
            .iter()
            .all(|input| self.live.iter().any(|f| &f.file == input && f.merges()))
    }

    /// Freezes every live file a merge may still take in, or returns `None` when there is none.
    pub(super) fn freeze(&self) -> Option<Self> {
        let open = self.live.iter().any(LiveFile::merges);
        open.then(|| Self {
            live: self
                .live
                .iter()
                .map(|f| LiveFile {
                    upload: f.upload.or(Some(Upload::Pending)),
                    ..f.clone()
                })
                .collect(),
            retired: self.retired.clone(),
        })
    }

    /// Returns the frozen files the bucket has not confirmed.
    pub(super) fn pending_uploads(&self) -> Vec<&DataFile> {
        self.live
            .iter()
            .filter(|f| f.upload == Some(Upload::Pending))
            .map(|f| &f.file)
            .collect()
    }

    /// Records that the bucket holds `file`; unless it is a live file still pending, the manifest
    /// is left as it is.
    pub(super) fn uploaded(mut self, file: &DataFile) -> Self {
        self.live
            .iter_mut()
            .filter(|f| f.file == *file && f.upload == Some(Upload::Pending))
            .for_each(|f| f.upload = Some(Upload::Done));
        self
    }

    /// Returns when the first retired file outlives `grace`, or `None` when none is retired.
    pub(super) fn next_expiry(&self, grace: Duration) -> Option<SystemTime> {
        self.retired
            .iter()
            .filter_map(|r| r.retired_at.checked_add(grace))
            .min()
    }

    /// Returns the manifest without the retired files older than `grace`, or `None` when none
    /// are. Their files then count as unregistered and are swept.
    pub(super) fn without_expired(&self, grace: Duration, now: SystemTime) -> Option<Self> {
        let (expired, kept): (Vec<&RetiredFile>, Vec<&RetiredFile>) =
            self.retired.iter().partition(|r| {
                now.duration_since(r.retired_at)
                    .is_ok_and(|age| age >= grace)
            });
        (!expired.is_empty()).then(|| Self {
            live: self.live.clone(),
            retired: kept.into_iter().cloned().collect(),
        })
    }

    /// Returns what this manifest says of the file named `file`.
    pub(super) fn state(&self, file: &DataFile) -> FileState {
        if self.live.iter().any(|f| f.file == *file) {
            FileState::Live
        } else if self.retired.iter().any(|r| r.file == *file) {
            FileState::Retired
        } else {
            FileState::Unlisted
        }
    }
}

/// What a manifest says of one data file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileState {
    Live,
    /// Merged away, and kept for a reader of an older manifest until `grace` has passed.
    Retired,
    /// Neither live nor retired: never registered, or retired past `grace`.
    Unlisted,
}

/// Returns the next merge `live` is due, if any, among the files a merge may take in. A past day
/// merges into one file (no row can be sealed into it any more, bar a late one); otherwise the
/// lowest level holding `rule.fan_in` files merges its oldest `rule.fan_in`.
pub(super) fn plan(
    live: &[LiveFile],
    rule: CompactRule,
    day_is_past: bool,
) -> Option<CompactionPlan> {
    let open: Vec<&LiveFile> = live.iter().filter(|f| f.merges()).collect();
    if day_is_past {
        whole_day(&open)
    } else {
        tiered(&open, rule)
    }
}

fn whole_day(live: &[&LiveFile]) -> Option<CompactionPlan> {
    let top = live.iter().map(|f| f.level).max()?;
    (live.len() > 1).then(|| CompactionPlan {
        inputs: live.iter().map(|f| f.file.clone()).collect(),
        level: top.saturating_add(1),
    })
}

fn tiered(live: &[&LiveFile], rule: CompactRule) -> Option<CompactionPlan> {
    let at_level = |level: u8| live.iter().filter(move |f| f.level == level);
    let levels: BTreeSet<u8> = live.iter().map(|f| f.level).collect();
    let full = levels
        .into_iter()
        .find(|&level| at_level(level).count() >= rule.fan_in)?;
    Some(CompactionPlan {
        inputs: at_level(full)
            .take(rule.fan_in)
            .map(|f| f.file.clone())
            .collect(),
        level: full.saturating_add(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(stem: &str) -> DataFile {
        DataFile::of_stem(stem)
    }

    fn stems(files: Vec<&DataFile>) -> Vec<&str> {
        files.into_iter().map(DataFile::stem).collect()
    }

    fn live(files: &[(&str, u8)]) -> Vec<LiveFile> {
        files
            .iter()
            .map(|&(name, level)| LiveFile {
                file: file(name),
                level,
                upload: None,
            })
            .collect()
    }

    fn names(plan: &CompactionPlan) -> Vec<&str> {
        plan.inputs.iter().map(DataFile::stem).collect()
    }

    const FOUR: CompactRule = CompactRule::new(4);

    #[test]
    fn below_the_fan_in_nothing_merges() {
        let files = live(&[("a", 0), ("b", 0), ("c", 0), ("x", 1)]);
        assert_eq!(plan(&files, FOUR, false), None);
    }

    #[test]
    fn the_lowest_full_level_merges_its_oldest_files_into_the_next() {
        let files = live(&[
            ("y1", 1),
            ("a", 0),
            ("y2", 1),
            ("b", 0),
            ("y3", 1),
            ("c", 0),
            ("y4", 1),
            ("d", 0),
            ("e", 0),
        ]);
        let p = plan(&files, FOUR, false).unwrap();
        assert_eq!(names(&p), vec!["a", "b", "c", "d"]);
        assert_eq!(p.level, 1);

        let files = live(&[("y1", 1), ("y2", 1), ("y3", 1), ("y4", 1), ("a", 0)]);
        let p = plan(&files, FOUR, false).unwrap();
        assert_eq!(names(&p), vec!["y1", "y2", "y3", "y4"]);
        assert_eq!(p.level, 2);
    }

    #[test]
    fn a_past_day_merges_every_file_into_one() {
        let files = live(&[("a", 0), ("y", 2)]);
        let p = plan(&files, FOUR, true).unwrap();
        assert_eq!(names(&p), vec!["a", "y"]);
        assert_eq!(p.level, 3);
        assert_eq!(plan(&live(&[("y", 2)]), FOUR, true), None);
        assert_eq!(plan(&[], FOUR, true), None);
    }

    #[test]
    fn retire_swaps_inputs_for_the_output_and_remembers_when() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let manifest = Manifest::default()
            .register(file("a"))
            .register(file("b"))
            .register(file("c"));
        let p = CompactionPlan {
            inputs: vec![file("a"), file("b")],
            level: 1,
        };

        let merged = manifest.retire(p, file("L1-x"), now).unwrap();

        assert_eq!(merged.live, live(&[("c", 0), ("L1-x", 1)]));
        let retired: Vec<(&str, SystemTime)> = merged
            .retired
            .iter()
            .map(|r| (r.file.stem(), r.retired_at))
            .collect();
        assert_eq!(retired, vec![("a", now), ("b", now)]);
        assert_eq!(merged.state(&file("a")), FileState::Retired);
        assert_eq!(merged.state(&file("c")), FileState::Live);
        assert_eq!(merged.state(&file("L1-x")), FileState::Live);
        assert_eq!(merged.state(&file("d")), FileState::Unlisted);
    }

    #[test]
    fn a_plan_whose_input_left_live_since_it_was_made_lands_nothing() {
        let now = SystemTime::UNIX_EPOCH;
        let p = || CompactionPlan {
            inputs: vec![file("a"), file("b")],
            level: 1,
        };
        let before = Manifest::default().register(file("a")).register(file("b"));
        let landed = before.retire(p(), file("L1-x"), now).unwrap();

        assert_eq!(landed.clone().retire(p(), file("L1-x"), now), None);
        let partly = Manifest::default().register(file("a"));
        assert_eq!(partly.retire(p(), file("L1-x"), now), None);
    }

    #[test]
    fn retired_files_expire_only_once_grace_has_passed() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let grace = Duration::from_secs(600);
        let manifest = Manifest {
            live: live(&[("L1-x", 1)]),
            retired: vec![
                RetiredFile {
                    file: file("a"),
                    retired_at: t0,
                },
                RetiredFile {
                    file: file("b"),
                    retired_at: t0 + Duration::from_secs(1),
                },
            ],
        };

        assert_eq!(
            manifest.without_expired(grace, t0 + grace - Duration::from_secs(1)),
            None
        );
        let kept = manifest.without_expired(grace, t0 + grace).unwrap();
        assert_eq!(kept.live, manifest.live);
        let files: Vec<&str> = kept.retired.iter().map(|r| r.file.stem()).collect();
        assert_eq!(files, vec!["b"]);
    }

    #[test]
    fn the_next_expiry_is_when_the_oldest_retired_file_outlives_grace() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let grace = Duration::from_secs(600);
        let retired = |file: &str, retired_at| RetiredFile {
            file: DataFile::of_stem(file),
            retired_at,
        };
        let manifest = Manifest {
            retired: vec![retired("b", t0 + Duration::from_secs(5)), retired("a", t0)],
            ..Manifest::default()
        };

        assert_eq!(manifest.next_expiry(grace), Some(t0 + grace));
        let kept = manifest.without_expired(grace, t0 + grace).unwrap();
        assert_eq!(
            kept.next_expiry(grace),
            Some(t0 + Duration::from_secs(5) + grace)
        );
        assert_eq!(Manifest::default().next_expiry(grace), None);
    }

    #[test]
    fn the_output_name_is_a_function_of_the_level_and_inputs() {
        let p = |inputs: &[&str], level| CompactionPlan {
            inputs: inputs.iter().map(|s| file(s)).collect(),
            level,
        };
        let output = p(&["a", "b"], 1).output();
        let stem = output.stem();
        assert!(
            stem.starts_with("L1-") && stem.len() == "L1-".len() + 16,
            "{stem}"
        );
        assert_eq!(output, p(&["a", "b"], 1).output());
        assert_ne!(output, p(&["a", "c"], 1).output());
    }

    fn frozen(files: &[(&str, u8, Option<Upload>)]) -> Vec<LiveFile> {
        files
            .iter()
            .map(|&(file, level, upload)| LiveFile {
                upload,
                ..live(&[(file, level)]).remove(0)
            })
            .collect()
    }

    const PENDING: Option<Upload> = Some(Upload::Pending);
    const DONE: Option<Upload> = Some(Upload::Done);

    #[test]
    fn a_frozen_file_is_no_tiered_merge_input() {
        let open = frozen(&[("a", 0, None), ("b", 0, None)]);
        let two = CompactRule::new(2);
        assert!(plan(&open, two, false).is_some());

        let one_frozen = frozen(&[("a", 0, None), ("b", 0, PENDING)]);
        assert_eq!(plan(&one_frozen, two, false), None);
    }

    #[test]
    fn a_frozen_file_is_no_whole_day_merge_input() {
        let files = frozen(&[("a", 0, None), ("b", 0, None), ("y", 2, DONE)]);
        let p = plan(&files, FOUR, true).unwrap();
        assert_eq!(names(&p), vec!["a", "b"]);
        assert_eq!(p.level, 1, "the frozen file's level does not count");
    }

    #[test]
    fn one_open_file_beside_frozen_ones_is_no_whole_day_merge() {
        let files = frozen(&[("late", 0, None), ("y", 2, DONE), ("z", 0, PENDING)]);
        assert_eq!(plan(&files, FOUR, true), None);
    }

    #[test]
    fn freezing_makes_every_open_file_pending_and_leaves_frozen_ones() {
        let manifest = Manifest {
            live: frozen(&[("a", 0, None), ("y", 2, DONE), ("b", 1, None)]),
            ..Manifest::default()
        };

        let frozen_manifest = manifest.freeze().unwrap();

        assert_eq!(stems(frozen_manifest.pending_uploads()), vec!["a", "b"]);
        assert_eq!(frozen_manifest.live[1].upload, DONE);
        assert_eq!(frozen_manifest.freeze(), None, "nothing is left to freeze");
        assert_eq!(Manifest::default().freeze(), None);
    }

    #[test]
    fn shipped_marks_a_pending_file_done_and_nothing_else() {
        let manifest = Manifest {
            live: frozen(&[("a", 0, PENDING), ("b", 0, None)]),
            ..Manifest::default()
        };

        let marked = manifest.clone().uploaded(&file("a"));
        assert_eq!(marked.live[0].upload, DONE);
        assert!(marked.pending_uploads().is_empty());

        assert_eq!(
            manifest.clone().uploaded(&file("gone")),
            manifest,
            "not live"
        );
        assert_eq!(
            manifest.clone().uploaded(&file("b")),
            manifest,
            "not frozen"
        );
    }

    #[test]
    fn a_plan_whose_input_was_frozen_since_lands_nothing() {
        let manifest = Manifest::default().register(file("a")).register(file("b"));
        let p = plan(&manifest.live, CompactRule::new(2), true).unwrap();
        assert!(!manifest.is_stale(&p));

        let frozen_since = manifest.freeze().unwrap();

        assert!(frozen_since.is_stale(&p));
        assert_eq!(
            frozen_since.retire(p, file("L1-x"), SystemTime::UNIX_EPOCH),
            None
        );
    }

    #[test]
    fn a_manifest_with_nothing_frozen_writes_no_ship_field() {
        let manifest = Manifest::default().register(file("a"));
        let open = serde_json::to_string(&manifest).unwrap();
        assert!(!open.contains("upload"), "{open}");

        let pending = serde_json::to_string(&manifest.freeze().unwrap()).unwrap();
        let read: Manifest = serde_json::from_str(&pending).unwrap();
        assert_eq!(stems(read.pending_uploads()), vec!["a"]);
        let old: Manifest = serde_json::from_str(&open).unwrap();
        assert_eq!(
            old.live[0].upload, None,
            "a manifest without the field is open"
        );
    }
}
