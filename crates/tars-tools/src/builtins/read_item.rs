//! Finds a definition in a source file by name, so an agent reads the whole item instead of
//! guessing a line window around a `grep` hit; a window that cuts a function in half looks like
//! a short function. It also outlines a file and classifies the places a name appears.
//!
//! The language knowledge is behind [`ItemFinder`]; this crate ships only [`RustItemFinder`], which
//! matches declaration lines and balances braces rather than parsing.

use std::path::Path;

use serde::Serialize;

/// The declaration keyword of a definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Fn,
    Struct,
    Enum,
    Trait,
    Impl,
    Type,
    Const,
    Static,
    Mod,
}

impl ItemKind {
    /// Every kind, in the order a declaration line is tried against them.
    const ALL: [ItemKind; 9] = [
        ItemKind::Fn,
        ItemKind::Struct,
        ItemKind::Enum,
        ItemKind::Trait,
        ItemKind::Impl,
        ItemKind::Type,
        ItemKind::Const,
        ItemKind::Static,
        ItemKind::Mod,
    ];

    /// Returns the keyword as Rust source spells it.
    pub fn keyword(self) -> &'static str {
        match self {
            ItemKind::Fn => "fn",
            ItemKind::Struct => "struct",
            ItemKind::Enum => "enum",
            ItemKind::Trait => "trait",
            ItemKind::Impl => "impl",
            ItemKind::Type => "type",
            ItemKind::Const => "const",
            ItemKind::Static => "static",
            ItemKind::Mod => "mod",
        }
    }
}

/// The questions a finder can answer. It is declared so that "not supported" does not look like
/// "nothing found": an empty reference list from a finder without reference support would say
/// that nobody uses the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FinderSupport {
    pub items: bool,
    pub imports: bool,
    pub refs: bool,
}

/// One definition with the doc comment and attributes above it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Item {
    /// The declaration; its lines span the doc comment and attributes, as `text` does.
    #[serde(flatten)]
    pub sig: ItemSignature,
    /// The source, including the doc comment and attributes above the declaration, since the
    /// doc comment often says why the code has its shape.
    pub text: String,
}

/// A file's headline, imports and definitions without their bodies, so an agent can learn what
/// names to ask for before reading the file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Outline {
    /// The first non-empty line of the module doc.
    pub headline: Option<String>,
    pub imports: Vec<String>,
    /// The top-level definitions and the members of `impl` blocks.
    pub items: Vec<ItemSignature>,
    pub lines: u32,
}

/// One definition's declaration, without its body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ItemSignature {
    /// The qualified name, such as `RepoScope::blame`.
    pub name: String,
    pub kind: ItemKind,
    /// Says whether the declaration is visible outside its module (Rust `pub`, Python no
    /// leading `_`, TypeScript `export`).
    pub exported: bool,
    /// The first line, 1-based and inclusive, as `grep -n` counts.
    pub start_line: u32,
    pub end_line: u32,
    /// The declaration line, trimmed.
    pub signature: String,
}

/// How a name appears at one place. When the name changes, the definition is what changes, a
/// call is what must be updated and an import is what must still resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum OccurrenceKind {
    Definition,
    Call,
    Import,
    /// Names it in a way the finder does not classify, such as a type position, a macro
    /// argument or a comment.
    Mention,
}

/// One place a name appears.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Occurrence {
    pub kind: OccurrenceKind,
    pub line: u32,
    /// The line, trimmed.
    pub text: String,
}

/// Finds definitions in a source file.
pub trait ItemFinder: Send + Sync {
    /// Returns whether this finder handles the file at `path`.
    fn handles(&self, path: &Path) -> bool;

    /// Returns the questions this finder answers.
    fn support(&self) -> FinderSupport;

    /// Returns the definition called `name`, or `None` when it is not found or its block does
    /// not close.
    fn find(&self, source: &str, name: &str) -> Option<Item>;

    /// Returns the file's [`Outline`].
    fn outline(&self, source: &str) -> Outline;

