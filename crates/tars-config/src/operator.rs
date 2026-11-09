//! Lays the operator sections (`[llm.retry]` and the like) of every config file over their owners'
//! defaults, and says where each effective value came from.
//!
//! It knows the layers, the section names and the merge rule, and nothing of any section's fields:
//! a section's type, its one default and its validation belong to the crate that owns it, and a
//! composition root names that type when it takes the section. The owner never sees a layer or a
//! source, only the merged value.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;
use toml::{Table, Value};

/// A config file layer, in the order a later one overrides an earlier one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileLayer {
    /// The platform's file, read only when a path is given (`TARS_SYSTEM_CONFIG`).
    System,
    /// `<home>/config.toml`.
    User,
}

impl fmt::Display for FileLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::System => "system",
            Self::User => "user",
        })
    }
}

/// Where an effective setting came from, as PostgreSQL's `pg_settings.source` / `sourcefile`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingSource {
    /// No file set it, so it is the owner type's `Default`; `owner` is that type's path.
    Default {
        owner: &'static str,
    },
    File {
        layer: FileLayer,
        path: PathBuf,
    },
}

impl fmt::Display for SettingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default { owner } => write!(f, "default ({owner})"),
            Self::File { layer, path } => write!(f, "{layer} file {}", path.display()),
        }
    }
}

/// The operator sections tars knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    LlmRetry,
    LlmCircuitBreaker,
    LlmHttp,
    LlmReserve,
}

impl Section {
    pub const ALL: [Section; 4] = [
        Self::LlmRetry,
        Self::LlmCircuitBreaker,
        Self::LlmHttp,
        Self::LlmReserve,
    ];

    /// Returns the section's dotted path, its TOML table name.
    pub const fn path(self) -> &'static str {
        match self {
            Self::LlmRetry => "llm.retry",
            Self::LlmCircuitBreaker => "llm.circuit_breaker",
            Self::LlmHttp => "llm.http",
            Self::LlmReserve => "llm.reserve",
        }
    }

    /// Returns the section named `name` inside the `[llm]` table.
    fn in_llm(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|s| s.path().strip_prefix("llm.") == Some(name))
    }
}

impl fmt::Display for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.path())
    }
}

/// One effective setting, as `tars config explain` prints it.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveSetting {
    /// The full dotted key, e.g. `llm.retry.max_attempts`.
    pub key: String,
    pub value: Value,
    pub source: SettingSource,
}

impl fmt::Display for EffectiveSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {}  # {}", self.key, self.value, self.source)
    }
}

/// A section's merged value with the source of each of its settings.
#[derive(Debug)]
pub struct Effective<T> {
    value: T,
    settings: Vec<EffectiveSetting>,
}

impl<T> Effective<T> {
    pub fn into_value(self) -> T {
        self.value
    }

    /// Returns one setting per leaf of the value, sorted by key.
    pub fn settings(&self) -> &[EffectiveSetting] {
        &self.settings
    }
}

/// The operator sections one file wrote.
#[derive(Debug)]
struct FileSections {
    layer: FileLayer,
    path: PathBuf,
    sections: BTreeMap<Section, Table>,
    /// The keys under `[llm]` that are no section tars knows, or not a table.
    misfits: Vec<Misfit>,
}

#[derive(Debug)]
enum Misfit {
    Unknown(String),
    NotATable(Section),
}

/// The operator sections of every loaded file, waiting for a composition root to take them.
///
/// A root takes each section it uses once with [`OperatorLayers::take`], then calls
/// [`OperatorLayers::finish`], which fails when a file wrote a section nobody took: an operator
/// who set a value the process does not use is told, not ignored.
#[derive(Debug)]
pub struct OperatorLayers {
    files: Vec<FileSections>,
    taken: BTreeSet<Section>,
}

impl OperatorLayers {
    /// Returns the layers of `files`, given in override order with each file's `[llm]` table.
    pub(crate) fn new(files: Vec<(FileLayer, PathBuf, Option<Table>)>) -> Self {
        let files = files
            .into_iter()
            .map(|(layer, path, llm)| {
                let (sections, misfits) = classify(llm.unwrap_or_default());
                FileSections {
                    layer,
                    path,
                    sections,
                    misfits,
                }
            })
            .collect();
        Self {
            files,
            taken: BTreeSet::new(),
        }
    }

