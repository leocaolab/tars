//! [`bind`] — wire a [`SkillSet`] to the concrete tools that back it, so an
//! agent's advertised capabilities and its dispatchable [`ToolRegistry`]
//! can't drift apart (Doc 21 §5, closed by Doc 23 M3).
//!
//! A [`Skill`](tars_model::Skill) is the *advertised* capability (what the
//! agent says it can do); a [`Tool`] is the *executable* primitive. By
//! convention they share a name (`fs.write_file`, `bash.run`). `bind` builds
//! the registry and asserts every advertised skill has a tool of the same
//! name — catching the "agent claims `fs.edit` but nothing implements it"
//! bug at construction, not at the first model call.
//!
//! This lives in `tars-runtime` (not `tars-tools`) on purpose: `tars-tools`
//! is a leaf crate and must not depend on `tars-model`, where `SkillSet`
//! lives.

use std::sync::Arc;

use tars_model::SkillSet;
use tars_tools::{Tool, ToolRegistry, ToolRegistryError};

/// Why a [`bind`] failed.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// A skill is advertised but no tool of that name was supplied.
    #[error("skill `{0}` is advertised but no tool backs it")]
    Unbacked(String),
    /// Two tools claimed the same name.
    #[error(transparent)]
    Registry(#[from] ToolRegistryError),
}

/// Build a [`ToolRegistry`] from `tools` and verify it backs every skill in
/// `skills` (by name). Extra tools with no advertised skill are allowed — a
/// skill is the *advertised* capability, and an agent may hold private tools
/// it doesn't advertise. The reverse (an advertised skill with no tool) is
/// the drift this guards against.
pub fn bind(skills: &SkillSet, tools: Vec<Arc<dyn Tool>>) -> Result<ToolRegistry, BindError> {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool)?;
    }
    for skill in skills.iter() {
        if registry.get(&skill.name).is_none() {
            return Err(BindError::Unbacked(skill.name.clone()));
        }
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_model::Skill;
    use tars_tools::builtins::WriteFileTool;

    #[test]
    fn binds_when_every_skill_has_a_backing_tool() {
        let skills = SkillSet::new().with(Skill::new("fs.write_file", "write files"));
        let reg = bind(&skills, vec![Arc::new(WriteFileTool::new())]).expect("should bind");
        assert!(reg.get("fs.write_file").is_some());
    }

    // E2E-5: advertised skill with no backing tool is rejected at bind time.
    #[test]
    fn rejects_an_advertised_skill_with_no_tool() {
        let skills = SkillSet::new().with(Skill::new("fs.edit", "edit files"));
        // WriteFileTool is named `fs.write_file`, not `fs.edit`.
        let err = bind(&skills, vec![Arc::new(WriteFileTool::new())])
            .err()
            .expect("fs.edit is unbacked");
        match err {
            BindError::Unbacked(name) => assert_eq!(name, "fs.edit"),
            other => panic!("expected Unbacked, got {other:?}"),
        }
    }

    #[test]
    fn extra_unadvertised_tools_are_allowed() {
        // A tool with no matching skill is fine — skills are what's
        // advertised, not an exhaustive list of what's registered.
        let skills = SkillSet::new();
        let reg = bind(&skills, vec![Arc::new(WriteFileTool::new())]).expect("should bind");
        assert!(reg.get("fs.write_file").is_some());
    }
}
