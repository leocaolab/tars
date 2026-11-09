//! Holds the built-in tools; a caller registers only the ones it wants.

mod bash;
mod edit_file;
mod fs_open;
mod glob;
mod grep;
mod list_dir;
pub mod memory;
mod read_file;
pub mod read_item;
mod read_jail;
mod roots;
mod web;
mod write_file;
mod write_jail;

pub use bash::BashTool;
pub use edit_file::EditFileTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_dir::ListDirTool;
pub use memory::MemoryTool;
pub use read_file::ReadFileTool;
pub use read_item::{
    FinderSupport, Item, ItemFinder, ItemKind, ItemSignature, Occurrence, OccurrenceKind, Outline,
    RustItemFinder,
};
pub use web::{WebFetchTool, WebSearchTool};
pub use write_file::WriteFileTool;

/// How many entries of the first few a search lists by name when it left them out.
const SHOWN_SKIPS: usize = 5;

/// The entries a search left out for one reason: how many, and the first [`SHOWN_SKIPS`] of them
/// by name, with the error when the reason alone does not say it.
#[derive(Debug, Default)]
struct SkipList {
    count: usize,
    shown: Vec<String>,
}

impl SkipList {
    fn push(&mut self, entry: String) {
        self.count += 1;
        if self.shown.len() < SHOWN_SKIPS {
            self.shown.push(entry);
        }
    }

    /// Returns the note a search appends for the entries it left out `why`, naming the first few,
    /// so the model does not read "no match" as "nothing there"; empty when it left none out.
    fn note(&self, why: &str) -> String {
        let n = self.count;
        if n == 0 {
            return String::new();
        }
        let entries = if n == 1 { "entry" } else { "entries" };
        let more = n - self.shown.len();
        let more = if more > 0 {
            format!("; and {more} more")
        } else {
            String::new()
        };
        format!(
            "\n\n(skipped {n} {entries} {why}: {}{more})",
            self.shown.join("; ")
        )
    }
}

/// Returns whether the model can get past a filesystem failure by choosing another path: a
/// component that is a file, a target that is a directory, a file that is not there or that it
/// may not open. Any other failure is the host's, not the model's.
fn model_can_fix(kind: std::io::ErrorKind) -> bool {
    use std::io::ErrorKind;
    matches!(
        kind,
        ErrorKind::NotADirectory
            | ErrorKind::IsADirectory
            | ErrorKind::AlreadyExists
            | ErrorKind::NotFound
            | ErrorKind::PermissionDenied
    )
}

#[cfg(test)]
mod tests {
    use super::{SHOWN_SKIPS, SkipList, model_can_fix};
    use std::io::ErrorKind;

    #[test]
    fn a_wrong_kind_of_path_is_the_models_to_fix() {
        assert!(model_can_fix(ErrorKind::NotADirectory));
        assert!(model_can_fix(ErrorKind::IsADirectory));
        assert!(model_can_fix(ErrorKind::NotFound));
    }

    #[test]
    fn a_file_it_may_not_open_is_the_models_to_route_around() {
        assert!(model_can_fix(ErrorKind::PermissionDenied));
    }

    #[test]
    fn a_failing_disk_is_not_the_models_to_fix() {
        assert!(!model_can_fix(ErrorKind::StorageFull));
        assert!(!model_can_fix(ErrorKind::Other));
    }

    #[test]
    fn a_walk_that_skipped_nothing_adds_no_note() {
        assert_eq!(SkipList::default().note("that could not be read"), "");
    }

    fn skipped(n: usize) -> SkipList {
        let mut list = SkipList::default();
        for i in 0..n {
            list.push(format!("`e{i}`: Permission denied"));
        }
        list
    }

    #[test]
    fn a_walk_that_skipped_entries_says_how_many() {
        assert!(
            skipped(1)
                .note("that could not be read")
                .contains("skipped 1 entry")
        );
        assert!(
            skipped(3)
                .note("that could not be read")
                .contains("skipped 3 entries")
        );
    }

    #[test]
    fn a_skipped_entry_is_named_with_its_error() {
        assert_eq!(
            skipped(1).note("that could not be read"),
            "\n\n(skipped 1 entry that could not be read: `e0`: Permission denied)"
        );
    }

    #[test]
    fn past_the_first_few_skipped_entries_are_counted_not_named() {
        let note = skipped(SHOWN_SKIPS + 2).note("that could not be read");
        assert!(note.contains(&format!("`e{}`", SHOWN_SKIPS - 1)), "{note}");
        assert!(!note.contains(&format!("`e{SHOWN_SKIPS}`")), "{note}");
        assert!(note.ends_with("; and 2 more)"), "{note}");
    }
}
