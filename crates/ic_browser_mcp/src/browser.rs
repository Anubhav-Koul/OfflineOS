//! Driving a real browser over CDP with `chromiumoxide`.
//!
//! [`BrowserSession`] owns a launched Chrome/Edge, a single visible page (one
//! window the user can watch and take over for a CAPTCHA or login), and the
//! background handler task that pumps the CDP connection. It implements
//! [`ToolExecutor`], so the loopback [`crate::server`] serves the six tools by
//! calling straight into it.
//!
//! Every call is serialized and time-bounded: the browser is one window, so
//! interleaving a click with a navigation is nonsense, and a wedged page must
//! not hang the sidecar forever.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::{Page, ScreenshotParams};
use futures_util::StreamExt as _;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::classify;
use crate::consent::{self, Approver};
use crate::error::{Error, Result};
use crate::launcher::{self, BrowserExecutable};
use crate::protocol::{Tool, params};
use crate::server::ToolExecutor;

/// A page load has this long to settle before the tool reports a timeout.
const NAV_TIMEOUT: Duration = Duration::from_secs(30);
/// Any other single browser operation has this long.
const OP_TIMEOUT: Duration = Duration::from_secs(15);
/// JPEG quality for screenshots. See [`BrowserSession::screenshot`] for why this
/// is a JPEG at all — the host's 1 MiB result cap.
const SCREENSHOT_QUALITY: i64 = 70;

/// How to launch the browser.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// The dedicated user-data directory. Never the user's real profile.
    pub profile_dir: PathBuf,
    /// Run without a visible window. Off by default: the whole point of the
    /// desktop browser is that the user can watch and complete a CAPTCHA/login.
    pub headless: bool,
}

/// A human name for the field, for the consent prompt. Falls back to the selector
/// when the page gave us nothing better — "allow a fill into `#x`" is poor, but it
/// is honest, and it is still better than not asking.
fn field_name(signals: &classify::FieldSignals, selector: &str) -> String {
    let label = signals.label.trim();
    if label.is_empty() {
        selector.to_string()
    } else {
        label.to_string()
    }
}

/// A launched browser with one active page.
pub struct BrowserSession {
    /// Never read. Held for its `Drop`, which kills the browser process — the
    /// page alone would not. Dropping this field would leak a Chrome per run.
    #[allow(dead_code, reason = "held for its Drop; see the doc comment")]
    browser: Browser,
    page: Page,
    kind: &'static str,
    /// Serializes tool calls; the browser is a single window.
    lock: Mutex<()>,
    /// Pumps the CDP event stream; aborted on drop.
    handler: JoinHandle<()>,
    /// Who to ask before typing into a sensitive field. `DenyAll` when no channel
    /// was wired, so a standalone sidecar denies rather than proceeds.
    approver: Arc<dyn Approver>,
}

impl BrowserSession {
    /// Probe for a browser and launch it against a dedicated profile.
    ///
    /// Sensitive fills are **denied** — there is no approval channel. Use
    /// [`BrowserSession::launch_with_approver`] to wire one; the sidecar binary
    /// does. This default is the fail-closed one on purpose: a session with no way
    /// to ask must refuse, never proceed.
    pub async fn launch(options: LaunchOptions) -> Result<Self> {
        let executable = launcher::find_browser()?;
        Self::launch_with(executable, options, Arc::new(consent::DenyAll)).await
    }

    /// Launch and wire an [`Approver`] for sensitive fills.
    pub async fn launch_with_approver(
        options: LaunchOptions,
        approver: Arc<dyn Approver>,
    ) -> Result<Self> {
        let executable = launcher::find_browser()?;
        Self::launch_with(executable, options, approver).await
    }

    /// Launch a specific browser executable. Split out for tests and for callers
    /// that already resolved the binary.
    pub async fn launch_with(
        executable: BrowserExecutable,
        options: LaunchOptions,
        approver: Arc<dyn Approver>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&options.profile_dir).map_err(|source| {
            Error::io(
                format!(
                    "creating the browser profile directory {}",
                    options.profile_dir.display()
                ),
                source,
            )
        })?;

