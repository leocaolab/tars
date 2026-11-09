use std::path::Path;
use std::sync::Arc;

use chrono::TimeZone;
use serde_json::{Value, json};
use tars_types::TenantId;

use super::*;

struct FixedClock(DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

const SESSION: &str = "01900000-0000-7000-8000-00000000002a";
const STAMP_TIME: &str = "2026-09-28T12:34:56.789Z";

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
    tool: MemoryTool,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.env"), "TOKEN=1\n").unwrap();
    let root = MemoryRoot::open(tmp.path().join("memory")).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 9, 28, 12, 34, 56).unwrap()
        + chrono::Duration::milliseconds(789);
    let tool = MemoryTool::with_clock(
        root.clone(),
        SESSION.parse::<SessionId>().unwrap(),
        Arc::new(FixedClock(now)),
    );
    Fixture {
        root: root.path().to_path_buf(),
        outside: std::fs::canonicalize(outside).unwrap(),
        _tmp: tmp,
        tool,
    }
}

async fn call(tool: &MemoryTool, args: Value) -> ToolOutput {
    tool.execute(args, &ToolEnv::default(), CancellationToken::new())
        .await
        .unwrap()
}

fn ok(r: &ToolOutput) -> &str {
    assert!(!r.is_error, "expected success, got error: {}", r.content);
    &r.content
}

fn err(r: &ToolOutput) -> &str {
    assert!(r.is_error, "expected error, got success: {}", r.content);
    &r.content
}

fn put(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

// ── layout ──────────────────────────────────────────────────────────────────

#[test]
fn memory_dir_is_per_tenant_and_scope() {
    let home = Path::new("/h");
    let t = TenantId::new("acme");
    assert_eq!(
        memory_dir(home, &t, &MemoryScope::User).unwrap(),
        Path::new("/h/tenants/acme/memory")
    );
    let project = MemoryScope::Project(ProjectKey::parse("tars").unwrap());
    assert_eq!(
        memory_dir(home, &t, &project).unwrap(),
        Path::new("/h/tenants/acme/projects/tars/memory")
    );
}

#[test]
fn a_key_that_could_climb_out_is_not_a_segment() {
    for bad in ["..", ".", "", "a/b", "a\\b"] {
        assert!(ProjectKey::parse(bad).is_err(), "{bad:?} accepted");
    }
    let err = memory_dir(Path::new("/h"), &TenantId::new(".."), &MemoryScope::User).unwrap_err();
    assert!(err.to_string().contains("tenant id `..`"), "{err}");
}

// ── view ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn view_of_an_empty_root_lists_just_the_root() {
    let f = fixture();
    let r = call(&f.tool, json!({"command": "view", "path": "/memories"})).await;
    let lines: Vec<&str> = ok(&r).lines().collect();
    assert_eq!(
        lines[0],
        "Here're the files and directories up to 2 levels deep in /memories, excluding hidden items and node_modules:"
    );
    assert_eq!(lines.len(), 2);
    assert!(lines[1].ends_with("\t/memories"), "{}", lines[1]);
}

#[tokio::test]
async fn view_of_a_directory_lists_two_levels_skipping_hidden_and_node_modules() {
    let f = fixture();
    put(&f.root, "b.md", &"x".repeat(1536));
    put(&f.root, "a/one.md", "1");
    put(&f.root, "a/deep/too_deep.md", "3");
    put(&f.root, ".hidden", "h");
    put(&f.root, "node_modules/pkg.js", "n");
    let r = call(&f.tool, json!({"command": "view", "path": "/memories"})).await;
    let rows: Vec<&str> = ok(&r)
        .lines()
        .skip(2)
        .map(|l| l.split('\t').nth(1).unwrap())
        .collect();
    assert_eq!(
        rows,
        [
            "/memories/a/",
            "/memories/a/deep/",
            "/memories/a/one.md",
            "/memories/b.md"
        ]
    );
    assert!(ok(&r).contains("1.5K\t/memories/b.md"), "{}", r.content);
    assert!(ok(&r).contains("1B\t/memories/a/one.md"), "{}", r.content);
}

