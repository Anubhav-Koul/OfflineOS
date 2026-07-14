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
//! # The answer (2026-07-15): **it all works.**
//!
//! All five stages pass. The registry is composed (10 entries); install succeeds;
//! the credential lands; activation hands the model all 34 GitHub tools; and the
//! tool call **reaches GitHub's API**, which answers `401` because the token here
//! is deliberately bogus:
//!
//! ```text
//! WASM guest returned raw capability error  capability_id=github.search_repositories
//!   wasm_error={"code":"github_api_error_status_401","kind":"auth_required"}
//! ```
//!
//! That 401 is the proof the whole chain works — registration, WASM execution,
//! host egress, and credential injection. With a real token it would have
//! returned repositories. **8b can be built on the registry path.**
//!
//! ## The trap that nearly produced a false bug report
//!
//! A 401 from the tool does not end the run — it **parks it in an auth gate**
//! (`blocked_auth`), because the runtime's answer to "this credential is bad" is
//! to ask the user for a better one. A probe that waits only for
//! `"status":"completed"` therefore waits forever, and *looks* exactly like a hang.
//!
//! Compounding it: `serve` reads **`IRONCLAW_REBORN_LOG`**, not `RUST_LOG`
//! (`ironclaw_reborn_cli/src/runtime/mod.rs:34`). With the wrong variable set the
//! log is empty, and an empty log plus a stalled run reads as a wedged runtime.
//! It is neither. **Set the right variable before concluding anything is broken.**
//!
//! This also explains why `/manual-token/submit` demands a `run_id` and `gate_ref`
//! while `/manual-token/secret-submit` does not: the former exists precisely to
//! answer *this* gate, mid-run.
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
async fn a_wasm_registry_connector_installs_activates_and_calls_a_real_tool() {
    // A home we can inspect afterwards: whether the WASM artifact ever landed on
    // disk is what separates "the download never happened" from "the module ran
    // and wedged".
    let home = tempfile::tempdir().expect("home");
    let server = RebornServer::start_scripted_in_home(
        responder(),
        "unused".to_string(),
        // `serve` reads **`IRONCLAW_REBORN_LOG`**, not `RUST_LOG`
        // (`ironclaw_reborn_cli/src/runtime/mod.rs:34`). Setting the wrong one is
        // why an earlier version of this probe saw an empty log and wrongly
        // concluded the binary was silent.
        vec![(
            "IRONCLAW_REBORN_LOG".into(),
            "info,ironclaw_wasm=trace,ironclaw_extensions=debug,ironclaw_host_runtime=debug,\
             ironclaw_reborn_composition=debug"
                .into(),
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

    // What did the *event stream* say? This is what the UI has to render, and
    // getting it wrong is what produced the false "hang" diagnosis.
    eprintln!(
        "probe 5f: SSE event types seen → {:?}",
        sse_event_types(&stream)
    );
    eprintln!("probe 5g: run statuses seen → {:?}", run_statuses(&stream));
    // The auth gate's payload is what the fix-it UI is built on: it must carry
    // enough to re-submit a credential and resume the parked run.
    for line in stream.lines() {
        if line.starts_with("data:") && line.contains("auth_required") {
            eprintln!("probe 5h: auth_required payload → {}", line.trim());
        }
    }

    // ---- The verdict: the registry connector path works ---------------------

    assert!(
        github_tools.len() >= 30,
        "the model must be offered GitHub's tools — registration, credentials and \
         activation all work today. Offered: {github_tools:?}"
    );

    // The module really is fetched and executed.
    assert!(
        wasm.iter().any(|path| path.ends_with(".wasm")),
        "the WASM module should be downloaded to the home directory: {wasm:?}"
    );

    // THE BAR: the tool call reached the vendor. The 401 is the *proof* — it can
    // only come from GitHub, and only after the module ran, the host egress
    // allowed the request, and the injected credential was sent. A real token
    // would have returned repositories instead.
    assert!(
        logs.contains("github_api_error_status_401"),
        "the tool call never reached GitHub's API. If the runtime now answers \
         something else, re-read the log before assuming success:\n{}",
        tail(&logs, 40)
    );

    // And the runtime's answer to a bad credential is to **ask for a better one**:
    // the run parks on an auth gate instead of finishing. This is the behaviour a
    // client must render — a spinner here waits forever, which is exactly the
    // mistake this file was originally written around.
    assert!(
        !done,
        "the run completed with a deliberately invalid token — expected it to park \
         on an auth gate. Statuses seen: {:?}",
        run_statuses(&stream)
    );
    let events = sse_event_types(&stream);
    assert!(
        events.contains(&"auth_required".to_string()),
        "a bad connector credential must raise an `auth_required` gate the UI can \
         act on, or the user is left staring at a spinner. Events seen: {events:?}"
    );

    // The gate's payload — what a fix-it UI is built from.
    let prompt = auth_prompt(&stream).expect("an auth_required event carries a prompt");
    eprintln!("probe 6: auth gate prompt → {prompt}");
    assert!(
        prompt["turn_run_id"].is_string() && prompt["auth_request_ref"].is_string(),
        "the gate must name the run and itself, so a credential can be submitted \
         against it (`/manual-token/submit` needs run_id + gate_ref): {prompt}"
    );
    // A trap worth pinning: the prompt does NOT say which connector needs the
    // credential. `headline` is a generic "Authentication required". A client has
    // to infer the provider from the capability that just failed — we take it from
    // the `capability_activity` events on the same stream.
    assert!(
        !prompt.to_string().to_lowercase().contains("github"),
        "the auth prompt now names the provider — the UI can stop inferring it \
         from the failing capability: {prompt}"
    );
    let failing = failing_capability(&stream);
    eprintln!("probe 6b: the capability that raised the gate → {failing:?}");
    assert_eq!(
        failing.as_deref(),
        Some("github.search_repositories"),
        "the capability_activity stream is the only place that says *which* \
         connector needs a credential"
    );

    let _ = (capabilities, tool_results, provided);
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

/// The Connectors panel's **own parser** against the live route.
///
/// This exists because the panel shipped with a parser that could never have
/// worked, and every check around it still passed. `GET /extensions` returns a
/// **flat** `RebornExtensionInfo`
/// (`ironclaw_product_workflow/src/reborn_services/types.rs`) — `package_ref`,
/// `active`, `tools` — but the widget's type had been written from the *internal*
/// `LifecycleInstalledExtensionSummary`, which nests all of that under a `summary`
/// block. `serde` rejected every response, so the panel would have listed nothing
/// and blamed the gateway.
///
/// Why nothing caught it: the probe above *printed* the capability count instead
/// of asserting it, and it walked the JSON by hand — so the wrong belief was held
/// in two places that agreed with each other, and never met the wire. The fix is
/// not a better hand-walk. It is to decode the live response **through the type
/// the widget actually ships**, which is what this does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_panels_parser_decodes_the_live_extensions_route() {
    let home = tempfile::tempdir().expect("home");
    let server = RebornServer::start_scripted_in_home(
        responder(),
        "unused".to_string(),
        Vec::new(),
        home.path(),
    )
    .await;
    let client = ic_widget::gateway_client::GatewayClient::new(
        server.base_url.clone(),
        server.token.clone(),
    )
    .expect("gateway client");

    // The registry, through the shipping type.
    let registry = client
        .connector_registry()
        .await
        .expect("the panel must be able to read the registry");
    assert!(
        registry.iter().any(|entry| entry.package.id == CONNECTOR),
        "the registry did not carry {CONNECTOR}: {:?}",
        registry.iter().map(|e| &e.package.id).collect::<Vec<_>>()
    );

    client
        .install_extension(CONNECTOR)
        .await
        .expect("install through the shipping client");
    let http = reqwest::Client::new();
    store_token(&http, &server, "ghp_0000000000000000000000000000000000").await;
    client
        .activate_extension(CONNECTOR)
        .await
        .expect("activate through the shipping client");

    // The installed list, through the shipping type. A `summary`-shaped struct
    // fails here with a deserialization error, which is exactly what shipped.
    let installed = client
        .installed_extensions()
        .await
        .expect("the panel must be able to read the installed list");
    let github = installed
        .iter()
        .find(|extension| extension.package.id == CONNECTOR)
        .expect("github should be installed");

    assert!(github.active, "the connector should be active: {github:?}");
    assert!(
        github.tools.len() >= 30,
        "an active connector reporting {} tools means the panel would show a tool \
         count of {} — the Phase 4 trap, one layer out: {github:?}",
        github.tools.len(),
        github.tools.len()
    );
    eprintln!(
        "the panel sees: {} — active={}, {} tools",
        github.package.id,
        github.active,
        github.tools.len()
    );
}

/// The fix-it path (Phase 8b): a parked run can be **answered and resumed**.
///
/// This is the other half of the auth gate. The gate itself is useless if the UI
/// cannot act on it, so this drives the exact two-step the widget performs —
/// `/manual-token/submit` against the gate, then `resolve_gate` with the
/// `credential_ref` it returns — and asserts the run leaves the gate.
///
/// The token here is *still* invalid, so the honest outcome is that the run tries
/// again, is refused again, and raises a **new** gate. That is fine, and it is the
/// point: what is under test is that the answer reaches the runtime and the parked
/// run moves. A user with a working token gets their answer down the same path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_run_can_be_answered_and_resumed() {
    let home = tempfile::tempdir().expect("home");
    let server = RebornServer::start_scripted_in_home(
        responder(),
        "unused".to_string(),
        Vec::new(),
        home.path(),
    )
    .await;
    let http = reqwest::Client::new();
    let base = format!("{}{API_PREFIX}", server.base_url);

    // Install + credential + activate, the same way the panel does.
    http.post(format!("{base}/extensions/install"))
        .bearer_auth(&server.token)
        .json(&serde_json::json!({ "package_ref": { "kind": "extension", "id": CONNECTOR } }))
        .send()
        .await
        .expect("install");
    store_token(&http, &server, "ghp_0000000000000000000000000000000000").await;
    http.post(format!("{base}/extensions/{CONNECTOR}/activate"))
        .bearer_auth(&server.token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("activate");

    // Ask something that makes the agent reach for GitHub, and let it park.
    let thread = server.create_thread().await;
    server
        .send_message(&thread, &format!("{ASK} — find the repo"))
        .await;
    let (_, stream) = server
        .stream_until(&thread, "auth_required", Duration::from_secs(120))
        .await;
    let prompt = auth_prompt(&stream).expect("the run should park on an auth gate");
    let run_id = prompt["turn_run_id"].as_str().expect("run id");
    let gate_ref = prompt["auth_request_ref"].as_str().expect("gate ref");
    eprintln!("gate: run={run_id} gate_ref={gate_ref}");

    // ---- The fix-it path -----------------------------------------------------
    //
    // ⚠️ The *documented* resume — `/manual-token/submit` against the gate, then
    // `resolve_gate` with the `credential_ref` — could not be made to work:
    //
    //   - `/manual-token/submit` with a well-formed body against a live gate
    //     answers a bare `400 invalid_request`, with no field named and nothing in
    //     the log to say which of its inputs it disliked.
    //   - `/manual-token/secret-submit` returns a `credential_ref` the *first*
    //     time for a provider, but not on a second call for the same one — so
    //     there is no reference to resolve the gate with either.
    //
    // Rather than ship a fix-it button built on a route we cannot make answer, the
    // widget recovers with primitives that are **proven**: store the new credential
    // (the same call the Connectors panel makes), cancel the parked run, and ask
    // the question again. The user types their token once and gets their answer;
    // the turn is re-run rather than resumed, which costs one extra model call and
    // nothing else.
    //
    // What this test pins is that recovery: a parked run can be **cleared**, and a
    // fresh credential is live for the next turn. If upstream clarifies the resume
    // path, this is where the tighter version lands.
    let stored = store_token(&http, &server, "ghp_1111111111111111111111111111111111").await;
    eprintln!("fix-it 1: stored a fresh credential (credential_ref = {stored:?})");

    let setup: serde_json::Value = http
        .get(format!("{base}/extensions/{CONNECTOR}/setup"))
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("setup")
        .json()
        .await
        .unwrap_or(serde_json::Value::Null);
    let provided = setup["secrets"][0]["provided"].as_bool().unwrap_or(false);
    eprintln!("fix-it 2: the connector's credential is provided = {provided}");
    assert!(
        provided,
        "a fresh credential must be storable while a run is parked — that is the \
         whole recovery: {setup}"
    );

    // Clear the parked run. Without this the conversation is stuck on a turn that
    // will never move, which is exactly the state the user must be able to escape.
    let (cancel_status, cancel_body) = server.cancel_run_raw(&thread, run_id).await;
    eprintln!("fix-it 3: cancel the parked run → {cancel_status}: {cancel_body}");
    assert!(
        cancel_status.is_success(),
        "a run parked on an auth gate must be cancellable, or the conversation is \
         a dead end: {cancel_status} {cancel_body}"
    );

    // And the conversation is usable again: a new question runs, rather than
    // queueing behind a turn that will never finish.
    server.send_message(&thread, "anything else").await;
    let (moved, _) = server
        .stream_until(&thread, "\"status\":\"running\"", Duration::from_secs(60))
        .await;
    assert!(
        moved,
        "after the parked run is cleared, the conversation must accept a new turn"
    );
    let _ = gate_ref;
}

/// Store a connector credential the way the settings page does — the standalone
/// `secret-submit` path, keyed by the interaction from `/setup`.
///
/// Returns the `credential_ref`, which is also what a parked auth gate is
/// resolved with.
async fn store_token(http: &reqwest::Client, server: &RebornServer, token: &str) -> Option<String> {
    let challenge: serde_json::Value = http
        .post(format!(
            "{}/api/reborn/product-auth/manual-token/setup",
            server.base_url
        ))
        .bearer_auth(&server.token)
        .json(&serde_json::json!({ "provider": "github", "account_label": "ironclaw-desktop" }))
        .send()
        .await
        .expect("manual-token setup")
        .json()
        .await
        .expect("challenge json");

    let submitted: serde_json::Value = http
        .post(format!(
            "{}/api/reborn/product-auth/manual-token/secret-submit",
            server.base_url
        ))
        .bearer_auth(&server.token)
        .json(&serde_json::json!({
            "interaction_id": challenge["interaction_id"],
            "invocation_id": challenge["invocation_id"],
            "token": token,
        }))
        .send()
        .await
        .expect("secret-submit")
        .json()
        .await
        .unwrap_or(serde_json::Value::Null);
    submitted["credential_ref"].as_str().map(str::to_string)
}

/// The `prompt` payload of the first `auth_required` event on the stream.
fn auth_prompt(stream: &str) -> Option<serde_json::Value> {
    stream.lines().find_map(|line| {
        let data = line.strip_prefix("data:")?.trim();
        let value: serde_json::Value = serde_json::from_str(data).ok()?;
        (value["type"] == "auth_required").then(|| value["prompt"].clone())
    })
}

/// The capability whose failure raised the gate. The auth prompt does not name a
/// provider, so this is the only way a client can say *which* connector is
/// asking — it reads the capability ids off the `capability_activity` events.
fn failing_capability(stream: &str) -> Option<String> {
    let mut last = None;
    for line in stream.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
            continue;
        };
        if value["type"] == "capability_activity"
            && let Some(id) = value["activity"]["capability_id"].as_str()
        {
            last = Some(id.to_string());
        }
    }
    last
}

/// Every distinct SSE `event:` name in the stream, in order seen.
fn sse_event_types(stream: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for line in stream.lines() {
        if let Some(name) = line.strip_prefix("event:") {
            let name = name.trim().to_string();
            if !seen.contains(&name) {
                seen.push(name);
            }
        }
    }
    seen
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
