//! Provider lifecycle for the Claude CLI backend: builder, `LlmProvider`
//! impl (delegates the subprocess work to the runner trait), default
//! capabilities, and the `claude_cli()` convenience constructor. Tests
//! that exercise the builder / argv shape live at the bottom of this
//! file; they reach through [`super::argv`] for the pure helpers and
//! through [`super::subprocess::RealSubprocessRunner`] only for the
//! production path.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use tars_types::{
    Capabilities, ChatEvent, ChatRequest, Modality, PromptCacheKind, ProviderError, ProviderId,
    RequestContext, StopReason, StructuredOutputMode,
};

use crate::provider::{LlmEventStream, LlmProvider};

use super::argv::{
    ClaudeCliEffort, ClaudeCliTools, STRIPPED_ENV_KEYS_UPPER, SubprocessInvocation,
    SubprocessRunner, serialize_messages_for_cli,
};
use super::subprocess::{RealSubprocessRunner, extract_result_text, extract_usage};

#[derive(Clone, Debug)]
pub struct ClaudeCliProviderBuilder {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    capabilities: Option<Capabilities>,
    tools: ClaudeCliTools,
    bare: bool,
    effort: Option<ClaudeCliEffort>,
    exclude_dynamic_sections: bool,
    extra_args: Vec<String>,
}

impl ClaudeCliProviderBuilder {
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            id: id.into(),
            executable: "claude".to_string(),
            timeout: Duration::from_secs(300),
            capabilities: None,
            // Default-safe values — see field doc-comments below.
            tools: ClaudeCliTools::Disabled,
            bare: false,
            effort: None,
            exclude_dynamic_sections: true,
            extra_args: Vec::new(),
        }
    }

    builder_setter! {
        /// Override the binary path / name. Defaults to `claude` (PATH lookup).
        executable: into String
    }
    builder_setter!(timeout: Duration);
    builder_setter!(capabilities: opt Capabilities);
    builder_setter! {
        /// Configure `--tools`. Default: [`ClaudeCliTools::Disabled`] — kills
        /// the CLI's internal agent loop without affecting auth. Use
        /// [`ClaudeCliTools::Allow`] for a curated tool whitelist or
        /// [`ClaudeCliTools::Default`] to get the CLI's full agent behavior.
        tools: ClaudeCliTools
    }
    builder_setter! {
        /// Set `--bare`. **Default: `false`.** Setting `true` makes the CLI
        /// skip auto-memory / `CLAUDE.md` auto-discovery / hooks / plugin sync
        /// — but **also disables OAuth + keychain auth**, requiring
        /// `ANTHROPIC_API_KEY` or `apiKeyHelper` to be set. Most `claude_cli`
        /// users authenticate via `claude login` (OAuth + keychain), so the
        /// default is `false` to preserve that path.
        bare: bool
    }
    builder_setter! {
        /// Set `--effort`. Default: `None` (CLI default, currently `medium`).
        effort: Option<ClaudeCliEffort>
    }
    builder_setter! {
        /// Set `--exclude-dynamic-system-prompt-sections`. Default: `true`
        /// (improves cross-tenant prompt-cache reuse by stripping per-machine
        /// `cwd` / `env` / `git status` sections out of the system prompt).
        exclude_dynamic_sections: bool
    }
    builder_setter! {
        /// Escape hatch: append raw argv tokens after every flag the Builder
        /// constructs. Use for flags the Builder doesn't yet model. Don't use
        /// to override flags already set — argv order matters for some flags
        /// and the Builder's value will win on others.
        extra_args: Vec<String>
    }

    /// Build with the default real-process runner.
    pub fn build(self) -> Arc<ClaudeCliProvider> {
        self.build_with_runner(Arc::new(RealSubprocessRunner))
    }

    /// Build with a substituted runner — for tests.
    pub fn build_with_runner(self, runner: Arc<dyn SubprocessRunner>) -> Arc<ClaudeCliProvider> {
        let caps = self.capabilities.unwrap_or_else(default_capabilities);
        Arc::new(ClaudeCliProvider {
            id: self.id,
            executable: self.executable,
            timeout: self.timeout,
            capabilities: caps,
            tools: self.tools,
            bare: self.bare,
            effort: self.effort,
            exclude_dynamic_sections: self.exclude_dynamic_sections,
            extra_args: self.extra_args,
            runner,
        })
    }
}