#[tokio::test]
async fn view_of_a_file_numbers_its_lines() {
    let f = fixture();
    put(&f.root, "notes.txt", "Hello World\nThis is line two\n");
    let r = call(
        &f.tool,
        json!({"command": "view", "path": "/memories/notes.txt"}),
    )
    .await;
    assert_eq!(
        ok(&r),
        "Here's the content of /memories/notes.txt with line numbers:\n     1\tHello World\n     2\tThis is line two"
    );
}

#[tokio::test]
async fn view_range_selects_lines_and_minus_one_runs_to_the_end() {
    let f = fixture();
    put(&f.root, "n.txt", "a\nb\nc\nd\n");
    let r = call(
        &f.tool,
        json!({"command": "view", "path": "/memories/n.txt", "view_range": [2, 3]}),
    )
    .await;
    assert_eq!(
        ok(&r),
        "Here's the content of /memories/n.txt with line numbers:\n     2\tb\n     3\tc"
    );
    let r = call(
        &f.tool,
        json!({"command": "view", "path": "/memories/n.txt", "view_range": [3, -1]}),
    )
    .await;
    assert_eq!(
        ok(&r),
        "Here's the content of /memories/n.txt with line numbers:\n     3\tc\n     4\td"
    );
}

#[tokio::test]
async fn view_of_a_missing_path_says_so() {
    let f = fixture();
    let r = call(
        &f.tool,
        json!({"command": "view", "path": "/memories/nope.md"}),
    )
    .await;
    assert_eq!(
        err(&r),
        "The path /memories/nope.md does not exist. Please provide a valid path."
    );
}

// ── create ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_writes_the_file_and_refuses_an_existing_one() {
    let f = fixture();
    let args = json!({"command": "create", "path": "/memories/sub/notes.txt", "file_text": "Meeting notes\n"});
    let r = call(&f.tool, args.clone()).await;
    assert_eq!(
        ok(&r),
        "File created successfully at: /memories/sub/notes.txt"
    );
    assert_eq!(
        std::fs::read_to_string(f.root.join("sub/notes.txt")).unwrap(),
        "Meeting notes\n"
    );

    let r = call(&f.tool, args).await;
    assert_eq!(
        err(&r),
        "Error: File /memories/sub/notes.txt already exists"
    );
}

// ── str_replace ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn str_replace_edits_once_and_shows_a_snippet() {
    let f = fixture();
    put(
        &f.root,
        "p.txt",
        "l1\nl2\nFavorite color: blue\nl4\nl5\nl6\n",
    );
    let r = call(
        &f.tool,
        json!({"command": "str_replace", "path": "/memories/p.txt",
               "old_str": "Favorite color: blue", "new_str": "Favorite color: green"}),
    )
    .await;
    assert_eq!(
        ok(&r),
        "The memory file has been edited. Here is the snippet showing the change (with line numbers):\n\
         \x20    1\tl1\n     2\tl2\n     3\tFavorite color: green\n     4\tl4\n     5\tl5"
    );
    assert!(
        std::fs::read_to_string(f.root.join("p.txt"))
            .unwrap()
            .contains("green")
    );
}

#[tokio::test]
async fn str_replace_without_new_str_deletes_old_str() {
    let f = fixture();
    put(&f.root, "p.txt", "keep drop keep\n");
    let r = call(
        &f.tool,
        json!({"command": "str_replace", "path": "/memories/p.txt", "old_str": " drop"}),
    )
    .await;
    ok(&r);
    assert_eq!(
        std::fs::read_to_string(f.root.join("p.txt")).unwrap(),
        "keep keep\n"
    );
}