    /// Returns every line where `name` appears as a whole token, classified. It matches text
    /// and does not resolve trait dispatch, macro expansion or re-exports, so it tells where to
    /// look first, not every use.
    fn refs(&self, source: &str, name: &str) -> Vec<Occurrence>;
}

/// Finds Rust items by declaration line and brace balance.
pub struct RustItemFinder;

impl ItemFinder for RustItemFinder {
    fn handles(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "rs")
    }

    fn support(&self) -> FinderSupport {
        FinderSupport {
            items: true,
            imports: true,
            refs: true,
        }
    }

    fn refs(&self, source: &str, name: &str) -> Vec<Occurrence> {
        source
            .lines()
            .enumerate()
            .filter_map(|(i, line)| {
                let t = line.trim();
                if !mentions(t, name) {
                    return None;
                }
                let kind = if Self::declaration(t).map(|(_, n)| n) == Some(name.to_string()) {
                    OccurrenceKind::Definition
                } else if t.starts_with("use ") || t.starts_with("pub use ") {
                    OccurrenceKind::Import
                } else if t.contains(&format!("{name}(")) || t.contains(&format!(".{name}(")) {
                    OccurrenceKind::Call
                } else {
                    OccurrenceKind::Mention
                };
                Some(Occurrence {
                    kind,
                    line: i as u32 + 1,
                    text: t.to_string(),
                })
            })
            .collect()
    }

    fn outline(&self, source: &str) -> Outline {
        let lines: Vec<&str> = source.lines().collect();

        let headline = lines
            .iter()
            .filter_map(|l| l.trim_start().strip_prefix("//!"))
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_string);

        let imports = lines
            .iter()
            .filter_map(|l| l.trim_start().strip_prefix("use "))
            .map(|u| u.trim_end_matches(';').trim().to_string())
            .collect();

        // Lines indented past one level are skipped: they belong to something already listed.
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
            // A method is named `Type::method`, as a caller writes it.
            let qname = match (&current_impl, kind) {
                (_, ItemKind::Impl) => format!("impl {name}"),
                (Some(ty), _) if i < impl_ends => format!("{ty}::{name}"),
                _ => name.clone(),
            };
            items.push(ItemSignature {
                name: qname,
                kind,
                exported: is_exported(t),
                start_line: i as u32 + 1,
                end_line: end as u32 + 1,
                signature: signature_of(t),
            });
            if kind == ItemKind::Impl {
                current_impl = Some(name);
                impl_ends = end;
                i += 1;
            } else {
                i = end + 1;
            }
        }

        Outline {
            headline,
            imports,
            items,
            lines: lines.len() as u32,
        }
    }

    fn find(&self, source: &str, name: &str) -> Option<Item> {
        // `Type::method` is looked for in every `impl` block of the type, `impl T` and
        // `impl Trait for T` alike, since the caller cannot know which one holds it.
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
                        sig: ItemSignature {
                            name: name.to_string(),
                            start_line: i as u32 + inner.sig.start_line,
                            end_line: i as u32 + inner.sig.end_line,
                            ..inner.sig
                        },
                        text: inner.text,
                    });
                }
            }
            return None;
        }

        let lines: Vec<&str> = source.lines().collect();
        let (decl, kind) = lines.iter().enumerate().find_map(|(i, l)| {
            let (kind, at) = declared_keyword(l.trim_start())?;
            declares(&l.trim_start()[at + kind.keyword().len()..], name).then_some((i, kind))
        })?;

        // The doc comment and attributes directly above belong to the item.
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
        let declared = lines[decl].trim_start();
        Some(Item {
            sig: ItemSignature {
                name: name.to_string(),
                kind,
                exported: is_exported(declared),
                start_line: start as u32 + 1,
                end_line: end as u32 + 1,
                signature: signature_of(declared),
            },
            text: lines[start..=end].join("\n"),
        })
    }
}

impl RustItemFinder {
    /// Returns the declaration keyword and name on this line, if it declares something.
    fn declaration(trimmed: &str) -> Option<(ItemKind, String)> {
        let (kind, at) = declared_keyword(trimmed)?;
        if kind == ItemKind::Impl {
            return impl_target(&trimmed[at..]).map(|target| (kind, target.to_string()));
        }
        let rest = trimmed[at + kind.keyword().len()..].trim_start();
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        (!name.is_empty()).then_some((kind, name))
    }
}

