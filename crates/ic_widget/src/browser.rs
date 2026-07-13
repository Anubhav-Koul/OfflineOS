//! Supervising the browser MCP sidecar, and registering it with the gateway.
//!
//! The sidecar (`ic-browser-mcp`) is a child process that launches a dedicated
//! Chrome/Edge and serves the six browser tools as an MCP server on loopback.
//! Reborn reaches it through its hosted-MCP lane (see CP-4 in
//! `docs/desktop/core-patches.md`).
//!
//! ## The launch order is not arbitrary
//!
//! Three properties of the runtime force the exact sequence in [`start`] and
//! [`register`]. Get any of them wrong and you get an extension that installs,
//! reports `activated: true`, and has no tools:
//!
//! 1. **The extension catalogue is scanned once, at gateway boot.** So the
//!    manifest must be written *before* `ironclaw-reborn serve` starts. This is
//!    why the sidecar comes up first: its port goes into the manifest's `url`,
//!    and the gateway pins its egress allowlist to that exact `host:port`.
//! 2. **Tool discovery runs at *activation*, not at boot.** The gateway calls our
//!    `tools/list` when the extension is activated, so the sidecar must be
//!    listening then — and activation has to be driven against the *running*
//!    gateway.
//! 3. **A restart does not re-discover.** The gateway republishes the bundled
//!    manifest, which carries only a capability *template*. So [`register`] runs
//!    on **every** launch, not just the first.
//!
//! And because a discovery failure makes the gateway *silently* fall back to that
//! template while still reporting success, [`register`] verifies the capability
//! count instead of trusting the activation response.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use ic_browser_mcp::{ALL_TOOLS, EXTENSION_ID};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

use crate::gateway_client::GatewayClient;
use crate::job_object::ProcessJob;

/// The handshake line the sidecar prints once it is listening. The browser
/// itself starts on the first tool call, not here — see `LazyBrowser` in
/// `ic_browser_mcp` for why.
const LISTENING_PREFIX: &str = "IC_BROWSER_MCP_LISTENING ";
/// The stdout line carrying a sensitive-fill approval request from the sidecar.
const APPROVAL_PREFIX: &str = "IC_BROWSER_MCP_APPROVAL ";
/// The stdin line carrying our decision back.
const DECISION_PREFIX: &str = "IC_BROWSER_MCP_DECISION ";

/// How long to wait for that line. It now only covers binding a loopback port
/// (the browser launch moved to the first tool call), but the budget is left
/// generous: a cold sidecar behind an on-access AV scan is not instant.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(45);

/// A running browser sidecar.
///
/// Held in app state for the process lifetime. The child rides in the widget's
/// Windows Job Object, so a hard kill of the widget takes the sidecar *and* its
/// Chrome down with it — an orphaned automation browser would otherwise sit there
/// holding a profile lock and a port.
pub struct BrowserSidecar {
    /// Kept so the child is killed on a graceful exit too.
    child: Child,
    url: String,
    port: u16,
    /// The sidecar's stdin, for answering approval requests. Behind a mutex
    /// because a decision may be written from any command handler.
    stdin: Arc<Mutex<ChildStdin>>,
}

impl BrowserSidecar {
    /// The MCP URL the manifest points at.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The loopback port it is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Answer a sensitive-fill approval request. `id` is the request id from the
    /// `browser://approval` event; `approved` is the user's verdict.
    ///
    /// A write failure is not fatal here: the sidecar denies on its own timeout if
    /// the answer never arrives, so a dropped decision degrades to a denial rather
    /// than a hang.
    pub async fn answer_fill(&self, id: u64, approved: bool) -> Result<(), String> {
        let line = format!(
            "{DECISION_PREFIX}{}\n",
            serde_json::json!({ "id": id, "approved": approved })
        );
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| format!("could not send the approval decision: {error}"))?;
        stdin
            .flush()
            .await
            .map_err(|error| format!("could not flush the approval decision: {error}"))
    }
}

impl Drop for BrowserSidecar {
    fn drop(&mut self) {
        // Best-effort: the Job Object is the guarantee, this is the courtesy.
        let _ = self.child.start_kill();
    }
}

/// Where the sidecar keeps its dedicated browser profile.
///
/// **Never the user's real profile.** A fresh user-data dir means the automation
/// browser starts with none of their cookies, history, or logged-in sessions —
/// the agent only ever has access to what the user logs into inside the
/// automation window themselves.
fn profile_dir() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|base| base.join("IronClaw Desktop").join("browser-profile"))
        .ok_or_else(|| "could not locate the local application data directory".to_string())
}

/// The directory the gateway scans for extension manifests.
///
/// This is a real directory on the host disk (the local-dev root filesystem
/// mounts `/system/extensions` to it), *not* a path inside the libSQL database —
/// which is why the widget can simply write into it.
pub fn extensions_root(reborn_home: &Path) -> PathBuf {
    reborn_home
        .join("local-dev")
        .join("system")
        .join("extensions")
}

