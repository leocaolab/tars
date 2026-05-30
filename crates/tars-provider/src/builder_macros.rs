//! Shared builder-setter macro.
//!
//! Every `*ProviderBuilder` in [`crate::backends`] declares the same
//! `mut self → assign field → return self` setter shape — 31 of them
//! across the 8+ backends at last count. `arc scan --judge` finding
//! `ARC-L5-DUP-2` flagged that mechanical repetition; this macro
//! folds it into one rule with three variants matching the actual
//! field shapes:
//!
//! - `builder_setter!(field: T)` — concrete value assignment.
//! - `builder_setter!(field: into T)` — `impl Into<T>` (for the
//!   ergonomic-conversion fields, mostly strings).
//! - `builder_setter!(field: opt T)` — wraps the argument in `Some`
//!   (for `Option<T>` builder fields whose setter takes the inner
//!   type).
//!
//! ## Doc comments
//!
//! Outer doc comments on the macro invocation are forwarded to the
//! generated `pub fn`, so per-field documentation (e.g.
//! `ClaudeCliProviderBuilder::bare`'s long OAuth-caveat note) is
//! preserved in rustdoc:
//!
//! ```ignore
//! impl ClaudeCliProviderBuilder {
//!     builder_setter! {
//!         /// Set `--bare`. **Default: `false`.** Setting `true` …
//!         bare: bool
//!     }
//! }
//! ```
//!
//! rustdoc renders the expanded `fn bare(mut self, b: bool) -> Self`
//! the same as a hand-written setter would.
//!
//! ## What's NOT covered
//!
//! Setters with non-trivial bodies (validation, normalisation, side
//! effects) stay hand-written. The macro is for the 90 % "pure
//! assignment" case.

/// Generate one `pub fn` setter for a `*ProviderBuilder`. See module
/// docs for the three argument-shape variants and the doc-attribute
/// forwarding rule.
macro_rules! builder_setter {
    // Concrete value: `self.field = v;`
    ($(#[$m:meta])* $name:ident : $ty:ty) => {
        $(#[$m])*
        pub fn $name(mut self, v: $ty) -> Self {
            self.$name = v;
            self
        }
    };

    // `impl Into<T>` (string-y fields): `self.field = v.into();`
    ($(#[$m:meta])* $name:ident : into $ty:ty) => {
        $(#[$m])*
        pub fn $name(mut self, v: impl Into<$ty>) -> Self {
            self.$name = v.into();
            self
        }
    };

    // Optional fields: `self.field = Some(v);`
    ($(#[$m:meta])* $name:ident : opt $ty:ty) => {
        $(#[$m])*
        pub fn $name(mut self, v: $ty) -> Self {
            self.$name = Some(v);
            self
        }
    };

    // Optional + `impl Into<T>`: `self.field = Some(v.into());` —
    // the union of `into` and `opt`, used by string-typed
    // `Option<String>` fields (e.g. ClaudeSdkProviderBuilder's
    // `script_path` / `default_model`).
    ($(#[$m:meta])* $name:ident : into_opt $ty:ty) => {
        $(#[$m])*
        pub fn $name(mut self, v: impl Into<$ty>) -> Self {
            self.$name = Some(v.into());
            self
        }
    };
}

#[cfg(test)]
mod tests {
    // The macro is exercised across the crate's builders. A standalone
    // smoke test pins the three variants compile + behave as advertised.

    #[derive(Default)]
    struct DummyBuilder {
        name: String,
        timeout_ms: u64,
        label: Option<String>,
    }

    impl DummyBuilder {
        builder_setter!(name: into String);
        builder_setter!(timeout_ms: u64);
        builder_setter!(label: opt String);
    }

    #[test]
    fn into_variant_accepts_str_and_string() {
        let b = DummyBuilder::default().name("x").name(String::from("y"));
        assert_eq!(b.name, "y");
    }

    #[test]
    fn concrete_variant_assigns_directly() {
        let b = DummyBuilder::default().timeout_ms(500);
        assert_eq!(b.timeout_ms, 500);
    }

    #[test]
    fn opt_variant_wraps_in_some() {
        let b = DummyBuilder::default().label("hello".into());
        assert_eq!(b.label.as_deref(), Some("hello"));
    }
}
