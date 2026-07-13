//! The Reborn extension package for this sidecar.
//!
//! Reborn learns about the browser tools by scanning
//! `<reborn-home>/local-dev/system/extensions/<id>/manifest.toml` at boot. The
//! manifest lives here rather than in `ic_widget` because it has to agree with
//! [`crate::protocol`] — the extension id, the capability-id prefix, and the tool
//! names are one contract, and splitting them across crates is how they drift.
//!
//! ## The three timing rules this package obeys
//!
//! These are properties of the runtime, learned the hard way; violating any one
//! of them yields an extension that looks installed and has no tools:
//!
//! 1. **The catalogue is scanned once, at `serve` boot.** The manifest must be on
//!    disk *before* the gateway starts. A file dropped in later is invisible
//!    until the next restart.
//! 2. **Tool discovery happens at *activation*, not at boot.** Reborn calls our
//!    `tools/list` when the extension is activated, so the sidecar must be
//!    listening at that moment — and activation must be driven against the
//!    *running* gateway.
//! 3. **A restart does not re-discover.** `restore_extension_lifecycle_state`
//!    republishes the *bundled manifest*, which carries only the capability
//!    template below — not the six real tools. So the widget re-activates on every
//!    launch. A transient discovery failure *silently* falls back to the template,
//!    which is why the widget verifies the capability count instead of trusting
//!    that activation "succeeded".
//!
//! ## Why all six capabilities are declared here
//!
//! In principle Reborn **discards** these declarations and rebuilds every
//! capability from the live `tools/list`
//! (`hosted_mcp_discovery::package_with_discovered_hosted_mcp_tools`), which is
//! why this file used to declare a single representative *template*.
//!
//! **In practice hosted-MCP discovery can never succeed in `ironclaw-reborn
//! serve`.** The discovery call goes out through `RuntimeHttpEgress`, which
//! resolves its `NetworkPolicy` from the staged `NetworkObligationPolicyStore`
//! keyed by `(scope, capability_id)`. That store is only ever written during a
//! *capability-dispatch* obligation preflight (`obligations.rs::finish_prepare`).
//! Discovery runs at **activation**, outside any dispatch, so nothing has staged a
//! policy and the lookup fails with `network_policy_missing` — which the error
//! boundary collapses into an opaque `network_error`. Reborn then logs at `debug!`
//! and **silently falls back to this bundled manifest while still reporting
//! `activated: true`**. Verified against the running gateway: the sidecar is never
//! contacted at all.
//!
//! So the fallback is not a degraded path — it is the *only* path. Declaring all
//! six capabilities here is therefore what actually gives the agent its browser
//! tools. Each one is generated from [`crate::protocol::Tool`], the same source
//! `tools/list` is generated from, so the two cannot drift; and if upstream ever
//! fixes discovery, it will rebuild these same six from the live list and nothing
//! here has to change.
//!
//! Effects are declared explicitly rather than inferred from annotations (the
//! discovery path's `destructiveHint` → `ExternalWrite` promotion never runs), so
//! `browser_fill` and `browser_click` carry `external_write` and the four reads do
//! not.

use crate::protocol::{ALL_TOOLS, Tool};
use crate::server::MCP_PATH;

/// The capability block for one tool.
///
/// `required_host_ports` and the absence of runtime credentials are identical
/// across all six — `hosted_mcp_capability_template` rejects a provider whose
/// capabilities disagree on those, so a future discovery path stays valid.
fn capability_toml(tool: Tool) -> String {
    let name = tool.wire_name();
    // Only the two writes get `external_write`; see `Tool::annotations`, which is
    // the same split `tools/list` publishes.
    let effects = match tool {
        Tool::BrowserFill | Tool::BrowserClick => {
            r#"["dispatch_capability", "network", "external_write"]"#
        }
        _ => r#"["dispatch_capability", "network"]"#,
    };
    let description = tool.description().replace('"', "'");
    format!(
        r#"
[[capabilities]]
id = "{EXTENSION_ID}.{name}"
description = "{description}"
effects = {effects}
default_permission = "ask"
visibility = "model"
input_schema_ref = "schemas/{EXTENSION_ID}/{name}.input.v1.json"
output_schema_ref = "schemas/{EXTENSION_ID}/{name}.output.v1.json"
required_host_ports = ["host.runtime.http_egress"]
"#
    )
}

