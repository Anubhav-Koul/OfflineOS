//! Phase 8a.5 ⚠️ VERIFY: which provider protocols does the gateway accept via
//! `POST /llm/providers` under our profile?
//!
//! **Answer: none of them.** The route is mounted and it validates — an unknown
//! adapter is a clean `400` — but every *valid* protocol comes back `503
//! service_unavailable`. The operator LLM-config service is not composed under
//! `local-dev`, so there is nothing to persist a provider into.
//!
//! That settles the 8a.5 design rather than merely constraining it: the provider
//! panel cannot be built on the gateway's config routes at all, and everything —
//! the directory, the key store, the probe — is the widget's own. Which is what
//! we do: `LLM_BACKEND` + the provider's own key variable, handed to the gateway
//! at spawn (`apply_provider`), with keys in the Windows Credential Manager.
//!
//! This test is the tripwire: the day upstream composes the service, these 503s
//! turn into successes and a richer, gateway-native provider lane becomes
//! possible.
//!
//! Contract verified against upstream `a492857` (`reborn-integration`).
#![cfg(feature = "webui-v2-beta")]

use ic_integration_tests::{API_PREFIX, RebornServer};

/// Every distinct `protocol` value in `providers.json`, and what happens when we
/// ask the gateway to configure a provider that speaks it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_gateway_tells_us_which_protocols_it_will_configure() {
    let server = RebornServer::start().await;
    let client = reqwest::Client::new();
    let base = format!("{}{API_PREFIX}", server.base_url);

    // The 11 protocols in the shipped catalog.
    let protocols = [
        "open_ai_completions",
        "anthropic",
        "open_router",
        "gemini",
        "deep_seek",
        "ollama",
        "github_copilot",
        "nearai",
        "gemini_oauth",
        "openai_codex",
        "bedrock",
        // And one that does not exist, to prove the route validates at all.
        "not_a_real_protocol",
    ];

    let mut accepted = Vec::new();
    let mut rejected = Vec::new();

    for protocol in protocols {
        let response = client
            .post(format!("{base}/llm/providers"))
            .bearer_auth(&server.token)
            .json(&serde_json::json!({
                "id": format!("probe-{protocol}"),
                "adapter": protocol,
                "base_url": "https://example.invalid/v1",
                "default_model": "probe-model",
                "api_key": "probe-key",
                "set_active": false,
            }))
            .send()
            .await
            .expect("the gateway should answer");
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
        if status.is_success() {
            accepted.push(protocol);
        } else {
            let reason = body["validation_code"]
                .as_str()
                .or_else(|| body["kind"].as_str())
                .unwrap_or("?")
                .to_string();
            rejected.push((protocol, status.as_u16(), reason));
        }
    }

    eprintln!("probe: POST /llm/providers ACCEPTS  → {accepted:?}");
    eprintln!("probe: POST /llm/providers REJECTS  → {rejected:?}");

    // The route *parses* — an unknown adapter is a 400 validation error. So the
    // 503s below are the service being absent, not the request being malformed.
    assert!(
        rejected.iter().any(
            |(protocol, status, kind)| *protocol == "not_a_real_protocol"
                && *status == 400
                && kind == "validation"
        ),
        "an unknown adapter must be refused as invalid, or this probe proves nothing"
    );

    // THE FINDING: no protocol can be configured over HTTP under our profile.
    assert!(
        accepted.is_empty(),
        "a protocol became configurable ({accepted:?}) — upstream has composed the \
         LLM-config service, and the provider panel could now use the gateway's own \
         lane instead of the widget's. Revisit the 8a.5 design."
    );
    for (protocol, status, kind) in &rejected {
        if *protocol == "not_a_real_protocol" {
            continue;
        }
        assert_eq!(
            (*status, kind.as_str()),
            (503, "service_unavailable"),
            "{protocol} answered something other than \u{201c}the service is not here\u{201d}"
        );
    }
}
