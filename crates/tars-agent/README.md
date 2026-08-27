The domain model: the Agent abstraction + its vocabulary — pure contracts, zero implementation; the dependency boundary IS the design (an agent that uses no LLM stays first-class).

- Role (hex): core + port (domain contracts; `Agent` is the port that tars-runtime implements)
- Effect budget: none (traits + value types only)
- Deps: may depend on [tars-types ONLY]; MUST NOT import [tars-pipeline (LlmService) or tars-tools (ToolRegistry) — lib.rs states this ban: the Agent trait must physically not be able to reference LLM machinery; rusqlite → tars-storage; reqwest → tars-provider]
- Owns concepts: [Agent, Task, TaskInput, TaskError, AgentOutput, AgentContext, AgentId, TaskId, AgentRole, Skill, SkillSet, Permissions, Decision]
- Reason to change (the ONE): the agent CONTRACT changes (what it means to hand a Task to an Agent)
- Belongs here: a new Permission decision variant; a field on Task; a new agent-vocabulary trait with no impl
- Does NOT belong: a Session loop or any `impl Agent` → tars-runtime; anything mentioning ChatRequest/LlmService → tars-pipeline (and its appearance here would be the exact leak this crate exists to prevent); tool dispatch → tars-tools
