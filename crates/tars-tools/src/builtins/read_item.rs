//! `read_item` — read a *definition*, not a slice of a file.
//!
//! # Why lines are the wrong unit
//!
//! An agent that wants `RepoScope::blame` has two ways to ask for it today, and both
//! are wrong in the same way: read the whole 33 KB file, or `grep` for the line
//! number and then guess a window around it.
//!
//! The guess is the problem. Observed in a real run, an agent asked for lines 1–80
//! and then 150–230 of one file to see one function — two guesses, 160 lines fetched
//! for the ~20 that mattered. And a wrong guess is **invisible**: a window that cuts
//! a function in half looks exactly like a function that is that short, so the agent
//! edits against context that is not there.
//!
//! A definition has real boundaries. Asking for it by name returns all of it or says
//! it is not there — no guessing, and no way to be silently half-right.
//!
//! # Where language knowledge belongs
//!
//! [`ItemFinder`] is the seam. This crate ships the Rust one, because tars's own
//! agents work on tars. A consumer with a parser plugs its own in — arc already has
//! tree-sitter across eight languages and a `SymbolDef` carrying `sig_span` and
//! `body_span`, which is exactly this shape.
//!
//! The Rust finder here is deliberately not a parser. It matches a declaration line
//! and balances braces from it, which either finds the item or does not — the failure
//! mode is "not found", never "here is most of it".

use std::path::Path;

use serde::Serialize;

/// What a finder can actually answer.
///
/// Declared rather than discovered, because **"not supported" and "nothing found"
/// must not look the same**. A driver with no call-graph support returning an empty
/// reference list says "nobody uses this", which is the opposite of what it knows.
/// arc's `LangCaps` exists for the same reason and this mirrors it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Support {
    pub items: bool,
    pub imports: bool,
    pub refs: bool,
}

/// One definition, with enough around it to be worth reading.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Item {
    /// Qualified name — `RepoScope::blame`, not `blame`. arc's `SymbolDef.qname`.
    pub name: String,
    /// `fn`, `struct`, `enum`, `trait`, `impl` — whatever the finder recognised.
    pub kind: String,
    /// 1-based, inclusive, so it lines up with what `grep -n` reports.
    pub start_line: u32,
    pub end_line: u32,
    /// The text, including the doc comment and attributes above it.
    ///
    /// The doc comment is not decoration: in this codebase it routinely carries the
    /// reason the code is shaped the way it is, and an agent that changes the code
    /// without it is changing something it has only half read.
    pub text: String,
}

/// What a file says it is, before you read any of it.
///
/// The rung between "this file exists" and "here are 39 KB". It is also what makes
/// asking for a definition by name possible at all: you cannot ask for
/// `RepoScope::blame` if you do not know it is there.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Outline {
    /// The first line of the module doc — what the file claims to be, in its own
    /// words. Written by whoever wrote the file, so it beats anything inferred.
    pub headline: Option<String>,
    /// What it pulls in. The cheapest description of what a file depends on.
    pub imports: Vec<String>,
    /// Every definition, signature only.
    pub items: Vec<ItemSig>,
    pub lines: u32,
}

/// One definition's head: enough to decide whether to ask for its body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ItemSig {
    /// Qualified name. arc's `SymbolDef.qname`.
    pub name: String,
    pub kind: String,
    /// Visible outside its module — Rust `pub`, Python no leading `_`, TS `export`.
    ///
    /// The question "can changing this break another crate" is answered by this field
    /// and nothing else, and it is the first thing a refactor needs to know.
    pub exported: bool,
    pub start_line: u32,
    pub end_line: u32,
    /// The declaration line, trimmed. Not the body.
    pub signature: String,
}

/// How a name appears at one place.
///
/// The distinction that matters for "what breaks if I change this": a definition is
/// the thing being changed, a call is what must be updated, and an import is what
/// must still resolve afterwards. Lumping them together — which is what a bare grep
/// does — puts the definition in the list of things to fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum RefKind {
    Definition,
    Call,
    Import,
    /// Named, but not in a way this finder can classify — a type position, a macro
    /// argument, a comment. Reported as itself rather than guessed at.
    Mention,
}

/// One place a name appears.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Ref {
    pub kind: RefKind,
    pub line: u32,
    /// The line, trimmed.
    pub text: String,
}

