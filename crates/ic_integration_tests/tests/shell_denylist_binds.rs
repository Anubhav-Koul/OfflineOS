//! The security gate for `builtin.shell`: prove the containment **binds on
//! Windows**, against a running gateway, rather than merely existing in a list.
//!
//! The finding this closes: `shell_core`'s denylist was entirely Unix-shaped —
//! `rm -rf /`, `dd if=/dev/zero`, `sudo `, `/etc/passwd`, `> /dev/sda` — on a
//! fork whose only supported target is Windows, where none of it binds. That is
//! the same failure class this repo has named twice before: a control that
//! reports present and cannot report whether it works. A denylist without a
//! Windows-executing test is exactly that class again, so this file is
//! deliberately *not* a unit test over the matcher (those live in
//! `shell_core.rs`); it drives commands through the real `serve` process, the
//! real capability dispatch, and the real `cmd /C` process port.
//!
//! Three things are pinned, and all three matter:
//!
//! 1. **The kill-switch withholds the capability.** With
//!    `IRONCLAW_SHELL_TOOL_ENABLED=false` — what the widget sends unless the user
//!    turns the shell on — `builtin.shell` is absent from the capability surface
//!    the model is offered, and a call to it does not execute.
//! 2. **The denylist refuses a Windows command,** with the shell switched *on*.
//!    Without part 2 gated on `enabled=true`, a green run would only prove the
//!    flag works and would say nothing about the denylist.
//! 3. **The positive control.** The blocked command is run directly through the
//!    same `cmd /C` the process port uses, against a sacrificial directory, and
//!    it must destroy it. Without this, "the directory survived" could equally
//!    mean the command was a no-op on this machine and the test would pass for
//!    the wrong reason — a green suite next to a broken control, which is the
//!    thing this file exists to prevent.
#![cfg(feature = "webui-v2-beta")]

use std::sync::Arc;
use std::time::Duration;

use ic_integration_tests::{MockReply, MockResponder, RebornServer};

const MARK: &str = "SHELL-GATE";
const DONE: &str = "shell-gate-done";

/// `rd /s /q <dir>` — recursive, forced, no confirmation. A real Windows
/// destructive command that the Unix list has no word for (there is no `rm`, no
/// `-rf`, and no `/`), and one the fork's own denylist must refuse.
///
/// Deliberately a **cmd.exe builtin** rather than `powershell -Command
/// "Remove-Item -Recurse -Force"`. The first draft of this test used PowerShell
/// and the positive control immediately failed with *"'powershell' is not
/// recognized"*: the process port spawns `cmd /C` with a scrubbed environment,
/// and under a shell whose `PATH` has been rewritten (Git Bash, MSYS) the child
/// cannot resolve anything on disk. The refusal assertions would then have gone
/// green for want of an interpreter rather than because the denylist bound —
/// the exact vacuous pass this file exists to rule out. A builtin needs no
/// `PATH`, so the command is reliably lethal and the control is honest.
/// Left unquoted deliberately. The process port spawns
/// `Command::new("cmd").args(["/C", cmd])`, and Rust's Windows argument
/// escaping turns an embedded `"` into `\"`, which `cmd` does not understand —
/// the second draft of this test quoted the path and `cmd` answered *"The
/// filename, directory name, or volume label syntax is incorrect"* (exit 123),
/// tripping the positive control again. [`sacrificial_dir`] asserts the path
/// has no spaces so the unquoted form is unambiguous.
fn destructive_command(target: &str) -> String {
    format!("rd /s /q {target}")
}

fn responder(command: String) -> MockResponder {
    Arc::new(move |body: &str| {
        if body.contains("\"role\":\"tool\"") {
            return MockReply::Text(DONE.to_string());
        }
        if body.contains(MARK) {
            return MockReply::ToolCall {
                name: "builtin__shell".to_string(),
                arguments: serde_json::json!({ "command": command }),
            };
        }
        MockReply::Text("fallthrough".to_string())
    })
}

/// A directory with a file in it, outside any workspace, that must still be
/// there when the test ends.
fn sacrificial_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(
        !dir.path().display().to_string().contains(' '),
        "the temp path contains a space, so the unquoted command in \
         `destructive_command` would be ambiguous: {}",
        dir.path().display()
    );
    std::fs::write(dir.path().join("canary.txt"), b"still here").expect("seed canary");
    dir
}

