//! The `ic-browser-mcp` sidecar.
//!
//! Launches a dedicated Chrome/Edge and serves the six browser tools as an MCP
//! server on `http://127.0.0.1:<port>/mcp`. The URL is printed as the first line
//! of stdout so the supervising widget can read it back — the same handshake
//! shape as the llama.cpp sidecar's base URL.
//!
//! The widget normally pins the port (`IC_BROWSER_MCP_PORT`) rather than letting
//! the OS choose, because the port is baked into the extension manifest's `url`
//! and the runtime pins its egress allowlist to that exact host:port. A port that
//! moved between runs would leave the manifest pointing at nothing.
//!
//! Configuration comes from the environment, so the widget can spawn it with a
//! plain `Command`:
//!
//! - `IC_BROWSER_MCP_PORT`     — port to bind, or `0` / unset for a free one.
//! - `IC_BROWSER_MCP_PROFILE`  — the dedicated user-data dir (required).
//! - `IC_BROWSER_MCP_HEADLESS` — `1` to run headless (default: visible window,
//!   because the user must be able to watch it and take over for a CAPTCHA or a
//!   login).

use std::path::PathBuf;
use std::process::ExitCode;

use ic_browser_mcp::browser::LaunchOptions;
use ic_browser_mcp::{BrowserSession, Server, StdioApprover};

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "the browser sidecar exited with an error");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> ic_browser_mcp::Result<()> {
    let profile_dir = match std::env::var_os("IC_BROWSER_MCP_PROFILE") {
        Some(dir) => PathBuf::from(dir),
        None => {
            return Err(ic_browser_mcp::Error::browser(
                "IC_BROWSER_MCP_PROFILE is required (the dedicated browser profile directory)",
            ));
        }
    };
    let port = std::env::var("IC_BROWSER_MCP_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    let headless = matches!(std::env::var("IC_BROWSER_MCP_HEADLESS").as_deref(), Ok("1"));

    // Launch the browser BEFORE announcing the URL. The widget activates the
    // extension as soon as it sees this line, and activation is when Reborn runs
    // hosted-MCP discovery against us — a `tools/list` that arrives before the
    // browser is up would still be answered (the catalogue is static), but the
    // first `tools/call` would race a half-launched Chrome. Announcing last means
    // "listening" also means "ready".
    let server = Server::bind(port).await?;

    // The consent channel. Sensitive fills ask the parent (the widget) over
    // stdout/stdin before typing; with no parent this would still deny, but the
    // sidecar is always a child of the widget in normal use. Wired here — not the
    // `DenyAll` default of `BrowserSession::launch` — so that fills can actually be
    // approved.
    let approver = StdioApprover::new();
    let session = BrowserSession::launch_with_approver(
        LaunchOptions {
            profile_dir,
            headless,
        },
        approver,
    )
    .await?;
    tracing::info!(
        browser = session.browser_kind(),
        url = %server.mcp_url(),
        "browser sidecar ready"
    );

    // The handshake line the supervisor parses. Kept first and stable.
    println!("IC_BROWSER_MCP_LISTENING {}", server.mcp_url());
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    server.serve(std::sync::Arc::new(session)).await
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("ic_browser_mcp=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
