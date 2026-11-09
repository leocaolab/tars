//! Builds the Claude Code CLI provider: an
//! [`AgentCliBackend`](crate::provider::backends::cli::AgentCliBackend) with a
//! [`ClaudeCliDialect`](crate::provider::backends::cli::ClaudeCliDialect) and the real runner.

use std::sync::Arc;
use std::time::Duration;

use tars_types::{ProviderId, ProviderProfile};

use crate::provider::backends::cli::{
    AgentCliBackend, ClaudeCliDialect, ClaudeCliEffort, ClaudeToolAccess, RealSubprocessRunner,
    SubprocessRunner,
};
use crate::provider::wire::WireRecorder;

#[derive(Clone, Debug)]
pub struct ClaudeCliProviderBuilder {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    capabilities: Option<ProviderProfile>,
    tools: ClaudeToolAccess,
    bare: bool,
    effort: Option<ClaudeCliEffort>,
    exclude_dynamic_sections: bool,
    extra_args: Vec<String>,
    wire: Option<WireRecorder>,
}

impl ClaudeCliProviderBuilder {
    pub fn new(id: ProviderId) -> Self {
        Self {
            id,
            executable: tars_config::providers::CLAUDE_CLI_EXECUTABLE.to_string(),
            timeout: Duration::from_secs(tars_config::providers::CLI_TIMEOUT_SECS),
            capabilities: None,
            tools: ClaudeToolAccess::Disabled,
            bare: false,
            effort: None,
            exclude_dynamic_sections: tars_config::providers::CLAUDE_CLI_EXCLUDE_DYNAMIC_SECTIONS,
            extra_args: Vec::new(),
            wire: None,
        }
    }

    builder_setter! {
        /// Sets the executable, by default `claude` looked up in `PATH`.
        executable: into String
    }
    builder_setter!(timeout: Duration);
    builder_setter!(capabilities: opt ProviderProfile);
    builder_setter! {
        /// Sets the tools claude may use, by default none, which keeps it from starting its own
        /// agent loop.
        tools: ClaudeToolAccess
    }
    builder_setter! {
        /// Sets `--bare`, off by default. It skips memory, `CLAUDE.md` discovery, hooks and plugin
        /// sync, but also the `claude login` session, so it needs `ANTHROPIC_API_KEY` or an
        /// `apiKeyHelper`, and `ANTHROPIC_API_KEY` is stripped from the child.
        bare: bool
    }
    builder_setter! {
        /// Sets `--effort`; `None`, the default, leaves the CLI's own.
        effort: Option<ClaudeCliEffort>
    }
    builder_setter! {
        /// Sets `--exclude-dynamic-system-prompt-sections`, on by default: without the
        /// per-machine cwd, environment and git status, the system prompt caches across machines.
        exclude_dynamic_sections: bool
    }
    builder_setter! {
        /// Sets arguments appended after all the others, for flags the builder does not model.
        /// Repeating a flag the builder already sets does not reliably override it.
        extra_args: Vec<String>
    }
    builder_setter! {
        /// Sets where each run's exchanged bytes are recorded; `None` records none.
        wire: Option<WireRecorder>
    }

    /// Returns the provider, spawning the real `claude`.
    pub fn build(self) -> Arc<AgentCliBackend> {
        self.build_with_runner(Arc::new(RealSubprocessRunner))
    }

    /// Returns the provider with `runner` in place of the real one.
    pub fn build_with_runner(self, runner: Arc<dyn SubprocessRunner>) -> Arc<AgentCliBackend> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        let dialect = Arc::new(ClaudeCliDialect::new(
            self.executable,
            self.timeout,
            self.tools,
            self.bare,
            self.effort,
            self.exclude_dynamic_sections,
            self.extra_args,
        ));
        Arc::new(AgentCliBackend::new(self.id, caps, dialect, runner).wire(self.wire))
    }
}

/// Returns the capabilities of the `claude_cli` block of `data/provider.toml`.
fn default_capabilities() -> ProviderProfile {
    tars_config::capabilities_for("claude_cli", "")
}