        let mut builder = BrowserConfig::builder()
            .chrome_executable(&executable.path)
            .user_data_dir(&options.profile_dir)
            // Isolate this instance from any Chrome/Edge the user is running:
            // a distinct profile dir already forks the session; these keep the
            // automation window from adopting the user's default-browser state.
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-background-networking");
        if !options.headless {
            builder = builder.with_head();
        }
        let config = builder
            .build()
            .map_err(|reason| Error::browser(format!("invalid browser config: {reason}")))?;

        let (browser, mut handler) = Browser::launch(config).await.map_err(|error| {
            Error::browser(format!("launching {}: {error}", executable.kind.label()))
        })?;

        // The handler must be driven or every CDP call stalls: it owns the
        // receiving end of the channel every `Page` method sends on.
        //
        // **Do not break out of this loop on an error.** An earlier version did,
        // and it made the browser unusable against current Chrome: Chrome emits
        // CDP events that chromiumoxide 0.7 cannot deserialize ("data did not
        // match any variant of untagged enum Message"), the loop exited on the
        // first one, the handler was dropped, and every subsequent tool call
        // failed with "send failed because receiver is gone" — a dead browser
        // roughly 60 ms after a launch that reported success.
        //
        // A single event we cannot parse is not a broken connection. Log it and
        // keep pumping; the loop still ends naturally when the stream closes,
        // which is what a genuinely dead browser looks like.
        let pump = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(error) = event {
                    tracing::debug!(%error, "ignoring an unparseable CDP event");
                }
            }
            tracing::info!("the CDP connection closed");
        });

        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|error| Error::browser(format!("opening the first page: {error}")))?;

        Ok(Self {
            browser,
            page,
            kind: executable.kind.label(),
            lock: Mutex::new(()),
            handler: pump,
            approver,
        })
    }

    /// The browser family that was launched, for diagnostics.
    pub fn browser_kind(&self) -> &'static str {
        self.kind
    }

    async fn navigate(&self, args: params::Navigate) -> Result<serde_json::Value> {
        with_timeout(NAV_TIMEOUT, "navigate", async {
            self.page
                .goto(&args.url)
                .await
                .map_err(|error| Error::browser(format!("navigating to {}: {error}", args.url)))?;
            // A page that never fires its load event should not wedge the tool;
            // a timeout here still leaves the page usable for the next call.
            let _ = self.page.wait_for_navigation().await;
            let url = self.page.url().await.ok().flatten().unwrap_or_default();
            let title = self
                .page
                .get_title()
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            Ok(serde_json::json!({ "url": url, "title": title }))
        })
        .await
    }

    async fn get_text(&self, args: params::GetText) -> Result<serde_json::Value> {
        with_timeout(OP_TIMEOUT, "get_text", async {
            let text = match &args.selector {
                Some(selector) => self
                    .find_one(selector)
                    .await?
                    .inner_text()
                    .await
                    .map_err(|error| Error::browser(format!("reading element text: {error}")))?
                    .unwrap_or_default(),
                None => self
                    .page
                    .evaluate("document.body ? document.body.innerText : ''")
                    .await
                    .map_err(|error| Error::browser(format!("reading page text: {error}")))?
                    .into_value()
                    .unwrap_or_default(),
            };
            Ok(serde_json::json!({ "text": text }))
        })
        .await
    }

    async fn find(&self, args: params::Find) -> Result<serde_json::Value> {
        with_timeout(OP_TIMEOUT, "find", async {
            // `find` is a query, not an action: matching nothing is a valid
            // answer (`found: false`), not a `NoElement` error.
            let elements = self
                .page
                .find_elements(&args.selector)
                .await
                .unwrap_or_default();
            let first_text = match elements.first() {
                Some(element) => element.inner_text().await.ok().flatten(),
                None => None,
            };
            Ok(serde_json::json!({
                "found": !elements.is_empty(),
                "count": elements.len(),
                "first_text": first_text,
            }))
        })
        .await
    }

    /// Type into a field — **after** a human has approved it, if it is anything but
    /// a positively-ordinary text box.
    ///
    /// This is the enforcement point for the consent gate (see [`crate::consent`]).
    /// It lives here, on the far side of the MCP boundary, because the model cannot
    /// route around it: the sidecar decides, not the prompt. The runtime's own
    /// approval flow is a no-op (`default_permission: Ask` is never read), so this
    /// is the only thing standing between the agent and the user's password field.
    ///
    /// Note the ordering: classify → ask → *then* focus and type. Nothing touches
    /// the field before consent, so a denied fill leaves the page untouched.
    async fn fill(&self, args: params::Fill) -> Result<serde_json::Value> {
        // Deliberately outside `with_timeout`: the human has
        // `consent::APPROVAL_TIMEOUT` to answer, which is far longer than a browser
        // op is allowed to take. Bounding consent by OP_TIMEOUT would auto-deny
        // every prompt after 15 seconds.
        let signals = self.probe_field(&args.selector).await;
        let sensitivity = classify::classify(&signals);

        if sensitivity.needs_approval() {
            let request = consent::FillApproval {
                id: 0, // assigned by the approver
                url: signals.url.clone(),
                secure: signals.secure,
                field: field_name(&signals, &args.selector),
                selector: args.selector.clone(),
                value: args.value.clone(),
                reason: classify::reason_for(&signals, sensitivity),
            };
            if !self.approver.approve(request).await {
                // A refusal is the user's answer, not a fault. Give it back as a
                // recoverable tool error so the agent reports it and moves on
                // instead of failing the whole run.
                return Err(Error::NotApproved {
                    field: field_name(&signals, &args.selector),
                });
            }
        }

        with_timeout(OP_TIMEOUT, "fill", async {
            let element = self.find_one(&args.selector).await?;
            element
                .click()
                .await
                .map_err(|error| Error::browser(format!("focusing {}: {error}", args.selector)))?;
            element.type_str(&args.value).await.map_err(|error| {
                Error::browser(format!("typing into {}: {error}", args.selector))
            })?;
            Ok(serde_json::json!({ "filled": true, "selector": args.selector }))
        })
        .await
    }

    async fn click(&self, args: params::Click) -> Result<serde_json::Value> {
        with_timeout(OP_TIMEOUT, "click", async {
            self.find_one(&args.selector)
                .await?
                .click()
                .await
                .map_err(|error| Error::browser(format!("clicking {}: {error}", args.selector)))?;
            Ok(serde_json::json!({ "clicked": true, "selector": args.selector }))
        })
        .await
    }

    /// Capture the viewport as an MCP image block.
    ///
    /// **JPEG, not PNG, and viewport-only, on purpose.** The host caps an MCP
    /// tool result at `max_output_bytes` (1 MiB by default) and base64 inflates
    /// by a third, so a full-page PNG of a real site blows the budget and the
    /// call fails with an opaque `response_error`. A quality-70 JPEG of the
    /// viewport is comfortably inside it and is plenty for the vision-fallback
    /// case this exists for (CLAUDE.md Phase 4.4: a selector keeps failing, so
    /// look at the page).
    async fn screenshot(&self) -> Result<serde_json::Value> {
        use base64::Engine as _;
        with_timeout(OP_TIMEOUT, "screenshot", async {
            let jpeg = self
                .page
                .screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Jpeg)
                        .quality(SCREENSHOT_QUALITY)
                        .full_page(false)
                        .build(),
                )
                .await
                .map_err(|error| Error::browser(format!("capturing a screenshot: {error}")))?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&jpeg);
            tracing::debug!(bytes = jpeg.len(), "captured a screenshot");
            Ok(crate::protocol::tool_image_result(encoded, "image/jpeg"))
        })
        .await
    }

    /// Ask the page about a fill target.
    ///
    /// A probe that throws, times out, or returns something unexpected yields
    /// **default** signals — and default signals classify as `Unknown`, which
    /// means "ask". So every failure here fails closed; there is no path where a
    /// broken probe lets a fill through unprompted.
    async fn probe_field(&self, selector: &str) -> classify::FieldSignals {
        let script = classify::probe_for(selector);
        let probed = tokio::time::timeout(OP_TIMEOUT, self.page.evaluate(script)).await;

        let signals = match probed {
            Ok(Ok(value)) => value.into_value::<classify::FieldSignals>().ok(),
            Ok(Err(error)) => {
                tracing::warn!(%error, "could not inspect the fill target; treating it as sensitive");
                None
            }
            Err(_) => {
                tracing::warn!("inspecting the fill target timed out; treating it as sensitive");
                None
            }
        };
        signals.unwrap_or_default()
    }

    /// Find exactly one element, mapping "no such node" to [`Error::NoElement`]
    /// so the caller (and the agent) sees a clear "selector matched nothing"
    /// rather than a raw CDP error.
    async fn find_one(&self, selector: &str) -> Result<chromiumoxide::element::Element> {
        self.page
            .find_element(selector)
            .await
            .map_err(|_| Error::NoElement {
                selector: selector.to_string(),
            })
    }
}