fn default_capabilities() -> Capabilities {
    let mut text = std::collections::HashSet::new();
    text.insert(Modality::Text);
    Capabilities {
        max_context_tokens: 200_000,
        // CLI doesn't expose --max-output-tokens; we post-truncate.
        max_output_tokens: 64_000,
        supports_tool_use: false, // CLI -p mode doesn't expose tool use
        supports_parallel_tool_calls: false,
        supports_structured_output: StructuredOutputMode::None,
        supports_vision: false,
        supports_thinking: false,
        // First iteration: spawn-per-call; cancel works only via Drop
        // before the call begins. Mid-call cancel needs the long-lived
        // mode (Doc 01 §6.2.1).
        supports_cancel: false,
        prompt_cache: PromptCacheKind::Delegated,
        streaming: false,
        modalities_in: text.clone(),
        modalities_out: text,
        // Subscription-billed; per-token pricing N/A here.
        pricing: tars_types::Pricing::default(),
    }
}

pub struct ClaudeCliProvider {
    id: ProviderId,
    executable: String,
    timeout: Duration,
    capabilities: Capabilities,
    tools: ClaudeCliTools,
    bare: bool,
    effort: Option<ClaudeCliEffort>,
    exclude_dynamic_sections: bool,
    extra_args: Vec<String>,
    runner: Arc<dyn SubprocessRunner>,
}

#[async_trait]
impl LlmProvider for ClaudeCliProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    // Boundary log — Err exits auto-emit with provider/model context
    // (see anthropic.stream for the rationale).
    #[tracing::instrument(
        name = "claude_cli.stream",
        skip_all,
        fields(provider = %self.id, model = %req.model.label()),
        err(Display),
    )]
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        _ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        let model = req
            .model
            .explicit()
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "model must be explicit before reaching CLI provider".into(),
                )
            })?
            .to_string();

        let prompt = serialize_messages_for_cli(&req);
        let system = req.system.clone();

        let invocation = SubprocessInvocation {
            executable: self.executable.clone(),
            model: model.clone(),
            system,
            prompt,
            timeout: self.timeout,
            stripped_env: STRIPPED_ENV_KEYS_UPPER
                .iter()
                .map(|s| s.to_string())
                .collect(),
            tools: self.tools.clone(),
            bare: self.bare,
            effort: self.effort,
            exclude_dynamic_sections: self.exclude_dynamic_sections,
            extra_args: self.extra_args.clone(),
        };

        let payload = self.runner.run(invocation).await?;
        let response_text = extract_result_text(&payload);
        let usage = extract_usage(&payload);

        let max_chars = req.max_output_tokens.map(|t| (t as usize) * 4);
        let (truncated, was_truncated) = match max_chars {
            // Truncate on a UTF-8 char boundary — `cap` (max_output_tokens
            // * 4) can land mid-codepoint, and byte-indexing `[..cap]`
            // would panic. `truncate_utf8` rounds down to the previous
            // boundary (no ellipsis, so the byte cap is still honored).
            Some(cap) if response_text.len() > cap => (
                crate::http_base::truncate_utf8(&response_text, cap).to_string(),
                true,
            ),
            _ => (response_text, false),
        };

        // Report MaxTokens when WE clipped the output to honor the
        // caller's budget — otherwise a truncated reply looks like a
        // natural end-of-turn and consumers won't know it was cut.
        let stop_reason = if was_truncated {
            StopReason::MaxTokens
        } else {
            StopReason::EndTurn
        };

        let events: Vec<Result<ChatEvent, ProviderError>> = vec![
            Ok(ChatEvent::started(model)),
            Ok(ChatEvent::Delta { text: truncated }),
            Ok(ChatEvent::Finished { stop_reason, usage }),
        ];

        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// Convenience builder — the most common path.