fn canary_survives(dir: &tempfile::TempDir) -> bool {
    dir.path().join("canary.txt").exists()
}

/// The widget's spelling of the runtime's kill-switch must not drift from the
/// runtime's own. `ic_widget` deliberately depends on no core crate, so the
/// constant is written out there; this is the only place that can compare them.
#[test]
fn the_widget_and_the_runtime_agree_on_the_kill_switch_name() {
    assert_eq!(
        ic_widget::supervisor::SHELL_TOOL_ENABLED_ENV,
        ironclaw_host_runtime::SHELL_TOOL_ENABLED_ENV,
    );
}

/// The positive control, and the reason the two assertions below are not
/// vacuous. Windows-only: it is asserting a fact about `cmd.exe`.
#[cfg(windows)]
#[test]
fn the_denylisted_command_really_is_destructive_on_this_machine() {
    let dir = sacrificial_dir();
    let target = dir.path().display().to_string();

    let status = std::process::Command::new("cmd")
        .args(["/C", &destructive_command(&target)])
        .status()
        .expect("run the command the process port would run");

    assert!(
        !canary_survives(&dir),
        "the command this test expects the denylist to refuse did not actually \
         delete anything (exit {status}). Every other assertion in this file \
         would then pass for the wrong reason."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shell_tool_is_withheld_when_the_kill_switch_is_off() {
    let dir = sacrificial_dir();
    let command = destructive_command(&dir.path().display().to_string());
    let server = RebornServer::start_scripted(
        responder(command),
        "unused".to_string(),
        // Exactly what `GatewayConfig::env` sends by default.
        vec![(
            ironclaw_host_runtime::SHELL_TOOL_ENABLED_ENV.to_string(),
            "false".to_string(),
        )],
    )
    .await;

    let thread = server.create_thread().await;
    server
        .send_message(&thread, &format!("{MARK} — clean that folder up"))
        .await;
    let (_, stream) = server
        .stream_until(&thread, "\"status\":\"completed\"", Duration::from_secs(90))
        .await;

    // The capability is not on the surface at all: the tool list the runtime
    // sends the provider must not mention it. This is the load-bearing
    // assertion — a model cannot call a tool it was never offered.
    let offered = server.chat_requests();
    assert!(
        !offered.is_empty(),
        "the mock provider was never called, so nothing was proven.\n\
         stream:\n{stream}\n--- stderr ---\n{}",
        server.stderr_snapshot()
    );
    assert!(
        offered
            .iter()
            .all(|request| !request.contains("builtin__shell")),
        "`builtin.shell` was offered to the model with the kill-switch off"
    );

    // And nothing ran, whatever the loop did with a tool call for a capability
    // that does not exist.
    assert!(
        canary_survives(&dir),
        "the shell executed with the kill-switch off.\nstream:\n{stream}\n\
         --- stderr ---\n{}",
        server.stderr_snapshot()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_denylisted_windows_command_is_refused_by_the_running_gateway() {
    let dir = sacrificial_dir();
    let command = destructive_command(&dir.path().display().to_string());
    let server = RebornServer::start_scripted(
        responder(command),
        "unused".to_string(),
        // Shell **on**, or this would only re-test the switch above.
        vec![(
            ironclaw_host_runtime::SHELL_TOOL_ENABLED_ENV.to_string(),
            "true".to_string(),
        )],
    )
    .await;

    let thread = server.create_thread().await;
    server
        .send_message(&thread, &format!("{MARK} — clean that folder up"))
        .await;
    let (_, stream) = server
        .stream_until(&thread, "\"status\":\"completed\"", Duration::from_secs(90))
        .await;

    // The tool was genuinely offered and genuinely called — otherwise the
    // survival assertion below proves nothing about the denylist.
    assert!(
        server
            .chat_requests()
            .iter()
            .any(|request| request.contains("builtin__shell")),
        "`builtin.shell` was not offered with the kill-switch on, so this test \
         did not exercise the denylist.\nstream:\n{stream}\n--- stderr ---\n{}",
        server.stderr_snapshot()
    );

    assert!(
        canary_survives(&dir),
        "a denylisted Windows command executed. The denylist does not bind on \
         the fork's only supported platform.\nstream:\n{stream}\n\
         --- stderr ---\n{}",
        server.stderr_snapshot()
    );
}
