//! The in-process canvas MCP server, and registering its tool with the gateway.
//!
//! Unlike the browser sidecar (a child process driving Chrome), the canvas server
//! runs on a tokio task **inside the widget**, because a render must reach a Tauri
//! window and every gateway channel to the widget sanitizes/truncates content
//! first. See `ic_canvas_mcp` for why.
//!
//! The startup order matches the browser's, and for the same reasons (learned in
//! Phase 4):
//!
//! 1. **Bind before the gateway boots.** The extension catalogue is scanned once
//!    at boot and the manifest's `url` carries the server's live port, so the
//!    server must be listening and the manifest written first.
//! 2. **Register against the running gateway, on every launch.** Activation is when
//!    the gateway calls `tools/list`; a restart republishes only the bundled
//!    template, and a discovery failure falls back to it silently — so [`register`]
//!    verifies the capability landed rather than trusting activation.

use std::path::Path;
use std::sync::Arc;

pub use ic_canvas_mcp::RenderRequest;
use ic_canvas_mcp::server::CanvasSink;
use ic_canvas_mcp::{EXTENSION_ID, Server};

use crate::browser::extensions_root;
use crate::gateway_client::GatewayClient;

/// A running in-process canvas server. Held in app state; dropping it aborts the
/// serve task (and the OS reclaims the port).
pub struct CanvasServer {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl CanvasServer {
    /// The loopback port it is serving on.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for CanvasServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bind the in-process canvas server and write its extension manifest.
///
/// Must run **before** the gateway starts. Best-effort: a bind failure means no
/// canvas tool, not a failed launch — the same degradation as a missing browser or
/// model.
pub async fn start(reborn_home: &Path, sink: Arc<dyn CanvasSink>) -> Option<CanvasServer> {
    match try_start(reborn_home, sink).await {
        Ok(server) => {
            tracing::info!(
                port = server.port,
                "the canvas server is ready (in-process)"
            );
            Some(server)
        }
        Err(error) => {
            tracing::warn!(%error, "the canvas server did not start; running without the canvas tool");
            None
        }
    }
}

async fn try_start(reborn_home: &Path, sink: Arc<dyn CanvasSink>) -> Result<CanvasServer, String> {
    let server = Server::bind(0)
        .await
        .map_err(|error| format!("could not bind the canvas server: {error}"))?;
    let port = server.local_addr().port();

    // The manifest must exist before the gateway scans its catalogue.
    let root = extensions_root(reborn_home);
    ic_canvas_mcp::install_package(&root, port)
        .map_err(|error| format!("could not write the canvas extension manifest: {error}"))?;

    let task = tokio::spawn(async move {
        if let Err(error) = server.serve(sink).await {
            tracing::warn!(%error, "the canvas server stopped");
        }
    });

    Ok(CanvasServer { port, task })
}

/// Install and activate the canvas extension against a **running** gateway, and
/// confirm its tool was discovered.
///
/// Same discipline as the browser: activation is when discovery runs, a restart
/// republishes only the template, and a discovery failure is silent — so we check
/// the capability count rather than trust the activation response.
pub async fn register(client: &GatewayClient) {
    if let Err(error) = client.install_extension(EXTENSION_ID).await {
        tracing::debug!(%error, "installing the canvas extension was not needed or failed");
    }

    match client.activate_extension(EXTENSION_ID).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!("the gateway did not activate the canvas extension");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "could not activate the canvas extension; no canvas tool this session");
            return;
        }
    }

    match client.extension_capabilities(EXTENSION_ID).await {
        Ok(capabilities) if !capabilities.is_empty() => {
            tracing::info!(
                count = capabilities.len(),
                "the canvas tool is live on the agent"
            );
        }
        Ok(_) => {
            tracing::warn!(
                "the canvas extension activated but its tool was not discovered — the gateway \
                 fell back to the bundled template. The server was probably not reachable at \
                 activation."
            );
        }
        Err(error) => {
            tracing::warn!(%error, "could not confirm the canvas tool was discovered");
        }
    }
}

/// A [`CanvasSink`] that forwards each render to a callback — wired by `main.rs`
/// to emit into the canvas window. Kept here (not in `ic_canvas_mcp`) so the sink
/// can be Tauri-flavoured while the crate stays Tauri-free.
pub struct CallbackSink<F>(pub F);

impl<F> CanvasSink for CallbackSink<F>
where
    F: Fn(RenderRequest) -> Result<(), String> + Send + Sync + 'static,
{
    fn render(&self, request: RenderRequest) -> Result<(), String> {
        (self.0)(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn start_binds_a_loopback_port_and_writes_the_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let seen: Arc<Mutex<Vec<RenderRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let seen = Arc::clone(&seen);
            Arc::new(CallbackSink(move |request: RenderRequest| {
                seen.lock().unwrap().push(request);
                Ok(())
            }))
        };

        let server = start(temp.path(), sink)
            .await
            .expect("the server should start");
        assert!(server.port() > 0);

        // The manifest is on disk for the gateway to find, with the live port.
        let manifest = std::fs::read_to_string(
            extensions_root(temp.path())
                .join(EXTENSION_ID)
                .join("manifest.toml"),
        )
        .expect("manifest written");
        assert!(manifest.contains(&format!("127.0.0.1:{}", server.port())));
    }
}
