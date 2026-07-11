//! Phase 4 contract gate: the browser sidecar really is a hosted-MCP provider
//! that *this* IronClaw accepts.
//!
//! Every assertion here runs against **real IronClaw code**, not a restatement of
//! what we believe it does:
//!
//! - the manifest is parsed by `ironclaw_extensions::ExtensionManifest::parse`;
//! - it is classified by the real `is_hosted_http_mcp_package` (the CP-4 gate);
//! - the sidecar is driven over real HTTP with a real MCP handshake;
//! - its `tools/list` output is fed through the real
//!   `package_with_discovered_hosted_mcp_tools`, which is the exact function the
//!   runtime uses at activation to turn discovered tools into capabilities.
//!
//! That last step is the one that matters. The failure mode this phase nearly
//! shipped — and that a stdio manifest still has — is an extension that installs
//! and activates cleanly and then has no usable tools. A test that only checked
//! our own JSON against our own expectations would not have caught it. This one
//! fails if the runtime would refuse us.
//!
//! No browser and no gateway are launched: the sidecar's `ToolExecutor` seam is
//! filled with a fake, so this runs on a machine with no Chrome.

use std::sync::Arc;

use async_trait::async_trait;
use ic_browser_mcp::protocol::Tool;
use ic_browser_mcp::server::ToolExecutor;
use ic_browser_mcp::{EXTENSION_ID, Server};
use ironclaw_extensions::{
    ExtensionManifest, ExtensionPackage, HostedMcpDiscoveredTool,
    HostedMcpDiscoveredToolAnnotations, ManifestSource, is_hosted_http_mcp_package,
    package_with_discovered_hosted_mcp_tools,
};
use ironclaw_host_api::{EffectKind, PermissionMode, VirtualPath};
use serde_json::{Value, json};

/// Stands in for a real browser. The contract under test is the MCP seam, not CDP.
struct FakeBrowser;

#[async_trait]
impl ToolExecutor for FakeBrowser {
    async fn call(&self, tool: Tool, arguments: Value) -> ic_browser_mcp::Result<Value> {
        Ok(ic_browser_mcp::protocol::tool_success_result(
            json!({ "tool": tool.wire_name(), "echo": arguments }),
        ))
    }
}

/// Start the real sidecar server on a free loopback port.
async fn spawn_sidecar() -> (String, u16) {
    let server = Server::bind(0).await.expect("bind the sidecar");
    let port = server.local_addr().port();
    let url = server.mcp_url();
    tokio::spawn(async move {
        let _ = server.serve(Arc::new(FakeBrowser)).await;
    });
    (url, port)
}

/// POST one JSON-RPC message, exactly as `ironclaw_mcp::McpHostHttpClient` does:
/// same `Content-Type`, same `Accept`.
async fn rpc(client: &reqwest::Client, url: &str, body: Value) -> reqwest::Response {
    client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(body.to_string())
        .send()
        .await
        .expect("the sidecar answered")
}

/// Build the package the way the runtime does at boot: parse the manifest we
/// actually ship, as a host-bundled filesystem drop-in.
fn package_for(port: u16) -> ExtensionPackage {
    let manifest = ExtensionManifest::parse(
        &ic_browser_mcp::manifest_toml(port),
        ManifestSource::HostBundled,
        // The catalogue the runtime itself validates against, not an empty stub:
        // our manifest declares `host.runtime.http_egress`, and an empty catalog
        // would reject it — which is precisely the kind of mismatch this test
        // exists to catch.
        &ironclaw_host_runtime::default_host_port_catalog().expect("the host port catalog"),
    )
    .expect("the shipped manifest must parse");

    ExtensionPackage::from_manifest(
        manifest,
        VirtualPath::new(format!("/system/extensions/{EXTENSION_ID}")).expect("a valid root"),
    )
    .expect("the shipped manifest must build a package")
}

/// The whole point of CP-4. Before the patch this returned `false` (the URL is
/// `http`, not `https`) and the extension was silently not a hosted-MCP provider:
/// it would install, activate, and expose no tools.
#[tokio::test]
async fn the_shipped_manifest_is_recognized_as_a_hosted_mcp_provider() {
    let package = package_for(8931);
    assert!(
        is_hosted_http_mcp_package(&package),
        "the runtime must classify the loopback sidecar as a hosted-MCP provider; \
         without CP-4 this is false and the extension activates with zero tools"
    );
}

/// A manifest pointing anywhere but loopback must still be refused — CP-4 is an
/// exemption for on-device sidecars, not a hole.
#[tokio::test]
async fn a_non_loopback_http_manifest_is_still_refused() {
    let hostile = ic_browser_mcp::manifest_toml(8931)
        .replace("http://127.0.0.1:8931/mcp", "http://169.254.169.254/mcp");
    let manifest = ExtensionManifest::parse(
        &hostile,
        ManifestSource::HostBundled,
        // The catalogue the runtime itself validates against, not an empty stub:
        // our manifest declares `host.runtime.http_egress`, and an empty catalog
        // would reject it — which is precisely the kind of mismatch this test
        // exists to catch.
        &ironclaw_host_runtime::default_host_port_catalog().expect("the host port catalog"),
    )
    .expect("it still parses; it just must not qualify");
    let package = ExtensionPackage::from_manifest(
        manifest,
        VirtualPath::new(format!("/system/extensions/{EXTENSION_ID}")).expect("root"),
    )
    .expect("package");

    assert!(
        !is_hosted_http_mcp_package(&package),
        "plain http to a non-loopback host (here: the cloud-metadata address) \
         must never qualify as a hosted-MCP provider"
    );
}