/// Locate the sidecar binary: an explicit override, else beside the widget.
fn sidecar_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("IC_BROWSER_MCP_BIN") {
        return PathBuf::from(path);
    }
    let name = if cfg!(windows) {
        "ic-browser-mcp.exe"
    } else {
        "ic-browser-mcp"
    };
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && dir.join(name).exists()
    {
        return dir.join(name);
    }
    PathBuf::from(name)
}

/// Start the sidecar and write the extension manifest for the port it landed on.
///
/// Must be called **before** the gateway starts: the manifest has to be on disk
/// when the gateway scans its catalogue, and the catalogue is never re-scanned.
///
/// Best-effort by design. If there is no Chrome or Edge on the machine, or the
/// sidecar binary is missing, this returns `None` and the app runs without browser
/// tools — the same way it runs without local inference when no model is
/// installed. It never blocks the app from starting.
/// A sink for sensitive-fill approval requests. The widget's `main.rs` passes one
/// that emits a `browser://approval` Tauri event; keeping it a plain callback keeps
/// this module Tauri-free and unit-testable.
pub type ApprovalSink = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

/// Spawn the sidecar, register its tools' manifest, and start pumping approval
/// requests to `on_approval`.
///
/// Must be called **before** the gateway starts: the manifest has to be on disk
/// when the gateway scans its catalogue, and the catalogue is never re-scanned.
///
/// Best-effort by design. If there is no Chrome or Edge on the machine, or the
/// sidecar binary is missing, this returns `None` and the app runs without browser
/// tools — the same way it runs without local inference when no model is
/// installed. It never blocks the app from starting.
pub async fn start(
    job: Arc<ProcessJob>,
    reborn_home: &Path,
    on_approval: ApprovalSink,
) -> Option<BrowserSidecar> {
    match try_start(job, reborn_home, on_approval).await {
        Ok(sidecar) => {
            tracing::info!(url = sidecar.url(), "the browser sidecar is ready");
            Some(sidecar)
        }
        Err(error) => {
            tracing::warn!(%error, "the browser sidecar did not start; running without browser tools");
            None
        }
    }
}

async fn try_start(
    job: Arc<ProcessJob>,
    reborn_home: &Path,
    on_approval: ApprovalSink,
) -> Result<BrowserSidecar, String> {
    let profile = profile_dir()?;
    let binary = sidecar_binary();

    let mut child = Command::new(&binary)
        .env("IC_BROWSER_MCP_PROFILE", &profile)
        // Port 0: let the OS pick. There is no need to pin it across runs, because
        // the manifest is rewritten with the live port on every launch, before the
        // gateway boots and reads it.
        .env("IC_BROWSER_MCP_PORT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("could not spawn {}: {error}", binary.display()))?;

    // Enlist before we wait on it: a sidecar that hangs during startup must still
    // die with the widget.
    job.assign(&child)
        .map_err(|error| format!("could not enlist the browser sidecar in the job: {error}"))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "the browser sidecar has no stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "the browser sidecar has no stdout".to_string())?;

    let mut lines = BufReader::new(stdout).lines();
    let url = match tokio::time::timeout(STARTUP_TIMEOUT, read_listening_url(&mut lines)).await {
        Ok(Ok(url)) => url,
        Ok(Err(error)) => {
            let _ = child.start_kill();
            return Err(error);
        }
        Err(_) => {
            let _ = child.start_kill();
            return Err(format!(
                "the browser sidecar did not report a listening URL within {STARTUP_TIMEOUT:?}"
            ));
        }
    };

    // Keep reading stdout after the handshake, forwarding approval requests. The
    // reader owns the rest of the stream; it ends when the sidecar exits.
    tokio::spawn(pump_approvals(lines, on_approval));

    let port = url
        .rsplit(':')
        .next()
        .and_then(|tail| tail.split('/').next())
        .and_then(|port| port.parse::<u16>().ok())
        .ok_or_else(|| format!("could not read a port out of the sidecar URL {url:?}"))?;

    // The manifest must exist before the gateway boots — this is the whole reason
    // the sidecar starts first.
    let root = extensions_root(reborn_home);
    ic_browser_mcp::install_package(&root, port)
        .map_err(|error| format!("could not write the browser extension manifest: {error}"))?;

    Ok(BrowserSidecar {
        child,
        url,
        port,
        stdin: Arc::new(Mutex::new(stdin)),
    })
}

/// Read stdout until the sidecar announces its URL, leaving the reader positioned
/// for [`pump_approvals`] to continue from.
async fn read_listening_url<R>(lines: &mut tokio::io::Lines<BufReader<R>>) -> Result<String, String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|error| format!("could not read the browser sidecar's output: {error}"))?
    {
        if let Some(url) = line.strip_prefix(LISTENING_PREFIX) {
            return Ok(url.trim().to_string());
        }
    }
    Err("the browser sidecar exited before it began listening".to_string())
}