/// Finds definitions in a source file.
pub trait ItemFinder: Send + Sync {
    /// Does this finder handle that file?
    fn handles(&self, path: &Path) -> bool;

    /// What it can answer. See [`Support`] — an unsupported question must be
    /// distinguishable from one that was asked and came back empty.
    fn support(&self) -> Support;

    /// The definition called `name`, or `None`.
    ///
    /// `None` means "not found here", never "found part of it" — a partial answer is
    /// the failure this whole module exists to avoid.
    fn find(&self, source: &str, name: &str) -> Option<Item>;

    /// Everything the file defines, signatures only.
    fn outline(&self, source: &str) -> Outline;

    /// Every place `name` appears in this file, classified.
    ///
    /// **Textual, and it says so.** It finds what is written; it does not resolve
    /// trait dispatch, macro expansion, or re-exports. A caller wanting completeness
    /// has the compiler for that — this is for deciding where to look first, which is
    /// the question an agent actually has before it starts.
    fn refs(&self, source: &str, name: &str) -> Vec<Ref>;
}

/// Rust, by declaration line and brace balance.
pub struct RustItems;

/// The declaration keywords worth asking for by name.
const KINDS: &[&str] = &["fn", "struct", "enum", "trait", "impl", "type", "const", "static", "mod"];

impl ItemFinder for RustItems {
    fn handles(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "rs")
    }

    fn support(&self) -> Support {
        // `refs` is textual, and says so at its own definition. It is supported in
        // the sense that asking is meaningful; completeness is the compiler's job.
        Support { items: true, imports: true, refs: true }
    }

    fn refs(&self, source: &str, name: &str) -> Vec<Ref> {
        source
            .lines()
            .enumerate()
            .filter_map(|(i, line)| {
                let t = line.trim();
                if !mentions(t, name) {
                    return None;
                }
                let kind = if Self::declaration(t).map(|(_, n)| n) == Some(name.to_string()) {
                    RefKind::Definition
                } else if t.starts_with("use ") || t.starts_with("pub use ") {
                    RefKind::Import
                } else if t.contains(&format!("{name}(")) || t.contains(&format!(".{name}(")) {
                    RefKind::Call
                } else {
                    RefKind::Mention
                };
                Some(Ref { kind, line: i as u32 + 1, text: t.to_string() })
            })
            .collect()
    }

    fn outline(&self, source: &str) -> Outline {
        let lines: Vec<&str> = source.lines().collect();

        // The file's own words about itself, from the module doc. Anything inferred
        // would be a guess competing with a statement.
        let headline = lines
            .iter()
            .find_map(|l| l.trim_start().strip_prefix("//!"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let imports = lines
            .iter()
            .filter_map(|l| l.trim_start().strip_prefix("use "))
            .map(|u| u.trim_end_matches(';').trim().to_string())
            .collect();

        // Only top-level definitions and the members of `impl` blocks. Everything
        // nested deeper is an implementation detail of something already listed, and
        // listing it turns the outline back into the wall of text it replaces.
        let mut items = Vec::new();
        let mut current_impl: Option<String> = None;
        let mut impl_ends = 0usize;
        let mut i = 0usize;
        while i < lines.len() {
            let t = lines[i].trim_start();
            let indent = lines[i].len() - t.len();
            if indent > 4 {
                i += 1;
                continue;
            }
            let Some((kind, name)) = Self::declaration(t) else {
                i += 1;
                continue;
            };
            let Some(end) = balance_from(&lines, i) else {
                i += 1;
                continue;
            };
            // Inside an `impl`, a method's name is qualified by the type — that is
            // what a caller writes and therefore what it should be asked for by.
            let qname = match (&current_impl, kind) {
                (_, "impl") => format!("impl {name}"),
                (Some(ty), _) if i < impl_ends => format!("{ty}::{name}"),
                _ => name.clone(),
            };
            items.push(ItemSig {
                name: qname,
                kind: kind.to_string(),
                exported: t.starts_with("pub"),
                start_line: i as u32 + 1,
                end_line: end as u32 + 1,
                signature: t.trim_end_matches('{').trim().to_string(),
            });
            if kind == "impl" {
                current_impl = Some(name);
                impl_ends = end;
                i += 1;
            } else {
                i = end + 1;
            }
        }

        Outline { headline, imports, items, lines: lines.len() as u32 }
    }

    fn find(&self, source: &str, name: &str) -> Option<Item> {
        // `Type::method` — search the type's `impl` blocks, not the type itself. The
        // struct declaration does not contain the method, and a method can live in
        // any of several blocks (`impl T`, `impl Trait for T`), so every one of them
        // is tried and the answer comes from whichever actually has it.
        if let Some((ty, method)) = name.split_once("::") {
            let lines: Vec<&str> = source.lines().collect();
            for (i, l) in lines.iter().enumerate() {
                if !is_impl_of(l.trim_start(), ty) {
                    continue;
                }
                let Some(end) = balance_from(&lines, i) else {
                    continue;
                };
                let block = lines[i..=end].join("\n");
                if let Some(inner) = self.find(&block, method) {
                    return Some(Item {
                        name: name.to_string(),
                        start_line: i as u32 + inner.start_line,
                        end_line: i as u32 + inner.end_line,
                        ..inner
                    });
                }
            }
            return None;
        }

        let lines: Vec<&str> = source.lines().collect();
        let (decl, kind) = lines.iter().enumerate().find_map(|(i, l)| {
            let t = l.trim_start();
            KINDS.iter().find_map(|k| declares(t, k, name).then(|| (i, *k)))
        })?;

        // Everything attached above it. A doc comment and its attributes are part of
        // the definition even though the compiler lets them float.
        let mut start = decl;
        while start > 0 {
            let prev = lines[start - 1].trim_start();
            if prev.starts_with("///") || prev.starts_with("//!") || prev.starts_with("#[") {
                start -= 1;
            } else {
                break;
            }
        }

        let end = balance_from(&lines, decl)?;
        Some(Item {
            name: name.to_string(),
            kind: kind.to_string(),
            start_line: start as u32 + 1,
            end_line: end as u32 + 1,
            text: lines[start..=end].join("\n"),
        })
    }
}

impl RustItems {
    /// The declaration keyword and name on this line, if it declares something.
    fn declaration(trimmed: &str) -> Option<(&'static str, String)> {
        for kind in KINDS {
            let Some(at) = trimmed.find(kind) else { continue };
            if !trimmed[..at].split_whitespace().all(|w| {
                matches!(w, "pub" | "async" | "unsafe" | "const" | "extern" | "default")
                    || w.starts_with("pub(")
            }) {
                continue;
            }
            let rest = trimmed[at + kind.len()..].trim_start();
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some((kind, name));
            }
        }
        None
    }
}