    /// Returns `section` merged from the owner's default and every file that wrote it, a later
    /// layer overriding an earlier one key by key.
    ///
    /// # Errors
    ///
    /// Returns every file whose section does not deserialize as `T` (an unknown key, a wrong
    /// type, a value the owner refuses), the merged value when only it does not, and a second
    /// take of one section.
    pub fn take<T>(&mut self, section: Section) -> Result<Effective<T>, SectionErrors>
    where
        T: DeserializeOwned + Serialize + Default,
    {
        if !self.taken.insert(section) {
            return Err(SectionErrors::one(SectionError::TakenTwice { section }));
        }
        let owner = std::any::type_name::<T>();
        let written: Vec<(&FileSections, &Table)> = self
            .files
            .iter()
            .filter_map(|f| f.sections.get(&section).map(|table| (f, table)))
            .collect();

        let refused = written.iter().filter_map(|(file, table)| {
            deserialize::<T>(table)
                .err()
                .map(|source| SectionError::Parse {
                    section,
                    layer: file.layer,
                    path: file.path.clone(),
                    source: Box::new(source),
                })
        });
        if let Some(errors) = SectionErrors::collect(refused) {
            return Err(errors);
        }

        let default = default_table(T::default()).map_err(|source| {
            SectionErrors::one(SectionError::DefaultUnwritable {
                section,
                owner,
                source: Box::new(source),
            })
        })?;
        let merged = written.iter().fold(
            Merged::of(default, SettingSource::Default { owner }),
            |merged, (file, table)| {
                let source = SettingSource::File {
                    layer: file.layer,
                    path: file.path.clone(),
                };
                merged.overlay(table, &source)
            },
        );
        let value = deserialize::<T>(&merged.table).map_err(|source| {
            SectionErrors::one(SectionError::Invalid {
                section,
                source: Box::new(source),
            })
        })?;
        Ok(Effective {
            value,
            settings: merged.settings(section),
        })
    }

    /// Ends the assembly.
    ///
    /// # Errors
    ///
    /// Returns every section a file wrote that nobody took, and every key under `[llm]` that is
    /// no section tars knows.
    pub fn finish(self) -> Result<(), SectionErrors> {
        let taken = self.taken;
        let errors: Vec<SectionError> = self
            .files
            .into_iter()
            .flat_map(|file| {
                let path = file.path;
                let not_taken = file
                    .sections
                    .into_keys()
                    .filter(|s| !taken.contains(s))
                    .map({
                        let path = path.clone();
                        move |section| SectionError::NotTaken {
                            section,
                            path: path.clone(),
                        }
                    });
                let misfits = file.misfits.into_iter().map(move |m| match m {
                    Misfit::Unknown(key) => SectionError::UnknownSection {
                        key,
                        path: path.clone(),
                    },
                    Misfit::NotATable(section) => SectionError::NotATable {
                        section,
                        path: path.clone(),
                    },
                });
                not_taken.chain(misfits).collect::<Vec<_>>()
            })
            .collect();
        SectionErrors::collect(errors).map_or(Ok(()), Err)
    }
}

/// Splits a file's `[llm]` table into the sections it names and the keys that are none.
fn classify(llm: Table) -> (BTreeMap<Section, Table>, Vec<Misfit>) {
    let mut sections = BTreeMap::new();
    let mut misfits = Vec::new();
    for (name, value) in llm {
        match (Section::in_llm(&name), value) {
            (Some(section), Value::Table(table)) => {
                sections.insert(section, table);
            }
            (Some(section), _) => misfits.push(Misfit::NotATable(section)),
            (None, _) => misfits.push(Misfit::Unknown(format!("llm.{name}"))),
        }
    }
    (sections, misfits)
}

fn deserialize<T: DeserializeOwned>(table: &Table) -> Result<T, toml::de::Error> {
    Value::Table(table.clone()).try_into()
}

