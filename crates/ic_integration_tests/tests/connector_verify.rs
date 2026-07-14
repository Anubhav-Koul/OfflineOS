//! Phase 8b ⚠️ VERIFY: can a **registry** connector actually reach the agent?
//!
//! The bar the spec sets is deliberately high: *one real tool call*, not a green
//! install. Phase 4 taught this the hard way — the browser extension installed,
//! activated, reported `activated: true`, and every tool call died, twice over
//! (CP-4: loopback egress denied; CP-5: the capability never granted its own
//! endpoint). A green install proves nothing.
//!
//! Our browser/canvas MCPs are **in-process, host-bundled first-party**
//! extensions, which sidesteps the whole external-registry path. This asks
//! whether that path works at all, and it does so in stages so a failure names
//! itself:
//!
//! 1. Does the gateway even expose a registry to install from?
//! 2. Does an install of a registry entry succeed?
//! 3. Does the setup projection tell us where credentials go?
//! 4. Does activation publish capabilities the model can see?
//! 5. **Does a tool call reach the internet?**
//!
//! Step 5 needs no real secret, which is the trick that makes this runnable: a
//! *bogus* GitHub token still separates the three unknowns. If the tool call
//! comes back with GitHub saying `401`, then registration, egress, and credential
//! injection all work — only the key is wrong, and a real key would work. If it
//! dies in obligation preflight (`network_policy_missing`, `Network`), egress is
//! blocked and the registry path is upstream-broken, exactly as it was for
//! CP-4/CP-5.
//!
//! # The answer (2026-07-15)
//!
//! Stages 1–4 **work**, and work well — this is *not* the Phase 4 trap. The
//! registry is composed (10 entries), install succeeds, the credential lands
//! through the product-auth lane, and activation genuinely hands the model all 34
//! GitHub tools.
//!
//! **Stage 5 hangs.** The capability reports `started` and then never completes,
//! never fails, and never times out — observed at 90 s, 120 s and 300 s. The
//! module is *not* the problem: `system/extensions/github/wasm/github_tool.wasm`
//! is downloaded and sitting on disk. The artifact arrives, the capability is
//! published, the model calls it, and the **execution wedges**. A user who
//! installs a WASM registry connector gets an agent that freezes on first use,
//! with no error to show for it and no timeout to recover from.
//!
//! So 8b takes the fallback the spec allows: in-process first-party connectors,
//! the proven `ic_browser_mcp` / `ic_canvas_mcp` pattern. The assertions at the
//! bottom are a **tripwire asserting the broken behaviour** — the day upstream
//! fixes WASM tool execution, this test goes red and says so.
//!
//! Contract verified against upstream `a492857` (`reborn-integration`).
#![cfg(feature = "webui-v2-beta")]

use std::sync::Arc;
use std::time::Duration;

use ic_integration_tests::{API_PREFIX, MockReply, RebornServer};

/// The connector under test: a WASM `tool` whose auth is a **manual token**, not
/// OAuth. Every hosted-MCP entry in the registry (Notion, Linear, …) is `auth:
/// dcr` — a browser OAuth dance that cannot run in a test — so GitHub is the only
/// registry connector that can be driven end to end without a human.
const CONNECTOR: &str = "github";

/// What the agent is told to do. Its presence in the mock's request log is how we
/// know the turn reached the model at all.
const ASK: &str = "CONNECTOR-VERIFY";

