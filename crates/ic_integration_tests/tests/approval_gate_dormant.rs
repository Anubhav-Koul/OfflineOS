//! Phase 8d canary: under `local-dev`, the runtime's tool-approval gate has **no
//! producer** — consent-sensitive tools run unprompted.
//!
//! The wire protocol has a generic tool-approval mechanism (the `gate` SSE event
//! and `resolve_gate`), distinct from the auth/credential gate (`auth_required`).
//! But nothing drives it under `local-dev serve`: the wired `GrantAuthorizer`
//! returns only `Decision::Allow`/`Deny` (never `RequireApproval`), no hook
//! dispatcher is installed, and budget-approval fails a run rather than gating it.
//! So `builtin.apply_patch` — a consent-sensitive capability that edits an
//! existing file — dispatches and the run completes without ever blocking on
//! approval. `apply_patch` is the one such capability not otherwise driven by a
//! gate (`skill_install` is pinned by `skill_install.rs`, `trigger_create` by
//! `ambient_surfacing.rs`); this drives it live, per the Phase 4 lesson that a
//! source trace is not a running gateway.
//!
//! **This test is the tripwire for Phase 8d's deferred half.** The day upstream
//! wires `RequireApproval` (or installs a hook gate) for a capability like this,
//! the run will park at `blocked_approval` instead of completing and a `gate`
//! event will appear on the stream — both assertions flip. That is the signal
//! that the runtime finally emits universal tool-approval gates, and the moment
//! to render them as the red consent card in the bubble (which we deliberately do
//! NOT build now, over a mechanism that never fires). See
//! `docs/desktop/approval-gates.md`.
#![cfg(feature = "webui-v2-beta")]

use std::sync::Arc;
use std::time::Duration;

use ic_integration_tests::{MockReply, RebornServer};

const MARK: &str = "GATE-CANARY";
const DONE: &str = "gate-canary-done";

/// `apply_patch` → end. The gate (if it fired) would block at **authorization**,
/// before the handler runs, so the patch does not need to succeed to prove the
/// point — it targets a nonexistent path and errors *after* dispatch, which keeps
/// the test from writing anything into the repo (serve's cwd during a test).
/// Sequenced by whether a tool result has come back yet.
fn responder() -> ic_integration_tests::MockResponder {
    Arc::new(|body: &str| {
        if body.contains("\"role\":\"tool\"") {
            return MockReply::Text(DONE.to_string());
        }
        if body.contains(MARK) {
            return MockReply::ToolCall {
                name: "builtin__apply_patch".to_string(),
                arguments: serde_json::json!({
                    "path": "no-such-canary-file.txt",
                    "old_string": "BEFORE",
                    "new_string": "AFTER",
                }),
            };
        }
        MockReply::Text("fallthrough".to_string())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_consent_sensitive_tool_runs_without_an_approval_gate() {
    let server = RebornServer::start_scripted(responder(), "unused".to_string(), Vec::new()).await;
    let thread = server.create_thread().await;
    server
        .send_message(&thread, &format!("{MARK} — edit a file"))
        .await;

    // Watch the live projection stream to a terminal run_status, capturing every
    // frame. A tool-approval gate would park the run at `blocked_approval`, so it
    // would never reach `completed` and this would time out.
    let (completed, stream) = server
        .stream_until(&thread, "\"status\":\"completed\"", Duration::from_secs(90))
        .await;
    assert!(
        completed,
        "the run should reach `completed` unblocked — a tool-approval gate would \
         park it at blocked_approval.\nstream:\n{stream}\n--- stderr ---\n{}",
        server.stderr_snapshot()
    );

    // The definitive pins: no `gate` (tool-approval) event on the wire, and the
    // run never entered the blocked-approval state. Either firing means the
    // dormant mechanism went live — build the universal consent card then.
    assert!(
        !stream.contains("event: gate") && !stream.contains("\"type\":\"gate\""),
        "a `gate` (tool-approval) event fired — upstream may have wired \
         RequireApproval. Build the universal consent card (8d).\nstream:\n{stream}"
    );
    assert!(
        !stream.contains("blocked_approval"),
        "the run blocked on approval — the gate mechanism is live now.\nstream:\n{stream}"
    );

    // And the consent-sensitive tool truly dispatched unprompted: `apply_patch`
    // was invoked against the real runtime (not just granted in a config), and the
    // turn finished with the mock's end text.
    assert!(
        server
            .chat_requests()
            .iter()
            .any(|request| request.contains("apply_patch")),
        "apply_patch should have been dispatched to the real runtime"
    );
    let (done, timeline) = server
        .wait_for_timeline_text(&thread, DONE, Duration::from_secs(30))
        .await;
    assert!(
        done,
        "the turn should finish after the tool ran unprompted: {timeline}\n\
         --- stderr ---\n{}",
        server.stderr_snapshot()
    );
}
