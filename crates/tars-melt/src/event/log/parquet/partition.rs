//! One partition directory: its `_live.json` manifest, its `flock`, and
//! everything that changes what the manifest accounts for — publishing,
//! merging, and cleanup — each change made while holding that lock.
//!
//! Where a data file sits follows from the manifest: a live file is
//! `<file>` (`*.parquet`), a retired one `<file>.retired`, which a glob of
//! `*.parquet` does not match, and a file the manifest no longer accounts for
//! is gone. So outside a locked step the directory's `*.parquet` is the live
//! set. A locked step renames a file in before the manifest names it and
//! renames a file out after the manifest drops it, so a file the manifest
//! lists live always has its own name; a crash between the two leaves an
//! extra `*.parquet`, which the next locked pass deletes or hides
//! ([`Cleanup`]).
//!
//! A sealed file becomes visible in one locked step: rename `.tmp` →
//! `.parquet`, then add it to the manifest. Another writer's `.tmp` may go
//! once it is older than `grace`, which a live writer's `.tmp` never is. The
//! upkeeping writer's own `.tmp`s — sealed or merged — go unless they are in
//! flight ([`InFlight`]): a seal or merge runs beside the upkeep, so its
//! `.tmp` may be on its way to publication at any age, while a failed one's
//! `.tmp` is swept by the next pass.
//! A merge takes the same shape as a seal: its plan is read under the lock,
//! its output written to the merger's own `.tmp` outside it, and under the
//! lock again the output is renamed in, the manifest replaced whole, and
//! the inputs renamed to their retired names — or, if another merge took an
//! input in meanwhile, the `.tmp` is dropped. So a long merge never holds
//! up a publish, and a crash leaves only files the next pass puts right.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tars_types::UtcDay;
use uuid::Uuid;

use super::super::LogError;
use super::super::sink::lock;
use super::compact::{CompactRule, CompactionPlan, DataFile, FileState, Manifest, plan};
use super::file;
use super::{DATE_PREFIX, ParquetTable, Round, WriterId, is_gone};

const MANIFEST: &str = "_live.json";
const MANIFEST_TMP: &str = "_live.json.tmp";
const LOCK: &str = ".lock";
const TMP_EXT: &str = "tmp";

/// How old a partition's day is in one round of upkeep, which decides what the pass over it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DayAge {
    /// Today, or a day the clock cannot place: tiered merges only.
    Current,
    /// A day before today: every file merges into one, and with `freeze` what is left is frozen
    /// for the bucket.
    Past { freeze: bool },
}

impl DayAge {
    fn is_past(self) -> bool {
        matches!(self, Self::Past { .. })
    }
}

/// A `.tmp` in a partition directory: `<writer>-<stem>.tmp` when a writer of this crate named it,
/// so a pass can tell its own writer's residue, gone at any age, from another writer's.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct TmpFile {
    /// `None` for a `.tmp` whose name does not start with a writer's id.
    writer: Option<WriterId>,
    stem: String,
}

impl TmpFile {
    pub(super) fn of(writer: WriterId, stem: impl std::fmt::Display) -> Self {
        Self {
            writer: Some(writer),
            stem: stem.to_string(),
        }
    }

    /// Reads a directory entry's name as a `.tmp`; `None` for any other entry.
    pub(super) fn parse(name: &str) -> Option<Self> {
        let stem = name.strip_suffix(TMP_EXT)?.strip_suffix('.')?;
        let writer = stem
            .get(..UUID_TEXT_LEN)
            .and_then(|id| Uuid::parse_str(id).ok())
            .zip(
                stem.get(UUID_TEXT_LEN..)
                    .and_then(|rest| rest.strip_prefix('-')),
            );
        Some(match writer {
            Some((id, rest)) => Self {
                writer: Some(WriterId(id)),
                stem: rest.to_owned(),
            },
            None => Self {
                writer: None,
                stem: stem.to_owned(),
            },
        })
    }