/// Returns the keyword a trimmed line declares an item with and where it starts, reading whole
/// words: the modifiers before it (`pub`, `pub(crate)`, `async`, `unsafe`, `default`, `extern`
/// with its ABI string, and `const` before `fn`) are passed over, and the first other word must
/// be the keyword itself, so `const fn_count` is a `const` and `fn x()` called in an expression is
/// nothing.
fn declared_keyword(trimmed: &str) -> Option<(ItemKind, usize)> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut at = 0;
    loop {
        at += trimmed[at..].len() - trimmed[at..].trim_start().len();
        let rest = &trimmed[at..];
        let word_len = rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len());
        let word = &rest[..word_len];
        let after = &rest[word_len..];
        let modifier_len = match word {
            "pub" => match after.strip_prefix('(') {
                Some(scope) => word_len + 1 + scope.find(')')? + 1,
                None => word_len,
            },
            "async" | "unsafe" | "default" => word_len,
            "extern" => {
                let abi = after.trim_start();
                match abi.strip_prefix('"') {
                    Some(quoted) => rest.len() - abi.len() + 1 + quoted.find('"')? + 1,
                    None => word_len,
                }
            }
            "const" if next_word(after) == Some("fn") || next_word(after) == Some("unsafe") => {
                word_len
            }
            _ => {
                let kind = ItemKind::ALL.into_iter().find(|k| k.keyword() == word)?;
                return Some((kind, at));
            }
        };
        at += modifier_len;
    }
}

/// Returns the first word of `s` after any leading whitespace.
fn next_word(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    (end > 0).then(|| &s[..end])
}

/// Returns whether a trimmed declaration line makes its item visible outside its module.
fn is_exported(trimmed: &str) -> bool {
    trimmed.starts_with("pub")
}

/// Returns a trimmed declaration line without the brace that opens its block.
fn signature_of(trimmed: &str) -> String {
    trimmed.trim_end_matches('{').trim().to_string()
}

/// Returns whether `line` contains `name` as a whole token, so `blame` does not match
/// `blame_range`.
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

/// Returns whether this line opens an `impl Ty` or `impl Trait for Ty` block.
fn is_impl_of(trimmed: &str, ty: &str) -> bool {
    impl_target(trimmed) == Some(ty)
}

/// Returns the type an `impl` line implements for, the `Ty` of `impl<T> Trait for Ty<T> {`, or
/// `None` when the line does not start with `impl` or the type has no name of its own (`[T; 3]`,
/// `(A, B)`, `&T`).
fn impl_target(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("impl")?;
    if !rest.starts_with(|c: char| c == '<' || c.is_whitespace()) {
        return None;
    }
    let rest = rest.trim_start();
    let rest = match rest.strip_prefix('<') {
        Some(generics) => generics[closing_angle(generics)? + 1..].trim_start(),
        None => rest,
    };
    // The type is what stands before the opening brace, after any `Trait for`.
    let head = rest.split('{').next().unwrap_or(rest).trim();
    let target = head.rsplit(" for ").next().unwrap_or(head).trim();
    // The name is the leading path; a generic argument list is not part of it: `Ty<'a>` is `Ty`.
    let end = target
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
        .unwrap_or(target.len());
    let name = &target[..end];
    name.starts_with(|c: char| c.is_alphabetic() || c == '_')
        .then_some(name)
}

/// Returns the index in `s` of the `>` that closes a generic list opened just before `s`; the
/// `>` of an `->` does not close it.
fn closing_angle(s: &str) -> Option<usize> {
    let mut depth = 1u32;
    let mut prev = ' ';
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' if prev != '-' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        prev = c;
    }
    None
}

/// Returns whether `after_keyword`, the rest of a declaration line after its keyword, names
/// `name` as a whole token, so `fn blame_range` is not `blame`.
fn declares(after_keyword: &str, name: &str) -> bool {
    let Some(after) = after_keyword.trim_start().strip_prefix(name) else {
        return false;
    };
    // `fn blame(` and `fn blame<T>` yes; `fn blame_range(` no.
    after.is_empty() || !after.starts_with(|c: char| c.is_alphanumeric() || c == '_')
}

