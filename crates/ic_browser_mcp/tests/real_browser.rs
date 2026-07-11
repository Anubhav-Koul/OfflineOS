//! Round-trip against a **real** Chrome/Edge.
//!
//! `#[ignore]`d, because it needs a browser installed and opens a window: CI runs
//! `cargo test` without it, and a developer runs
//! `cargo test -p ic_browser_mcp -- --ignored` when touching the CDP layer.
//!
//! This file exists because of a bug that **every** fake-executor test passed
//! straight through. The unit tests fill the `ToolExecutor` seam with a fake, so
//! they exercise the MCP transport and never touch CDP — and the CDP layer was
//! broken in a way that only current Chrome reveals:
//!
//! > Chrome emits CDP events that chromiumoxide 0.7 cannot deserialize ("data did
//! > not match any variant of untagged enum Message"). The event pump treated the
//! > first such event as fatal and exited, dropping the handler. Launch reported
//! > success; ~60 ms later every tool call failed with "send failed because
//! > receiver is gone".
//!
//! A green unit suite and a browser that is dead on arrival. The pump now logs an
//! unparseable event and keeps going, and [`the_browser_survives_unparseable_cdp_events`]
//! is what keeps it that way.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ic_browser_mcp::browser::LaunchOptions;
use ic_browser_mcp::consent::{Approver, FillApproval};
use ic_browser_mcp::server::ToolExecutor as _;
use ic_browser_mcp::{BrowserSession, Tool};
use serde_json::json;

/// An approver that answers a fixed verdict and records what it was asked. Lets a
/// live-page test assert *both* that the gate fired for the right field and what
/// the user would have seen — the caller-level coverage `classify` alone can't give.
struct RecordingApprover {
    verdict: bool,
    calls: AtomicUsize,
    last: std::sync::Mutex<Option<FillApproval>>,
}

impl RecordingApprover {
    fn new(verdict: bool) -> Arc<Self> {
        Arc::new(Self {
            verdict,
            calls: AtomicUsize::new(0),
            last: std::sync::Mutex::new(None),
        })
    }
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn last(&self) -> Option<FillApproval> {
        self.last.lock().unwrap().clone()
    }
}

#[async_trait]
impl Approver for RecordingApprover {
    async fn approve(&self, request: FillApproval) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some(request);
        self.verdict
    }
}

/// A page fixture: writes HTML to a temp file and returns its `file://` URL.
fn page(html: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("page.html");
    std::fs::write(&path, html).expect("write html");
    let url = format!("file:///{}", path.display().to_string().replace('\\', "/"));
    (dir, url)
}

/// Launch against a throwaway profile. Headless: this is a test, nobody is
/// watching it, and a window would steal focus.
async fn session() -> Option<(BrowserSession, tempfile::TempDir)> {
    session_with(Arc::new(ic_browser_mcp::DenyAll)).await
}

async fn session_with(approver: Arc<dyn Approver>) -> Option<(BrowserSession, tempfile::TempDir)> {
    let profile = tempfile::tempdir().expect("a temp profile dir");
    let options = LaunchOptions {
        profile_dir: profile.path().to_path_buf(),
        headless: true,
    };
    match BrowserSession::launch_with_approver(options, approver).await {
        Ok(session) => Some((session, profile)),
        Err(error) => {
            // No Chrome/Edge on this machine: skip rather than fail. The point of
            // the `--ignored` gate is that whoever runs it *has* a browser.
            eprintln!("skipping: no usable browser ({error})");
            None
        }
    }
}

/// The regression. If the CDP pump ever again dies on the first event it cannot
/// parse, this fails: navigation reports "receiver is gone".
#[tokio::test]
#[ignore = "needs a real browser; run with --ignored"]
async fn the_browser_survives_unparseable_cdp_events() {
    let Some((session, _profile)) = session().await else {
        return;
    };

    let result = session
        .call(
            Tool::BrowserNavigate,
            json!({ "url": "https://example.com" }),
        )
        .await
        .expect("navigation must succeed against a live browser");

    assert_eq!(
        result["isError"], false,
        "a live browser must not report a tool error: {result}"
    );
    assert_eq!(
        result["structuredContent"]["title"], "Example Domain",
        "the page must actually have loaded: {result}"
    );

    // The real assertion: a SECOND call still works. The old bug let the first
    // call succeed and killed the connection underneath it, so a one-shot test
    // would have passed.
    let text = session
        .call(Tool::BrowserGetText, json!({}))
        .await
        .expect("the CDP connection must survive the first call");
    let body = text["structuredContent"]["text"]
        .as_str()
        .expect("page text");
    assert!(
        body.contains("Example Domain"),
        "expected the page's text, got {body:?}"
    );
}

/// A selector that matches nothing must come back as a *recoverable* tool error,
/// so the model can look at the page and try again — not as a hard failure that
/// kills the run.
#[tokio::test]
#[ignore = "needs a real browser; run with --ignored"]
async fn a_missing_selector_is_recoverable_against_a_live_page() {
    let Some((session, _profile)) = session().await else {
        return;
    };
    session
        .call(
            Tool::BrowserNavigate,
            json!({ "url": "https://example.com" }),
        )
        .await
        .expect("navigate");

    let result = session
        .call(Tool::BrowserClick, json!({ "selector": "#nope" }))
        .await
        .expect("a missing selector is not a transport failure");

    assert_eq!(result["isError"], true, "{result}");

    // ...and the session is still usable afterwards.
    let after = session
        .call(Tool::BrowserFind, json!({ "selector": "h1" }))
        .await
        .expect("the session must survive a missing selector");
    assert_eq!(after["structuredContent"]["found"], true, "{after}");
}