    pub(super) fn name(&self) -> String {
        match self.writer {
            Some(writer) => format!("{}-{}.{TMP_EXT}", writer.0, self.stem),
            None => format!("{}.{TMP_EXT}", self.stem),
        }
    }
}

/// The length of a hyphenated UUID, the writer id a `.tmp`'s name starts with.
const UUID_TEXT_LEN: usize = 36;

/// The `.tmp`s one writer has in flight — its seals' and merges', written
/// outside the lock, not yet published or landed — shared by its sealer and
/// all its upkeep passes: the only `.tmp`s of its own its sweep spares.
#[derive(Debug, Clone, Default)]
pub(super) struct InFlight(Arc<Mutex<BTreeSet<TmpFile>>>);

impl InFlight {
    /// Claims `tmp` as in flight until the returned guard drops, whether it was published,
    /// landed, went stale or failed. It is claimed before the `.tmp` is written, so a sweep that
    /// lists it finds it claimed.
    pub(super) fn claim(&self, tmp: TmpFile) -> Claim<'_> {
        lock(&self.0).insert(tmp.clone());
        Claim {
            in_flight: self,
            tmp,
        }
    }

    fn held(&self) -> BTreeSet<TmpFile> {
        lock(&self.0).clone()
    }
}

pub(super) struct Claim<'a> {
    in_flight: &'a InFlight,
    tmp: TmpFile,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        lock(&self.in_flight.0).remove(&self.tmp);
    }
}

/// One partition directory, `<table dir>/date=<day>`, with the day its name states.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct PartitionDir {
    dir: PathBuf,
    day: UtcDay,
}

