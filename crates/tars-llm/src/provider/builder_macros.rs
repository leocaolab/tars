//! Generates the builders' plain-assignment setters. A setter that validates or normalises stays
//! hand-written.
//!
//! - `builder_setter!(field: T)` assigns the value.
//! - `builder_setter!(field: into T)` takes `impl Into<T>`.
//! - `builder_setter!(field: opt T)` stores `Some(value)` in an `Option<T>` field.
//! - `builder_setter!(field: into_opt T)` stores `Some(value.into())`.
//!
//! Doc comments on an invocation become the generated setter's docs.

/// Generates one `pub fn` setter of a builder; the forms are listed in the module docs.
#[macro_export]
macro_rules! builder_setter {
    ($(#[$m:meta])* $name:ident : $ty:ty) => {
        $(#[$m])*
        pub fn $name(mut self, v: $ty) -> Self {
            self.$name = v;
            self
        }
    };

    ($(#[$m:meta])* $name:ident : into $ty:ty) => {
        $(#[$m])*
        pub fn $name(mut self, v: impl Into<$ty>) -> Self {
            self.$name = v.into();
            self
        }
    };

    ($(#[$m:meta])* $name:ident : opt $ty:ty) => {
        $(#[$m])*
        pub fn $name(mut self, v: $ty) -> Self {
            self.$name = Some(v);
            self
        }
    };

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
