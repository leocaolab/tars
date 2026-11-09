//! Runs a stand-in `claude` through [`RealSubprocessRunner::run`] under the real OS sandbox and
//! checks which variables of the tars process reach it.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use tars_llm::provider::backends::claude_cli::{
    ClaudeToolAccess, RealSubprocessRunner, SubprocessInvocation, SubprocessRunner,
};

/// `CARGO_MANIFEST_DIR` stands for any variable of the tars process the CLI does not need, a
/// provider key say; cargo sets it for every test run, and setting one needs `unsafe`.
#[tokio::test]
async fn a_delegate_gets_the_system_variables_and_not_the_rest() {
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    let dir = tempfile::tempdir().unwrap();
    let worktree = std::fs::canonicalize(dir.path()).unwrap();
    let script = worktree.join("mock_claude.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\ncat > /dev/null\n\
         printf '{\"type\":\"result\",\"result\":\"%s|%s\",\"is_error\":false}\\n' \
         \"${CARGO_MANIFEST_DIR:-unset}\" \"${PATH:+path}\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let inv = SubprocessInvocation {
        executable: script.to_string_lossy().into_owned(),
        model: "mock-model".into(),
        system: None,
        prompt: "hi".into(),
        timeout: Duration::from_secs(30),
        stripped_env: HashSet::new(),
        tools: ClaudeToolAccess::default(),
        bare: false,
        effort: None,
        exclude_dynamic_sections: false,
        extra_args: Vec::new(),
        cwd: Some(worktree.clone()),
        sandbox: tars_sandbox::SandboxPolicy::default(),
    };

    let payload = RealSubprocessRunner.run(inv).await.unwrap().payload;

    assert_eq!(payload["result"], "unset|path", "{payload}");
}