impl PartitionDir {
    /// Returns the partition of `day` under a tenant's table directory.
    pub(super) fn new(table_dir: &Path, day: UtcDay) -> Self {
        Self {
            dir: table_dir.join(format!("{DATE_PREFIX}{day}")),
            day,
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.dir
    }

    pub(super) fn day(&self) -> UtcDay {
        self.day
    }

    /// Returns where `tmp` is written before it is published.
    pub(super) fn tmp_path(&self, tmp: &TmpFile) -> PathBuf {
        self.dir.join(tmp.name())
    }

    pub(super) fn create(&self) -> Result<(), LogError> {
        fs::create_dir_all(&self.dir).map_err(io_at(&self.dir))
    }

    /// Returns the published files, in publication order. A partition nothing has
    /// been sealed into has no manifest: that is an empty log, not an error.
    pub(super) fn live_files(&self) -> Result<Vec<PathBuf>, LogError> {
        let manifest = self.manifest()?;
        Ok(manifest
            .live
            .iter()
            .map(|f| self.dir.join(f.file.as_str()))
            .collect())
    }

    /// Makes the sealed `tmp` visible as `file`, under the partition lock so
    /// concurrent writers never lose an entry.
    pub(super) fn publish(&self, tmp: &TmpFile, file: DataFile) -> Result<(), LogError> {
        let _lock = self.lock()?;

        self.rename_into(&self.tmp_path(tmp), &file)?;
        let manifest = self.manifest()?.register(file);
        self.write_manifest(&manifest)
    }

    /// Merges whatever `round.compact` says is due for a day of `age`, freezes what is left if
    /// `age` says so, then forgets retired files past `grace` and sweeps the files nothing
    /// accounts for. A failed merge leaves the manifest as it was, and nothing frozen.
    pub(super) fn maintain<T: ParquetTable>(
        &self,
        round: &Round,
        age: DayAge,
    ) -> Result<(), LogError> {
        self.maintain_by(round, age, file::merge::<T>)
    }

    /// Maintains as [`Self::maintain`] does; `merge`, which writes a plan's inputs into one
    /// `.tmp`, is a parameter so a test can hold a merge at the point where it runs outside the
    /// lock.
    pub(super) fn maintain_by(
        &self,
        round: &Round,
        age: DayAge,
        merge: impl Fn(&[PathBuf], &Path) -> Result<(), LogError>,
    ) -> Result<(), LogError> {
        if let Some(rule) = round.compact {
            self.compact(rule, round, age, &merge)?;
        }
        if age == (DayAge::Past { freeze: true }) {
            self.freeze()?;
        }
        self.tidy(round)
    }

    /// Freezes every live file a merge may still take in.
    fn freeze(&self) -> Result<(), LogError> {
        let _lock = self.lock()?;

        match self.manifest()?.freeze() {
            Some(frozen) => self.write_manifest(&frozen),
            None => Ok(()),
        }
    }

    /// Returns the frozen files the bucket has not confirmed.
    pub(super) fn pending_uploads(&self) -> Result<Vec<DataFile>, LogError> {
        let manifest = self.manifest()?;
        Ok(manifest.pending_uploads().into_iter().cloned().collect())
    }

    /// Returns the path of the live file `file`.
    pub(super) fn file_path(&self, file: &DataFile) -> PathBuf {
        self.dir.join(file.as_str())
    }

    /// Records, under the lock, that the bucket holds `file`.
    pub(super) fn mark_uploaded(&self, file: &DataFile) -> Result<(), LogError> {
        let _lock = self.lock()?;

        let marked = self.manifest()?.uploaded(file);
        self.write_manifest(&marked)
    }

    /// Merges until no plan is due. A landed merge shrinks the live set, and
    /// a dropped one means another merge shrank it, so this ends.
    fn compact(
        &self,
        rule: CompactRule,
        round: &Round,
        age: DayAge,
        merge: &impl Fn(&[PathBuf], &Path) -> Result<(), LogError>,
    ) -> Result<(), LogError> {
        while let Some(plan) = self.next_plan(rule, age.is_past())? {
            self.merge(plan, round, merge)?;
        }
        Ok(())
    }

    fn next_plan(
        &self,
        rule: CompactRule,
        day_is_past: bool,
    ) -> Result<Option<CompactionPlan>, LogError> {
        let _lock = self.lock()?;
        Ok(plan(&self.manifest()?.live, rule, day_is_past))
    }

    /// Runs one merge. The output is written and checked outside the lock, where publishing goes
    /// on; inputs are safe to read there, being deleted only `grace` after they are retired,
    /// unless the merge outlasts `grace`, and then an input found gone is a plan gone stale, not a
    /// failure.
    fn merge(
        &self,
        plan: CompactionPlan,
        round: &Round,
        merge: &impl Fn(&[PathBuf], &Path) -> Result<(), LogError>,
    ) -> Result<(), LogError> {
        let output = plan.output();
        let tmp_file = round.writer.merge_tmp(&output);
        let tmp = self.tmp_path(&tmp_file);
        let _in_flight = round.in_flight.claim(tmp_file);
        let inputs: Vec<PathBuf> = plan.inputs.iter().map(|f| self.file_path(f)).collect();

        match merge(&inputs, &tmp) {
            Err(e) if is_gone(&e) => self.give_up_if_stale(&plan, e),
            written => {
                written?;
                self.land(plan, &tmp, output, round.now)
            }
        }
    }

    /// Decides, under the lock as at landing, what a missing input means: an input gone because
    /// another merge took it in is a stale plan, given up; gone while still live, it is `missing`
    /// itself. What the merge wrote is swept once it is no longer in flight.
    fn give_up_if_stale(&self, plan: &CompactionPlan, missing: LogError) -> Result<(), LogError> {
        let _lock = self.lock()?;

        if self.manifest()?.is_stale(plan) {
            Ok(())
        } else {
            Err(missing)
        }
    }

    /// Lands a merge under the lock: the output is renamed in, the manifest replaced, and only
    /// then are the inputs renamed to their retired names. A file the manifest lists live thus
    /// always has its own name, which planning and merging rely on; for the moment between the
    /// rename and the last input's retirement, a glob of `*.parquet` counts the merged rows twice.
    /// A crash in that moment leaves an unregistered output or a retired input under its own name,
    /// and the next pass deletes or hides it. A reader of the old manifest finds an input at its
    /// retired name. A plan gone stale lands nothing, and its `.tmp` goes.
    fn land(
        &self,
        plan: CompactionPlan,
        tmp: &Path,
        output: DataFile,
        now: SystemTime,
    ) -> Result<(), LogError> {
        let _lock = self.lock()?;

        let inputs = plan.inputs.clone();
        let target = output.clone();
        match self.manifest()?.retire(plan, output, now) {
            Some(merged) => {
                self.rename_into(tmp, &target)?;
                self.write_manifest(&merged)?;
                inputs
                    .iter()
                    .try_for_each(|input| self.rename_in_dir(input.as_str(), &input.retired_name()))
            }
            None => fs::remove_file(tmp).map_err(io_at(tmp)),
        }
    }

    /// Returns when the next retired file outlives `grace`, which is when the pass that deletes it
    /// is due.
    pub(super) fn next_expiry(
        &self,
        grace: std::time::Duration,
    ) -> Result<Option<SystemTime>, LogError> {
        Ok(self.manifest()?.next_expiry(grace))
    }

    /// Forgets retired files past `grace`, then puts every file where the manifest says, in one
    /// locked pass.
    fn tidy(&self, round: &Round) -> Result<(), LogError> {
        let _lock = self.lock()?;

        let manifest = self.expire(self.manifest()?, round)?;
        self.sweep(&manifest, round)
    }

    /// Drops the retired files past `grace` from the manifest; the caller holds the lock.
    fn expire(&self, manifest: Manifest, round: &Round) -> Result<Manifest, LogError> {
        match manifest.without_expired(round.grace, round.now) {
            None => Ok(manifest),
            Some(kept) => {
                self.write_manifest(&kept)?;
                Ok(kept)
            }
        }
    }

    /// Renames `tmp` to `file`. The caller holds the lock, so a renamed file is registered before
    /// anyone sees it.
    fn rename_into(&self, tmp: &Path, file: &DataFile) -> Result<(), LogError> {
        let target = self.file_path(file);
        fs::rename(tmp, &target).map_err(io_at(&target))
    }

    /// Renames the entry `from` of this directory to `to`; the caller holds the lock.
    fn rename_in_dir(&self, from: &str, to: &str) -> Result<(), LogError> {
        let (from, to) = (self.dir.join(from), self.dir.join(to));
        fs::rename(&from, &to).map_err(io_at(&to))
    }

    /// Blocks until this process holds the partition's `flock`; dropping the
    /// returned file releases it.
    pub(super) fn lock(&self) -> Result<File, LogError> {
        let path = self.dir.join(LOCK);
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(io_at(&path))?;
        fs4::FileExt::lock(&file).map_err(io_at(&path))?;
        Ok(file)
    }

    fn manifest(&self) -> Result<Manifest, LogError> {
        let path = self.dir.join(MANIFEST);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Manifest::default()),
            Err(e) => return Err(io_at(&path)(e)),
        };
        serde_json::from_slice(&bytes).map_err(|source| LogError::Manifest { path, source })
    }

    /// Replaces the manifest whole (write, fsync, rename), so a reader sees the old manifest or the
    /// new one, never half of one.
    fn write_manifest(&self, manifest: &Manifest) -> Result<(), LogError> {
        let tmp = self.dir.join(MANIFEST_TMP);
        let bytes = serde_json::to_vec(manifest).map_err(|source| LogError::Manifest {
            path: tmp.clone(),
            source,
        })?;
        fs::write(&tmp, bytes).map_err(io_at(&tmp))?;
        File::open(&tmp)
            .and_then(|f| f.sync_all())
            .map_err(io_at(&tmp))?;
        let path = self.dir.join(MANIFEST);
        fs::rename(&tmp, &path).map_err(io_at(&path))
    }

    /// Deletes the crash residue and moves each data file to where the manifest puts it; the
    /// caller holds the lock. What is in flight is read after the listing: a seal or merge claims
    /// its `.tmp` before writing it, and cannot publish or land it while the lock is held, so a
    /// listed `.tmp` still in flight is still claimed.
    fn sweep(&self, manifest: &Manifest, round: &Round) -> Result<(), LogError> {
        let found = self.entries()?;
        let in_flight = round.in_flight.held();
        cleanups(&found, manifest, round, &in_flight)
            .into_iter()
            .try_for_each(|cleanup| match cleanup {
                Cleanup::Delete(name) => {
                    let path = self.dir.join(name);
                    fs::remove_file(&path).map_err(io_at(&path))
                }
                Cleanup::Rename { from, to } => self.rename_in_dir(from, &to),
            })
    }

    fn entries(&self) -> Result<Vec<Entry>, LogError> {
        fs::read_dir(&self.dir)
            .map_err(io_at(&self.dir))?
            .map(|entry| {
                let entry = entry.map_err(io_at(&self.dir))?;
                let path = entry.path();
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map_err(io_at(&path))?;
                Ok(Entry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    modified,
                })
            })
            .collect()
    }
}