/// The extension id. Also the directory name under `/system/extensions/`, and the
/// prefix of every capability id (`ic-browser.browser_navigate`, …).
pub const EXTENSION_ID: &str = "ic-browser";

/// The manifest for a sidecar listening on `port`.
///
/// `url` is pinned to `127.0.0.1` — not `localhost`. CP-4 only exempts a literal
/// loopback **IP**, precisely because a DNS name could be rebound; a `localhost`
/// URL here would be refused by the very patch that makes this work.
pub fn manifest_toml(port: u16) -> String {
    let url = format!("http://127.0.0.1:{port}{MCP_PATH}");
    let capabilities: String = ALL_TOOLS.map(capability_toml).join("");
    format!(
        r#"# Generated by ic_widget on launch — do not edit by hand.
#
# The port is baked into `url` because Reborn pins its egress allowlist to this
# exact host:port. If the sidecar's port changes, this file must be rewritten and
# the gateway restarted (the extension catalogue is only scanned at boot).
schema_version = "reborn.extension_manifest.v2"
id = "{EXTENSION_ID}"
name = "Browser"
version = "0.1.0"
description = "Drive a dedicated Chrome/Edge instance: navigate, read pages, find elements, fill inputs, click, and screenshot."
trust = "third_party"

[runtime]
kind = "mcp"
transport = "http"
url = "{url}"

# All six tools, NOT a template. Hosted-MCP discovery cannot succeed in Reborn
# (see the module docs), so this manifest is what the agent actually gets.
{capabilities}"#
    )
}

/// A capability's input schema — the same one `tools/list` publishes.
pub fn input_schema(tool: Tool) -> String {
    serde_json::to_string_pretty(&tool.input_schema()).unwrap_or_else(|_| "{}".to_string())
}

/// A capability's output schema. Deliberately permissive: the real outputs are
/// MCP `content` blocks, which Reborn passes through as-is.
pub fn output_schema() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "type": "object",
        "additionalProperties": true,
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