/// Returns a provider with every default.
pub fn claude_cli(id: ProviderId) -> Arc<AgentCliBackend> {
    ClaudeCliProviderBuilder::new(id).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderError;
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use tars_types::{Caller, ChatRequest, Message};

    use crate::provider::LlmProvider;
    use crate::provider::backends::cli::claude_argv::{
        STRIPPED_ENV_KEYS_UPPER, build_argv, build_argv_with,
    };
    use crate::provider::backends::cli::prompt::serialize_messages_for_cli;
    use crate::provider::backends::cli::{RunFailure, RunOutput, SubprocessInvocation};

    /// Test runner that returns a canned payload and records the invocation.
    struct FakeRunner {
        payload: Value,
        recorded: std::sync::Mutex<Option<SubprocessInvocation>>,
    }

    #[async_trait]
    impl SubprocessRunner for FakeRunner {
        async fn run(&self, invocation: SubprocessInvocation) -> Result<RunOutput, RunFailure> {
            *self.recorded.lock().unwrap() = Some(invocation);
            Ok(RunOutput {
                payload: self.payload.clone(),
                exchanged: Default::default(),
            })
        }
    }

    fn make_provider(payload: Value) -> (Arc<AgentCliBackend>, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner {
            payload,
            recorded: std::sync::Mutex::new(None),
        });
        let p = ClaudeCliProviderBuilder::new(ProviderId::new("claude_cli_test"))
            .build_with_runner(runner.clone());
        (p, runner)
    }

    #[tokio::test]
    async fn happy_path_returns_text_and_usage() {
        let payload = json!({
            "result": "hello from claude",
            "is_error": false,
            "usage": {
                "input_tokens": 12,
                "output_tokens": 5,
                "cache_read_input_tokens": 3
            }
        });
        let (provider, runner) = make_provider(payload);
        let resp = provider
            .complete(ChatRequest::user("hi"), "opus", Caller::test_default())
            .await
            .unwrap();
        assert_eq!(resp.text, "hello from claude");
        assert_eq!(resp.usage.unwrap().input_tokens, 15);
        assert_eq!(resp.usage.unwrap().output_tokens, 5);
        assert_eq!(resp.usage.unwrap().cached_input_tokens, 3);

        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.model, "opus");
        assert!(inv.stripped_env.contains("ANTHROPIC_API_KEY"));
        assert!(inv.stripped_env.contains("CLAUDE_CODE_USE_BEDROCK"));
    }

    #[tokio::test]
    async fn upstream_error_propagates() {
        struct ErrRunner;
        #[async_trait]
        impl SubprocessRunner for ErrRunner {
            async fn run(&self, _: SubprocessInvocation) -> Result<RunOutput, RunFailure> {
                Err(RunFailure::before_exchange(
                    ProviderError::CliSubprocessDied {
                        exit_code: Some(0),
                        stderr: "claude CLI returned error: rate limited".into(),
                    },
                ))
            }
        }
        let provider = ClaudeCliProviderBuilder::new(ProviderId::new("c"))
            .build_with_runner(Arc::new(ErrRunner));
        let err = match provider
            .clone()
            .complete(ChatRequest::user("x"), "opus", Caller::test_default())
            .await
        {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(matches!(err, ProviderError::CliSubprocessDied { .. }));
    }

    #[tokio::test]
    async fn null_result_becomes_empty_text() {
        let payload = json!({"result": null, "is_error": false});
        let (provider, _) = make_provider(payload);
        let resp = provider
            .complete(ChatRequest::user("hi"), "opus", Caller::test_default())
            .await
            .unwrap();
        assert_eq!(resp.text, "");
    }

    #[tokio::test]
    async fn truncates_when_max_output_tokens_exceeded() {
        let big = "x".repeat(1000);
        let payload = json!({"result": big, "is_error": false});
        let (provider, _) = make_provider(payload);
        let mut req = ChatRequest::user("hi");
        req.max_output_tokens = Some(10); // 40 bytes
        let resp = provider
            .complete(req, "opus", Caller::test_default())
            .await
            .unwrap();
        assert_eq!(resp.text.len(), 40);
    }

    #[tokio::test]
    async fn system_prompt_propagates_into_invocation() {
        let payload = json!({"result": "ok", "is_error": false});
        let (provider, runner) = make_provider(payload);
        let _ = provider
            .complete(
                ChatRequest::user("x").with_system("be brief"),
                "opus",
                Caller::test_default(),
            )
            .await
            .unwrap();
        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        let system = inv.system.as_ref().unwrap();
        assert_eq!(std::fs::read_to_string(system.path()).unwrap(), "be brief");
    }

    #[test]
    fn message_serializer_preserves_role_tags() {
        let req = ChatRequest {
            system: None,
            messages: vec![
                Message::user_text("first user turn"),
                Message::assistant_text("first assistant"),
                Message::user_text("second user turn"),
            ],
            tools: vec![],
            tool_choice: Default::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            seed: None,
            cache_directives: vec![],
            thinking: None,
            enable_chat_template_thinking: None,
        };
        let s = serialize_messages_for_cli(&req);
        assert!(s.contains("[user]\nfirst user turn"));
        assert!(s.contains("[assistant]\nfirst assistant"));
        assert!(s.contains("[user]\nsecond user turn"));
    }

    #[test]
    fn env_strip_list_is_uppercase_for_case_insensitive_match() {
        for k in STRIPPED_ENV_KEYS_UPPER {
            assert_eq!(*k, k.to_uppercase());
        }
        // With it claude bills an API account instead of the subscription.
        assert!(STRIPPED_ENV_KEYS_UPPER.contains(&"ANTHROPIC_API_KEY"));
    }

    fn make_invocation(
        configure: impl FnOnce(ClaudeCliProviderBuilder) -> ClaudeCliProviderBuilder,
    ) -> SubprocessInvocation {
        struct RecRunner(std::sync::Mutex<Option<SubprocessInvocation>>);
        #[async_trait]
        impl SubprocessRunner for RecRunner {
            async fn run(&self, inv: SubprocessInvocation) -> Result<RunOutput, RunFailure> {
                *self.0.lock().unwrap() = Some(inv);
                Ok(RunOutput {
                    payload: json!({"result": "", "is_error": false}),
                    exchanged: Default::default(),
                })
            }
        }
        let runner = Arc::new(RecRunner(std::sync::Mutex::new(None)));
        let provider = configure(ClaudeCliProviderBuilder::new(ProviderId::new("test")))
            .build_with_runner(runner.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _ = provider
                .complete(ChatRequest::user("hi"), "opus", Caller::test_default())
                .await;
        });
        runner.0.lock().unwrap().clone().unwrap()
    }

    #[test]
    fn cwd_flows_from_request_context_into_invocation() {
        // Without the cwd, claude's own tools edit the process cwd and the worktree's diff
        // shows no fix.
        struct RecRunner(std::sync::Mutex<Option<SubprocessInvocation>>);
        #[async_trait]
        impl SubprocessRunner for RecRunner {
            async fn run(&self, inv: SubprocessInvocation) -> Result<RunOutput, RunFailure> {
                *self.0.lock().unwrap() = Some(inv);
                Ok(RunOutput {
                    payload: json!({"result": "", "is_error": false}),
                    exchanged: Default::default(),
                })
            }
        }
        let runner = Arc::new(RecRunner(std::sync::Mutex::new(None)));
        let provider = ClaudeCliProviderBuilder::new(ProviderId::new("test"))
            .build_with_runner(runner.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let wt = std::path::PathBuf::from("/tmp/arc/worktrees/fix-abc");
        rt.block_on(async {
            let _ = provider
                .complete(
                    ChatRequest::user("hi"),
                    "opus",
                    Caller::test_default().with_cwd(wt.clone()),
                )
                .await;
        });
        let inv = runner.0.lock().unwrap().clone().unwrap();
        assert_eq!(
            inv.cwd,
            Some(wt),
            "Caller.cwd must reach SubprocessInvocation.cwd (→ subprocess current_dir)"
        );
    }

    #[test]
    fn no_cwd_in_request_context_leaves_invocation_cwd_none() {
        let inv = make_invocation(|b| b);
        assert_eq!(inv.cwd, None);
    }

    /// Returns where `flag` is in `argv`, panicking when it is absent.
    fn idx(argv: &[String], flag: &str) -> usize {
        argv.iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("flag {flag:?} not in argv: {argv:?}"))
    }

    #[test]
    fn argv_default_is_pure_inference_subscription_friendly() {
        let inv = make_invocation(|b| b);
        let argv = build_argv(&inv);

        assert_eq!(argv[0], "-p");
        assert_eq!(argv[1], "-");
        let m = idx(&argv, "--model");
        assert_eq!(argv[m + 1], "opus");
        let of = idx(&argv, "--output-format");
        assert_eq!(argv[of + 1], "json");
        assert!(argv.iter().any(|a| a == "--disable-slash-commands"));

        let t = idx(&argv, "--tools");
        assert_eq!(
            argv[t + 1],
            "",
            "tools=Disabled must yield literal empty-string value, not omission or sentinel"
        );

        assert!(
            !argv.iter().any(|a| a == "--bare"),
            "bare default must be false to preserve OAuth/keychain auth"
        );

        assert!(!argv.iter().any(|a| a == "--effort"));

        assert!(
            !argv.iter().any(|a| a == "--permission-mode"),
            "tools=Disabled must not request a permission bypass — it has no tools to run"
        );

        assert!(
            argv.iter()
                .any(|a| a == "--exclude-dynamic-system-prompt-sections")
        );

        assert_eq!(
            argv.last().map(|s| s.as_str()),
            Some("--exclude-dynamic-system-prompt-sections")
        );
    }

    #[test]
    fn argv_streaming_true_switches_format_and_adds_partial_msg_verbose() {
        let inv = make_invocation(|b| b);
        let argv = build_argv_with(&inv, true);

        let of = idx(&argv, "--output-format");
        assert_eq!(argv[of + 1], "stream-json");

        assert!(argv.iter().any(|a| a == "--include-partial-messages"));
        assert!(argv.iter().any(|a| a == "--verbose"));
    }

    #[test]
    fn argv_streaming_false_keeps_buffered_json_format() {
        let inv = make_invocation(|b| b);
        let argv = build_argv_with(&inv, false);

        let of = idx(&argv, "--output-format");
        assert_eq!(argv[of + 1], "json");
        assert!(!argv.iter().any(|a| a == "--include-partial-messages"));
        assert!(!argv.iter().any(|a| a == "--verbose"));
    }

    #[test]
    fn argv_tools_default_omits_flag() {
        let inv = make_invocation(|b| b.tools(ClaudeToolAccess::Default));
        let argv = build_argv(&inv);
        assert!(
            !argv.iter().any(|a| a == "--tools"),
            "tools=Default must omit --tools entirely (lets CLI use its own default)"
        );
        let pm = idx(&argv, "--permission-mode");
        assert_eq!(
            argv[pm + 1],
            "bypassPermissions",
            "tools=Default runs the agent loop and edits files — it must bypass the -p permission prompt"
        );
    }

    #[test]
    fn argv_tools_allow_serializes_as_csv() {
        let inv = make_invocation(|b| {
            b.tools(ClaudeToolAccess::Allow(vec![
                "Read".into(),
                "Bash(git *)".into(),
            ]))
        });
        let argv = build_argv(&inv);
        let t = idx(&argv, "--tools");
        assert_eq!(argv[t + 1], "Read,Bash(git *)");
    }

    #[test]
    fn argv_tools_allow_empty_is_disabled_without_permission_bypass() {
        let inv = make_invocation(|b| b.tools(ClaudeToolAccess::Allow(vec![])));
        let argv = build_argv(&inv);
        let t = idx(&argv, "--tools");
        assert_eq!(argv[t + 1], "");
        assert!(!argv.iter().any(|a| a == "--permission-mode"));
    }

    #[test]
    fn argv_bare_opt_in_adds_flag() {
        let inv = make_invocation(|b| b.bare(true));
        let argv = build_argv(&inv);
        assert!(argv.iter().any(|a| a == "--bare"));
    }

    #[test]
    fn argv_effort_renders_each_variant() {
        for (variant, expected) in [
            (ClaudeCliEffort::Low, "low"),
            (ClaudeCliEffort::Medium, "medium"),
            (ClaudeCliEffort::High, "high"),
            (ClaudeCliEffort::Xhigh, "xhigh"),
            (ClaudeCliEffort::Max, "max"),
        ] {
            let inv = make_invocation(|b| b.effort(Some(variant)));
            let argv = build_argv(&inv);
            let e = idx(&argv, "--effort");
            assert_eq!(argv[e + 1], expected, "wrong arg for {variant:?}");
        }
    }

    #[test]
    fn argv_exclude_dynamic_sections_opt_out_drops_flag() {
        let inv = make_invocation(|b| b.exclude_dynamic_sections(false));
        let argv = build_argv(&inv);
        assert!(
            !argv
                .iter()
                .any(|a| a == "--exclude-dynamic-system-prompt-sections"),
            "exclude_dynamic_sections=false must drop the flag"
        );
    }

    #[test]
    fn argv_extra_args_appended_verbatim_at_end() {
        let inv = make_invocation(|b| {
            b.extra_args(vec![
                "--betas".into(),
                "experimental-x".into(),
                "--debug".into(),
            ])
        });
        let argv = build_argv(&inv);
        let last_three = &argv[argv.len() - 3..];
        assert_eq!(last_three, &["--betas", "experimental-x", "--debug"]);
    }

    fn with_system(dir: &std::path::Path, text: String) -> SubprocessInvocation {
        let file = crate::provider::backends::cli::SystemPromptFile::write_in(dir, text)
            .expect("the prompt file");
        SubprocessInvocation {
            executable: "claude".into(),
            model: "opus".into(),
            system: Some(Arc::new(file)),
            prompt: String::new(),
            timeout: Duration::from_secs(1),
            stripped_env: HashSet::new(),
            tools: ClaudeToolAccess::Disabled,
            bare: false,
            effort: None,
            exclude_dynamic_sections: true,
            extra_args: vec![],
            cwd: None,
            sandbox: tars_sandbox::SandboxPolicy::default(),
        }
    }

    #[test]
    fn a_system_prompt_of_any_size_goes_to_claude_as_a_file_never_an_argument() {
        let dir = tempfile::tempdir().expect("a dir");
        let big = "x".repeat(200 * 1024);
        let inv = with_system(dir.path(), big.clone());
        let argv = build_argv(&inv);

        let at = idx(&argv, "--system-prompt-file");
        let path = std::path::PathBuf::from(&argv[at + 1]);
        assert_eq!(
            std::fs::read_to_string(&path).expect("claude can read it"),
            big
        );
        assert!(
            !argv.iter().any(|a| a == "--system-prompt"),
            "{:?}",
            &argv[..at]
        );
        assert!(
            argv.iter().all(|a| a.len() < 4096),
            "no argument carries the prompt"
        );

        drop(inv);
        assert!(
            !path.exists(),
            "the file goes with the last invocation holding it"
        );
    }

    #[test]
    fn a_delegated_claude_runs_no_hooks() {
        let dir = tempfile::tempdir().expect("a dir");
        let argv = build_argv(&with_system(dir.path(), "be brief".into()));
        let at = idx(&argv, "--settings");
        let settings: serde_json::Value =
            serde_json::from_str(&argv[at + 1]).expect("JSON settings");
        assert_eq!(settings["disableAllHooks"], serde_json::Value::Bool(true));
    }

    #[test]
    fn argv_system_prompt_appears_when_set() {
        let dir = tempfile::tempdir().expect("a dir");
        let file = crate::provider::backends::cli::SystemPromptFile::write_in(
            dir.path(),
            "be brief".into(),
        )
        .expect("the prompt file");
        let inv = SubprocessInvocation {
            executable: "claude".into(),
            model: "opus".into(),
            system: Some(Arc::new(file)),
            prompt: String::new(),
            timeout: Duration::from_secs(1),
            stripped_env: HashSet::new(),
            tools: ClaudeToolAccess::Disabled,
            bare: false,
            effort: None,
            exclude_dynamic_sections: true,
            extra_args: vec![],
            cwd: None,
            sandbox: tars_sandbox::SandboxPolicy::default(),
        };
        let argv = build_argv(&inv);
        let sp = idx(&argv, "--system-prompt-file");
        assert_eq!(
            std::fs::read_to_string(&argv[sp + 1]).expect("the file"),
            "be brief"
        );
    }
}