/// Returns the line the item declared at `decl` ends on: where its block closes, or, for an item
/// with no block, the `;` that ends it. `None` means it never ends, as in a truncated file.
/// Delimiters inside comments, string and raw string literals and char literals are not counted,
/// and a literal or block comment may span lines. A `;` inside parentheses or brackets (`[u8;
/// 32]`) does not end the item.
fn balance_from(lines: &[&str], decl: usize) -> Option<usize> {
    let mut lex = LexState::Code;
    let mut depth = 0u32;
    let mut nest = 0u32;
    let mut seen = false;
    for (i, line) in lines.iter().enumerate().skip(decl) {
        let chars: Vec<char> = line.chars().collect();
        let mut at = 0;
        while at < chars.len() {
            let (next, delim) = step(lex, &chars, at);
            lex = next.lex;
            at = next.at;
            match delim {
                Some('{') => {
                    depth += 1;
                    seen = true;
                }
                Some('}') if depth > 0 => {
                    depth -= 1;
                    if seen && depth == 0 {
                        return Some(i);
                    }
                }
                Some('(' | '[') => nest += 1,
                Some(')' | ']') => nest = nest.saturating_sub(1),
                // A one-line `type X = Y;` or `const N: u32 = 1;` never opens a brace.
                Some(';') if !seen && nest == 0 => return Some(i),
                _ => {}
            }
        }
        if lex == LexState::LineComment {
            lex = LexState::Code;
        }
    }
    None
}

/// Where the lexer is between two characters of Rust source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LexState {
    Code,
    LineComment,
    /// Inside a block comment, at this nesting depth (Rust block comments nest).
    BlockComment(u32),
    Str,
    /// Inside a raw string closed by `"` and this many `#`.
    RawStr(usize),
}

/// The lexer's position after one step: its state and the next index to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cursor {
    lex: LexState,
    at: usize,
}

/// Reads one token of `chars` at `at` in state `lex`, and returns where it leaves the lexer and
/// the delimiter or semicolon it read in code, if it was one.
fn step(lex: LexState, chars: &[char], at: usize) -> (Cursor, Option<char>) {
    let c = chars[at];
    let peek = |k: usize| chars.get(at + k).copied();
    let to = |lex, len| Cursor { lex, at: at + len };
    match lex {
        LexState::LineComment => (to(lex, chars.len() - at), None),
        LexState::BlockComment(n) => match (c, peek(1)) {
            ('*', Some('/')) if n == 1 => (to(LexState::Code, 2), None),
            ('*', Some('/')) => (to(LexState::BlockComment(n - 1), 2), None),
            ('/', Some('*')) => (to(LexState::BlockComment(n + 1), 2), None),
            _ => (to(lex, 1), None),
        },
        LexState::Str => match c {
            '\\' => (to(lex, 2.min(chars.len() - at)), None),
            '"' => (to(LexState::Code, 1), None),
            _ => (to(lex, 1), None),
        },
        LexState::RawStr(hashes) => {
            let closes = c == '"' && (1..=hashes).all(|k| peek(k) == Some('#'));
            if closes {
                (to(LexState::Code, 1 + hashes), None)
            } else {
                (to(lex, 1), None)
            }
        }
        LexState::Code => code_step(chars, at),
    }
}

/// Reads one token in code: a comment or literal opening, a char literal whole, or one char.
fn code_step(chars: &[char], at: usize) -> (Cursor, Option<char>) {
    let c = chars[at];
    let peek = |k: usize| chars.get(at + k).copied();
    let to = |lex, len| Cursor { lex, at: at + len };
    let starts_token = at == 0 || !is_ident(chars[at - 1]);
    match c {
        '/' if peek(1) == Some('/') => (to(LexState::LineComment, 2), None),
        '/' if peek(1) == Some('*') => (to(LexState::BlockComment(1), 2), None),
        '"' => (to(LexState::Str, 1), None),
        'r' | 'b' if starts_token => match raw_string_open(&chars[at..]) {
            Some((hashes, len)) => (to(LexState::RawStr(hashes), len), None),
            None => (to(LexState::Code, 1), None),
        },
        '\'' => (to(LexState::Code, char_literal_len(&chars[at..])), None),
        '{' | '}' | ';' | '(' | ')' | '[' | ']' => (to(LexState::Code, 1), Some(c)),
        _ => (to(LexState::Code, 1), None),
    }
}