/// Does this line contain `name` as a whole token?
///
/// Whole-token, so `blame` does not match `blame_range` — the same reason `declares`
/// checks the boundary. A substring match here would report every longer name that
/// happens to start the same way as something that breaks.
fn mentions(line: &str, name: &str) -> bool {
    let boundary = |c: char| !(c.is_alphanumeric() || c == '_');
    let mut from = 0usize;
    while let Some(at) = line[from..].find(name) {
        let at = from + at;
        let before_ok = at == 0 || line[..at].chars().next_back().is_some_and(boundary);
        let after = at + name.len();
        let after_ok = after >= line.len() || line[after..].chars().next().is_some_and(boundary);
        if before_ok && after_ok {
            return true;
        }
        from = at + name.len();
    }
    false
}

/// Is this the head of an `impl` block for `ty`?
///
/// Matches both `impl Ty` and `impl Trait for Ty`, because a method the caller named
/// as `Ty::method` can be in either and the caller has no reason to know which.
fn is_impl_of(trimmed: &str, ty: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("impl") else {
        return false;
    };
    let rest = rest.trim_start();
    // `impl<T> Ty` — step over the generics.
    let rest = if let Some(after) = rest.strip_prefix('<') {
        match after.find('>') {
            Some(i) => after[i + 1..].trim_start(),
            None => return false,
        }
    } else {
        rest
    };
    // The type is what stands before the opening brace, after any `Trait for`.
    let head = rest.split('{').next().unwrap_or(rest).trim();
    let target = head.rsplit(" for ").next().unwrap_or(head).trim();
    // Ignore a generic argument list on the type itself: `Ty<'a>` is still `Ty`.
    let target = target.split(['<', ' ']).next().unwrap_or(target);
    target == ty
}