pub fn claude_cli(id: impl Into<ProviderId>) -> Arc<ClaudeCliProvider> {
    ClaudeCliProviderBuilder::new(id).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use tars_types::{Message, ModelHint};

    use super::super::argv::{build_argv, build_argv_with};

    /// Test runner that returns a canned payload and records the invocation.
    struct FakeRunner {
        payload: Value,
        recorded: std::sync::Mutex<Option<SubprocessInvocation>>,
    }

    #[async_trait]
    impl SubprocessRunner for FakeRunner {
        async fn run(&self, invocation: SubprocessInvocation) -> Result<Value, ProviderError> {
            *self.recorded.lock().unwrap() = Some(invocation);
            Ok(self.payload.clone())
        }
    }

    fn make_provider(payload: Value) -> (Arc<ClaudeCliProvider>, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner {
            payload,
            recorded: std::sync::Mutex::new(None),
        });
        let p = ClaudeCliProviderBuilder::new("claude_cli_test").build_with_runner(runner.clone());
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
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "hello from claude");
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 5);
        assert_eq!(resp.usage.cached_input_tokens, 3);

        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.model, "opus");
        // env strip list is correctly populated case-insensitively
        assert!(inv.stripped_env.contains("ANTHROPIC_API_KEY"));
        assert!(inv.stripped_env.contains("CLAUDE_CODE_USE_BEDROCK"));
    }

    #[tokio::test]
    async fn upstream_error_propagates() {
        struct ErrRunner;
        #[async_trait]
        impl SubprocessRunner for ErrRunner {
            async fn run(&self, _: SubprocessInvocation) -> Result<Value, ProviderError> {
                Err(ProviderError::CliSubprocessDied {
                    exit_code: Some(0),
                    stderr: "claude CLI returned error: rate limited".into(),
                })
            }
        }
        let provider = ClaudeCliProviderBuilder::new("c").build_with_runner(Arc::new(ErrRunner));
        let err = match provider
            .clone()
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "x"),
                RequestContext::test_default(),
            )
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
            .complete(
                ChatRequest::user(ModelHint::Explicit("opus".into()), "hi"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "");
    }

    #[tokio::test]
    async fn truncates_when_max_output_tokens_exceeded() {
        let big = "x".repeat(1000);
        let payload = json!({"result": big, "is_error": false});
        let (provider, _) = make_provider(payload);
        let mut req = ChatRequest::user(ModelHint::Explicit("opus".into()), "hi");
        req.max_output_tokens = Some(10); // ~40 chars at 4 chars/token
        let resp = provider
            .complete(req, RequestContext::test_default())
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
                ChatRequest::user(ModelHint::Explicit("opus".into()), "x").with_system("be brief"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        let inv = runner.recorded.lock().unwrap().clone().unwrap();
        assert_eq!(inv.system.as_deref(), Some("be brief"));
    }

    #[test]
    fn message_serializer_preserves_role_tags() {
        let req = ChatRequest {
            model: ModelHint::Explicit("x".into()),
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
            thinking: Default::default(),
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
        // The list MUST include ANTHROPIC_API_KEY — that's the entire
        // point of CLI-mode auth.
        assert!(STRIPPED_ENV_KEYS_UPPER.contains(&"ANTHROPIC_API_KEY"));
    }

    // ─── argv-shape tests ──────────────────────────────────────────────
    //
    // The point of these tests is *not* to assert that the CLI does what
    // we hope. The point is: when Anthropic renames `--tools` to
    // `--enabled-tools` (or whatever), exactly the tests below break,
    // they break loudly, and we know to update one constant in one place.

    fn make_invocation(
        configure: impl FnOnce(ClaudeCliProviderBuilder) -> ClaudeCliProviderBuilder,
    ) -> SubprocessInvocation {
        // Record-only runner — doesn't return a real payload, but
        // captures the SubprocessInvocation that build_argv consumed.
        struct RecRunner(std::sync::Mutex<Option<SubprocessInvocation>>);
        #[async_trait]
        impl SubprocessRunner for RecRunner {
            async fn run(&self, inv: SubprocessInvocation) -> Result<Value, ProviderError> {
                *self.0.lock().unwrap() = Some(inv);
                Ok(json!({"result": "", "is_error": false}))
            }
        }
        let runner = Arc::new(RecRunner(std::sync::Mutex::new(None)));
        let provider =
            configure(ClaudeCliProviderBuilder::new("test")).build_with_runner(runner.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _ = provider
                .complete(
                    ChatRequest::user(ModelHint::Explicit("opus".into()), "hi"),
                    RequestContext::test_default(),
                )
                .await;
        });
        runner.0.lock().unwrap().clone().unwrap()
    }

    /// Helper: find argv index of a flag token; panic if not present.
    fn idx(argv: &[String], flag: &str) -> usize {
        argv.iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("flag {flag:?} not in argv: {argv:?}"))
    }

    #[test]
    fn argv_default_is_pure_inference_subscription_friendly() {
        let inv = make_invocation(|b| b);
        let argv = build_argv(&inv);

        // Required backbone — unchanged from the pre-Builder-overhaul shape.
        assert_eq!(argv[0], "-p");
        assert_eq!(argv[1], "-");
        let m = idx(&argv, "--model");
        assert_eq!(argv[m + 1], "opus");
        let of = idx(&argv, "--output-format");
        assert_eq!(argv[of + 1], "json");
        assert!(argv.iter().any(|a| a == "--disable-slash-commands"));

        // Default tools = Disabled → "--tools" followed by literal "".
        let t = idx(&argv, "--tools");
        assert_eq!(
            argv[t + 1],
            "",
            "tools=Disabled must yield literal empty-string value, not omission or sentinel"
        );

        // Default bare = false → no --bare token.
        assert!(
            !argv.iter().any(|a| a == "--bare"),
            "bare default must be false to preserve OAuth/keychain auth"
        );

        // Default effort = None → no --effort token.
        assert!(!argv.iter().any(|a| a == "--effort"));

        // Default exclude_dynamic_sections = true.
        assert!(
            argv.iter()
                .any(|a| a == "--exclude-dynamic-system-prompt-sections")
        );

        // No stray extra_args.
        assert_eq!(
            argv.last().map(|s| s.as_str()),
            Some("--exclude-dynamic-system-prompt-sections")
        );
    }

    #[test]
    fn argv_streaming_true_switches_format_and_adds_partial_msg_verbose() {
        // Pass `streaming = true` directly — no env mutation. (Workspace
        // forbids `unsafe`; Rust 2024 makes env::set_var `unsafe`.)
        let inv = make_invocation(|b| b);
        let argv = build_argv_with(&inv, true);

        let of = idx(&argv, "--output-format");
        assert_eq!(argv[of + 1], "stream-json");

        // claude requires --verbose for the per-event stream to emit
        // alongside stream-json; --include-partial-messages is what
        // adds the text_delta / thinking_delta chunks.
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
        let inv = make_invocation(|b| b.tools(ClaudeCliTools::Default));
        let argv = build_argv(&inv);
        assert!(
            !argv.iter().any(|a| a == "--tools"),
            "tools=Default must omit --tools entirely (lets CLI use its own default)"
        );
    }

    #[test]
    fn argv_tools_allow_serializes_as_csv() {
        let inv = make_invocation(|b| {
            b.tools(ClaudeCliTools::Allow(vec![
                "Read".into(),
                "Bash(git *)".into(),
            ]))
        });
        let argv = build_argv(&inv);
        let t = idx(&argv, "--tools");
        assert_eq!(argv[t + 1], "Read,Bash(git *)");
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

    #[test]
    fn argv_system_prompt_appears_when_set() {
        // Direct construction — we already proved propagation in
        // `system_prompt_propagates_into_invocation` above.
        let inv = SubprocessInvocation {
            executable: "claude".into(),
            model: "opus".into(),
            system: Some("be brief".into()),
            prompt: String::new(),
            timeout: Duration::from_secs(1),
            stripped_env: HashSet::new(),
            tools: ClaudeCliTools::Disabled,
            bare: false,
            effort: None,
            exclude_dynamic_sections: true,
            extra_args: vec![],
        };
        let argv = build_argv(&inv);
        let sp = idx(&argv, "--system-prompt");
        assert_eq!(argv[sp + 1], "be brief");
    }
}