#[tokio::test]
async fn str_replace_reports_no_match_duplicates_and_missing_files() {
    let f = fixture();
    put(&f.root, "p.txt", "x\ny\nx\n");
    std::fs::create_dir_all(f.root.join("dir")).unwrap();
    let args = |path: &str, old: &str| json!({"command": "str_replace", "path": path, "old_str": old, "new_str": "z"});

    let r = call(&f.tool, args("/memories/p.txt", "q")).await;
    assert_eq!(
        err(&r),
        "No replacement was performed, old_str `q` did not appear verbatim in /memories/p.txt."
    );
    let r = call(&f.tool, args("/memories/p.txt", "x")).await;
    assert_eq!(
        err(&r),
        "No replacement was performed. Multiple occurrences of old_str `x` in lines: 1, 3. Please ensure it is unique"
    );
    for missing in ["/memories/none.txt", "/memories/dir"] {
        let r = call(&f.tool, args(missing, "x")).await;
        assert_eq!(
            err(&r),
            format!("Error: The path {missing} does not exist. Please provide a valid path.")
        );
    }
}

// ── insert ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn insert_puts_text_after_the_line() {
    let f = fixture();
    put(&f.root, "todo.txt", "a\nb\n");
    let r = call(
        &f.tool,
        json!({"command": "insert", "path": "/memories/todo.txt", "insert_line": 1, "insert_text": "- new\n"}),
    )
    .await;
    assert_eq!(ok(&r), "The file /memories/todo.txt has been edited.");
    assert_eq!(
        std::fs::read_to_string(f.root.join("todo.txt")).unwrap(),
        "a\n- new\nb\n"
    );

    let r = call(&f.tool, json!({"command": "insert", "path": "/memories/todo.txt", "insert_line": 0, "insert_text": "top"})).await;
    ok(&r);
    assert!(
        std::fs::read_to_string(f.root.join("todo.txt"))
            .unwrap()
            .starts_with("top\na\n")
    );
}

#[tokio::test]
async fn insert_rejects_a_line_out_of_range_and_a_missing_file() {
    let f = fixture();
    put(&f.root, "todo.txt", "a\nb\n");
    for bad in [3, -1] {
        let r = call(&f.tool, json!({"command": "insert", "path": "/memories/todo.txt", "insert_line": bad, "insert_text": "x"})).await;
        assert_eq!(
            err(&r),
            format!(
                "Error: Invalid `insert_line` parameter: {bad}. It should be within the range of lines of the file: [0, 2]"
            )
        );
    }
    let r = call(&f.tool, json!({"command": "insert", "path": "/memories/none.txt", "insert_line": 0, "insert_text": "x"})).await;
    assert_eq!(err(&r), "Error: The path /memories/none.txt does not exist");
}

// ── delete ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_removes_files_and_directories_but_never_the_root() {
    let f = fixture();
    put(&f.root, "old.txt", "x");
    put(&f.root, "dir/inner.txt", "x");
    let r = call(
        &f.tool,
        json!({"command": "delete", "path": "/memories/old.txt"}),
    )
    .await;
    assert_eq!(ok(&r), "Successfully deleted /memories/old.txt");
    let r = call(
        &f.tool,
        json!({"command": "delete", "path": "/memories/dir"}),
    )
    .await;
    assert_eq!(ok(&r), "Successfully deleted /memories/dir");
    assert!(!f.root.join("dir").exists());

    let r = call(
        &f.tool,
        json!({"command": "delete", "path": "/memories/old.txt"}),
    )
    .await;
    assert_eq!(err(&r), "Error: The path /memories/old.txt does not exist");
    for root in ["/memories", "/memories/", "/memories/."] {
        let r = call(&f.tool, json!({"command": "delete", "path": root})).await;
        assert_eq!(err(&r), "Cannot delete the /memories directory itself");
    }
    assert!(f.root.exists());
}

