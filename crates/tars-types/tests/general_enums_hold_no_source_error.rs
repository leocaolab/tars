//! Checks that the enums every producer turns its failure into hold no variant carrying one
//! producer's error type. Such a variant pins that producer's type into the shared abstraction,
//! and the next producer asks for a variant of its own; a producer projects its error onto the
//! general variants at its boundary instead, as `CapError::failure`
//! (`crates/tars-types/src/step/cap.rs`) does.
use std::path::{Path, PathBuf};

use syn::visit::Visit;

/// The enums many producers convert into. `CapError` is in the list because every cap, the
/// user's included, returns it: a cap's own database error belongs in the cap, not in it.
const GENERAL_ENUMS: [&str; 4] = ["Outcome", "Reason", "CapError", "InvalidArgs"];

/// How a variant's field ties the enum to one source's error.
#[derive(Debug, PartialEq, Eq)]
enum Pin {
    /// The field's type names a type whose name ends in `Error`, anywhere inside it.
    ErrorType(String),
    /// The field carries thiserror's `#[from]` or `#[source]`.
    Attribute(&'static str),
    /// The field is named `source`, which thiserror reads as `#[source]`.
    SourceField,
}

#[derive(Debug, PartialEq, Eq)]
struct PinnedVariant {
    enum_name: String,
    variant: String,
    pin: Pin,
}

fn error_type_in(ty: &syn::Type) -> Option<String> {
    struct FirstErrorName(Option<String>);
    impl<'a> Visit<'a> for FirstErrorName {
        fn visit_path_segment(&mut self, segment: &'a syn::PathSegment) {
            if self.0.is_none() && segment.ident.to_string().ends_with("Error") {
                self.0 = Some(segment.ident.to_string());
            }
            syn::visit::visit_path_segment(self, segment);
        }
    }
    let mut found = FirstErrorName(None);
    found.visit_type(ty);
    found.0
}

fn field_pin(field: &syn::Field) -> Option<Pin> {
    let attribute = ["from", "source"]
        .into_iter()
        .find(|name| field.attrs.iter().any(|a| a.path().is_ident(name)));
    if let Some(name) = attribute {
        return Some(Pin::Attribute(name));
    }
    if let Some(name) = error_type_in(&field.ty) {
        return Some(Pin::ErrorType(name));
    }
    field
        .ident
        .as_ref()
        .filter(|ident| *ident == "source")
        .map(|_| Pin::SourceField)
}

fn pinned_variants(item: &syn::ItemEnum) -> Vec<PinnedVariant> {
    item.variants
        .iter()
        .flat_map(|variant| {
            variant
                .fields
                .iter()
                .filter_map(field_pin)
                .map(|pin| PinnedVariant {
                    enum_name: item.ident.to_string(),
                    variant: variant.ident.to_string(),
                    pin,
                })
        })
        .collect()
}

fn general_enums(file: &syn::File) -> Vec<syn::ItemEnum> {
    struct GeneralEnums(Vec<syn::ItemEnum>);
    impl<'a> Visit<'a> for GeneralEnums {
        fn visit_item_enum(&mut self, item: &'a syn::ItemEnum) {
            if GENERAL_ENUMS.iter().any(|name| item.ident == name) {
                self.0.push(item.clone());
            }
        }
    }
    let mut found = GeneralEnums(Vec::new());
    found.visit_file(file);
    found.0
}

fn pins_in(source: &str) -> Vec<PinnedVariant> {
    let file = syn::parse_file(source).expect("the fixture is valid Rust");
    general_enums(&file)
        .iter()
        .flat_map(pinned_variants)
        .collect()
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
                .path()
        })
        .flat_map(|path| match path.extension() {
            _ if path.is_dir() => rust_files(&path),
            Some(ext) if ext == "rs" => vec![path],
            _ => vec![],
        })
        .collect()
}

#[test]
fn no_general_enum_variant_holds_one_sources_error() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let parsed: Vec<(PathBuf, syn::File)> = rust_files(&src)
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            let file = syn::parse_file(&text)
                .unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
            (path, file)
        })
        .collect();

    let enums: Vec<(&Path, syn::ItemEnum)> = parsed
        .iter()
        .flat_map(|(path, file)| {
            general_enums(file)
                .into_iter()
                .map(move |e| (path.as_path(), e))
        })
        .collect();
    for name in GENERAL_ENUMS {
        let count = enums.iter().filter(|(_, e)| e.ident == name).count();
        assert_eq!(
            count,
            1,
            "`{name}` is defined {count} times under {}; the check reads each general enum \
             once, so update GENERAL_ENUMS when one is renamed, moved out or split",
            src.display()
        );
    }

    let pinned: Vec<String> = enums
        .iter()
        .flat_map(|(path, e)| {
            pinned_variants(e).into_iter().map(move |p| {
                format!(
                    "{}: `{}::{}` {:?}",
                    path.display(),
                    p.enum_name,
                    p.variant,
                    p.pin
                )
            })
        })
        .collect();
    assert!(
        pinned.is_empty(),
        "these variants pin one source's error type into an enum every producer converts \
         into; project the source's error onto the general variants at its boundary instead \
         (see `CapError::failure` in crates/tars-types/src/step/cap.rs):\n{}",
        pinned.join("\n")
    );
}

#[test]
fn a_from_attribute_is_a_pin_and_a_plain_string_is_not() {
    let pins = pins_in("enum Reason { Denied(String), Provider(#[from] ProviderError) }");
    assert_eq!(
        pins,
        vec![PinnedVariant {
            enum_name: "Reason".into(),
            variant: "Provider".into(),
            pin: Pin::Attribute("from"),
        }]
    );
}

#[test]
fn a_source_attribute_on_a_type_not_named_error_is_a_pin() {
    let pins = pins_in("enum Reason { Wrapped { #[source] cause: Detail } }");
    assert_eq!(pins[0].pin, Pin::Attribute("source"));
}

#[test]
fn a_type_ending_in_error_is_a_pin_without_any_attribute() {
    let pins = pins_in("enum CapError { Io(std::io::Error) }");
    assert_eq!(pins[0].pin, Pin::ErrorType("Error".into()));
}

#[test]
fn an_error_type_inside_a_generic_is_a_pin() {
    let pins = pins_in("enum Outcome { Failed(Box<dyn std::error::Error + Send>) }");
    assert_eq!(pins[0].pin, Pin::ErrorType("Error".into()));
}

#[test]
fn a_type_that_only_starts_with_error_is_not_a_pin() {
    assert_eq!(
        pins_in("enum Reason { Replayed { class: crate::ErrorClass } }"),
        vec![]
    );
}

#[test]
fn a_field_named_source_is_a_pin() {
    let pins = pins_in("enum CapError { Failed { source: Detail } }");
    assert_eq!(pins[0].pin, Pin::SourceField);
}

#[test]
fn another_reason_is_not_a_pin() {
    assert_eq!(pins_in("enum CapError { Failed(crate::Reason) }"), vec![]);
}

#[test]
fn an_enum_outside_the_list_is_not_read() {
    assert_eq!(
        pins_in("enum LlmError { Provider(#[from] ProviderError) }"),
        vec![]
    );
}