/// Write the extension package into `extensions_root` (i.e.
/// `<reborn-home>/local-dev/system/extensions`), creating
/// `<extensions_root>/ic-browser/`.
///
/// Idempotent: it overwrites, because the port changes the manifest and a stale
/// `url` is worse than no extension at all.
pub fn install_package(extensions_root: &std::path::Path, port: u16) -> std::io::Result<()> {
    let package_root = extensions_root.join(EXTENSION_ID);
    let schema_dir = package_root.join("schemas").join(EXTENSION_ID);
    std::fs::create_dir_all(&schema_dir)?;

    std::fs::write(package_root.join("manifest.toml"), manifest_toml(port))?;
    // Every capability declares schema refs, so every capability needs its files
    // on disk — a missing one fails the whole manifest at catalogue scan.
    for tool in ALL_TOOLS {
        let name = tool.wire_name();
        std::fs::write(
            schema_dir.join(format!("{name}.input.v1.json")),
            input_schema(tool),
        )?;
        std::fs::write(
            schema_dir.join(format!("{name}.output.v1.json")),
            output_schema(),
        )?;
    }
    tracing::info!(root = %package_root.display(), port, "installed the browser extension manifest");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_pins_a_loopback_ip_not_a_hostname() {
        let manifest = manifest_toml(8931);
        assert!(
            manifest.contains(r#"url = "http://127.0.0.1:8931/mcp""#),
            "{manifest}"
        );
        // `localhost` would be refused by CP-4, which requires an IP literal.
        assert!(!manifest.contains("localhost"));
    }

    #[test]
    fn the_manifest_declares_the_hosted_http_mcp_runtime_shape() {
        // `hosted_http_mcp_url` requires exactly this: transport "http", a url,
        // no `command`, and no `args`. Anything else and the package silently
        // fails to qualify as a hosted-MCP provider and never gets discovered.
        let manifest = manifest_toml(1234);
        assert!(manifest.contains(r#"kind = "mcp""#));
        assert!(manifest.contains(r#"transport = "http""#));
        assert!(!manifest.contains("command"));
        assert!(!manifest.contains("args"));
    }

    /// The regression that motivated declaring all six: discovery never runs, so
    /// whatever this manifest declares *is* the agent's browser. A single
    /// template capability meant the agent could only navigate — it could not
    /// read, find, fill, click, or screenshot.
    #[test]
    fn the_manifest_declares_every_tool_because_discovery_never_replaces_them() {
        let manifest = manifest_toml(1);
        for tool in ALL_TOOLS {
            assert!(
                manifest.contains(&format!("id = \"{EXTENSION_ID}.{}\"", tool.wire_name())),
                "{} is missing from the manifest:\n{manifest}",
                tool.wire_name()
            );
        }
    }

    /// The two tools that can submit a form, log in as the user, or spend money
    /// must carry a write effect; the four reads must not. Annotations do not do
    /// this for us here — the discovery path that reads them never runs.
    #[test]
    fn only_the_writes_declare_an_external_write_effect() {
        let manifest = manifest_toml(1);
        for tool in ALL_TOOLS {
            let block = manifest
                .split("[[capabilities]]")
                .find(|block| block.contains(&format!(".{}\"", tool.wire_name())))
                .unwrap_or_else(|| panic!("no capability block for {}", tool.wire_name()));
            let declares_write = block.contains("external_write");
            let is_write = matches!(tool, Tool::BrowserFill | Tool::BrowserClick);
            assert_eq!(
                declares_write,
                is_write,
                "{} declares external_write = {declares_write}",
                tool.wire_name()
            );
        }
    }

    #[test]
    fn the_template_capability_id_is_prefixed_with_the_extension_id() {
        // Reborn builds capability ids as `<extension>.<tool>`; a template whose
        // id lacks the prefix fails manifest validation.
        assert!(manifest_toml(1).contains(&format!("id = \"{EXTENSION_ID}.browser_navigate\"")));
    }

    /// Every declared capability points at schema files by ref. A capability whose
    /// schema file is missing fails the manifest at catalogue scan — which would
    /// take the whole extension down, not just that one tool.
    #[test]
    fn install_package_writes_a_schema_file_for_every_declared_capability() {
        let temp = tempfile::tempdir().expect("tempdir");
        install_package(temp.path(), 4321).expect("install");

        let package_root = temp.path().join(EXTENSION_ID);
        let manifest = std::fs::read_to_string(package_root.join("manifest.toml")).expect("read");
        assert!(manifest.contains("127.0.0.1:4321"));

        // The catalogue keys the package by directory name, so it must equal the
        // manifest id or the scan drops it.
        assert!(package_root.is_dir());

        let schema_dir = package_root.join("schemas").join(EXTENSION_ID);
        for tool in ALL_TOOLS {
            let name = tool.wire_name();
            assert!(
                schema_dir.join(format!("{name}.input.v1.json")).is_file(),
                "{name} has no input schema on disk"
            );
            assert!(
                schema_dir.join(format!("{name}.output.v1.json")).is_file(),
                "{name} has no output schema on disk"
            );
        }
    }

    #[test]
    fn reinstalling_with_a_new_port_replaces_the_stale_url() {
        let temp = tempfile::tempdir().expect("tempdir");
        install_package(temp.path(), 1111).expect("first install");
        install_package(temp.path(), 2222).expect("second install");

        let manifest =
            std::fs::read_to_string(temp.path().join(EXTENSION_ID).join("manifest.toml"))
                .expect("read");
        assert!(manifest.contains("127.0.0.1:2222"));
        assert!(!manifest.contains("1111"), "a stale port must not survive");
    }
}