/// Does this line declare `kind name`?
///
/// Matched on the token rather than with `contains`, so `fn blame_range` is not
/// returned for `blame` and a call site is not mistaken for a definition.
fn declares(trimmed: &str, kind: &str, name: &str) -> bool {
    let Some(at) = trimmed.find(kind) else {
        return false;
    };
    // Only modifiers may precede it — otherwise this is a use, not a declaration.
    if !trimmed[..at]
        .split_whitespace()
        .all(|w| matches!(w, "pub" | "async" | "unsafe" | "const" | "extern" | "default") || w.starts_with("pub("))
    {
        return false;
    }
    let rest = trimmed[at + kind.len()..].trim_start();
    let Some(after) = rest.strip_prefix(name) else {
        return false;
    };
    // `fn blame(` and `fn blame<T>` yes; `fn blame_range(` no.
    after.is_empty() || !after.starts_with(|c: char| c.is_alphanumeric() || c == '_')
}

/// The line the block opened at `decl` closes on.
///
/// `None` when it never balances — a truncated file, or a declaration with no body.
/// Returning the rest of the file instead would be the silent half-answer this
/// module exists to prevent.
fn balance_from(lines: &[&str], decl: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut seen = false;
    for (i, line) in lines.iter().enumerate().skip(decl) {
        let mut in_str = false;
        let mut prev = ' ';
        for c in line.chars() {
            match c {
                '"' if prev != '\\' => in_str = !in_str,
                '{' if !in_str => {
                    depth += 1;
                    seen = true;
                }
                '}' if !in_str => {
                    depth -= 1;
                    if seen && depth == 0 {
                        return Some(i);
                    }
                }
                // A one-line `type X = Y;` or `const N: u32 = 1;` never opens a brace.
                ';' if !in_str && !seen => return Some(i),
                _ => {}
            }
            prev = c;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"//! module docs

use std::path::Path;

/// What a scope is.
#[derive(Debug)]
pub struct RepoScope {
    root: String,
}

impl RepoScope {
    /// Who last touched each line.
    ///
    /// The question a review comment raises.
    pub fn blame(&self, rel: &str, start: u32) -> Vec<String> {
        let s = "a } brace in a string";
        vec![rel.to_string(), s.to_string()]
    }

    pub fn blame_range(&self) -> u32 {
        1
    }
}

pub type Line = u32;
"#;

    /// The whole point: a definition comes back whole, with the doc comment that says
    /// why it is shaped the way it is.
    #[test]
    fn a_function_comes_back_with_its_doc_comment_and_nothing_after_it() {
        let it = RustItems.find(SRC, "blame").expect("found");
        assert_eq!(it.kind, "fn");
        assert!(it.text.starts_with("    /// Who last touched"), "{}", it.text);
        assert!(it.text.trim_end().ends_with('}'), "{}", it.text);
        assert!(!it.text.contains("blame_range"), "stopped at its own end: {}", it.text);
    }

    /// A brace inside a string literal is not a brace. Counting it would end the
    /// function early and hand back something that does not compile.
    #[test]
    fn a_brace_inside_a_string_does_not_close_the_block() {
        let it = RustItems.find(SRC, "blame").unwrap();
        assert!(it.text.contains("a } brace in a string"));
        assert!(it.text.contains("vec![rel.to_string()"), "kept going past it: {}", it.text);
    }

    /// `blame` must not return `blame_range`. Matching on `contains` would.
    #[test]
    fn a_longer_name_starting_with_the_same_prefix_is_a_different_item() {
        let short = RustItems.find(SRC, "blame").unwrap();
        let long = RustItems.find(SRC, "blame_range").unwrap();
        assert_ne!(short.start_line, long.start_line);
        assert!(long.text.contains("blame_range"));
    }

    /// Line numbers are 1-based so they line up with `grep -n`, which is where the
    /// agent got the name in the first place.
    #[test]
    fn line_numbers_are_one_based_and_match_the_source() {
        let it = RustItems.find(SRC, "RepoScope").unwrap();
        let lines: Vec<&str> = SRC.lines().collect();
        assert!(lines[it.start_line as usize - 1].contains("/// What a scope is"));
        assert!(lines[it.end_line as usize - 1].trim() == "}");
    }

    /// A struct's attributes come with it — `#[derive(Debug)]` is part of what the
    /// type is, not decoration above it.
    #[test]
    fn attributes_above_a_definition_belong_to_it() {
        let it = RustItems.find(SRC, "RepoScope").unwrap();
        assert!(it.text.contains("#[derive(Debug)]"), "{}", it.text);
    }

    /// `Type::method` narrows to that impl first, so a method name that appears in
    /// several impls resolves to the one asked for.
    #[test]
    fn a_qualified_name_resolves_within_its_impl() {
        let it = RustItems.find(SRC, "RepoScope::blame").expect("found");
        assert!(it.text.contains("pub fn blame(&self"), "{}", it.text);
        let lines: Vec<&str> = SRC.lines().collect();
        assert!(
            lines[it.start_line as usize - 1].contains("Who last touched"),
            "the qualified line numbers are absolute, not relative to the impl"
        );
    }

    /// A one-line item with no block still has an end.
    #[test]
    fn a_declaration_with_no_body_ends_at_its_semicolon() {
        let it = RustItems.find(SRC, "Line").expect("found");
        assert_eq!(it.kind, "type");
        assert!(it.text.contains("pub type Line = u32;"), "{}", it.text);
    }

    /// Not found is not found. A partial answer is the failure this exists to avoid,
    /// so there is no "closest match".
    #[test]
    fn something_that_is_not_there_is_reported_absent_rather_than_approximated() {
        assert!(RustItems.find(SRC, "nonexistent").is_none());
        assert!(RustItems.find(SRC, "RepoScope::nonexistent").is_none());
    }

    /// A call is not a definition.
    #[test]
    fn a_use_of_a_name_is_not_mistaken_for_its_definition() {
        let src = "fn caller() {\n    other.blame(1);\n}\n";
        assert!(RustItems.find(src, "blame").is_none(), "a call site is not a definition");
    }

    /// An unbalanced file yields nothing rather than the rest of the file.
    #[test]
    fn a_block_that_never_closes_is_not_answered_with_the_remainder() {
        let src = "pub fn broken() {\n    let x = 1;\n";
        assert!(RustItems.find(src, "broken").is_none());
    }
/// Against a real 39 KB file, on the case that motivated this.
    ///
    /// A refactor of `RepoScope::blame` needs `RepoScope::blame`. Reading the file to
    /// get it costs 39,576 bytes; guessing a window around the `grep` hit is what an
    /// agent actually did — two windows, ~10.7 KB, for the ~20 lines that mattered.
    /// Asking for the definition costs under a kilobyte and cannot be half-right.
    #[test]
    fn a_definition_from_a_real_file_costs_a_fraction_of_the_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tars-git/src/repo.rs");
        let Ok(src) = std::fs::read_to_string(path) else {
            return; // not a checkout with that crate; nothing to assert against
        };

        let it = RustItems.find(&src, "RepoScope::blame").expect("the function is there");
        assert_eq!(it.kind, "fn");
        assert!(it.text.len() < src.len() / 20, "{} of {} bytes", it.text.len(), src.len());
        assert!(it.text.contains("pub fn blame("), "the signature: {}", it.text);
        assert!(it.text.trim_end().ends_with('}'), "and all of the body: {}", it.text);
        assert!(
            it.text.contains("///"),
            "with the doc comment that says why it is shaped that way"
        );

        // A type, a free function, and a private helper all resolve the same way.
        for name in ["BlameLine", "patch_paths", "RepoScope::safe_rev"] {
            let found = RustItems.find(&src, name).unwrap_or_else(|| panic!("{name} not found"));
            assert!(found.start_line > 0 && found.end_line >= found.start_line, "{name}");
        }
    }

    /// The rung between "this file exists" and "here are 39 KB". Against the real
    /// file: an outline of a 974-line source costs a fraction of it, and it is what
    /// makes asking for a definition by name possible — you cannot ask for
    /// `RepoScope::blame` if you do not know it is there.
    #[test]
    fn an_outline_of_a_real_file_lists_what_can_be_asked_for() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tars-git/src/repo.rs");
        let Ok(src) = std::fs::read_to_string(path) else { return };

        let o = RustItems.outline(&src);
        assert!(o.headline.as_deref().unwrap_or("").contains("RepoScope"), "{:?}", o.headline);
        assert!(o.lines > 900);
        assert!(o.imports.iter().any(|i| i.contains("std::path")), "{:?}", o.imports);

        let names: Vec<&str> = o.items.iter().map(|i| i.name.as_str()).collect();
        // Qualified, because that is what a caller writes and therefore what it
        // should be asked for by.
        for want in
            ["RepoScope", "RepoScope::blame", "RepoScope::apply", "BlameLine", "patch_paths"]
        {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }

        // Signatures only. An outline carrying bodies is the wall of text it replaces.
        let rendered: usize = o.items.iter().map(|i| i.signature.len() + i.name.len()).sum();
        assert!(rendered < src.len() / 10, "{rendered} of {} bytes", src.len());
        assert!(
            !o.items.iter().any(|i| i.signature.contains('\n')),
            "a signature is one line"
        );
    }

    /// The headline comes from the file's own module doc. Anything inferred would be
    /// a guess competing with a statement its author already made.
    #[test]
    fn the_headline_is_the_files_own_first_words() {
        let src = "//! `Thing` — what it is.\n//! more\n\nuse a::b;\npub fn f() {}\n";
        let o = RustItems.outline(src);
        assert_eq!(o.headline.as_deref(), Some("`Thing` — what it is."));
        assert_eq!(o.imports, vec!["a::b".to_string()]);
    }

    /// Methods of an `impl` are listed; things nested deeper are not. Listing every
    /// inner binding turns the outline back into the file.
    #[test]
    fn methods_are_listed_but_their_innards_are_not() {
        let src = "\
struct T;
impl T {
    pub fn method(&self) {
        fn helper() {}
        let x = 1;
    }
}
";
        let names: Vec<String> = RustItems.outline(src).items.iter().map(|i| i.name.clone()).collect();
        assert!(names.contains(&"T::method".to_string()), "qualified by its type: {names:?}");
        assert!(!names.contains(&"helper".to_string()), "nested: {names:?}");
        assert!(names.contains(&"impl T".to_string()), "{names:?}");

        let o = RustItems.outline(src);
        let method = o.items.iter().find(|i| i.name == "T::method").unwrap();
        assert!(method.exported, "`pub fn` is visible outside: {method:?}");
    }

    /// A file with nothing in it outlines to nothing, rather than to a guess.
    #[test]
    fn an_empty_file_outlines_to_nothing() {
        let o = RustItems.outline("");
        assert!(o.headline.is_none());
        assert!(o.items.is_empty());
        assert_eq!(o.lines, 0);
    }
    /// The distinction a bare grep cannot make. "What breaks if I change this" needs
    /// the definition kept OUT of the list of things to fix, and the imports kept in.
    #[test]
    fn references_are_classified_not_just_located() {
        let src = "\
use crate::repo::blame;

pub fn blame(&self, rel: &str) -> u32 { 1 }

fn caller() {
    let x = self.blame(\"a.rs\");
}

/// see blame for details
struct Holder { f: fn() -> u32 }
";
        let refs = RustItems.refs(src, "blame");
        let kinds: Vec<RefKind> = refs.iter().map(|r| r.kind).collect();
        assert!(kinds.contains(&RefKind::Import), "{refs:#?}");
        assert!(kinds.contains(&RefKind::Definition), "{refs:#?}");
        assert!(kinds.contains(&RefKind::Call), "{refs:#?}");
        // The doc comment mentions it and is neither — reported as itself, not guessed.
        assert!(kinds.contains(&RefKind::Mention), "{refs:#?}");
    }

    /// Whole-token, so changing `blame` does not report every `blame_range` as a site
    /// that breaks.
    #[test]
    fn a_longer_name_is_not_reported_as_a_reference() {
        let src = "fn blame_range() {}\nlet y = blame_range();\n";
        assert!(RustItems.refs(src, "blame").is_empty(), "{:?}", RustItems.refs(src, "blame"));
        assert_eq!(RustItems.refs(src, "blame_range").len(), 2);
    }

    /// Real file, real question: where is `blame` used, and is its definition kept
    /// out of the list?
    #[test]
    fn references_in_a_real_file_separate_the_definition_from_the_uses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tars-git/src/repo.rs");
        let Ok(src) = std::fs::read_to_string(path) else { return };

        let refs = RustItems.refs(&src, "blame");
        let defs: Vec<&Ref> = refs.iter().filter(|r| r.kind == RefKind::Definition).collect();
        assert_eq!(defs.len(), 1, "one definition: {defs:#?}");
        assert!(refs.len() > 1, "and uses besides it: {}", refs.len());
    }
}