/// A directory entry as the sweep decision sees it.
struct Entry {
    name: String,
    modified: SystemTime,
}

/// Returns the entry name the manifest's `state` of `file` puts it at, or `None` when it should not
/// be on disk: renamed in and the writer died before registering it, or retired past `grace`.
fn wanted_name(file: &DataFile, state: FileState) -> Option<String> {
    match state {
        FileState::Live => Some(file.as_str().to_owned()),
        FileState::Retired => Some(file.retired_name()),
        FileState::Unlisted => None,
    }
}

/// Returns the data file a directory entry named `name` is, at its own name or its retired one,
/// or `None` for an entry that is not a data file.
fn data_entry(name: &str) -> Option<DataFile> {
    DataFile::parse_retired(name).or_else(|| DataFile::parse(name))
}

/// What one pass does to one directory entry.
#[derive(Debug, PartialEq, Eq)]
enum Cleanup<'a> {
    Delete(&'a str),
    Rename { from: &'a str, to: String },
}

/// Returns what to do with each entry: delete another writer's `.tmp` older than `grace`, this
/// writer's `.tmp` not `in_flight` at any age, and a data file the manifest no longer accounts
/// for; rename a data file to where the manifest puts it, which only an interrupted locked step
/// leaves elsewhere.
fn cleanups<'a>(
    found: &'a [Entry],
    manifest: &Manifest,
    round: &Round,
    in_flight: &BTreeSet<TmpFile>,
) -> Vec<Cleanup<'a>> {
    let stale = |e: &Entry| {
        round
            .now
            .duration_since(e.modified)
            .is_ok_and(|age| age >= round.grace)
    };
    let cleanup = |e: &'a Entry| -> Option<Cleanup<'a>> {
        let name = e.name.as_str();
        if let Some(file) = data_entry(name) {
            return match wanted_name(&file, manifest.state(&file)) {
                None => Some(Cleanup::Delete(name)),
                Some(wanted) if wanted == name => None,
                Some(wanted) => Some(Cleanup::Rename {
                    from: name,
                    to: wanted,
                }),
            };
        }
        let tmp = TmpFile::parse(name)?;
        let own = tmp.writer == Some(round.writer);
        let residue = !in_flight.contains(&tmp) && (own || stale(e));
        residue.then_some(Cleanup::Delete(name))
    };
    found.iter().filter_map(cleanup).collect()
}