#[async_trait]
impl ToolExecutor for BrowserSession {
    async fn call(&self, tool: Tool, arguments: serde_json::Value) -> Result<serde_json::Value> {
        // One window, one call at a time.
        let _guard = self.lock.lock().await;
        // A screenshot is already an MCP image block; the rest are JSON results.
        // `tool_result_from` maps a missing selector to a recoverable `isError`
        // result and leaves real faults as errors.
        if tool == Tool::BrowserScreenshot {
            return self.screenshot().await;
        }
        let outcome = match tool {
            Tool::BrowserNavigate => self.navigate(parse(arguments)?).await,
            Tool::BrowserGetText => self.get_text(parse(arguments)?).await,
            Tool::BrowserFind => self.find(parse(arguments)?).await,
            Tool::BrowserFill => self.fill(parse(arguments)?).await,
            Tool::BrowserClick => self.click(parse(arguments)?).await,
            Tool::BrowserScreenshot => unreachable!("handled above"),
        };
        crate::server::tool_result_from(outcome)
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        // Stop pumping CDP events. We deliberately do NOT try to close the
        // browser here: `Browser::close` and `Browser::kill` are both `async`,
        // and a `Drop` cannot await — calling one and discarding the future
        // (which an earlier version did) polls it zero times and closes nothing,
        // while reading like a graceful shutdown.
        //
        // The browser process is torn down for real by two things that do work:
        // `chromiumoxide::Browser` kills its child process on drop, and the child
        // is enlisted in the widget's Windows Job Object, so even a hard kill of
        // the widget (`TerminateProcess`, no unwinding, no `Drop`) takes the
        // automation browser with it.
        self.handler.abort();
    }
}

/// Deserialize per-tool params, turning a shape mismatch into a protocol error.
fn parse<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T> {
    serde_json::from_value(value)
        .map_err(|error| Error::Protocol(format!("invalid tool parameters: {error}")))
}

/// Bound a browser operation so a wedged page cannot hang the sidecar.
async fn with_timeout<F>(limit: Duration, what: &str, future: F) -> Result<serde_json::Value>
where
    F: std::future::Future<Output = Result<serde_json::Value>>,
{
    match tokio::time::timeout(limit, future).await {
        Ok(result) => result,
        Err(_) => Err(Error::browser(format!("{what} timed out after {limit:?}"))),
    }
}

/// Build a session behind an `Arc` for the server.
pub async fn shared(options: LaunchOptions) -> Result<Arc<BrowserSession>> {
    Ok(Arc::new(BrowserSession::launch(options).await?))
}
