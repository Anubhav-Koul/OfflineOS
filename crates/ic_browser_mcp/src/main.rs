//! The `ic-browser-mcp` sidecar.
//!
//! Serves the six browser tools as an MCP server on `http://127.0.0.1:<port>/mcp`,
//! driving a dedicated Chrome/Edge that is launched **on the first tool call**,
//! not at startup. The URL is printed as the first line of stdout so the
//! supervising widget can read it back — the same handshake shape as the
//! llama.cpp sidecar's base URL.
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

use ic_browser_mcp::browser::{LaunchOptions, LazyBrowser};
use ic_browser_mcp::{Server, StdioApprover};

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

    let server = Server::bind(port).await?;

    // The consent channel. Sensitive fills ask the parent (the widget) over
    // stdout/stdin before typing; with no parent this would still deny, but the
    // sidecar is always a child of the widget in normal use. Wired here — not the
    // `DenyAll` default of `BrowserSession::launch` — so that fills can actually be
    // approved.
    let approver = StdioApprover::new();

    // The browser is NOT launched here. It comes up on the first `tools/call`
    // (see `LazyBrowser`) — otherwise every app launch would put a Chrome window
    // on the user's desktop whether or not the agent ever browses. Discovery is
    // unaffected: the `tools/list` Reborn runs at activation is answered from the
    // static catalogue, so "listening" still means "discoverable".
    let executor = LazyBrowser::new(
        LaunchOptions {
            profile_dir,
            headless,
        },
        approver,
    );
    tracing::info!(url = %server.mcp_url(), "browser sidecar ready (browser starts on first use)");

    // The handshake line the supervisor parses. Kept first and stable.
    println!("IC_BROWSER_MCP_LISTENING {}", server.mcp_url());
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    server.serve(std::sync::Arc::new(executor)).await
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