/// The screenshot must fit the host's 1 MiB MCP result cap — which is why it is a
/// viewport JPEG and not a full-page PNG.
#[tokio::test]
#[ignore = "needs a real browser; run with --ignored"]
async fn a_screenshot_fits_the_hosts_result_cap() {
    const HOST_CAP: usize = 1024 * 1024;

    let Some((session, _profile)) = session().await else {
        return;
    };
    session
        .call(
            Tool::BrowserNavigate,
            json!({ "url": "https://example.com" }),
        )
        .await
        .expect("navigate");

    let result = session
        .call(Tool::BrowserScreenshot, json!({}))
        .await
        .expect("screenshot");

    let content = &result["content"][0];
    assert_eq!(content["type"], "image");
    assert_eq!(content["mimeType"], "image/jpeg");

    let encoded = content["data"].as_str().expect("base64 image data");
    assert!(
        encoded.len() < HOST_CAP,
        "a {} byte screenshot would blow the host's {HOST_CAP} byte result cap \
         and the call would fail with an opaque response_error",
        encoded.len()
    );
}

// ── the consent gate, driven through fill() against a live page ──────────────
//
// These go through the caller (`fill`), not just `classify`, per
// .claude/rules/testing.md: `classify` gates a side effect, and the only way to
// know the gate actually fires — and blocks the keystrokes on a "no" — is to drive
// the real fill against a real DOM.

/// A password field must prompt, and a denial must stop the keystrokes.
#[tokio::test]
#[ignore = "needs a real browser; run with --ignored"]
async fn a_password_fill_is_gated_and_a_denial_types_nothing() {
    let approver = RecordingApprover::new(false); // the user says no
    let Some((session, _profile)) = session_with(approver.clone()).await else {
        return;
    };
    let (_page, url) = page(
        r#"<!doctype html><input id="pw" type="password">
           <script>window.typed = () => document.getElementById('pw').value</script>"#,
    );
    session
        .call(Tool::BrowserNavigate, json!({ "url": url }))
        .await
        .expect("navigate");

    let result = session
        .call(
            Tool::BrowserFill,
            json!({ "selector": "#pw", "value": "hunter2" }),
        )
        .await
        .expect("a denied fill is a recoverable result, not a transport error");

    // The gate fired, for this field, showing the real value to the user.
    assert_eq!(approver.count(), 1, "the password fill must have prompted");
    let asked = approver.last().expect("a recorded request");
    assert_eq!(asked.selector, "#pw");
    assert_eq!(asked.value, "hunter2");

    // The denial surfaced as a recoverable isError...
    assert_eq!(result["isError"], true, "{result}");

    // ...and — the point of the whole exercise — NOTHING was typed.
    let typed = session
        .call(Tool::BrowserGetText, json!({ "selector": "body" }))
        .await
        .expect("read back");
    let _ = typed; // the field's value isn't in body text; check it directly
    let value = session
        .call(Tool::BrowserFind, json!({ "selector": "#pw" }))
        .await
        .expect("find");
    assert_eq!(value["structuredContent"]["found"], true);
    // The field is still empty: the deny happened before any keystroke.
    let readback = session
        .call(Tool::BrowserGetText, json!({ "selector": "#pw" }))
        .await
        .expect("get_text on the input");
    assert_eq!(
        readback["structuredContent"]["text"], "",
        "a denied fill must leave the field untouched: {readback}"
    );
}

/// An ordinary search box must NOT prompt — or the gate is noise and users learn
/// to click through it.
#[tokio::test]
#[ignore = "needs a real browser; run with --ignored"]
async fn an_ordinary_field_is_not_gated() {
    let approver = RecordingApprover::new(true);
    let Some((session, _profile)) = session_with(approver.clone()).await else {
        return;
    };
    let (_page, url) =
        page(r#"<!doctype html><input id="q" type="search" name="q" aria-label="Search">"#);
    session
        .call(Tool::BrowserNavigate, json!({ "url": url }))
        .await
        .expect("navigate");

    let result = session
        .call(
            Tool::BrowserFill,
            json!({ "selector": "#q", "value": "rust async" }),
        )
        .await
        .expect("fill");

    assert_eq!(
        approver.count(),
        0,
        "an ordinary search box must not prompt"
    );
    assert_eq!(result["isError"], false, "{result}");
}

/// The user-called-out case: a plain text field inside a login form. We can't tell
/// it from a JS-masked password, so it must prompt.
#[tokio::test]
#[ignore = "needs a real browser; run with --ignored"]
async fn a_text_field_in_a_login_form_is_gated_against_a_live_dom() {
    let approver = RecordingApprover::new(true); // approve, so the fill completes
    let Some((session, _profile)) = session_with(approver.clone()).await else {
        return;
    };
    let (_page, url) = page(
        r#"<!doctype html><form>
             <input id="user" type="text" name="username">
             <input id="pw" type="password" name="password">
           </form>"#,
    );
    session
        .call(Tool::BrowserNavigate, json!({ "url": url }))
        .await
        .expect("navigate");

    // Filling the *text* username field must still prompt, because the form holds a
    // password — the exact ambiguity classify() can't resolve.
    let result = session
        .call(
            Tool::BrowserFill,
            json!({ "selector": "#user", "value": "alice" }),
        )
        .await
        .expect("fill");

    assert_eq!(
        approver.count(),
        1,
        "a text field in a form with a password must prompt"
    );
    assert_eq!(
        result["isError"], false,
        "approved, so it should have typed"
    );
}