/// Reads a file a manifest listed live a moment ago with `read`: at its own name, or, when a merge
/// has retired it since, at its retired name, which it keeps until `grace` has passed. Gone from
/// both, it fails with the error of the first read.
pub(super) fn read_listed<R>(
    path: &Path,
    read: impl Fn(&Path) -> Result<R, LogError>,
) -> Result<R, LogError> {
    match read(path) {
        Err(gone) if is_gone(&gone) => {
            let mut retired = path.as_os_str().to_owned();
            retired.push(super::compact::RETIRED_SUFFIX);
            match read(Path::new(&retired)) {
                Err(e) if is_gone(&e) => Err(gone),
                other => other,
            }
        }
        found => found,
    }
}

pub(super) fn io_at(path: &Path) -> impl FnOnce(io::Error) -> LogError + '_ {
    move |source| LogError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod lock_bench;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::event::log::parquet::compact::RetiredFile;

    fn file(name: &str) -> DataFile {
        DataFile::parse(name).unwrap()
    }

    fn round(grace: Duration, now: SystemTime) -> Round {
        Round {
            compact: None,
            uploads: false,
            grace,
            now,
            writer: WriterId::new(),
            in_flight: InFlight::default(),
        }
    }

    fn entry(name: &str, modified: SystemTime) -> Entry {
        Entry {
            name: name.to_owned(),
            modified,
        }
    }

    fn deleted<'a>(cleanups: Vec<Cleanup<'a>>) -> Vec<&'a str> {
        cleanups
            .into_iter()
            .map(|c| match c {
                Cleanup::Delete(name) => name,
                Cleanup::Rename { from, to } => panic!("{from} renamed to {to}"),
            })
            .collect()
    }

    #[test]
    fn a_pass_deletes_old_tmp_and_unaccounted_data_and_hides_a_retired_parquet() {
        let now = SystemTime::now();
        let grace = Duration::from_secs(600);
        let old = now - grace;
        let young = now - Duration::from_secs(1);
        let manifest = Manifest {
            retired: vec![RetiredFile {
                file: file("e-0.parquet"),
                retired_at: old,
            }],
            ..Manifest::default().register(file("a-0.parquet"))
        };
        let upkeep = round(grace, now);
        let own_tmp = upkeep.writer.seal_tmp(0).name();
        let own_merge_tmp = upkeep
            .writer
            .merge_tmp(&file("L1-0123456789abcdef.parquet"))
            .name();
        let found = [
            entry("a-0.parquet", old),
            entry("b-0.parquet", young),
            entry("c-0.tmp", old),
            entry("d-0.tmp", young),
            entry("e-0.parquet", old),
            entry(MANIFEST, old),
            entry(LOCK, old),
            entry(&own_tmp, old),
            entry(&own_merge_tmp, old),
        ];

        let in_flight = BTreeSet::from([
            TmpFile::parse(&own_tmp).unwrap(),
            TmpFile::parse(&own_merge_tmp).unwrap(),
        ]);

        assert_eq!(
            cleanups(&found, &manifest, &upkeep, &in_flight),
            vec![
                Cleanup::Delete("b-0.parquet"),
                Cleanup::Delete("c-0.tmp"),
                Cleanup::Rename {
                    from: "e-0.parquet",
                    to: "e-0.parquet.retired".to_owned()
                },
            ],
            "the upkeeping writer's own .tmps, sealed or merged, however old, are on their way to publication"
        );
    }

    #[test]
    fn a_writers_own_merge_tmp_not_in_flight_is_residue_at_any_age() {
        let now = SystemTime::now();
        let upkeep = round(Duration::from_secs(600), now);
        let failed = upkeep
            .writer
            .merge_tmp(&file("L1-0123456789abcdef.parquet"))
            .name();
        let landing = upkeep
            .writer
            .merge_tmp(&file("L1-fedcba9876543210.parquet"))
            .name();
        let sealing = upkeep.writer.seal_tmp(7).name();
        let found = [
            entry(&failed, now),
            entry(&landing, now),
            entry(&sealing, now),
        ];
        let _landing = upkeep.in_flight.claim(
            upkeep
                .writer
                .merge_tmp(&file("L1-fedcba9876543210.parquet")),
        );
        let _sealing = upkeep.in_flight.claim(upkeep.writer.seal_tmp(7));

        assert_eq!(
            deleted(cleanups(
                &found,
                &Manifest::default(),
                &upkeep,
                &upkeep.in_flight.held()
            )),
            vec![failed.as_str()],
            "only the merge in flight, and the sealer's, are spared"
        );
    }

    #[test]
    fn a_writers_own_sealed_tmp_not_in_flight_is_residue_at_any_age() {
        let now = SystemTime::now();
        let upkeep = round(Duration::from_secs(600), now);
        let failed = upkeep.writer.seal_tmp(3).name();
        let sealing = upkeep.writer.seal_tmp(4).name();
        let found = [entry(&failed, now), entry(&sealing, now)];
        let _sealing = upkeep.in_flight.claim(upkeep.writer.seal_tmp(4));

        assert_eq!(
            deleted(cleanups(
                &found,
                &Manifest::default(),
                &upkeep,
                &upkeep.in_flight.held()
            )),
            vec![failed.as_str()],
            "a failed seal's .tmp is swept at once; the seal in flight is spared"
        );
    }

    #[test]
    fn the_manifest_puts_a_live_file_in_the_glob_a_retired_one_beside_it_and_an_unlisted_one_nowhere()
     {
        let a = file("a.parquet");
        assert_eq!(
            wanted_name(&a, FileState::Live).as_deref(),
            Some("a.parquet")
        );
        assert_eq!(
            wanted_name(&a, FileState::Retired).as_deref(),
            Some("a.parquet.retired")
        );
        assert_eq!(wanted_name(&a, FileState::Unlisted), None);
    }

    #[test]
    fn only_a_parquet_name_with_or_without_the_retired_suffix_is_a_data_file() {
        assert_eq!(data_entry("a.parquet"), Some(file("a.parquet")));
        assert_eq!(data_entry("a.parquet.retired"), Some(file("a.parquet")));
        assert_eq!(data_entry("a.retired"), None);
        assert_eq!(data_entry("a.parquet.tmp"), None);
        assert_eq!(data_entry("a.tmp"), None);
        assert_eq!(data_entry(MANIFEST), None);
        assert_eq!(data_entry(LOCK), None);
    }

    #[test]
    fn a_pass_moves_each_data_file_to_where_the_manifest_puts_it() {
        let now = SystemTime::now();
        let manifest = Manifest {
            retired: vec![
                RetiredFile {
                    file: file("hidden.parquet"),
                    retired_at: now,
                },
                RetiredFile {
                    file: file("shown.parquet"),
                    retired_at: now,
                },
            ],
            ..Manifest::default()
                .register(file("live.parquet"))
                .register(file("rolled-back.parquet"))
        };
        let upkeep = round(Duration::from_secs(600), now);
        let found = [
            entry("live.parquet", now),
            entry("rolled-back.parquet.retired", now),
            entry("hidden.parquet.retired", now),
            entry("shown.parquet", now),
            entry("expired.parquet.retired", now),
            entry("unregistered.parquet", now),
        ];

        assert_eq!(
            cleanups(&found, &manifest, &upkeep, &BTreeSet::new()),
            vec![
                Cleanup::Rename {
                    from: "rolled-back.parquet.retired",
                    to: "rolled-back.parquet".to_owned()
                },
                Cleanup::Rename {
                    from: "shown.parquet",
                    to: "shown.parquet.retired".to_owned()
                },
                Cleanup::Delete("expired.parquet.retired"),
                Cleanup::Delete("unregistered.parquet"),
            ]
        );
    }
}
