//! Phase 8b.1: the inverse of `connector_oauth.rs`.
//!
//! `connector_oauth.rs` pinned the *stopping point*: with no Google OAuth client
//! in `serve`'s environment, `POST /extensions/gmail/setup/oauth/start` answers
//! **503 `backend_unavailable`**, because there is no client to build an
//! authorization URL from. This test pins the *fix*: boot `serve` **with** a
//! well-formed Google OAuth client (a fake one — no real Google account is
//! touched) whose redirect URI is the widget's fixed-port loopback callback, and
//! the same start route now answers **200** with a Google consent URL that
//! carries our client id, our redirect URI verbatim, and a CSRF `state`.
//!
//! That is the whole widget-side contract of 8b.1: setting the three
//! `IRONCLAW_REBORN_GOOGLE_*` variables (which the widget derives from the
//! stored client and the fixed callback port) unblocks the flow, and `serve`
//! honors *our* redirect URI — the one the widget's listener is bound to. What
//! this test deliberately does **not** do is complete the flow: the callback is
//! a real token exchange against Google's servers, which needs a real client, a
//! real user consenting, and a real Gmail account. That last hop — "summarize my
//! latest email" against a real token — is the manual smoke gate, exactly as
//! GitHub's real-token read is for `connector_verify.rs`.
//!
//! If this test starts failing with a status other than 200, either the env keys
//! `serve` reads changed or the OAuth start contract drifted — both are things
//! the widget's wiring depends on and must be re-verified.
//!
//! Verified against upstream `reborn-integration` @ `a492857`.
#![cfg(feature = "webui-v2-beta")]

use ic_integration_tests::{API_PREFIX, MockReply, RebornServer};

const CONNECTOR: &str = "gmail";

/// The fake OAuth client the test boots `serve` with. Well-formed enough to pass
/// `OAuthClientConfig::new` (a valid client id and a valid loopback redirect
/// URI); never used against real Google servers.
const CLIENT_ID: &str = "test-client-8b1.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "test-secret-8b1";
/// The widget's fixed-port loopback callback — what `ic_widget::oauth_callback::
/// redirect_uri(51789)` produces, and what the user registers with Google.
const REDIRECT_URI: &str = "http://127.0.0.1:51789/api/reborn/product-auth/oauth/google/callback";

/// The LLM is never invoked in this test (install/setup/oauth-start are plain
/// HTTP routes), so any responder works.
fn responder() -> ic_integration_tests::MockResponder {
    std::sync::Arc::new(|_: &str| MockReply::Text("unused".to_string()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_configured_google_client_turns_the_503_into_a_consent_url_with_our_redirect() {
    let server = RebornServer::start_scripted(
        responder(),
        "unused".to_string(),
        vec![
            (
                "IRONCLAW_REBORN_GOOGLE_CLIENT_ID".to_string(),
                CLIENT_ID.to_string(),
            ),
            (
                "IRONCLAW_REBORN_GOOGLE_CLIENT_SECRET".to_string(),
                CLIENT_SECRET.to_string(),
            ),
            (
                "IRONCLAW_REBORN_GOOGLE_OAUTH_REDIRECT_URI".to_string(),
                REDIRECT_URI.to_string(),
            ),
        ],
    )
    .await;

    let http = reqwest::Client::new();
    let base = format!("{}{API_PREFIX}", server.base_url);

    // ---- 1. Install gmail -----------------------------------------------------
    let install: serde_json::Value = http
        .post(format!("{base}/extensions/install"))
        .bearer_auth(&server.token)
        .json(&serde_json::json!({
            "package_ref": { "kind": "extension", "id": CONNECTOR },
        }))
        .send()
        .await
        .expect("install should answer")
        .json()
        .await
        .expect("install json");
    assert_eq!(
        install["onboarding_state"], "auth_required",
        "gmail should install and ask to be authorized: {install}"
    );

    // ---- 2. Read the OAuth secret's invocation + scopes -----------------------
    let setup: serde_json::Value = http
        .get(format!("{base}/extensions/{CONNECTOR}/setup"))
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("setup should answer")
        .json()
        .await
        .expect("setup json");
    let secret = &setup["secrets"][0];
    assert_eq!(secret["setup"]["kind"], "oauth");
    let invocation_id = secret["setup"]["invocation_id"]
        .as_str()
        .expect("the setup projection mints an invocation")
        .to_string();
    let scopes: Vec<String> = secret["setup"]["scopes"]
        .as_array()
        .expect("oauth secret lists scopes")
        .iter()
        .map(|scope| scope.as_str().expect("scope is a string").to_string())
        .collect();
    assert!(!scopes.is_empty(), "gmail must declare at least one scope");

    // ---- 3. OAuth start now succeeds, and honors OUR redirect URI -------------
    let expires_at = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
    let start = http
        .post(format!("{base}/extensions/{CONNECTOR}/setup/oauth/start"))
        .bearer_auth(&server.token)
        .json(&serde_json::json!({
            "provider": "google",
            "account_label": "gmail google",
            "scopes": scopes,
            "expires_at": expires_at,
            "invocation_id": invocation_id,
        }))
        .send()
        .await
        .expect("oauth start should answer");
    let status = start.status();
    let body = start.text().await.unwrap_or_default();
    assert_eq!(
        status.as_u16(),
        200,
        "with a Google client configured, oauth/start must succeed (it was 503 \
         without one — see connector_oauth.rs). A different status means the env \
         keys serve reads, or the start contract, drifted: {status}: {body}"
    );

    let response: serde_json::Value = serde_json::from_str(&body).expect("oauth start json");
    let authorization_url = response["authorization_url"]
        .as_str()
        .expect("oauth start returns an authorization_url to open");

    // The consent URL must point the user at Google, carry our client, our
    // redirect URI verbatim (percent-encoded), and a CSRF state the widget's
    // listener binds against.
    assert!(
        authorization_url.contains("accounts.google.com"),
        "authorization URL should send the user to Google: {authorization_url}"
    );
    assert!(
        authorization_url.contains(CLIENT_ID),
        "authorization URL should carry our client id: {authorization_url}"
    );
    assert!(
        authorization_url.contains(
            "http%3A%2F%2F127.0.0.1%3A51789%2Fapi%2Freborn%2Fproduct-auth%2Foauth%2Fgoogle%2Fcallback"
        ),
        "authorization URL must carry OUR fixed-port redirect URI, percent-encoded \
         — this is the whole point of the widget-owned callback: {authorization_url}"
    );
    assert!(
        authorization_url.contains("state="),
        "authorization URL must carry a CSRF state the listener binds against: {authorization_url}"
    );
}
