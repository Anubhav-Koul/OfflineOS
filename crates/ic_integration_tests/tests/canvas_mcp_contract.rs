//! Phase 5 contract gate: the in-process canvas server really is a hosted-MCP
//! provider that *this* IronClaw accepts, and `canvas_render` survives discovery
//! as a capability.
//!
//! Same shape as `browser_mcp_contract`, and for the same reason: the failure mode
//! to guard against is an extension that installs and activates cleanly and then
//! exposes no usable tool. This drives the real canvas server over HTTP and feeds
//! its `tools/list` through the runtime's own `package_with_discovered_hosted_mcp_tools`,
//! so it fails if the runtime would refuse us — not if our own beliefs drift.

use std::sync::Arc;

use ic_canvas_mcp::server::{CanvasSink, Server};
use ic_canvas_mcp::{CANVAS_RENDER, EXTENSION_ID, RenderRequest};
use ironclaw_extensions::{
    ExtensionManifest, ExtensionPackage, HostedMcpDiscoveredTool,
    HostedMcpDiscoveredToolAnnotations, ManifestSource, is_hosted_http_mcp_package,
    package_with_discovered_hosted_mcp_tools,
};
use ironclaw_host_api::{EffectKind, VirtualPath};
use serde_json::{Value, json};

/// A sink that accepts every render — the contract under test is the MCP seam and
/// discovery, not the rendering.
struct NullSink;

impl CanvasSink for NullSink {
    fn render(&self, _request: RenderRequest) -> Result<(), String> {
        Ok(())
    }
}

async fn spawn_server() -> u16 {
    let server = Server::bind(0).await.expect("bind the canvas server");
    let port = server.local_addr().port();
    tokio::spawn(async move {
        let _ = server.serve(Arc::new(NullSink)).await;
    });
    port
}

async fn rpc(client: &reqwest::Client, url: &str, body: Value) -> Value {
    client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(body.to_string())
        .send()
        .await
        .expect("the server answered")
        .json()
        .await
        .expect("json")
}

fn package_for(port: u16) -> ExtensionPackage {
    let manifest = ExtensionManifest::parse(
        &ic_canvas_mcp::manifest_toml(port),
        ManifestSource::HostBundled,
        &ironclaw_host_runtime::default_host_port_catalog().expect("host port catalog"),
    )
    .expect("the shipped canvas manifest must parse");
    ExtensionPackage::from_manifest(
        manifest,
        VirtualPath::new(format!("/system/extensions/{EXTENSION_ID}")).expect("root"),
    )
    .expect("the shipped canvas manifest must build a package")
}

#[tokio::test]
async fn the_canvas_manifest_is_recognized_as_a_hosted_mcp_provider() {
    // Same CP-4 dependency as the browser: an http loopback url only qualifies
    // because of the core patch.
    assert!(
        is_hosted_http_mcp_package(&package_for(8945)),
        "the runtime must classify the loopback canvas server as a hosted-MCP provider"
    );
}

#[tokio::test]
async fn the_live_tools_list_becomes_a_render_capability() {
    let port = spawn_server().await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/mcp");

    let body = rpc(
        &client,
        &url,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    let tools = body["result"]["tools"].as_array().expect("a tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], CANVAS_RENDER);

    let discovered: Vec<HostedMcpDiscoveredTool> = tools
        .iter()
        .map(|tool| HostedMcpDiscoveredTool {
            name: tool["name"].as_str().unwrap().to_string(),
            description: tool["description"].as_str().unwrap_or("").to_string(),
            input_schema: tool["inputSchema"].clone(),
            annotations: HostedMcpDiscoveredToolAnnotations {
                destructive_hint: tool["annotations"]["destructiveHint"]
                    .as_bool()
                    .unwrap_or(false),
                side_effects_hint: tool["annotations"]["sideEffectsHint"]
                    .as_bool()
                    .unwrap_or(false),
                read_only_hint: tool["annotations"]["readOnlyHint"]
                    .as_bool()
                    .unwrap_or(false),
            },
        })
        .collect();

    let active = package_with_discovered_hosted_mcp_tools(&package_for(port), &discovered)
        .expect("the runtime must publish canvas_render as a capability");

    let ids: Vec<&str> = active
        .capabilities
        .iter()
        .map(|capability| capability.id.as_str())
        .collect();
    assert_eq!(ids, ["ic-canvas.canvas_render"]);

    // canvas_render is a display action, not a write. Its read-only annotation
    // must keep it off the external-write effect path, or every chart render would
    // be classified as an external write.
    let capability = &active.capabilities[0];
    assert!(
        !capability.effects.contains(&EffectKind::ExternalWrite),
        "canvas_render must not be an external write"
    );
    assert!(
        capability.parameters_schema.is_object(),
        "the model must see canvas_render's input schema"
    );
}