/// Returns `value` written as a TOML table.
fn default_table<T: Serialize>(value: T) -> Result<Table, DefaultUnwritableError> {
    match Value::try_from(value) {
        Ok(Value::Table(table)) => Ok(table),
        Ok(other) => Err(DefaultUnwritableError::NotATable {
            found: other.type_str(),
        }),
        Err(source) => Err(DefaultUnwritableError::Serialize(source)),
    }
}

/// Says an owner's `Default` does not write as a TOML table, a bug in the owner's serde.
#[derive(Debug, thiserror::Error)]
pub enum DefaultUnwritableError {
    #[error("it writes as a TOML {found}, not a table")]
    NotATable { found: &'static str },
    #[error(transparent)]
    Serialize(toml::ser::Error),
}

/// A section's table with the source of each leaf, keyed by the leaf's path inside the section.
struct Merged {
    table: Table,
    sources: BTreeMap<String, SettingSource>,
}

impl Merged {
    /// Returns `table` with every leaf from `source`.
    fn of(table: Table, source: SettingSource) -> Self {
        let sources = leaves("", &table)
            .map(|(key, _)| (key, source.clone()))
            .collect();
        Self { table, sources }
    }

    /// Returns this table with `over` laid on it: two tables merge key by key, anything else
    /// replaces what was there whole (an array is not concatenated). Every leaf `over` wrote takes
    /// `source`, and the leaves of a subtree it replaced lose theirs.
    fn overlay(self, over: &Table, source: &SettingSource) -> Self {
        let Self { table, sources } = self;
        let (table, sources) = overlay_at("", table, sources, over, source);
        Self { table, sources }
    }

    /// Returns one setting per leaf, sorted by key, each under `section`'s path.
    fn settings(self, section: Section) -> Vec<EffectiveSetting> {
        let Self { table, mut sources } = self;
        leaves("", &table)
            .filter_map(|(key, value)| {
                sources.remove(&key).map(|source| EffectiveSetting {
                    key: format!("{}.{key}", section.path()),
                    value: value.clone(),
                    source,
                })
            })
            .collect()
    }
}

fn overlay_at(
    prefix: &str,
    mut base: Table,
    mut sources: BTreeMap<String, SettingSource>,
    over: &Table,
    source: &SettingSource,
) -> (Table, BTreeMap<String, SettingSource>) {
    for (name, value) in over {
        let key = join(prefix, name);
        match (base.remove(name), value) {
            (Some(Value::Table(below)), Value::Table(above)) => {
                let (merged, merged_sources) = overlay_at(&key, below, sources, above, source);
                sources = merged_sources;
                base.insert(name.clone(), Value::Table(merged));
            }
            _ => {
                let nested = format!("{key}.");
                sources.retain(|k, _| *k != key && !k.starts_with(&nested));
                sources.extend(single_or_leaves(&key, value).map(|(k, _)| (k, source.clone())));
                base.insert(name.clone(), value.clone());
            }
        }
    }
    (base, sources)
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

/// Returns `value` as the leaves under `key`: itself when it is not a table.
fn single_or_leaves<'a>(
    key: &str,
    value: &'a Value,
) -> Box<dyn Iterator<Item = (String, &'a Value)> + 'a> {
    match value {
        Value::Table(table) => leaves(key, table),
        leaf => Box::new(std::iter::once((key.to_string(), leaf))),
    }
}

/// Returns every non-table value under `table`, with its dotted path from `prefix`.
fn leaves<'a>(
    prefix: &str,
    table: &'a Table,
) -> Box<dyn Iterator<Item = (String, &'a Value)> + 'a> {
    let prefix = prefix.to_string();
    Box::new(
        table
            .iter()
            .flat_map(move |(name, value)| single_or_leaves(&join(&prefix, name), value)),
    )
}

/// One way the operator sections fail to assemble; the words are what the operator reads.
#[derive(Debug, thiserror::Error)]
pub enum SectionError {
    #[error("[{section}] in the {layer} file {}", path.display())]
    Parse {
        section: Section,
        layer: FileLayer,
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("[{section}] as the files merge it")]
    Invalid {
        section: Section,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error(
        "[{section}] in {} is not used by this process; remove it, or set it where a process \
         that uses it reads",
        path.display()
    )]
    NotTaken { section: Section, path: PathBuf },
    #[error(
        "[{key}] in {} is not an operator section; the sections are {}",
        path.display(),
        known_sections()
    )]
    UnknownSection { key: String, path: PathBuf },
    #[error("`{section}` in {} must be a table, as [{section}]", path.display())]
    NotATable { section: Section, path: PathBuf },
    /// Means the composition root took one section twice, a bug in the root.
    #[error("[{section}] was taken twice")]
    TakenTwice { section: Section },
    /// Means the owner's `Default` does not write as TOML, a bug in the owner's serde.
    #[error("the default of [{section}] ({owner})")]
    DefaultUnwritable {
        section: Section,
        owner: &'static str,
        #[source]
        source: Box<DefaultUnwritableError>,
    },
}