/// A mock that calls a GitHub tool once, then reports whatever came back.
///
/// The tool result rides back as a `role: "tool"` message, so the *second* request
/// the gateway makes to the model carries the verdict — that is what we read.
fn responder() -> ic_integration_tests::MockResponder {
    Arc::new(|body: &str| {
        if body.contains("\"role\":\"tool\"") {
            return MockReply::Text("tool call returned".to_string());
        }
        if body.contains(ASK) {
            return MockReply::ToolCall {
                // The capability id folds `.` to `__`. If discovery published
                // something else, the call fails as "unknown tool" — which is
                // itself a finding, and the assertions below say so.
                name: "github__search_repositories".to_string(),
                arguments: serde_json::json!({ "query": "ironclaw" }),
            };
        }
        MockReply::Text("nothing to do".to_string())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wasm_registry_connector_reaches_the_model_but_its_tool_call_hangs() {
    // A home we can inspect afterwards: whether the WASM artifact ever landed on
    // disk is what separates "the download never happened" from "the module ran
    // and wedged".
    let home = tempfile::tempdir().expect("home");
    let server = RebornServer::start_scripted_in_home(
        responder(),
        "unused".to_string(),
        vec![(
            "RUST_LOG".into(),
            "info,ironclaw_wasm=debug,ironclaw_extensions=debug".into(),
        )],
        home.path(),
    )
    .await;
    let http = reqwest::Client::new();
    let base = format!("{}{API_PREFIX}", server.base_url);

    // ---- 1. Is there a registry to install from? ------------------------------
    let registry: serde_json::Value = http
        .get(format!("{base}/extensions/registry"))
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("the registry route should answer")
        .json()
        .await
        .expect("registry json");
    // Raw first: the response schema is not pinned anywhere, and "0 entries"
    // could just as easily mean "my parser is wrong" as "the registry is empty".
    eprintln!(
        "probe 1 RAW: GET /extensions/registry → {}",
        truncate(&registry.to_string(), 2000)
    );
    let entries = registry_entry_ids(&registry);
    eprintln!(
        "probe 1: parsed {} entries: {:?}",
        entries.len(),
        entries.iter().take(12).collect::<Vec<_>>()
    );

    // And what the *installed* list looks like before we touch anything, for the
    // same reason.
    let before: serde_json::Value = http
        .get(format!("{base}/extensions"))
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("list extensions")
        .json()
        .await
        .unwrap_or(serde_json::Value::Null);
    eprintln!(
        "probe 1b RAW: GET /extensions → {}",
        truncate(&before.to_string(), 2000)
    );
    assert!(
        !entries.is_empty(),
        "the gateway exposes no registry at all, so no connector can ever be \
         installed from one: {registry}"
    );
    assert!(
        entries.iter().any(|id| id == CONNECTOR),
        "the registry does not carry {CONNECTOR}: {entries:?}"
    );
    eprintln!(
        "probe 1c: {CONNECTOR} is a {:?} entry",
        registry_kind(&registry, CONNECTOR)
    );

    // ---- 2. Does installing a registry entry work? ----------------------------
    let install = http
        .post(format!("{base}/extensions/install"))
        .bearer_auth(&server.token)
        .json(&serde_json::json!({
            "package_ref": { "kind": "extension", "id": CONNECTOR },
        }))
        .send()
        .await
        .expect("install should answer");
    let install_status = install.status();
    let install_body: serde_json::Value = install.json().await.unwrap_or(serde_json::Value::Null);
    eprintln!("probe 2: POST /extensions/install → {install_status}: {install_body}");
    assert!(
        install_status.is_success(),
        "a registry connector could not be installed at all — the external-extension \
         path is not composed under our profile. Fall back to in-process wrappers \
         (the ic_browser_mcp pattern) and record the registry path as \
         upstream-blocked. ({install_status}: {install_body})"
    );

    // ---- 3. Where do credentials go? -----------------------------------------
    let setup: serde_json::Value = http
        .get(format!("{base}/extensions/{CONNECTOR}/setup"))
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("setup should answer")
        .json()
        .await
        .unwrap_or(serde_json::Value::Null);
    eprintln!("probe 3: GET /extensions/{CONNECTOR}/setup → {setup}");

    // The credential does NOT go through the extension setup route. It goes
    // through the **product-auth** lane, in two steps:
    //
    //   POST /api/reborn/product-auth/manual-token/setup          → { interaction_id, invocation_id }
    //   POST /api/reborn/product-auth/manual-token/secret-submit  → { credential_ref }
    //
    // Note `secret-submit`, **not** `submit`. There are three manual-token routes
    // and they are easy to confuse:
    //
    //   - `/manual-token/submit`        — for an auth gate raised *during a run*;
    //                                     requires `run_id` + `gate_ref`.
    //   - `/manual-token/secret-submit` — the standalone one, keyed by the
    //                                     `interaction_id` from `/setup`. This is
    //                                     the one a settings page needs.
    //
    // The raw token rides its own dedicated body and is never echoed back. The
    // `invocation_id` from step 1 must be carried back in step 2's scope, or the
    // host cannot re-derive the pending interaction.
    let challenge = http
        .post(format!(
            "{}/api/reborn/product-auth/manual-token/setup",
            server.base_url
        ))
        .bearer_auth(&server.token)
        .json(&serde_json::json!({
            "provider": "github",
            "account_label": "connector-verify",
        }))
        .send()
        .await
        .expect("manual-token setup should answer");
    let challenge_status = challenge.status();
    let challenge_body: serde_json::Value =
        challenge.json().await.unwrap_or(serde_json::Value::Null);
    eprintln!("probe 3b: POST manual-token/setup → {challenge_status}: {challenge_body}");
    assert!(
        challenge_status.is_success(),
        "no credential path exists for a manual-token connector — (c) of the VERIFY \
         is a blocker: {challenge_status} {challenge_body}"
    );

    // A deliberately wrong (but well-formed) token. A real one is not needed:
    // what is under test is whether the *plumbing* carries a credential to the
    // vendor, and "GitHub rejected this token" proves that it did.
    let submit_request = serde_json::json!({
        "interaction_id": challenge_body["interaction_id"],
        "invocation_id": challenge_body["invocation_id"],
        "token": "ghp_0000000000000000000000000000000000",
    });
    let submit = http
        .post(format!(
            "{}/api/reborn/product-auth/manual-token/secret-submit",
            server.base_url
        ))
        .bearer_auth(&server.token)
        .json(&submit_request)
        .send()
        .await
        .expect("manual-token submit should answer");
    let submit_status = submit.status();
    // Text, not JSON: a rejected body answers with a plain message, and parsing
    // it as JSON throws away the only thing that says what was wrong.
    let submit_body = submit.text().await.unwrap_or_default();
    eprintln!("probe 3c: POST manual-token/submit → {submit_status}: {submit_body}");
    assert!(
        submit_status.is_success(),
        "the token could not be submitted ({submit_status}: {submit_body})\n\
         sent: {submit_request}"
    );

    // Did it stick? The setup projection says so, per-secret.
    let after_setup: serde_json::Value = http
        .get(format!("{base}/extensions/{CONNECTOR}/setup"))
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("setup should answer")
        .json()
        .await
        .unwrap_or(serde_json::Value::Null);
    let provided: Vec<(String, bool)> = after_setup["secrets"]
        .as_array()
        .map(|secrets| {
            secrets
                .iter()
                .filter_map(|secret| {
                    Some((
                        secret["name"].as_str()?.to_string(),
                        secret["provided"].as_bool()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    eprintln!("probe 3d: secrets now → {provided:?}");

    // ---- 4. Does activation publish capabilities the model can see? -----------
    let activate = http
        .post(format!("{base}/extensions/{CONNECTOR}/activate"))
        .bearer_auth(&server.token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("activate should answer");
    let activate_status = activate.status();
    let activate_body: serde_json::Value = activate.json().await.unwrap_or(serde_json::Value::Null);
    eprintln!("probe 4: POST activate → {activate_status}: {activate_body}");

    // Phase 4's lesson: `activated: true` is not evidence. Count the capabilities.
    let installed: serde_json::Value = http
        .get(format!("{base}/extensions"))
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("list extensions")
        .json()
        .await
        .unwrap_or(serde_json::Value::Null);
    let capabilities = capability_ids(&installed, CONNECTOR);
    eprintln!(
        "probe 4b: phase = {:?}, {} capabilities published (first 6: {:?})",
        phase_of(&installed, CONNECTOR),
        capabilities.len(),
        capabilities.iter().take(6).collect::<Vec<_>>()
    );

    // ---- 5. THE BAR: does a tool call reach the internet? ---------------------
    let thread = server.create_thread().await;
    server
        .send_message(&thread, &format!("{ASK} — find the ironclaw repo"))
        .await;
    let (done, stream) = server
        .stream_until(&thread, "\"status\":\"completed\"", Duration::from_secs(90))
        .await;
    eprintln!("probe 5: run completed = {done}");

    // Did the module ever arrive?
    let wasm: Vec<String> = walk(home.path())
        .into_iter()
        .filter(|path| path.to_lowercase().ends_with(".wasm"))
        .collect();
    eprintln!("probe 5e: .wasm modules on disk → {wasm:?}");

    // What tools did the MODEL actually get offered? This is the ground truth —
    // the gateway's own view can say `activated: true` while the model is handed
    // nothing (the Phase 4 trap).
    let offered: Vec<String> = server
        .chat_requests()
        .first()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value["tools"].as_array().cloned())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let github_tools: Vec<&String> = offered
        .iter()
        .filter(|name| name.starts_with("github"))
        .collect();
    eprintln!(
        "probe 5a: the model was offered {} tools, {} of them github's: {:?}",
        offered.len(),
        github_tools.len(),
        github_tools.iter().take(5).collect::<Vec<_>>()
    );

    // Every tool result the model was shown — the verdict is in here.
    let tool_results: Vec<String> = server
        .chat_requests()
        .iter()
        .filter_map(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter_map(|value| {
            let messages = value["messages"].as_array()?.clone();
            Some(messages)
        })
        .flatten()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str().map(str::to_string))
        .collect();
    eprintln!("probe 5b: tool results the model saw → {tool_results:?}");
    // Where did the run actually get stuck? The projection stream carries the
    // run's status, and `blocked_auth` means something very different from a
    // failed egress.
    eprintln!(
        "probe 5c: last run statuses seen → {:?}",
        run_statuses(&stream)
    );
    let logs = server.stderr_snapshot();
    eprintln!(
        "probe 5d: serve produced {} bytes of log; tail →\n{}",
        logs.len(),
        tail(&logs, 60)
    );

    // ---- The verdict --------------------------------------------------------
    //
    // Stages 1–4 all pass, and they pass *well*: the model is genuinely handed
    // all 34 GitHub tools. This is NOT the Phase 4 trap (a green install with no
    // tools behind it) — everything up to the invocation works.
    //
    // Stage 5 does not merely fail. It **hangs**: the capability reports
    // `started` and then never completes, never fails, and never times out —
    // observed at 90 s, 120 s and 300 s. No `.wasm` artifact is ever written to
    // the home directory, so the module the call needs is never fetched.
    //
    // So a WASM registry connector cannot be shipped: a user who installs one
    // gets an agent that freezes on first use, with no error to show them.
    //
    // These assertions are a TRIPWIRE, and they are deliberately the wrong way
    // round: they assert the *broken* behaviour, so that the day upstream fixes
    // WASM tool execution this test goes red and tells us the registry path is
    // open. Until then, 8b takes the fallback the spec allows — in-process
    // first-party connectors, the proven ic_browser_mcp / ic_canvas_mcp pattern.

    assert!(
        github_tools.len() >= 30,
        "the model should be offered GitHub's tools — registration, credentials \
         and activation all work. If this breaks, something *earlier* regressed. \
         Offered: {github_tools:?}"
    );

    assert!(
        !done && tool_results.is_empty(),
        "🎉 THE TOOL CALL RETURNED. Upstream has fixed WASM tool execution — the \
         registry connector path is open, and 8b can be rebuilt on it instead of \
         on in-process wrappers. Tool results: {tool_results:?}"
    );

    // And the module IS on disk — `system/extensions/github/wasm/github_tool.wasm`.
    // So the fetch is not the problem: the artifact downloads, the capability is
    // published, the model calls it, and the *execution* wedges. That narrows the
    // upstream bug to the WASM host, which is worth stating precisely when we
    // report it.
    assert!(
        wasm.iter().any(|path| path.ends_with(".wasm")),
        "the module is no longer being fetched — a different failure from the one \
         this test was written for. Re-diagnose rather than trusting the tripwire."
    );

    let _ = (capabilities, stream, provided);
}

/// The registry's shape, verified live:
/// `{ "entries": [ { "package_ref": { "kind": "extension", "id": "github" },
///                   "kind": "wasm_tool" | "mcp_server" | "first_party",
///                   "display_name", "description", "version", "installed" } ] }`
///
/// The id is under `package_ref.id` — *not* a top-level `id`, which is what an
/// earlier version of this probe assumed, and it reported an empty registry.
fn registry_entry_ids(body: &serde_json::Value) -> Vec<String> {
    body["entries"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry["package_ref"]["id"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The `kind` of a registry entry: `wasm_tool`, `mcp_server`, or `first_party`.
fn registry_kind(body: &serde_json::Value, id: &str) -> Option<String> {
    body["entries"].as_array()?.iter().find_map(|entry| {
        (entry["package_ref"]["id"].as_str() == Some(id))
            .then(|| entry["kind"].as_str().map(str::to_string))
            .flatten()
    })
}

/// The capability ids an installed extension exposes, and the phase it is in.
///
/// Verified shape: `{ "extensions": [ { "phase": "installed" | "active",
/// "summary": { "package_ref": {"id": …}, "visible_capability_ids": [ … ] } } ] }`.
/// The ids are under `summary.visible_capability_ids` — an earlier version of this
/// probe looked for a top-level `capabilities` and reported none, which is exactly
/// the false negative Phase 4 warned about.
fn capability_ids(body: &serde_json::Value, extension: &str) -> Vec<String> {
    entry_of(body, extension)
        .and_then(|entry| {
            entry["summary"]["visible_capability_ids"]
                .as_array()
                .cloned()
        })
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The lifecycle phase — `installed` is *not* `active`, and only an active
/// extension's tools reach the model.
fn phase_of(body: &serde_json::Value, extension: &str) -> Option<String> {
    entry_of(body, extension)?["phase"]
        .as_str()
        .map(str::to_string)
}

fn entry_of(body: &serde_json::Value, extension: &str) -> Option<serde_json::Value> {
    body["extensions"].as_array()?.iter().find_map(|entry| {
        (entry["summary"]["package_ref"]["id"].as_str() == Some(extension)).then(|| entry.clone())
    })
}

/// Every distinct `"status":"…"` the projection stream reported, in order seen.
fn run_statuses(stream: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut rest = stream;
    while let Some(at) = rest.find("\"status\":\"") {
        rest = &rest[at + 10..];
        if let Some(end) = rest.find('"') {
            let status = rest[..end].to_string();
            if !seen.contains(&status) {
                seen.push(status);
            }
        }
    }
    seen
}

/// Every file under `root`, as display paths. Small trees only.
fn walk(root: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path.display().to_string());
            }
        }
    }
    found
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}… ({} bytes total)", &text[..cut], text.len()),
        None => text.to_string(),
    }
}
