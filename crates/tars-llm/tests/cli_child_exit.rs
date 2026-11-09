//! Runs stand-in CLIs that answer, leave a background process behind and exit, through the real
//! runners under the real OS sandbox, and checks that the answer comes back when the CLI exits,
//! not when the process it left behind lets go of the pipes, and that the process is killed.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tars_llm::provider::backends::cli::{
    ClaudeToolAccess, CodexCliDialect, CodexSandbox, RealSubprocessRunner, SharedCliRunner,
    SubprocessInvocation, SubprocessRunner,
};

const TIMEOUT: Duration = Duration::from_secs(10);

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("{tag}_{}", std::process::id()));
    if d.exists() {
        std::fs::remove_dir_all(&d).expect("clear the old test dir");
    }
    std::fs::create_dir_all(&d).expect("make the test dir");
    d
}

fn script(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, body).expect("write the stand-in CLI");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    p.to_string_lossy().into_owned()
}

fn inv(executable: String, cwd: PathBuf) -> SubprocessInvocation {
    SubprocessInvocation {
        executable,
        model: "m".into(),
        system: None,
        prompt: "hi".into(),
        timeout: TIMEOUT,
        stripped_env: HashSet::new(),
        tools: ClaudeToolAccess::default(),
        bare: false,
        effort: None,
        exclude_dynamic_sections: false,
        extra_args: Vec::new(),
        cwd: Some(cwd),
        sandbox: tars_sandbox::SandboxPolicy::default(),
    }
}

/// Returns whether the process whose PID the stand-in wrote to `pid_file` still runs.
fn left_behind_runs(pid_file: &Path) -> bool {
    let pid = std::fs::read_to_string(pid_file).expect("the stand-in wrote its helper's pid");
    std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .status()
        .expect("run kill -0")
        .success()
}

#[tokio::test]
async fn claude_answers_when_it_exits_though_a_process_it_left_holds_the_pipes() {
    let wt = fresh_dir("tars_child_exit_claude");
    let pid_file = wt.join("helper.pid");
    let exe = script(
        &wt,
        "fake_claude.sh",
        &format!(
            "#!/bin/sh\ncat >/dev/null\nsleep 30 &\necho $! > {}\n\
             echo '{{\"type\":\"result\",\"result\":\"done\",\"is_error\":false}}'\nexit 0\n",
            pid_file.display()
        ),
    );
    let started = Instant::now();
    let out = RealSubprocessRunner
        .run(inv(exe, wt))
        .await
        .map_err(|f| f.error)
        .expect("the answer comes back");
    assert!(started.elapsed() < TIMEOUT, "took {:?}", started.elapsed());
    assert_eq!(out.payload["result"], "done");
    assert!(!left_behind_runs(&pid_file), "the helper outlived the run");
}

#[tokio::test]
async fn codex_answers_when_it_exits_though_a_process_it_left_holds_the_pipes() {
    let wt = fresh_dir("tars_child_exit_codex");
    let pid_file = wt.join("helper.pid");
    let exe = script(
        &wt,
        "fake_codex.sh",
        &format!(
            "#!/bin/sh\ncat >/dev/null\n\
             echo '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"done\"}}}}'\n\
             echo '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n\
             sleep 30 &\necho $! > {}\nexit 0\n",
            pid_file.display()
        ),
    );
    let dialect = Arc::new(CodexCliDialect::new(
        exe.clone(),
        TIMEOUT,
        CodexSandbox::ReadOnly,
        true,
    ));
    let started = Instant::now();
    let out = SharedCliRunner::new(dialect)
        .run(inv(exe, wt))
        .await
        .map_err(|f| f.error)
        .expect("the answer comes back");
    assert!(started.elapsed() < TIMEOUT, "took {:?}", started.elapsed());
    assert!(out.payload.to_string().contains("done"), "{}", out.payload);
    assert!(!left_behind_runs(&pid_file), "the helper outlived the run");
}

#[tokio::test]
async fn a_cli_that_does_not_exit_still_times_out_and_is_killed_with_its_helpers() {
    let wt = fresh_dir("tars_child_exit_timeout");
    let pid_file = wt.join("helper.pid");
    let exe = script(
        &wt,
        "fake_codex.sh",
        &format!(
            "#!/bin/sh\ncat >/dev/null\nsleep 30 &\necho $! > {}\nsleep 30\n",
            pid_file.display()
        ),
    );
    let dialect = Arc::new(CodexCliDialect::new(
        exe.clone(),
        TIMEOUT,
        CodexSandbox::ReadOnly,
        true,
    ));
    let mut i = inv(exe, wt);
    i.timeout = Duration::from_secs(2);
    let err = SharedCliRunner::new(dialect)
        .run(i)
        .await
        .expect_err("a CLI that never exits times out")
        .error;
    assert!(
        matches!(err, tars_llm::ProviderError::TimedOut { .. }),
        "{err:?}"
    );
    assert!(!left_behind_runs(&pid_file), "the helper outlived the run");
}