fn known_sections() -> String {
    Section::ALL
        .iter()
        .map(|s| s.path())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The failures of one assembly, displayed one per line. It is never empty.
#[derive(Debug)]
pub struct SectionErrors(Vec<SectionError>);

impl SectionErrors {
    fn one(error: SectionError) -> Self {
        Self(vec![error])
    }

    /// Returns `errors` as one value, or `None` when there are none.
    fn collect(errors: impl IntoIterator<Item = SectionError>) -> Option<Self> {
        let errors: Vec<_> = errors.into_iter().collect();
        if errors.is_empty() {
            None
        } else {
            Some(Self(errors))
        }
    }

    /// Returns both values, or the failures of both: a root that takes several sections reports
    /// every failure at once instead of stopping at the first.
    ///
    /// # Errors
    ///
    /// Returns the failures of `a` and of `b`, whichever failed.
    pub fn zip<A, B>(
        a: Result<A, SectionErrors>,
        b: Result<B, SectionErrors>,
    ) -> Result<(A, B), SectionErrors> {
        match (a, b) {
            (Ok(a), Ok(b)) => Ok((a, b)),
            (Err(mut a), Err(b)) => {
                a.0.extend(b.0);
                Err(a)
            }
            (Err(e), Ok(_)) | (Ok(_), Err(e)) => Err(e),
        }
    }

    pub fn as_slice(&self) -> &[SectionError] {
        &self.0
    }
}

impl fmt::Display for SectionErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for error in &self.0 {
            if !first {
                f.write_str("\n")?;
            }
            first = false;
            // A TOML error ends its message with a newline of its own.
            write!(f, "  {}", tars_utils::display_chain(error).trim_end())?;
        }
        Ok(())
    }
}

impl std::error::Error for SectionErrors {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn table(text: &str) -> Table {
        text.parse().expect("test TOML parses")
    }

    fn file(layer: FileLayer, path: &str) -> SettingSource {
        SettingSource::File {
            layer,
            path: PathBuf::from(path),
        }
    }

    const DEFAULT: SettingSource = SettingSource::Default { owner: "T" };

    fn sources_of(merged: &Merged) -> Vec<(&str, &SettingSource)> {
        merged
            .sources
            .iter()
            .map(|(k, s)| (k.as_str(), s))
            .collect()
    }

    #[test]
    fn a_leaf_over_a_leaf_takes_the_upper_value_and_source() {
        let user = file(FileLayer::User, "/u");
        let merged = Merged::of(table("a = 1\nb = 2"), DEFAULT).overlay(&table("a = 5"), &user);
        assert_eq!(merged.table, table("a = 5\nb = 2"));
        assert_eq!(sources_of(&merged), [("a", &user), ("b", &DEFAULT)]);
    }

    #[test]
    fn a_layer_that_writes_the_default_value_is_still_the_source() {
        let user = file(FileLayer::User, "/u");
        let merged = Merged::of(table("a = 1"), DEFAULT).overlay(&table("a = 1"), &user);
        assert_eq!(sources_of(&merged), [("a", &user)]);
    }