/// Forward each `IC_BROWSER_MCP_APPROVAL` line to the sink as a parsed JSON value.
///
/// A line we can't parse is dropped with a warning, never surfaced as an approval:
/// a malformed request must not become a prompt the user might approve. Non-approval
/// lines (the sidecar's own logging, if any leaks to stdout) are ignored.
async fn pump_approvals<R>(mut lines: tokio::io::Lines<BufReader<R>>, on_approval: ApprovalSink)
where
    R: tokio::io::AsyncRead + Unpin,
{
    while let Ok(Some(line)) = lines.next_line().await {
        let Some(payload) = line.strip_prefix(APPROVAL_PREFIX) else {
            continue;
        };
        match serde_json::from_str::<serde_json::Value>(payload.trim()) {
            Ok(request) => on_approval(request),
            Err(error) => {
                tracing::warn!(%error, "ignoring a malformed browser approval request");
            }
        }
    }
    tracing::info!("the browser sidecar's approval channel ended");
}

/// Install and activate the browser extension against a **running** gateway.
///
/// Activation is when the gateway calls the sidecar's `tools/list`, so this must
/// run while the sidecar is up — and it must run on *every* launch, because a
/// gateway restart republishes only the bundled capability template.
///
/// Verifies the outcome rather than trusting it: a discovery failure makes the
/// gateway fall back to that template *silently*, still reporting success. If the
/// six tools are not there afterwards, that is logged as a warning — the agent
/// still runs, it just has no browser.
pub async fn register(client: &GatewayClient) {
    if let Err(error) = client.install_extension(EXTENSION_ID).await {
        // An already-installed extension is the normal case on the second launch.
        tracing::debug!(%error, "installing the browser extension was not needed or failed");
    }

    match client.activate_extension(EXTENSION_ID).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!("the gateway did not activate the browser extension");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "could not activate the browser extension; no browser tools this session");
            return;
        }
    }

    // The honest check. `activated: true` above is not evidence that the agent
    // actually got the tools.
    //
    // Note this no longer depends on hosted-MCP *discovery*, which cannot succeed
    // in Reborn (the egress has no staged network policy at activation — see
    // `ic_browser_mcp::manifest`). The six capabilities come from the bundled
    // manifest itself, so a short count here means the manifest was not scanned:
    // it was written after the gateway booted, or it failed to parse.
    match client.extension_capabilities(EXTENSION_ID).await {
        Ok(capabilities) if capabilities.len() >= ALL_TOOLS.len() => {
            tracing::info!(
                count = capabilities.len(),
                "the browser tools are live on the agent"
            );
        }
        Ok(capabilities) => {
            tracing::warn!(
                found = capabilities.len(),
                expected = ALL_TOOLS.len(),
                "the browser extension activated with fewer capabilities than it declares, \
                 so the agent's browser is incomplete. The manifest was probably written \
                 after the gateway booted (the catalogue is only scanned once) or failed to parse."
            );
        }
        Err(error) => {
            tracing::warn!(%error, "could not confirm the browser tools reached the agent");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extensions_root_is_the_directory_the_gateway_scans() {
        let root = extensions_root(Path::new("C:\\data\\reborn"));
        assert!(root.ends_with(Path::new("local-dev/system/extensions")));
    }

    #[test]
    fn a_listening_line_yields_the_url_and_a_parseable_port() {
        // Locks the handshake the sidecar prints. If the two ever drift, the
        // widget silently runs with no browser tools.
        let line = format!("{LISTENING_PREFIX}http://127.0.0.1:51234/mcp");
        let url = line
            .strip_prefix(LISTENING_PREFIX)
            .expect("the prefix must match")
            .to_string();
        assert_eq!(url, "http://127.0.0.1:51234/mcp");

        let port = url
            .rsplit(':')
            .next()
            .and_then(|tail| tail.split('/').next())
            .and_then(|port| port.parse::<u16>().ok());
        assert_eq!(port, Some(51234));
    }

    /// The reader must skip past the handshake and then forward exactly the
    /// approval requests — no more, no less. Drives the real reader over an
    /// in-memory pipe standing in for the sidecar's stdout.
    #[tokio::test]
    async fn the_reader_forwards_approval_requests_after_the_handshake() {
        use std::sync::Mutex as StdMutex;

        let script = format!(
            "some sidecar log line\n\
             {LISTENING_PREFIX}http://127.0.0.1:9/mcp\n\
             {APPROVAL_PREFIX}{{\"id\":1,\"field\":\"Password\"}}\n\
             an unrelated line\n\
             {APPROVAL_PREFIX}not-json\n\
             {APPROVAL_PREFIX}{{\"id\":2,\"field\":\"CVV\"}}\n"
        );
        let mut lines = BufReader::new(std::io::Cursor::new(script.into_bytes())).lines();

        let url = read_listening_url(&mut lines).await.expect("handshake");
        assert_eq!(url, "http://127.0.0.1:9/mcp");

        let seen = Arc::new(StdMutex::new(Vec::<serde_json::Value>::new()));
        let sink: ApprovalSink = {
            let seen = Arc::clone(&seen);
            Arc::new(move |request| seen.lock().unwrap().push(request))
        };
        pump_approvals(lines, sink).await;

        let seen = seen.lock().unwrap();
        // The two well-formed requests, and neither the log lines nor the
        // malformed one.
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0]["id"], 1);
        assert_eq!(seen[1]["field"], "CVV");
    }
}
