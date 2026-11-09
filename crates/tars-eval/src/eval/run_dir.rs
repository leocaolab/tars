//! Holds the layout of a run directory: which file each fact of a run lives in. A run owns its
//! directory whole, so it is written only into a new or empty one, and nothing an earlier run
//! left can be read as this run's; `judge` adds its report beside the manifest. It does not know
//! how a case is called, scored or judged.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{
    Case, CaseId, CaseRun, EXPECTED_FILE, FileError, FileOp, INPUT_FILE, JudgeRunError, Manifest,
    RunEvalError, SYSTEM_FILE, ensure_dir, io_error, read_json, read_optional_text, read_text,
    write_pretty_json, write_text,
};
use crate::{JudgeItem, JudgeReport, VendorCheck};

const MANIFEST_FILE: &str = "manifest.json";
const JUDGE_REPORT_FILE: &str = "judge_report.json";
const OUTPUT_FILE: &str = "output.txt";
const CASE_REPORT_FILE: &str = "report.json";

/// The `judge_report.json` a judge pass writes: the report, and what was known of the judge's
/// vendor against the run's. A reader of the report alone ignores `vendors`.
#[derive(Serialize)]
struct JudgeReportFile<'a> {
    #[serde(flatten)]
    report: &'a JudgeReport,
    vendors: &'a VendorCheck,
}

/// A run directory.
pub(super) struct RunDir {
    root: PathBuf,
}

impl RunDir {
    /// Returns the directory at `root` for a new run, creating it when it is missing.
    ///
    /// # Errors
    ///
    /// Returns [`RunEvalError::OutputNotEmpty`] when `root` holds any entry, and a file error
    /// when it cannot be created or listed.
    pub(super) fn create(root: PathBuf) -> Result<Self, RunEvalError> {
        ensure_dir(&root)?;
        let mut entries = fs::read_dir(&root).map_err(io_error(FileOp::List, &root))?;
        match entries.next() {
            None => Ok(Self { root }),
            Some(Ok(_)) => Err(RunEvalError::OutputNotEmpty { path: root }),
            Some(Err(e)) => Err(io_error(FileOp::List, &root)(e).into()),
        }
    }

    /// Returns the directory a run already wrote at `root`.
    pub(super) fn open(root: PathBuf) -> Self {
        Self { root }
    }

    pub(super) fn path(&self) -> &Path {
        &self.root
    }

    /// Writes a case's output, its report and its copies of the corpus files it has.
    pub(super) fn write_case(&self, case: &Case, run: &CaseRun) -> Result<(), FileError> {
        let dir = self.root.join(case.id.as_str());
        ensure_dir(&dir)?;
        write_text(&dir.join(OUTPUT_FILE), &run.output_text)?;
        write_pretty_json(&dir.join(CASE_REPORT_FILE), &run.summary)?;
        for (name, text) in corpus_copies(case) {
            write_text(&dir.join(name), text)?;
        }
        Ok(())
    }

    /// Writes the run's manifest and returns where.
    pub(super) fn write_manifest(&self, manifest: &Manifest) -> Result<PathBuf, FileError> {
        let path = self.root.join(MANIFEST_FILE);
        write_pretty_json(&path, manifest)?;
        Ok(path)
    }

    pub(super) fn read_manifest(&self) -> Result<Manifest, FileError> {
        let path = self.root.join(MANIFEST_FILE);
        let body = read_text(&path)?;
        read_json(&path, &body)
    }

    /// Reads what the judge grades of the case `id`: the input and output every successful case
    /// has, and the reference answer and system prompt when the case had them.
    ///
    /// # Errors
    ///
    /// Returns [`JudgeRunError::CaseFileMissing`] when the input or the output is not there.
    pub(super) fn read_judge_item(&self, id: &CaseId) -> Result<JudgeItem, JudgeRunError> {
        let dir = self.root.join(id.as_str());
        Ok(JudgeItem {
            item_id: id.to_string(),
            input: read_case_file(&dir.join(INPUT_FILE))?,
            output: read_case_file(&dir.join(OUTPUT_FILE))?,
            expected: read_optional_text(&dir.join(EXPECTED_FILE))?,
            context: read_optional_text(&dir.join(SYSTEM_FILE))?,
        })
    }

    /// Writes the judge's report with what was known of its vendor, and returns where.
    pub(super) fn write_judge_report(
        &self,
        report: &JudgeReport,
        vendors: &VendorCheck,
    ) -> Result<PathBuf, FileError> {
        let path = self.root.join(JUDGE_REPORT_FILE);
        write_pretty_json(&path, &JudgeReportFile { report, vendors })?;
        Ok(path)
    }

    /// Reads the run's judge report; `Ok(None)` when the run was never judged.
    pub(super) fn read_judge_report(&self) -> Result<Option<JudgeReport>, FileError> {
        let path = self.root.join(JUDGE_REPORT_FILE);
        read_optional_text(&path)?
            .map(|body| read_json(&path, &body))
            .transpose()
    }
}

/// Returns whether a case named `id` would take the place of a file a run keeps beside its
/// cases.
pub(super) fn takes_a_run_file(id: &CaseId) -> bool {
    [MANIFEST_FILE, JUDGE_REPORT_FILE].contains(&id.as_str())
}

/// Returns the corpus files of `case` a run keeps a copy of, with their text, so `judge` grades
/// a run without the corpus.
fn corpus_copies(case: &Case) -> impl Iterator<Item = (&'static str, &str)> {
    [
        (INPUT_FILE, Some(case.input.as_str())),
        (SYSTEM_FILE, case.system.as_deref()),
        (EXPECTED_FILE, case.expected.as_deref()),
    ]
    .into_iter()
    .filter_map(|(name, text)| text.map(|text| (name, text)))
}

/// Reads a file every successful case's directory holds.
fn read_case_file(path: &Path) -> Result<String, JudgeRunError> {
    read_optional_text(path)?.ok_or_else(|| JudgeRunError::CaseFileMissing {
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(system: Option<&str>, expected: Option<&str>) -> Case {
        Case {
            id: "c".parse().unwrap(),
            input: "q".into(),
            system: system.map(String::from),
            expected: expected.map(String::from),
            expected_tools: None,
        }
    }

    #[test]
    fn a_run_copies_each_corpus_file_a_case_has_and_no_other() {
        assert_eq!(
            corpus_copies(&case(Some("sys"), Some("exp"))).collect::<Vec<_>>(),
            [
                (INPUT_FILE, "q"),
                (SYSTEM_FILE, "sys"),
                (EXPECTED_FILE, "exp")
            ]
        );
        assert_eq!(
            corpus_copies(&case(None, None)).collect::<Vec<_>>(),
            [(INPUT_FILE, "q")]
        );
    }

    #[test]
    fn a_run_is_written_only_into_a_new_or_empty_directory() {
        let parent = tempfile::TempDir::new().unwrap();
        let new = parent.path().join("new");
        assert!(RunDir::create(new.clone()).is_ok());
        assert!(RunDir::create(new.clone()).is_ok());

        fs::write(new.join("leftover"), "x").unwrap();
        assert!(matches!(
            RunDir::create(new.clone()),
            Err(RunEvalError::OutputNotEmpty { path }) if path == new
        ));
    }

    #[test]
    fn only_a_case_named_like_a_run_file_takes_its_place() {
        for (id, takes) in [
            ("manifest.json", true),
            ("judge_report.json", true),
            ("report.json", false),
            ("case_001", false),
        ] {
            assert_eq!(takes_a_run_file(&id.parse().unwrap()), takes, "{id}");
        }
    }
}
