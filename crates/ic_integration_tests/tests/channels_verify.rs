//! Phase 8f canary: `ironclaw-reborn serve` offers **no Telegram channel**, and
//! the Reborn Telegram adapter is **webhook-shaped**, not long-polling.
//!
//! ## The VERIFY that decided the sub-phase
//!
//! 8f opens on an assumption — *"the runtime compiles real
//! Slack/Telegram/WhatsApp adapters and serve exposes `GET
//! /channels/connectable`"* — and its step 4 says to verify that before
//! building. It does not hold, and the way it fails is what scopes the whole
//! sub-phase out:
//!
//! Telegram was the one channel in scope **because** it was believed to use
//! long-polling (`getUpdates`), which works behind NAT; Slack and WhatsApp were
//! ruled out for needing a publicly reachable endpoint on a desktop machine.
//! But in the Reborn stack Telegram is a **webhook** adapter
//! (`ironclaw_telegram_v2_adapter`, a self-described tracer-bullet), so it needs
//! exactly the property that ruled the other two out. `getUpdates` exists only
//! in the legacy v1 WASM channel wrapper (`src/channels/wasm/wrapper.rs`), which
//! is not compiled into the `ironclaw-reborn` binary.
//!
//! So 8f resolves to its spec's own permitted outcome — *"or the blocker is
//! documented"* — and this file is what stops that documentation going stale.
//! Full write-up in `docs/desktop/channels.md`.
//!
//! **A failure here is good news.** It means a channel became reachable: read
//! what appeared, and build the pairing flow 8f specifies (desktop shows a
//! one-time code, first inbound message must present it, one chat id
//! allowlisted, everything else dropped and logged — default deny).
#![cfg(feature = "webui-v2-beta")]

use ic_integration_tests::RebornServer;

/// Nothing in the running gateway offers a Telegram channel — through the
/// channels lane, the pairing lane, or 8b's connector lane.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_gateway_offers_no_telegram_channel() {
    let server = RebornServer::start().await;
    let client = reqwest::Client::new();

    // 1. The channels lane. The route exists and is honest: under our profile
    //    its facade is never wired, so it lists nothing. Its only possible entry
    //    upstream is a hardcoded Slack item behind the `slack-v2-host-beta`
    //    feature — there is no Telegram entry to enable.
    let response = client
        .get(format!(
            "{}/api/webchat/v2/channels/connectable",
            server.base_url
        ))
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("the connectable-channels request should reach the gateway");
    let status = response.status();
    let body = response.text().await.expect("body");
    assert_eq!(
        status.as_u16(),
        200,
        "the connectable-channels route should still answer: {body}"
    );
    let listed: serde_json::Value = serde_json::from_str(&body).expect("connectable-channels json");
    let channels = listed["channels"]
        .as_array()
        .expect("a `channels` array")
        .clone();
    assert!(
        channels.is_empty(),
        "a connectable channel appeared. Good news if it is Telegram — build \
         8f's pairing flow. Listing:\n{body}"
    );

    // 2. The pairing lane. `extensions/pairing/redeem` is part of the Slack
    //    host-beta mount, so it is not mounted here at all.
    let redeem = client
        .post(format!(
            "{}/api/webchat/v2/extensions/pairing/redeem",
            server.base_url
        ))
        .bearer_auth(&server.token)
        .json(&serde_json::json!({ "channel": "telegram", "code": "000000" }))
        .send()
        .await
        .expect("the pairing request should reach the gateway");
    assert_eq!(
        redeem.status().as_u16(),
        404,
        "a pairing route answered — a channel may be pairable now"
    );

    // 3. 8b's connector lane, the last plausible way in. A Telegram channel
    //    shipped as an installable package would surface here.
    for path in [
        "/api/webchat/v2/extensions",
        "/api/webchat/v2/extensions/registry",
    ] {
        let listing = client
            .get(format!("{}{path}", server.base_url))
            .bearer_auth(&server.token)
            .send()
            .await
            .expect("the extensions request should reach the gateway");
        let status = listing.status();
        let text = listing.text().await.unwrap_or_default();
        assert_eq!(status.as_u16(), 200, "GET {path} answered {status}");
        assert!(
            !text.to_ascii_lowercase().contains("telegram"),
            "a Telegram package appeared in {path} — 8f may be unblocked:\n{text}"
        );
    }

    // The control: the same client and token do reach a route that exists, so a
    // wrong base URL cannot make every check above pass vacuously.
    let control = client
        .get(format!("{}/api/webchat/v2/threads", server.base_url))
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("the control request should reach the gateway");
    assert!(
        control.status().is_success(),
        "the control route failed ({}) — the checks above prove nothing",
        control.status()
    );
}

/// The Reborn Telegram adapter is webhook-shaped by construction.
///
/// This is the structural half of the finding, and it is stronger than grepping
/// for `getUpdates`: `parse_telegram_update` refuses any payload whose
/// `ProtocolAuthEvidence` is not *host-verified*, and verified evidence cannot
/// be minted from outside the host (`host_verified` is `pub(crate)`;
/// `test_verified` needs the `test-support` feature). A long-polling client has
/// no inbound request to verify, so there is no shape in which it could hand
/// this adapter an acceptable payload. The webhook is not one transport option
/// among two — it is the only door.
#[test]
fn the_reborn_telegram_adapter_refuses_anything_it_did_not_receive_as_a_webhook() {
    use ironclaw_product_adapters::{AdapterInstallationId, ProtocolAuthEvidence};
    use ironclaw_telegram_v2_adapter::{
        GroupTriggerPolicy, PayloadParseError, parse_telegram_update,
    };

    let update = serde_json::json!({
        "update_id": 1,
        "message": {
            "message_id": 1,
            "date": 0,
            "chat": { "id": 42, "type": "private" },
            "from": { "id": 42, "is_bot": false, "first_name": "Someone" },
            "text": "hello",
        }
    })
    .to_string();
    let policy = GroupTriggerPolicy {
        bot_username: "ironclaw_bot".to_string(),
        bot_user_id: 7,
        recognized_commands: Vec::new(),
    };
    let installation = AdapterInstallationId::new("ic-8f-canary").expect("installation id");

    // The best a non-host caller can construct. There is no public constructor
    // for verified evidence — which is the point being pinned.
    let unverified =
        ProtocolAuthEvidence::failed(ironclaw_product_adapters::ProtocolAuthFailure::Missing);

    let error = parse_telegram_update(update.as_bytes(), &unverified, &installation, &policy)
        .expect_err("an unverified payload must be refused");
    assert_eq!(
        error,
        PayloadParseError::UnauthenticatedPayload,
        "the adapter accepted a payload with no verified webhook evidence — if \
         a long-poll ingress now exists, 8f may be unblocked"
    );

    // The other half of the claim: the payload itself is fine. Mint the evidence
    // a *host* would after verifying Telegram's shared-secret webhook header,
    // and the very same bytes parse. So the refusal above is about the missing
    // webhook, not about the message — there is no long-poll shape that gets in.
    let verified = ProtocolAuthEvidence::test_verified(
        ironclaw_product_adapters::AuthRequirement::SharedSecretHeader {
            header_name: "X-Telegram-Bot-Api-Secret-Token".to_string(),
        },
        "42",
    );
    parse_telegram_update(update.as_bytes(), &verified, &installation, &policy)
        .expect("the same payload parses once the webhook is host-verified");
}