// ── rename ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rename_moves_and_refuses_missing_sources_taken_destinations_and_the_root() {
    let f = fixture();
    put(&f.root, "draft.txt", "d");
    put(&f.root, "taken.txt", "t");
    let rename =
        |old: &str, new: &str| json!({"command": "rename", "old_path": old, "new_path": new});

    let r = call(
        &f.tool,
        rename("/memories/draft.txt", "/memories/done/final.txt"),
    )
    .await;
    assert_eq!(
        ok(&r),
        "Successfully renamed /memories/draft.txt to /memories/done/final.txt"
    );
    assert_eq!(
        std::fs::read_to_string(f.root.join("done/final.txt")).unwrap(),
        "d"
    );

    let r = call(&f.tool, rename("/memories/draft.txt", "/memories/x.txt")).await;
    assert_eq!(
        err(&r),
        "Error: The path /memories/draft.txt does not exist"
    );
    let r = call(
        &f.tool,
        rename("/memories/done/final.txt", "/memories/taken.txt"),
    )
    .await;
    assert_eq!(
        err(&r),
        "Error: The destination /memories/taken.txt already exists"
    );
    let r = call(&f.tool, rename("/memories", "/memories/elsewhere")).await;
    assert_eq!(err(&r), "Cannot rename the /memories directory itself");
}

// ── path traversal ──────────────────────────────────────────────────────────

const ESCAPES: [&str; 11] = [
    "../memories/a.md",
    "/memories/../outside/secret.env",
    "/memories/..",
    "/memories/a/../../outside/secret.env",
    "/memories/%2e%2e/outside/secret.env",
    "/memories/%2e%2e%2foutside%2fsecret.env",
    "/memories/%2E%2E%2Foutside",
    "/memories/..%2f..%2foutside",
    "/memories/..\\outside\\secret.env",
    "/memories/%2e%2e%5coutside",
    "/memories/link/secret.env",
];

const OFF_PREFIX: [&str; 3] = ["/etc/passwd", "/memoriesX/a.md", "memories/a.md"];

fn every_command_on(path: &str) -> Vec<Value> {
    vec![
        json!({"command": "view", "path": path}),
        json!({"command": "create", "path": path, "file_text": "pwned"}),
        json!({"command": "str_replace", "path": path, "old_str": "TOKEN", "new_str": "pwned"}),
        json!({"command": "insert", "path": path, "insert_line": 0, "insert_text": "pwned"}),
        json!({"command": "delete", "path": path}),
        json!({"command": "rename", "old_path": path, "new_path": "/memories/stolen"}),
        json!({"command": "rename", "old_path": "/memories/mine.txt", "new_path": path}),
    ]
}

#[tokio::test]
async fn every_traversal_variant_is_refused_by_every_command() {
    let f = fixture();
    put(&f.root, "mine.txt", "m");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&f.outside, f.root.join("link")).unwrap();

    for path in ESCAPES {
        for args in every_command_on(path) {
            let r = call(&f.tool, args.clone()).await;
            assert_eq!(
                err(&r),
                format!("Path {path} would escape /memories directory"),
                "{args}"
            );
        }
    }
    for path in OFF_PREFIX {
        for args in every_command_on(path) {
            let r = call(&f.tool, args.clone()).await;
            assert_eq!(
                err(&r),
                format!("Path must start with /memories, got: {path}"),
                "{args}"
            );
        }
    }

    assert_eq!(
        std::fs::read_to_string(f.outside.join("secret.env")).unwrap(),
        "TOKEN=1\n"
    );
    assert_eq!(
        std::fs::read_dir(&f.outside).unwrap().count(),
        1,
        "nothing written outside"
    );
    assert!(f.root.join("mine.txt").exists(), "nothing moved out");
}

// ── Claude Code behaviours: stamping + the index limit ──────────────────────