    #[test]
    fn the_last_layer_to_write_a_key_is_its_source() {
        let system = file(FileLayer::System, "/s");
        let user = file(FileLayer::User, "/u");
        let merged = Merged::of(table("a = 1\nb = 2"), DEFAULT)
            .overlay(&table("a = 2\nb = 3"), &system)
            .overlay(&table("a = 3"), &user);
        assert_eq!(merged.table, table("a = 3\nb = 3"));
        assert_eq!(sources_of(&merged), [("a", &user), ("b", &system)]);
    }

    #[test]
    fn two_tables_merge_key_by_key() {
        let user = file(FileLayer::User, "/u");
        let merged =
            Merged::of(table("[t]\nx = 1\ny = 2"), DEFAULT).overlay(&table("[t]\nx = 9"), &user);
        assert_eq!(merged.table, table("[t]\nx = 9\ny = 2"));
        assert_eq!(sources_of(&merged), [("t.x", &user), ("t.y", &DEFAULT)]);
    }

    #[test]
    fn a_leaf_over_a_table_drops_the_tables_leaves() {
        let user = file(FileLayer::User, "/u");
        let merged =
            Merged::of(table("[t]\nx = 1\ny = 2"), DEFAULT).overlay(&table("t = 5"), &user);
        assert_eq!(merged.table, table("t = 5"));
        assert_eq!(sources_of(&merged), [("t", &user)]);
    }

    #[test]
    fn a_table_over_a_leaf_brings_its_own_leaves() {
        let user = file(FileLayer::User, "/u");
        let merged = Merged::of(table("t = 5"), DEFAULT).overlay(&table("[t]\nx = 1"), &user);
        assert_eq!(sources_of(&merged), [("t.x", &user)]);
    }

    #[test]
    fn an_array_is_replaced_whole_not_concatenated() {
        let user = file(FileLayer::User, "/u");
        let merged = Merged::of(table("a = [1, 2]"), DEFAULT).overlay(&table("a = [3]"), &user);
        assert_eq!(merged.table, table("a = [3]"));
        assert_eq!(sources_of(&merged), [("a", &user)]);
    }

    #[test]
    fn every_leaf_of_the_merged_table_has_exactly_one_setting() {
        let merged = Merged::of(table("a = 1\n[t]\nx = 1"), DEFAULT)
            .overlay(&table("b = 2\n[t]\ny = 2"), &file(FileLayer::User, "/u"));
        let keys: Vec<String> = merged
            .settings(Section::LlmRetry)
            .into_iter()
            .map(|s| s.key)
            .collect();
        assert_eq!(
            keys,
            [
                "llm.retry.a",
                "llm.retry.b",
                "llm.retry.t.x",
                "llm.retry.t.y"
            ]
        );
    }

    #[test]
    fn a_section_name_is_known_only_inside_llm() {
        assert_eq!(Section::in_llm("retry"), Some(Section::LlmRetry));
        assert_eq!(
            Section::in_llm("circuit_breaker"),
            Some(Section::LlmCircuitBreaker)
        );
        assert_eq!(Section::in_llm("retri"), None);
        assert_eq!(Section::in_llm("llm.retry"), None);
    }

    #[test]
    fn a_misspelt_section_and_a_non_table_section_are_misfits() {
        let (sections, misfits) = classify(table("retri = {}\nhttp = 3\n[reserve]\n"));
        assert_eq!(
            sections.into_keys().collect::<Vec<_>>(),
            [Section::LlmReserve]
        );
        assert!(matches!(
            misfits.as_slice(),
            [Misfit::NotATable(Section::LlmHttp), Misfit::Unknown(k)] if k == "llm.retri"
        ));
    }

    /// A stand-in owner type: tars-config never sees a real section's type.
    #[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(default, deny_unknown_fields)]
    struct Knobs {
        tries: u32,
        wait_ms: u64,
    }

    fn layers(files: &[(FileLayer, &str, &str)]) -> OperatorLayers {
        OperatorLayers::new(
            files
                .iter()
                .map(|(layer, path, llm)| (*layer, PathBuf::from(path), Some(table(llm))))
                .collect(),
        )
    }

