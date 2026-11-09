//! Defines [`TypedCap`], the trait a cap author implements; a blanket impl turns it into
//! the [`Cap`] the runtime holds.
//!
//! `Cap` takes and returns JSON and must stay object-safe, which is right for the runtime
//! and wrong for an author: written directly, every cap would pick its arguments out of a
//! `Value` and hand-write a `CapSpec` beside the Rust types that already describe it. A
//! `TypedCap` declares typed input and output, and the spec is derived from them.
//!
//! Name, doc, effect and both schemas are constants of the type; an instance holds only
//! the resource it is bound to (a root, a pool), so the effect cannot depend on the
//! arguments. The effect stays off [`CapSpec`]: it is read by the gate and the egress
//! audit, and the model choosing a tool does not see it.
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::cap::{Cap, CapCtx, CapError, CapSpec, Effect, InvalidArgs};

/// A cap with typed arguments and reply.
///
/// It uses `#[async_trait]` because the blanket `Cap` impl must return the boxed `Send`
/// future `Arc<dyn Cap>` needs, which a bare `async fn` does not give.
#[async_trait::async_trait]
pub trait TypedCap: Send + Sync {
    /// The name grants refer to and a model emits, written `category.action`.
    const NAME: &'static str;
    /// What the cap does and when it applies, written for the model that chooses it.
    const DOC: &'static str;
    const EFFECT: Effect;

    /// The arguments; the `in_schema` the model is offered is derived from this type.
    type In: DeserializeOwned + schemars::JsonSchema;
    /// The reply; the `out_schema` is derived from this type.
    type Out: Serialize + schemars::JsonSchema;

    async fn call(&self, input: Self::In, ctx: &CapCtx) -> Result<Self::Out, CapError>;
}

pub fn schema_of<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

#[async_trait::async_trait]
impl<T: TypedCap> Cap for T {
    fn spec(&self) -> CapSpec {
        CapSpec {
            name: crate::CapName::new(T::NAME),
            doc: T::DOC.to_string(),
            in_schema: schema_of::<T::In>(),
            out_schema: schema_of::<T::Out>(),
        }
    }

    fn effect(&self) -> Effect {
        T::EFFECT
    }

    async fn invoke(&self, args: Value, ctx: &CapCtx) -> Result<Value, CapError> {
        let input = serde_json::from_value(args.clone()).map_err(|e| {
            CapError::Invalid(InvalidArgs::Undecodable {
                cap: T::NAME.to_string(),
                why: e.to_string(),
                args,
            })
        })?;
        let out = self.call(input, ctx).await?;
        serde_json::to_value(out)
            .map_err(|e| CapError::ExecuteFatal(format!("{}: reply did not encode: {e}", T::NAME)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CapName;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize, schemars::JsonSchema)]
    struct Read {
        /// The file, relative to the root.
        path: String,
        #[serde(default)]
        start: Option<u32>,
    }

    #[derive(Serialize, schemars::JsonSchema)]
    struct Text {
        text: String,
        lines: u32,
    }

    struct FsRead {
        root: &'static str,
    }

    #[async_trait::async_trait]
    impl TypedCap for FsRead {
        const NAME: &'static str = "fs.read";
        const DOC: &'static str = "Read a file under the root.";
        const EFFECT: Effect = Effect::Read;
        type In = Read;
        type Out = Text;

        async fn call(&self, input: Read, _ctx: &CapCtx) -> Result<Text, CapError> {
            Ok(Text {
                text: format!(
                    "{}/{} from {}",
                    self.root,
                    input.path,
                    input.start.unwrap_or(1)
                ),
                lines: 1,
            })
        }
    }

    fn ctx() -> CapCtx {
        CapCtx::unkeyed()
    }

    #[test]
    fn the_spec_is_derived_from_the_types() {
        let spec = Cap::spec(&FsRead { root: "/wt" });
        assert_eq!(spec.name, CapName::new("fs.read"));
        assert_eq!(spec.doc, "Read a file under the root.");
        assert_eq!(spec.in_schema["properties"]["path"]["type"], "string");
        assert_eq!(
            spec.in_schema["properties"]["path"]["description"], "The file, relative to the root.",
            "a doc comment on the field IS the argument's description: {}",
            spec.in_schema
        );
        assert_eq!(spec.in_schema["required"][0], "path");
        assert_eq!(
            spec.out_schema["properties"]["lines"]["type"], "integer",
            "the reply has a shape now: {}",
            spec.out_schema
        );
    }

    #[tokio::test]
    async fn json_in_typed_out() {
        let cap = FsRead { root: "/wt" };
        let out = Cap::invoke(&cap, json!({ "path": "a.rs", "start": 7 }), &ctx())
            .await
            .expect("decodes and runs");
        assert_eq!(out["text"], "/wt/a.rs from 7");
    }

    #[tokio::test]
    async fn a_decode_failure_carries_the_arguments() {
        let cap = FsRead { root: "/wt" };
        let err = Cap::invoke(&cap, json!({ "start": 7 }), &ctx())
            .await
            .expect_err("no `path`");
        let msg = err.to_string();
        assert!(matches!(err, CapError::Invalid(_)), "{msg}");
        assert!(msg.contains("fs.read"), "names the cap: {msg}");
        assert!(msg.contains("path"), "names the field: {msg}");
        assert!(
            msg.contains("\"start\":7"),
            "carries the raw arguments: {msg}"
        );
    }

    #[test]
    fn the_effect_is_a_property_of_the_type() {
        assert_eq!(Cap::effect(&FsRead { root: "/a" }), Effect::Read);
        assert_eq!(Cap::effect(&FsRead { root: "/b" }), Effect::Read);
    }
}