#[tokio::test]
async fn writes_stamp_modified_and_origin_session_id_into_frontmatter() {
    let f = fixture();
    let body =
        "---\nname: pnpm\nmodified: 1999-01-01\ndescription: use pnpm\n---\n\nAlways pnpm.\n";
    let r = call(
        &f.tool,
        json!({"command": "create", "path": "/memories/feedback_pnpm.md", "file_text": body}),
    )
    .await;
    ok(&r);
    assert_eq!(
        std::fs::read_to_string(f.root.join("feedback_pnpm.md")).unwrap(),
        format!(
            "---\nname: pnpm\ndescription: use pnpm\nmodified: {STAMP_TIME}\norigin_session_id: {SESSION}\n---\n\nAlways pnpm.\n"
        )
    );

    let r = call(
        &f.tool,
        json!({"command": "str_replace", "path": "/memories/feedback_pnpm.md", "old_str": "Always pnpm.", "new_str": "Always pnpm, never npm."}),
    )
    .await;
    assert!(
        ok(&r).contains("\n     8\tAlways pnpm, never npm."),
        "{}",
        r.content
    );
    let after = std::fs::read_to_string(f.root.join("feedback_pnpm.md")).unwrap();
    assert_eq!(after.matches("modified:").count(), 1, "{after}");
    assert_eq!(after.matches("origin_session_id:").count(), 1, "{after}");
}

#[tokio::test]
async fn a_file_without_frontmatter_is_written_as_is() {
    let f = fixture();
    call(
        &f.tool,
        json!({"command": "create", "path": "/memories/plain.md", "file_text": "just text\n"}),
    )
    .await;
    call(&f.tool, json!({"command": "insert", "path": "/memories/plain.md", "insert_line": 1, "insert_text": "more"})).await;
    assert_eq!(
        std::fs::read_to_string(f.root.join("plain.md")).unwrap(),
        "just text\nmore\n"
    );
}

fn lines(n: usize) -> String {
    (1..=n).map(|i| format!("- entry {i}\n")).collect()
}

#[tokio::test]
async fn an_index_over_200_lines_is_written_but_answered_with_a_rewrite_error() {
    let f = fixture();
    let r = call(
        &f.tool,
        json!({"command": "create", "path": "/memories/MEMORY.md", "file_text": lines(200)}),
    )
    .await;
    ok(&r);

    let r = call(&f.tool, json!({"command": "insert", "path": "/memories/MEMORY.md", "insert_line": 200, "insert_text": "- one too many"})).await;
    let msg = err(&r);
    assert!(
        msg.starts_with("The file /memories/MEMORY.md has been edited.\n"),
        "{msg}"
    );
    assert!(
        msg.contains("Error: this write left the memory index at MEMORY.md at 201 lines, over its 200-line read limit."),
        "{msg}"
    );
    assert!(msg.contains("Rewrite the index"), "{msg}");
    assert_eq!(
        std::fs::read_to_string(f.root.join("MEMORY.md"))
            .unwrap()
            .lines()
            .count(),
        201,
        "the write stands"
    );
}

#[tokio::test]
async fn an_index_over_25kb_is_answered_with_a_rewrite_error() {
    let f = fixture();
    let big = format!("{}\n", "x".repeat(25 * 1024));
    let r = call(
        &f.tool,
        json!({"command": "create", "path": "/memories/MEMORY.md", "file_text": big}),
    )
    .await;
    assert!(
        err(&r).contains(&format!(
            "at {} bytes, over its 25600-byte read limit",
            25 * 1024 + 1
        )),
        "{}",
        r.content
    );
    assert!(f.root.join("MEMORY.md").exists());
}

#[tokio::test]
async fn only_the_root_index_is_measured() {
    let f = fixture();
    for path in ["/memories/topic.md", "/memories/sub/MEMORY.md"] {
        let r = call(
            &f.tool,
            json!({"command": "create", "path": path, "file_text": lines(300)}),
        )
        .await;
        ok(&r);
    }
}

#[tokio::test]
async fn an_unknown_command_is_invalid_arguments() {
    let f = fixture();
    let e = f
        .tool
        .execute(
            json!({"command": "clear", "path": "/memories"}),
            &ToolEnv::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(e, ToolError::InvalidArguments(ref m) if m.to_string().contains("clear")),
        "{e}"
    );
}