    #[test]
    fn a_section_no_file_writes_is_the_owners_default_from_the_default() {
        let mut ops = layers(&[(FileLayer::User, "/u", "")]);
        let taken = ops.take::<Knobs>(Section::LlmRetry).expect("takes");
        assert!(taken.settings().iter().all(
            |s| matches!(s.source, SettingSource::Default { owner } if owner.ends_with("Knobs"))
        ));
        assert_eq!(taken.into_value(), Knobs::default());
        ops.finish().expect("nothing was left");
    }

    #[test]
    fn the_user_layer_overrides_the_system_layer_key_by_key() {
        let mut ops = layers(&[
            (FileLayer::System, "/s", "[retry]\ntries = 2\nwait_ms = 9"),
            (FileLayer::User, "/u", "[retry]\ntries = 5"),
        ]);
        let taken = ops.take::<Knobs>(Section::LlmRetry).expect("takes");
        let sources: Vec<(&str, &SettingSource)> = taken
            .settings()
            .iter()
            .map(|s| (s.key.as_str(), &s.source))
            .collect();
        assert_eq!(
            sources,
            [
                ("llm.retry.tries", &file(FileLayer::User, "/u")),
                ("llm.retry.wait_ms", &file(FileLayer::System, "/s")),
            ]
        );
        assert_eq!(
            taken.into_value(),
            Knobs {
                tries: 5,
                wait_ms: 9
            }
        );
    }

    #[test]
    fn every_file_whose_section_does_not_read_is_reported_with_its_path() {
        let mut ops = layers(&[
            (FileLayer::System, "/s", "[retry]\ntriez = 2"),
            (FileLayer::User, "/u", "[retry]\ntries = \"many\""),
        ]);
        let errors = ops.take::<Knobs>(Section::LlmRetry).unwrap_err();
        assert!(matches!(
            errors.as_slice(),
            [
                SectionError::Parse {
                    layer: FileLayer::System,
                    ..
                },
                SectionError::Parse {
                    layer: FileLayer::User,
                    ..
                },
            ]
        ));
        let message = errors.to_string();
        assert!(
            message.contains("triez") && message.contains("/s"),
            "{message}"
        );
    }

    #[test]
    fn a_section_written_but_not_taken_fails_finish_with_its_file() {
        let mut ops = layers(&[(FileLayer::System, "/s", "[retry]\n[reserve]\n")]);
        ops.take::<Knobs>(Section::LlmRetry).expect("takes");
        let errors = ops.finish().unwrap_err();
        assert!(matches!(
            errors.as_slice(),
            [SectionError::NotTaken { section: Section::LlmReserve, path }] if path == Path::new("/s")
        ));
    }

    #[test]
    fn finish_reports_a_misspelt_section_and_one_not_taken_together() {
        let ops = layers(&[(FileLayer::User, "/u", "[retri]\n[http]\n")]);
        let errors = ops.finish().unwrap_err();
        assert!(matches!(
            errors.as_slice(),
            [
                SectionError::NotTaken { section: Section::LlmHttp, .. },
                SectionError::UnknownSection { key, .. },
            ] if key == "llm.retri"
        ));
    }

    #[test]
    fn a_section_taken_twice_is_refused() {
        let mut ops = layers(&[]);
        ops.take::<Knobs>(Section::LlmRetry).expect("first take");
        let errors = ops.take::<Knobs>(Section::LlmRetry).unwrap_err();
        assert!(matches!(
            errors.as_slice(),
            [SectionError::TakenTwice {
                section: Section::LlmRetry
            }]
        ));
    }

    #[test]
    fn zip_keeps_the_failures_of_both_sides() {
        let fail = |s| Err::<(), _>(SectionErrors::one(SectionError::TakenTwice { section: s }));
        let both = SectionErrors::zip(fail(Section::LlmRetry), fail(Section::LlmHttp)).unwrap_err();
        assert_eq!(both.as_slice().len(), 2);
        let one =
            SectionErrors::zip(Ok::<_, SectionErrors>(1), fail(Section::LlmHttp)).unwrap_err();
        assert_eq!(one.as_slice().len(), 1);
        assert_eq!(
            SectionErrors::zip(Ok::<_, SectionErrors>(1), Ok::<_, SectionErrors>(2)).ok(),
            Some((1, 2))
        );
    }
}
