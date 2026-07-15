//! Phase 8b item 5: how far can the Connectors panel take a user through **Gmail**,
//! and where exactly does it stop?
//!
//! GitHub works end to end because its credential is a token the user can paste
//! (`connector_verify.rs`). Every other interesting connector — Gmail, Calendar,
//! Drive, Notion — is **OAuth**, and OAuth is not a thing a desktop app can simply
//! do on the user's behalf. This test establishes, against the running gateway,
//! precisely which step is ours and which step is not, so the panel can be honest
//! instead of hopeful.
//!
//! # The answer
//!
//! 1. **Install works.** `POST /extensions/install` succeeds and returns
//!    `onboarding_state: "auth_required"` with the vendor's own copy — *"Gmail needs
//!    Google OAuth authorization before mail tools can run."* The panel renders that
//!    text rather than inventing its own.
//! 2. **The setup projection is fully populated**: six capabilities, one credential
//!    requirement (`gmail_account`, provider `google`, `setup.kind: "oauth"`, three
//!    scopes), and a fresh `invocation_id`. There is nothing missing on our side.
//! 3. **The OAuth start route refuses, and it is right to.** `POST
//!    /extensions/gmail/setup/oauth/start` answers **503 `backend_unavailable`**,
//!    because `serve` builds its Google OAuth config from the environment
//!    (`IRONCLAW_REBORN_GOOGLE_CLIENT_ID` + `IRONCLAW_REBORN_GOOGLE_OAUTH_REDIRECT_URI`,
//!    `ironclaw_reborn_cli/src/runtime/mod.rs:436`) and there is none. Without a
//!    client id there is no authorization URL to send the user to.
//!
//! # Why we stop here rather than paper over it
//!
//! A Google OAuth **client** can only be created by a human in the Google Cloud
//! console, and it is bound to an exact **redirect URI** that Google matches
//! byte-for-byte. Two consequences:
//!
//! - **We cannot ship one.** A client id embedded in a public desktop MSI is a
//!   public client; its consent screen would name *us* while acting on the user's
//!   mailbox, and verification for restricted Gmail scopes is a review process, not
//!   a config value. So this is the user's client or nobody's.
//! - **The redirect URI collides with our dynamic port.** The widget picks a free
//!   port for `serve` at every launch (two instances must coexist), but Google will
//!   only redirect to a URI registered ahead of time. So wiring this needs a
//!   *stable* loopback callback — a fixed port reserved for OAuth alone, or a
//!   127.0.0.1 listener the widget owns — before the environment variables above are
//!   worth setting.
//!
//! Neither is hard; both are decisions with consequences, and neither belongs in a
//! commit that claims Gmail "works". The panel therefore shows Gmail with the
//! vendor's own instructions and says plainly that it needs a Google OAuth client,
//! which is the truth.
//!
//! **Phase 8b.1 wired the rest** (see `connector_oauth_wired.rs`): the widget owns
//! a fixed-port loopback callback, sets the `IRONCLAW_REBORN_GOOGLE_*` environment
//! from a user-registered client, and `serve` then answers this same start route
//! with a real consent URL instead of the 503 pinned below. This test stays as the
//! *no-client* baseline — it must keep answering 503 when the environment is
//! absent, which is exactly what makes the wired test's 200 meaningful.
#![cfg(feature = "webui-v2-beta")]

use ic_integration_tests::{API_PREFIX, RebornServer};

const CONNECTOR: &str = "gmail";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gmail_installs_but_its_oauth_start_needs_a_google_client_we_do_not_have() {
    let server = RebornServer::start().await;
    let http = reqwest::Client::new();
    let base = format!("{}{API_PREFIX}", server.base_url);

    // ---- 1. Install: succeeds, and asks for authorization ---------------------
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
        "gmail should install and then ask to be authorized: {install}"
    );
    assert!(
        !install["onboarding"]["credential_instructions"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the panel renders the vendor's own words, so they must be there: {install}"
    );

    // ---- 2. Setup projection: everything on our side is present ---------------
    //
    // NOTE the shape: **this** route nests under `summary`
    // (`payload.extensions[].summary.visible_capability_ids`), while `GET /extensions`
    // is flat. Both are true, of different routes — and confusing the two is what
    // produced the Connectors panel's unusable parser (see C8 in
    // `docs/desktop/gateway-api-notes.md`).
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
    assert_eq!(secret["provider"], "google");
    assert_eq!(secret["setup"]["kind"], "oauth");
    assert_eq!(
        secret["provided"], false,
        "no google credential should exist yet: {setup}"
    );
    let invocation_id = secret["setup"]["invocation_id"]
        .as_str()
        .expect("the setup projection mints an invocation to authorize against")
        .to_string();

    // ---- 3. OAuth start: refused, because there is no Google client -----------
    let expires_at = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
    let start = http
        .post(format!("{base}/extensions/{CONNECTOR}/setup/oauth/start"))
        .bearer_auth(&server.token)
        .json(&serde_json::json!({
            "provider": "google",
            "account_label": "gmail google",
            "scopes": ["https://www.googleapis.com/auth/gmail.readonly"],
            "expires_at": expires_at,
            "invocation_id": invocation_id,
        }))
        .send()
        .await
        .expect("oauth start should answer");
    let status = start.status();
    let body = start.text().await.unwrap_or_default();
    eprintln!("POST oauth/start → {status}: {body}");

    assert_eq!(
        status.as_u16(),
        503,
        "the route should refuse for want of a Google OAuth client, not for any \
         other reason — a different status here means this analysis is stale and \
         gmail may now be reachable: {status}: {body}"
    );
    assert!(
        body.contains("backend_unavailable"),
        "the refusal should name the missing backend config: {body}"
    );
}