#[test]
fn sizes_render_like_the_sdk() {
    assert_eq!(
        text::render_listing("/m", 0, &[]).lines().nth(1),
        Some("0B\t/m")
    );
    let sized = |n| {
        text::render_listing("/m", n, &[])
            .lines()
            .nth(1)
            .unwrap()
            .to_string()
    };
    assert_eq!(sized(512), "512B\t/m");
    assert_eq!(sized(1024), "1K\t/m");
    assert_eq!(sized(4096), "4K\t/m");
    assert_eq!(sized(1536), "1.5K\t/m");
    assert_eq!(sized(5 * 1024 * 1024 / 2), "2.5M\t/m");
}

#[test]
fn a_session_start_cuts_the_index_back_to_the_last_whole_line_inside_25kb() {
    let line = "x".repeat(999);
    let index = vec![line.as_str(); 30].join("\n");

    let loaded = text::loaded_index(&index).expect("a non-blank index loads");

    assert_eq!(loaded, vec![line.as_str(); 25].join("\n"));
}

#[test]
fn a_blank_index_loads_nothing() {
    assert_eq!(text::loaded_index(" \n\n "), None);
}

fn warning_for(index: &str) -> Option<String> {
    let loaded = text::loaded_index(index).expect("a non-blank index loads");
    text::truncation_warning(index, loaded)
}

#[test]
fn an_index_that_loads_whole_carries_no_warning() {
    let index = (1..=200)
        .map(|n| format!("- [m{n}](m{n}.md)"))
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(warning_for(&index), None);
}

#[test]
fn an_index_cut_by_lines_says_how_many_lines_and_where_the_cut_starts() {
    let index = (1..=250)
        .map(|n| format!("- [m{n}](m{n}.md)"))
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        warning_for(&index).as_deref(),
        Some(
            "MEMORY.md is 250 lines (limit: 200). Only part of it was loaded: \
             50 of 250 lines were cut off, starting at line 201 (\"- [m201](m201.md)\"). \
             Keep index entries to one line under ~200 chars; move detail into topic files."
        )
    );
}

#[test]
fn an_index_cut_by_bytes_says_its_entries_are_too_long() {
    let line = "x".repeat(999);
    let index = vec![line.as_str(); 30].join("\n");

    let warning = warning_for(&index).expect("30 KB is past the 25 KB limit");

    assert!(
        warning.starts_with(
            "MEMORY.md is 29.3KB (limit: 25KB) \u{2014} index entries are too long. \
             Only part of it was loaded: 5 of 30 lines were cut off, starting at line 26 (\""
        ),
        "{warning}"
    );
    assert!(
        warning.contains("\u{2026}\")"),
        "the quoted line is clipped: {warning}"
    );
}

#[test]
fn a_first_line_longer_than_the_limit_is_cut_inside_line_one() {
    let index = "y".repeat(30_000);

    assert_eq!(
        warning_for(&index).as_deref(),
        Some(
            "MEMORY.md is 29.3KB (limit: 25KB) \u{2014} index entries are too long. \
             Only part of it was loaded: everything after the first 25600 characters of line 1 was cut off. \
             Keep index entries to one line under ~200 chars; move detail into topic files."
        )
    );
}

fn io_failure(kind: io::ErrorKind) -> MemoryError {
    io_error(
        IoOp::Write,
        "/memories/notes.md",
        Path::new("/host/memory/notes.md"),
        io::Error::from(kind),
    )
}

#[test]
fn a_host_fault_reaches_the_caller_as_an_io_error() {
    match outcome(Err(io_failure(io::ErrorKind::StorageFull))) {
        Err(ToolError::Io { op, path, source }) => {
            assert_eq!(op, crate::tool::FsOp::Write);
            assert_eq!(path, Path::new("/host/memory/notes.md"));
            assert_eq!(source.kind(), io::ErrorKind::StorageFull);
        }
        other => panic!("expected ToolError::Io, got {other:?}"),
    }
}

#[test]
fn a_failure_the_model_can_route_around_stays_its_text() {
    let r = outcome(Err(io_failure(io::ErrorKind::IsADirectory))).unwrap();
    assert!(r.is_error);
    assert!(
        r.content
            .starts_with("Error: cannot write /memories/notes.md"),
        "{}",
        r.content
    );
}