/// The full handshake the runtime performs at activation, against the real server.
#[tokio::test]
async fn the_sidecar_completes_the_mcp_handshake_the_runtime_performs() {
    let (url, _) = spawn_sidecar().await;
    let client = reqwest::Client::new();

    // 1. initialize — the host reads `protocolVersion` out of the result and
    //    stores it; a missing or malformed one aborts the session.
    let response = rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": ic_browser_mcp::PROTOCOL_VERSION },
        }),
    )
    .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    let version = body["result"]["protocolVersion"]
        .as_str()
        .expect("the host requires a protocolVersion string");
    assert!(
        version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')),
        "the host validates this charset and errors out otherwise"
    );

    // 2. notifications/initialized — a notification: no id, and the host expects
    //    202 with no body. A 200-with-a-body here breaks the session.
    let response = rpc(
        &client,
        &url,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert_eq!(
        response.status(),
        202,
        "the host treats 202-with-no-body as the success case for a notification"
    );
    assert!(response.bytes().await.expect("body").is_empty());

    // 3. tools/call — and the id must come back untouched, or the host discards
    //    the response as a mismatch.
    let response = rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0", "id": 42, "method": "tools/call",
            "params": {
                "name": "browser_navigate",
                "arguments": { "url": "https://example.com" },
            },
        }),
    )
    .await;
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["id"], 42, "the host matches responses by id");
    assert_eq!(body["result"]["isError"], false);
}

/// The end-to-end proof: our live `tools/list` becomes six real capabilities,
/// through the runtime's own discovery code.
#[tokio::test]
async fn the_live_tools_list_becomes_six_gated_capabilities() {
    let (url, port) = spawn_sidecar().await;
    let client = reqwest::Client::new();

    let response = rpc(
        &client,
        &url,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    let body: Value = response.json().await.expect("json");
    let tools = body["result"]["tools"].as_array().expect("a tools array");
    assert_eq!(tools.len(), 6);

    // Project the wire JSON into the runtime's discovered-tool type exactly as
    // `mcp_discovery::discovered_tool_for_extension_domain` does.
    let discovered: Vec<HostedMcpDiscoveredTool> = tools
        .iter()
        .map(|tool| HostedMcpDiscoveredTool {
            name: tool["name"].as_str().expect("a name").to_string(),
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

    // The real function the runtime calls at activation. If our tool names,
    // schemas, or annotations were unacceptable, this errors — which in
    // production would show up only as a silent fallback to the bundled template.
    let active = package_with_discovered_hosted_mcp_tools(&package_for(port), &discovered)
        .expect("the runtime must be able to publish our tools as capabilities");

    let mut ids: Vec<&str> = active
        .capabilities
        .iter()
        .map(|capability| capability.id.as_str())
        .collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        [
            "ic-browser.browser_click",
            "ic-browser.browser_fill",
            "ic-browser.browser_find",
            "ic-browser.browser_get_text",
            "ic-browser.browser_navigate",
            "ic-browser.browser_screenshot",
        ],
        "all six tools must survive discovery as capabilities"
    );

    for capability in &active.capabilities {
        // Reborn hardcodes `Ask` for discovered MCP tools. This is what routes a
        // browser action through the approval flow, so it is worth pinning: if it
        // ever stops being true, every browser action would run unprompted.
        assert_eq!(
            capability.default_permission,
            PermissionMode::Ask,
            "{} must require approval",
            capability.id
        );
        // The model must actually see the parameters we published.
        assert!(
            capability.parameters_schema.is_object(),
            "{} lost its input schema in discovery",
            capability.id
        );
    }

    // The two tools that can act as the user — submit a form, click "Buy" — must
    // carry the write effect the safety layer keys off. Our `destructiveHint`
    // annotations are what produce this.
    for acting in ["ic-browser.browser_fill", "ic-browser.browser_click"] {
        let capability = active
            .capabilities
            .iter()
            .find(|capability| capability.id.as_str() == acting)
            .expect("present");
        assert!(
            capability.effects.contains(&EffectKind::ExternalWrite),
            "{acting} must be classified as an external write"
        );
    }

    // ...and a pure read must not be, or every page read would be filed as a
    // write and the effect classification would be meaningless.
    let reader = active
        .capabilities
        .iter()
        .find(|capability| capability.id.as_str() == "ic-browser.browser_get_text")
        .expect("present");
    assert!(
        !reader.effects.contains(&EffectKind::ExternalWrite),
        "reading page text is not an external write"
    );
}