/// Returns the `#` count and length of a raw string opening (`r"`, `r#"`, `br##"`) at the start
/// of `rest`, or `None` when it is an identifier that starts with `r` or `b`.
fn raw_string_open(rest: &[char]) -> Option<(usize, usize)> {
    let after_b = usize::from(rest.first() == Some(&'b'));
    if rest.get(after_b) != Some(&'r') {
        return None;
    }
    let hashes = rest[after_b + 1..]
        .iter()
        .take_while(|&&c| c == '#')
        .count();
    let quote = after_b + 1 + hashes;
    (rest.get(quote) == Some(&'"')).then_some((hashes, quote + 1))
}

/// Returns how many chars the `'` at the start of `rest` begins: a whole char literal (`'{'`,
/// `'\''`, `'\u{7b}'`), or just the quote of a lifetime or label (`'a`).
fn char_literal_len(rest: &[char]) -> usize {
    match rest.get(1) {
        // The escaped char is skipped before looking for the closing quote, so `'\''` closes
        // at its fourth char.
        Some('\\') => rest
            .get(3..)
            .and_then(|t| t.iter().position(|&c| c == '\''))
            .map_or(1, |k| k + 4),
        Some(_) if rest.get(2) == Some(&'\'') => 3,
        _ => 1,
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
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

    #[test]
    fn a_function_comes_back_with_its_doc_comment_and_nothing_after_it() {
        let it = RustItemFinder.find(SRC, "blame").expect("found");
        assert_eq!(it.sig.kind, ItemKind::Fn);
        assert!(
            it.text.starts_with("    /// Who last touched"),
            "{}",
            it.text
        );
        assert!(it.text.trim_end().ends_with('}'), "{}", it.text);
        assert!(
            !it.text.contains("blame_range"),
            "stopped at its own end: {}",
            it.text
        );
    }

    #[test]
    fn a_brace_inside_a_string_does_not_close_the_block() {
        let it = RustItemFinder.find(SRC, "blame").unwrap();
        assert!(it.text.contains("a } brace in a string"));
        assert!(
            it.text.contains("vec![rel.to_string()"),
            "kept going past it: {}",
            it.text
        );
    }

    #[test]
    fn a_longer_name_starting_with_the_same_prefix_is_a_different_item() {
        let short = RustItemFinder.find(SRC, "blame").unwrap();
        let long = RustItemFinder.find(SRC, "blame_range").unwrap();
        assert_ne!(short.sig.start_line, long.sig.start_line);
        assert!(long.text.contains("blame_range"));
    }

    #[test]
    fn line_numbers_are_one_based_and_match_the_source() {
        let it = RustItemFinder.find(SRC, "RepoScope").unwrap();
        let lines: Vec<&str> = SRC.lines().collect();
        assert!(lines[it.sig.start_line as usize - 1].contains("/// What a scope is"));
        assert!(lines[it.sig.end_line as usize - 1].trim() == "}");
    }

    #[test]
    fn attributes_above_a_definition_belong_to_it() {
        let it = RustItemFinder.find(SRC, "RepoScope").unwrap();
        assert!(it.text.contains("#[derive(Debug)]"), "{}", it.text);
    }

    #[test]
    fn a_qualified_name_resolves_within_its_impl() {
        let it = RustItemFinder.find(SRC, "RepoScope::blame").expect("found");
        assert!(it.text.contains("pub fn blame(&self"), "{}", it.text);
        let lines: Vec<&str> = SRC.lines().collect();
        assert!(
            lines[it.sig.start_line as usize - 1].contains("Who last touched"),
            "the qualified line numbers are absolute, not relative to the impl"
        );
    }

    #[test]
    fn a_declaration_with_no_body_ends_at_its_semicolon() {
        let it = RustItemFinder.find(SRC, "Line").expect("found");
        assert_eq!(it.sig.kind, ItemKind::Type);
        assert!(it.text.contains("pub type Line = u32;"), "{}", it.text);
    }

    #[test]
    fn something_that_is_not_there_is_reported_absent_rather_than_approximated() {
        assert!(RustItemFinder.find(SRC, "nonexistent").is_none());
        assert!(RustItemFinder.find(SRC, "RepoScope::nonexistent").is_none());
    }

    #[test]
    fn a_use_of_a_name_is_not_mistaken_for_its_definition() {
        let src = "fn caller() {\n    other.blame(1);\n}\n";
        assert!(
            RustItemFinder.find(src, "blame").is_none(),
            "a call site is not a definition"
        );
    }

    #[test]
    fn a_block_that_never_closes_is_not_answered_with_the_remainder() {
        let src = "pub fn broken() {\n    let x = 1;\n";
        assert!(RustItemFinder.find(src, "broken").is_none());
    }

    /// A sample with the shapes of a real module, owned by this crate so that another crate's
    /// wording cannot turn these tests red.
    const SAMPLE: &str = include_str!("../../tests/fixtures/outline_sample.rs");

    #[test]
    fn a_definition_costs_a_fraction_of_the_file() {
        let it = RustItemFinder
            .find(SAMPLE, "RepoScope::blame")
            .expect("the function is there");
        assert_eq!(it.sig.kind, ItemKind::Fn);
        assert!(
            it.text.len() < SAMPLE.len() / 4,
            "{} of {} bytes",
            it.text.len(),
            SAMPLE.len()
        );
        assert!(
            it.text.contains("pub fn blame("),
            "the signature: {}",
            it.text
        );
        assert!(
            it.text.trim_end().ends_with('}') && it.text.contains("unwrap_or_default"),
            "and all of the body, past the braces in literals and comments: {}",
            it.text
        );
        assert!(
            it.text.contains("/// Returns who last touched"),
            "with the doc comment that says why it is shaped that way"
        );

        for name in ["BlameLine", "patch_paths", "RepoScope::safe_rev"] {
            let found = RustItemFinder
                .find(SAMPLE, name)
                .unwrap_or_else(|| panic!("{name} not found"));
            assert!(
                found.sig.start_line > 0 && found.sig.end_line >= found.sig.start_line,
                "{name}"
            );
        }
    }

    #[test]
    fn an_outline_lists_what_can_be_asked_for() {
        let o = RustItemFinder.outline(SAMPLE);
        assert_eq!(
            o.headline.as_deref(),
            Some(
                "`RepoScope` answers questions about one checkout: who touched a line, what a \
                 patch changes."
            ),
            "the first non-empty `//!` line, past an empty one"
        );
        assert_eq!(o.lines as usize, SAMPLE.lines().count());
        assert!(
            o.imports.iter().any(|i| i.contains("std::path")),
            "{:?}",
            o.imports
        );

        let names: Vec<&str> = o.items.iter().map(|i| i.name.as_str()).collect();
        for want in [
            "RepoScope",
            "RepoScope::blame",
            "RepoScope::apply",
            "RepoScope::safe_rev",
            "BlameLine",
            "patch_paths",
            "Line",
            "MAX_LINES",
        ] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }

        let rendered: usize = o
            .items
            .iter()
            .map(|i| i.signature.len() + i.name.len())
            .sum();
        assert!(
            rendered < SAMPLE.len() / 3,
            "{rendered} of {} bytes",
            SAMPLE.len()
        );
        assert!(
            !o.items.iter().any(|i| i.signature.contains('\n')),
            "a signature is one line"
        );
    }

    #[test]
    fn the_headline_is_the_files_own_first_words() {
        let src = "//! `Thing` — what it is.\n//! more\n\nuse a::b;\npub fn f() {}\n";
        let o = RustItemFinder.outline(src);
        assert_eq!(o.headline.as_deref(), Some("`Thing` — what it is."));
        assert_eq!(o.imports, vec!["a::b".to_string()]);
    }

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
        let names: Vec<String> = RustItemFinder
            .outline(src)
            .items
            .iter()
            .map(|i| i.name.clone())
            .collect();
        assert!(
            names.contains(&"T::method".to_string()),
            "qualified by its type: {names:?}"
        );
        assert!(!names.contains(&"helper".to_string()), "nested: {names:?}");
        assert!(names.contains(&"impl T".to_string()), "{names:?}");

        let o = RustItemFinder.outline(src);
        let method = o.items.iter().find(|i| i.name == "T::method").unwrap();
        assert!(method.exported, "`pub fn` is visible outside: {method:?}");
    }

    #[test]
    fn an_empty_file_outlines_to_nothing() {
        let o = RustItemFinder.outline("");
        assert!(o.headline.is_none());
        assert!(o.items.is_empty());
        assert_eq!(o.lines, 0);
    }

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
        let refs = RustItemFinder.refs(src, "blame");
        let kinds: Vec<OccurrenceKind> = refs.iter().map(|r| r.kind).collect();
        assert!(kinds.contains(&OccurrenceKind::Import), "{refs:#?}");
        assert!(kinds.contains(&OccurrenceKind::Definition), "{refs:#?}");
        assert!(kinds.contains(&OccurrenceKind::Call), "{refs:#?}");
        assert!(kinds.contains(&OccurrenceKind::Mention), "{refs:#?}");
    }

    #[test]
    fn a_longer_name_is_not_reported_as_a_reference() {
        let src = "fn blame_range() {}\nlet y = blame_range();\n";
        assert!(
            RustItemFinder.refs(src, "blame").is_empty(),
            "{:?}",
            RustItemFinder.refs(src, "blame")
        );
        assert_eq!(RustItemFinder.refs(src, "blame_range").len(), 2);
    }

    #[test]
    fn references_separate_the_definition_from_the_uses() {
        let refs = RustItemFinder.refs(SAMPLE, "blame");
        let defs: Vec<&Occurrence> = refs
            .iter()
            .filter(|r| r.kind == OccurrenceKind::Definition)
            .collect();
        assert_eq!(defs.len(), 1, "one definition: {defs:#?}");
        assert!(refs.len() > 1, "and uses besides it: {}", refs.len());
    }

    fn end_of(src: &str) -> Option<usize> {
        let lines: Vec<&str> = src.lines().collect();
        balance_from(&lines, 0)
    }

    #[test]
    fn a_brace_char_literal_does_not_count() {
        assert_eq!(end_of("fn f() {\n    let c = '}';\n}\n"), Some(2));
        assert_eq!(end_of("fn f() {\n    let c = 'x';\n}\n"), Some(2));
    }

    #[test]
    fn an_escaped_quote_char_literal_does_not_open_a_string() {
        assert_eq!(
            end_of("fn f() {\n    let c = '\\'';\n    let d = '}';\n}\n"),
            Some(3)
        );
    }

    #[test]
    fn a_lifetime_is_not_a_char_literal() {
        assert_eq!(end_of("fn f<'a>(x: &'a str) {\n    g('}');\n}\n"), Some(2));
    }

    #[test]
    fn a_brace_in_a_raw_string_does_not_count() {
        assert_eq!(end_of("fn f() {\n    let s = r#\"}\"#;\n}\n"), Some(2));
        assert_eq!(end_of("fn f() {\n    let s = br\"}\";\n}\n"), Some(2));
    }

    #[test]
    fn a_string_spanning_lines_hides_its_braces() {
        assert_eq!(end_of("fn f() {\n    let s = \"a\n}\n\";\n}\n"), Some(4));
    }

    #[test]
    fn a_brace_in_a_comment_does_not_count() {
        assert_eq!(end_of("fn f() {\n    // }\n}\n"), Some(2));
        assert_eq!(end_of("fn f() {\n    /* } /* } */ } */\n}\n"), Some(2));
    }

    #[test]
    fn an_identifier_ending_in_r_is_not_a_raw_string() {
        assert_eq!(
            end_of("fn f() {\n    let br = 1; let r = \"}\";\n}\n"),
            Some(2)
        );
    }

    #[test]
    fn an_array_type_in_a_signature_does_not_end_the_item() {
        let src = "pub fn digest(x: &[u8]) -> [u8; 32] {\n    let mut out = [0u8; 32];\n    out[0] = x[0];\n    out\n}\n";
        assert_eq!(end_of(src), Some(4));
    }

    #[test]
    fn an_array_type_in_an_impl_header_does_not_end_the_impl() {
        let src = "impl<const N: usize> From<[u8; N]> for W {\n    fn from(_: [u8; N]) -> Self { W }\n}\n";
        assert_eq!(end_of(src), Some(2));
    }

    #[test]
    fn a_semicolon_outside_brackets_still_ends_a_blockless_item() {
        assert_eq!(end_of("struct S([u8; 4]);\nfn g() {}\n"), Some(0));
        assert_eq!(end_of("type A = [u8; 4];\nfn g() {}\n"), Some(0));
    }

    #[test]
    fn an_impl_names_the_type_it_implements_for() {
        assert_eq!(impl_target("impl Ty {"), Some("Ty"));
        assert_eq!(impl_target("impl<T> Trait for Ty<T> {"), Some("Ty"));
        assert_eq!(impl_target("impl<F: Fn() -> u8> Ty<F> {"), Some("Ty"));
        assert_eq!(
            impl_target("impl<const N: usize> From<[u8; N]> for W {"),
            Some("W")
        );
        assert_eq!(impl_target("implement {"), None);
    }

    #[test]
    fn an_impl_with_a_space_before_its_generics_still_names_its_type() {
        assert_eq!(impl_target("impl <T> Ty<T> {"), Some("Ty"));
    }

    #[test]
    fn an_impl_for_a_type_without_a_name_gives_none() {
        assert_eq!(impl_target("impl<T> Tr for [T; 3] {"), None);
        assert_eq!(impl_target("impl<A, B> Tr for (A, B) {"), None);
        assert_eq!(impl_target("impl<'a, T> Tr for &'a T {"), None);
    }

    #[test]
    fn an_impl_for_an_array_adds_no_broken_names_to_the_outline() {
        let src = "trait Tr { fn b(); }\nimpl<T> Tr for [T; 3] {\n    fn b() {}\n}\n";
        let outline = RustItemFinder.outline(src);
        assert!(
            outline.items.iter().all(|i| !i.name.contains('[')),
            "{:?}",
            outline.items.iter().map(|i| &i.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_impl_with_a_const_generic_array_is_outlined_whole() {
        let src = "struct W;\nimpl<const N: usize> From<[u8; N]> for W {\n    fn from(_: [u8; N]) -> Self { W }\n}\n";
        let outline = RustItemFinder.outline(src);
        let names: Vec<&str> = outline.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["W", "impl W", "W::from"]);
    }

    #[test]
    fn a_const_whose_name_starts_with_fn_is_a_const() {
        let outline = RustItemFinder.outline("pub const fn_count: usize = 3;\n");
        assert_eq!(outline.items.len(), 1, "{:?}", outline.items);
        assert_eq!(outline.items[0].kind, ItemKind::Const);
        assert_eq!(outline.items[0].name, "fn_count");
    }

    #[test]
    fn an_extern_c_fn_is_a_fn() {
        let src = "pub extern \"C\" fn callback(x: i32) -> i32 {\n    x\n}\n";
        let outline = RustItemFinder.outline(src);
        assert_eq!(outline.items.len(), 1, "{:?}", outline.items);
        assert_eq!(outline.items[0].kind, ItemKind::Fn);
        assert_eq!(outline.items[0].name, "callback");
        assert!(RustItemFinder.find(src, "callback").is_some());
    }

    #[test]
    fn a_const_fn_is_a_fn() {
        let outline = RustItemFinder.outline("pub const fn zero() -> u8 {\n    0\n}\n");
        assert_eq!(outline.items[0].kind, ItemKind::Fn);
        assert_eq!(outline.items[0].name, "zero");
    }
}
