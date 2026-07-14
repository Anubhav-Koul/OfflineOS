//! The one place this app talks to `ironclaw-reborn serve`.
//!
//! Reborn is beta and its wire shapes move. Every request, every response, and
//! every event is decoded here so that protocol drift is a compile error or a
//! single failing test in this module, not a mystery in the UI. Nothing outside
//! this module constructs a URL or a JSON body for the gateway.
//!
//! The contract is documented in `docs/desktop/gateway-api-notes.md`. Two of its
//! consequences shape this API:
//!
//! - **The assistant's reply text never arrives on the event stream.** Watch
//!   [`events::RunPhase`] for the run to go terminal, then call
//!   [`GatewayClient::timeline`]. See [`events`].
//! - **Only three streams may be open per caller.** [`sse::EventStream`] fails
//!   fast on the `429` rather than retrying.

pub mod events;
pub mod ids;
mod sse;

use serde::Deserialize;

pub use events::{
    AuthPrompt, CapabilityActivity, DisplayPreview, GatePrompt, GatewayEvent, ProjectionItem,
    ProjectionState, RunPhase, RunStatusItem, StreamError,
};
pub use ids::{ClientActionId, GateRef, RunId, ThreadId};
pub use sse::EventStream;

use crate::error::{Error, Result};

/// Every WebChat v2 route hangs off this prefix.
pub const API_PREFIX: &str = "/api/webchat/v2";

/// The gateway caps a message body at 64 KiB (`webui_inbound.rs:237`). Checking
/// here turns a `400` round-trip into an immediate, specific error.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// A typed client for one `ironclaw-reborn serve` instance.
#[derive(Clone)]
pub struct GatewayClient {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl std::fmt::Debug for GatewayClient {
    /// Redacts the bearer token, which would otherwise reach any log line that
    /// formats the client.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayClient")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl GatewayClient {
    /// Build a client for `base_url` (e.g. `http://127.0.0.1:3000`, no trailing
    /// slash) authenticating with `token`.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            // Deliberately no overall request timeout: `send_message` returns
            // as soon as the turn is admitted, but the gateway may be busy, and
            // the SSE stream must stay open for minutes.
            .build()
            .map_err(Error::ClientInit)?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        })
    }

    /// The base URL, for display.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{API_PREFIX}{path}", self.base_url)
    }

    /// Whether the gateway is up and our token is accepted.
    ///
    /// There is no dedicated health route, so this uses the cheapest
    /// authenticated read. A `401` here means the token is wrong, which is a
    /// different problem from the process being down.
    pub async fn health(&self) -> Result<()> {
        self.get::<serde_json::Value>("/threads").await?;
        Ok(())
    }

    /// Create a thread.
    pub async fn create_thread(&self) -> Result<ThreadId> {
        let body = serde_json::json!({ "client_action_id": ClientActionId::new() });
        let response: CreateThreadResponse = self.post("/threads", &body).await?;
        Ok(response.thread.thread_id)
    }

    /// List the caller's threads, newest page first.
    pub async fn list_threads(&self, limit: Option<u32>) -> Result<Vec<ThreadSummary>> {
        Ok(self.list_threads_page(limit, None).await?.threads)
    }

    /// One page of the caller's threads, with the cursor for the next.
    ///
    /// `next_cursor` is **absent** — not null — when there is no next page
    /// (verified against the running gateway; see
    /// `ic_integration_tests/tests/chat_control.rs`). Both spellings decode to
    /// `None` here, so a caller only has to handle "no more".
    pub async fn list_threads_page(
        &self,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<ThreadPage> {
        let mut query = Vec::new();
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor.to_string()));
        }
        let response: ListThreadsResponse = self.get_with_query("/threads", &query).await?;
        Ok(ThreadPage {
            threads: response.threads,
            next_cursor: response.next_cursor,
        })
    }

    /// List the caller's scheduled automations.
    ///
    /// This is the closest thing the serve API has to a "jobs" view. It returns
    /// **schedule entries, not run history** — no run-history route exists. See
    /// `docs/desktop/dashboard-gaps.md`.
    ///
    /// The response is a single capped page with no cursor, so there is nothing
    /// to paginate through.
    pub async fn list_automations(&self, limit: Option<u32>) -> Result<Vec<Automation>> {
        let path = match limit {
            Some(limit) => format!("/automations?limit={limit}"),
            None => "/automations".to_string(),
        };
        let response: ListAutomationsResponse = self.get(&path).await?;
        Ok(response.automations)
    }

    /// The connectors the gateway ships in its registry (Phase 8b).
    ///
    /// Ids live under **`package_ref.id`**, not a top-level `id` — see C8 in
    /// `gateway-api-notes.md`. Reading the wrong key reports an empty registry
    /// while ten entries sit there.
    pub async fn connector_registry(&self) -> Result<Vec<RegistryEntry>> {
        let response: RegistryResponse = self.get("/extensions/registry").await?;
        Ok(response.entries)
    }

    /// The extensions currently installed, with their phase and capabilities.
    pub async fn installed_extensions(&self) -> Result<Vec<InstalledExtension>> {
        let response: InstalledResponse = self.get("/extensions").await?;
        Ok(response.extensions)
    }

    /// Install an extension the gateway found in its catalogue.
    ///
    /// The catalogue is scanned **once, at gateway boot**, so this only works for
    /// a manifest that was already on disk when the process started. Installing
    /// an extension that is already installed is not an error.
    ///
    /// The response carries the **onboarding copy the UI should show**: what the
    /// credential is, where to get it, and what to do next. Rendering the vendor's
    /// own words beats inventing our own.
    pub async fn install_extension(&self, id: &str) -> Result<InstallOutcome> {
        let body = serde_json::json!({
            "package_ref": { "kind": "extension", "id": id },
        });
        self.post("/extensions/install", &body).await
    }

    /// What a connector still needs before it can run: which secrets, and whether
    /// each is already provided.
    pub async fn extension_setup(&self, id: &str) -> Result<ExtensionSetup> {
        self.get(&format!("/extensions/{id}/setup")).await
    }

    /// Remove an installed connector.
    pub async fn remove_extension(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post(&format!("/extensions/{id}/remove"), &serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// Activate an installed extension, publishing its capabilities to the agent.
    ///
    /// For a hosted-MCP provider this is also **when tool discovery runs**: the
    /// gateway calls the provider's `tools/list` and rebuilds its capabilities
    /// from the result. So the provider must be reachable *now*.
    ///
    /// It must be called on every launch, not just the first. A gateway restart
    /// republishes the *bundled manifest* — which for an MCP provider carries only
    /// a capability template, not the discovered tools — so without re-activating,
    /// the agent comes back up with no working tools.
    ///
    /// Returns whether the gateway reports the extension as activated. A discovery
    /// failure is **not** an error here: the gateway silently falls back to the
    /// bundled manifest and still answers `activated: true`. Confirm with
    /// [`GatewayClient::extension_capabilities`] rather than trusting this alone.
    pub async fn activate_extension(&self, id: &str) -> Result<bool> {
        let response: serde_json::Value = self
            .post(
                &format!("/extensions/{id}/activate"),
                &serde_json::json!({}),
            )
            .await?;
        Ok(response
            .get("activated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false))
    }

    /// The capability ids an installed extension currently exposes.
    ///
    /// This is the only honest check that discovery worked — see
    /// [`GatewayClient::activate_extension`] for why a successful activation does
    /// not imply the tools are there.
    pub async fn extension_capabilities(&self, id: &str) -> Result<Vec<String>> {
        let response: serde_json::Value = self.get("/extensions").await?;
        Ok(capability_ids_of(&response, id))
    }

    // ------------------------------------------------- connector credentials
    //
    // A connector's credential does NOT go through `/extensions/{id}/setup`. It
    // goes through the product-auth lane, which lives outside the WebChat prefix,
    // and there are three routes whose names invite exactly the wrong choice
    // (C7 in `gateway-api-notes.md`):
    //
    //   /manual-token/setup          — step 1, always. Mints an interaction.
    //   /manual-token/secret-submit  — step 2 from a settings page (standalone).
    //   /manual-token/submit         — step 2 answering a parked run's auth gate;
    //                                  requires run_id + gate_ref, and answers
    //                                  `422 missing field run_id` without them.
    //
    // Both step-2 routes are wrapped below so a caller picks by *intent*, not by
    // guessing at a URL.

    /// Step 1: ask the gateway for a credential interaction to submit against.
    async fn manual_token_setup(&self, provider: &str) -> Result<TokenChallenge> {
        let body = serde_json::json!({
            "provider": provider,
            "account_label": "ironclaw-desktop",
        });
        self.post_product_auth("/manual-token/setup", &body).await
    }

    /// Store a connector's credential from the settings page.
    ///
    /// This is the standalone path — no run is waiting on it. The raw token rides
    /// its own dedicated body and is never echoed back; only a `credential_ref`
    /// comes home.
    pub async fn store_connector_token(&self, provider: &str, token: &str) -> Result<()> {
        let challenge = self.manual_token_setup(provider).await?;
        let body = serde_json::json!({
            "interaction_id": challenge.interaction_id,
            "invocation_id": challenge.invocation_id,
            "token": token,
        });
        let _: serde_json::Value = self
            .post_product_auth("/manual-token/secret-submit", &body)
            .await?;
        Ok(())
    }

    /// Recover from a parked auth gate: store the new credential, then clear the
    /// run that is waiting on the old one. The caller re-asks the question.
    ///
    /// This is *not* the documented resume path, and that is deliberate. The
    /// documented one — `POST /manual-token/submit` against the gate (it takes
    /// `run_id` + `gate_ref` for exactly this), then `resolve_gate` with the
    /// `credential_ref` it hands back — could not be made to answer: a well-formed
    /// submit against a live gate returns a bare `400 invalid_request` naming no
    /// field, with nothing in the gateway's log to say what it disliked; and
    /// `/secret-submit` yields a `credential_ref` only on the *first* call for a
    /// provider, so a second call leaves nothing to resolve the gate with either.
    ///
    /// Rather than ship a button over a route we cannot make work, recovery is
    /// built from primitives that are *proven* (pinned by
    /// `ic_integration_tests/tests/connector_verify.rs`): the fresh credential is
    /// stored (the connector reports `provided: true` immediately), the parked run
    /// is cancelled, and the question is asked again. The user types their token
    /// once and gets their answer; the cost is one re-run of the turn.
    pub async fn recover_auth_gate(
        &self,
        provider: &str,
        token: &str,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<()> {
        self.store_connector_token(provider, token).await?;
        self.cancel_run(thread_id, run_id).await?;
        Ok(())
    }

    /// The product-auth routes sit at `/api/reborn/product-auth/…`, *outside* the
    /// WebChat v2 prefix every other call uses.
    async fn post_product_auth<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let url = format!("{}/api/reborn/product-auth{path}", self.base_url);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|source| Error::Http {
                url: url.clone(),
                source,
            })?;
        self.decode("POST", path, &url, response).await
    }

    /// Send a user message, starting a turn.
    ///
    /// `client_action_id` is the idempotency key: replaying the *same* id after
    /// a dropped connection yields [`SubmitOutcome::AlreadySubmitted`] rather
    /// than running the turn twice. Mint a fresh one per user action.
    pub async fn send_message(
        &self,
        thread_id: &ThreadId,
        content: &str,
        client_action_id: &ClientActionId,
    ) -> Result<SubmitOutcome> {
        validate_message(content)?;
        let body = serde_json::json!({
            "client_action_id": client_action_id,
            "content": content,
        });
        let response: SubmitTurnResponse = self
            .post(&format!("/threads/{thread_id}/messages"), &body)
            .await?;
        Ok(response.into())
    }

    /// Read the message history. **This is where the assistant's reply text
    /// lives** — it is never on the event stream.
    pub async fn timeline(&self, thread_id: &ThreadId, limit: Option<u32>) -> Result<Timeline> {
        let path = match limit {
            Some(limit) => format!("/threads/{thread_id}/timeline?limit={limit}"),
            None => format!("/threads/{thread_id}/timeline"),
        };
        self.get(&path).await
    }

    /// Cancel a run. This is the Stop button.
    ///
    /// `already_terminal` is `true` when the run had already finished, which is
    /// the common race when the user clicks Stop as the answer lands.
    pub async fn cancel_run(&self, thread_id: &ThreadId, run_id: &RunId) -> Result<CancelOutcome> {
        let body = serde_json::json!({
            "client_action_id": ClientActionId::new(),
            "reason": "user_requested",
        });
        self.post(&format!("/threads/{thread_id}/runs/{run_id}/cancel"), &body)
            .await
    }

    /// Answer a tool-approval or auth gate.
    pub async fn resolve_gate(
        &self,
        thread_id: &ThreadId,
        run_id: &RunId,
        gate_ref: &GateRef,
        resolution: GateResolution,
    ) -> Result<()> {
        let mut body = serde_json::json!({
            "client_action_id": ClientActionId::new(),
            "resolution": resolution.wire_value(),
        });
        if let GateResolution::CredentialProvided { credential_ref } = &resolution
            && let Some(object) = body.as_object_mut()
        {
            object.insert(
                "credential_ref".into(),
                serde_json::Value::String(credential_ref.clone()),
            );
        }
        // The response is a tagged `resumed`/`cancelled` envelope; the caller
        // learns the outcome from the event stream either way.
        let _: serde_json::Value = self
            .post(
                &format!("/threads/{thread_id}/runs/{run_id}/gates/{gate_ref}/resolve"),
                &body,
            )
            .await?;
        Ok(())
    }

    /// Open the event stream for a thread. It reconnects itself.
    pub fn events(&self, thread_id: ThreadId) -> EventStream {
        EventStream::new(self.client.clone(), &self.base_url, &self.token, thread_id)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.url(path);
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|source| Error::Http {
                url: url.clone(),
                source,
            })?;
        self.decode("GET", path, &url, response).await
    }

    /// A GET with query parameters, encoded by reqwest.
    ///
    /// A paging cursor is an **opaque gateway string** — it carries JSON
    /// structure — so it must never be pasted into a URL by hand.
    async fn get_with_query<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = self.url(path);
        let response = self
            .client
            .get(&url)
            .query(query)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|source| Error::Http {
                url: url.clone(),
                source,
            })?;
        self.decode("GET", path, &url, response).await
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let url = self.url(path);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|source| Error::Http {
                url: url.clone(),
                source,
            })?;
        self.decode("POST", path, &url, response).await
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        path: &str,
        url: &str,
        response: reqwest::Response,
    ) -> Result<T> {
        if !response.status().is_success() {
            return Err(gateway_error(method, url, response).await);
        }
        let body = response.bytes().await.map_err(|source| Error::Http {
            url: url.to_string(),
            source,
        })?;
        serde_json::from_slice(&body).map_err(|source| Error::Protocol {
            path: path.to_string(),
            reason: source.to_string(),
        })
    }
}

/// Turn a non-2xx response into an [`Error::Gateway`], preserving the gateway's
/// own sanitized `code`/`kind`/`retryable` taxonomy.
///
/// Falls back to the status alone when the body is not the documented shape —
/// a proxy or a panic can produce a non-JSON error page, and losing the status
/// to a parse failure would be worse than losing the detail.
pub(crate) async fn gateway_error(
    method: &'static str,
    url: &str,
    response: reqwest::Response,
) -> Error {
    let status = response.status().as_u16();
    let path = response.url().path().to_string();
    let body = response.text().await.unwrap_or_default(); // silent-ok: the status is the signal

    let error = match serde_json::from_str::<GatewayErrorBody>(&body) {
        Ok(parsed) => Error::Gateway {
            method,
            path,
            status,
            code: parsed.error,
            kind: parsed.kind,
            retryable: parsed.retryable,
        },
        Err(_) => Error::Gateway {
            method,
            path,
            status,
            code: "unknown".into(),
            kind: "unknown".into(),
            // 429 and 5xx are worth retrying even without a parsed body.
            retryable: status == 429 || (500..600).contains(&status),
        },
    };
    tracing::debug!(%url, %error, "gateway returned an error");
    error
}

/// `WebUiV2HttpErrorBody` (`crates/ironclaw_webui_v2/src/error.rs:64`).
#[derive(Debug, Deserialize)]
struct GatewayErrorBody {
    error: String,
    kind: String,
    #[serde(default)]
    retryable: bool,
}

/// Reject a message the gateway would reject, before spending a round trip.
fn validate_message(content: &str) -> Result<()> {
    let reject = |reason: &'static str| {
        Err(Error::InvalidId {
            kind: "message",
            value: content.chars().take(32).collect(),
            reason,
        })
    };
    if content.trim().is_empty() {
        return reject("must not be blank");
    }
    if content.len() > MAX_MESSAGE_BYTES {
        return reject("is longer than the gateway's 64 KiB limit");
    }
    // `webui_inbound.rs:442` rejects control characters other than newline and
    // tab.
    if content
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r')
    {
        return reject("contains a control character");
    }
    Ok(())
}

/// What `POST /threads/{id}/messages` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// The turn is running. Cancel it with this run id.
    Submitted {
        /// The new run.
        run_id: RunId,
    },
    /// Another run is already in flight on this thread; the message was
    /// accepted but not started.
    DeferredBusy {
        /// The run that is holding the thread.
        active_run_id: RunId,
    },
    /// This `client_action_id` was already used; the original run is returned.
    AlreadySubmitted {
        /// The run the first send created.
        run_id: RunId,
    },
}

impl SubmitOutcome {
    /// The run this send is associated with, whichever way it went. This is the
    /// id the Stop button cancels.
    pub fn run_id(&self) -> &RunId {
        match self {
            SubmitOutcome::Submitted { run_id }
            | SubmitOutcome::AlreadySubmitted { run_id }
            | SubmitOutcome::DeferredBusy {
                active_run_id: run_id,
            } => run_id,
        }
    }
}

/// `RebornSubmitTurnResponse` — internally tagged on `outcome`.
#[derive(Debug, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum SubmitTurnResponse {
    Submitted { run_id: RunId },
    DeferredBusy { active_run_id: RunId },
    AlreadySubmitted { run_id: RunId },
}

impl From<SubmitTurnResponse> for SubmitOutcome {
    fn from(value: SubmitTurnResponse) -> Self {
        match value {
            SubmitTurnResponse::Submitted { run_id } => SubmitOutcome::Submitted { run_id },
            SubmitTurnResponse::DeferredBusy { active_run_id } => {
                SubmitOutcome::DeferredBusy { active_run_id }
            }
            SubmitTurnResponse::AlreadySubmitted { run_id } => {
                SubmitOutcome::AlreadySubmitted { run_id }
            }
        }
    }
}

/// `RebornCancelRunResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CancelOutcome {
    /// The run that was cancelled.
    pub run_id: RunId,
    /// `true` when the run had already finished — the usual race when the user
    /// clicks Stop as the answer arrives.
    pub already_terminal: bool,
}

/// How the user answered a gate.
///
/// `always: true` is deliberately not offered: the facade refuses persistent
/// approvals until an approval-policy port lands
/// (`ironclaw_product_workflow/CLAUDE.md`), so a "don't ask again" checkbox
/// would be a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateResolution {
    /// Let the tool run, this once.
    Approved,
    /// Refuse the tool.
    Denied,
    /// Abandon the run.
    Cancelled,
    /// Supply a host-held credential reference. Never a raw secret.
    CredentialProvided {
        /// An opaque host reference, not the secret itself.
        credential_ref: String,
    },
}

impl GateResolution {
    fn wire_value(&self) -> &'static str {
        match self {
            GateResolution::Approved => "approved",
            GateResolution::Denied => "denied",
            GateResolution::Cancelled => "cancelled",
            GateResolution::CredentialProvided { .. } => "credential_provided",
        }
    }
}

#[derive(Debug, Deserialize)]
struct CreateThreadResponse {
    thread: ThreadSummary,
}

// ------------------------------------------------------------- connectors

#[derive(Debug, Deserialize)]
struct RegistryResponse {
    #[serde(default)]
    entries: Vec<RegistryEntry>,
}

/// One connector the gateway offers to install.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RegistryEntry {
    /// The id lives one level down, under `package_ref` — not at the top.
    #[serde(rename = "package_ref")]
    pub package: PackageRef,
    /// `wasm_tool`, `mcp_server`, or `first_party`.
    #[serde(default)]
    pub kind: Option<String>,
    /// The vendor's own name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// One line of what it does.
    #[serde(default)]
    pub description: Option<String>,
    /// The connector's version, as the registry states it.
    #[serde(default)]
    pub version: Option<String>,
    /// Whether it is already installed.
    #[serde(default)]
    pub installed: bool,
}

/// `{ "kind": "extension", "id": "github" }`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PackageRef {
    /// The connector id.
    pub id: String,
}

#[derive(Debug, Deserialize)]
struct InstalledResponse {
    #[serde(default)]
    extensions: Vec<InstalledExtension>,
}

/// An installed connector, exactly as `GET /extensions` sends it.
///
/// **Flat.** An earlier version of this type nested everything under a `summary`
/// block, copied from the *internal* `LifecycleInstalledExtensionSummary` rather
/// than from the wire — and `serde` rejected every response, so the panel listed
/// nothing and said the gateway was broken. The route's real shape is
/// `RebornExtensionInfo` (`ironclaw_product_workflow/src/reborn_services/types.rs`),
/// and it is pinned by `ic_integration_tests/tests/connector_verify.rs`, which now
/// decodes a live response through *this* type instead of a hand-rolled
/// `serde_json` walk that could agree with a wrong belief.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InstalledExtension {
    /// Which connector this is.
    #[serde(rename = "package_ref")]
    pub package: PackageRef,
    /// The vendor's own name for it.
    #[serde(default)]
    pub display_name: Option<String>,
    /// One line of what it does.
    #[serde(default)]
    pub description: Option<String>,
    /// `wasm_tool`, `mcp_server`, or `first_party`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Its tools reach the model. `installed` is *not* `active` — the difference
    /// is the whole point of the activate step.
    #[serde(default)]
    pub active: bool,
    /// Its credential requirements are satisfied.
    #[serde(default)]
    pub authenticated: bool,
    /// It is still waiting for something (a credential, an activation).
    #[serde(default)]
    pub needs_setup: bool,
    /// It wants a credential at all.
    #[serde(default)]
    pub has_auth: bool,
    /// The capability ids the agent can see. Empty on an `active` connector means
    /// the model got no tools from it, whatever the activate call claimed — the
    /// Phase 4 trap, and the reason this field is read rather than trusted.
    #[serde(default)]
    pub tools: Vec<String>,
    /// The vendor's onboarding copy.
    #[serde(default)]
    pub onboarding: Option<Onboarding>,
}

/// The vendor's own onboarding copy. Rendering this beats inventing our own.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Onboarding {
    /// What the credential is and how to make one.
    #[serde(default)]
    pub credential_instructions: Option<String>,
    /// What to do after saving it.
    #[serde(default)]
    pub credential_next_step: Option<String>,
    /// Where the user goes to mint the credential.
    #[serde(default)]
    pub setup_url: Option<String>,
    /// The headline instruction.
    #[serde(default)]
    pub instructions: Option<String>,
}

/// What `POST /extensions/install` answers with.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InstallOutcome {
    /// True when the connector cannot run until a credential is supplied.
    #[serde(default)]
    pub awaiting_token: bool,
    /// e.g. `setup_required`.
    #[serde(default)]
    pub onboarding_state: Option<String>,
    /// A one-line summary for the user.
    #[serde(default)]
    pub message: Option<String>,
    /// The vendor's onboarding copy.
    #[serde(default)]
    pub onboarding: Option<Onboarding>,
}

/// `GET /extensions/{id}/setup` — what the connector still needs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ExtensionSetup {
    /// One entry per secret, each saying whether it has been provided.
    #[serde(default)]
    pub secrets: Vec<SetupSecret>,
    /// The vendor's onboarding copy.
    #[serde(default)]
    pub onboarding: Option<Onboarding>,
}

/// One secret in the setup projection.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SetupSecret {
    /// The secret's name, e.g. `github_runtime_token`.
    pub name: String,
    /// **The** field that says whether the credential actually landed.
    #[serde(default)]
    pub provided: bool,
    /// The auth provider this secret belongs to, e.g. `github` — what the
    /// manual-token routes are keyed by.
    #[serde(default)]
    pub provider: Option<String>,
    /// Whether the connector can run without it.
    #[serde(default)]
    pub optional: bool,
    /// How the credential is obtained.
    #[serde(default)]
    pub setup: Option<SecretSetup>,
}

/// How a connector's credential is obtained — and the difference is not cosmetic.
///
/// A `manual_token` connector (GitHub) is finished by the user pasting a string,
/// which the panel can do. An `oauth` one (Gmail, Drive, Notion) needs a
/// browser round-trip against an OAuth **client** that only a human can register
/// with the vendor, and the gateway refuses to start the flow without one (503
/// `backend_unavailable`; pinned by
/// `ic_integration_tests/tests/connector_oauth.rs`). Offering a token box for that
/// connector would be inviting the user to paste something that can never work.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SecretSetup {
    /// `manual_token` or `oauth`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// `/manual-token/setup` — the interaction a token is submitted against.
#[derive(Debug, Deserialize)]
struct TokenChallenge {
    interaction_id: String,
    /// Must be carried back into step 2, or the host cannot re-derive the
    /// pending interaction.
    invocation_id: String,
}

#[derive(Debug, Deserialize)]
struct ListThreadsResponse {
    threads: Vec<ThreadSummary>,
    /// Omitted by the gateway when there is no next page — not sent as null.
    #[serde(default)]
    next_cursor: Option<String>,
}

/// One page of threads, and how to ask for the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPage {
    /// The rows.
    pub threads: Vec<ThreadSummary>,
    /// Pass back as `cursor`. `None` means this is the last page.
    pub next_cursor: Option<String>,
}

/// `SessionThreadRecord`, trimmed to what the widget renders.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ThreadSummary {
    /// The thread.
    pub thread_id: ThreadId,
    /// Its title, once the agent has named it.
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListAutomationsResponse {
    automations: Vec<Automation>,
}

/// `RebornAutomationInfo`, trimmed to what the dashboard renders.
///
/// Timestamps stay as RFC 3339 strings. They are display-only and the frontend
/// formats them with `Date`; decoding to a `chrono` type here would pull in a
/// dependency to turn a string back into a string. The `source` field is not
/// decoded — WebUI v2 only ever exposes user schedules.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Automation {
    /// Stable id for the schedule.
    pub automation_id: String,
    /// Its user-facing name.
    pub name: String,
    /// Where it is in its lifecycle.
    #[serde(deserialize_with = "deserialize_automation_state")]
    pub state: AutomationState,
    /// When it next fires, if it is scheduled to.
    #[serde(default)]
    pub next_run_at: Option<String>,
    /// When it last fired, if ever.
    #[serde(default)]
    pub last_run_at: Option<String>,
    /// How the last run ended. `None` before the first run.
    #[serde(default, deserialize_with = "deserialize_last_status")]
    pub last_status: Option<AutomationRunStatus>,
    /// Whether the schedule is currently armed.
    #[serde(default)]
    pub is_active: bool,
}

/// Browser-visible automation state.
///
/// The gateway already collapses states it does not expose into `unknown`, so
/// an unrecognized value carries no information worth preserving — unlike
/// [`RunPhase::Other`], which keeps the raw string because an unknown *run*
/// status is a real state the widget must not treat as terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationState {
    /// Armed and currently running.
    Active,
    /// Armed, waiting for its next trigger.
    Scheduled,
    /// Temporarily suspended by the user.
    Paused,
    /// Switched off.
    Disabled,
    /// Not armed and not scheduled.
    Inactive,
    /// Finished; it will not fire again.
    Completed,
    /// Not a state this build knows, or one the gateway declined to name.
    Unknown,
}

impl AutomationState {
    /// Parse a wire state. Anything unrecognized becomes [`Self::Unknown`].
    pub fn from_wire(state: &str) -> Self {
        match state {
            "active" => AutomationState::Active,
            "scheduled" => AutomationState::Scheduled,
            "paused" => AutomationState::Paused,
            "disabled" => AutomationState::Disabled,
            "inactive" => AutomationState::Inactive,
            "completed" => AutomationState::Completed,
            _ => AutomationState::Unknown,
        }
    }

    /// The snake_case wire spelling, for rendering.
    pub fn as_str(&self) -> &'static str {
        match self {
            AutomationState::Active => "active",
            AutomationState::Scheduled => "scheduled",
            AutomationState::Paused => "paused",
            AutomationState::Disabled => "disabled",
            AutomationState::Inactive => "inactive",
            AutomationState::Completed => "completed",
            AutomationState::Unknown => "unknown",
        }
    }
}

/// How an automation's last run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomationRunStatus {
    /// The run finished successfully.
    Ok,
    /// The run finished with an error.
    Error,
    /// A terminal status this build does not know.
    Other(String),
}

impl AutomationRunStatus {
    /// Parse a wire status.
    pub fn from_wire(status: &str) -> Self {
        match status {
            "ok" => AutomationRunStatus::Ok,
            "error" => AutomationRunStatus::Error,
            other => AutomationRunStatus::Other(other.to_string()),
        }
    }

    /// The wire spelling, for rendering.
    pub fn as_str(&self) -> &str {
        match self {
            AutomationRunStatus::Ok => "ok",
            AutomationRunStatus::Error => "error",
            AutomationRunStatus::Other(raw) => raw,
        }
    }
}

fn deserialize_automation_state<'de, D>(deserializer: D) -> Result<AutomationState, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(AutomationState::from_wire(&raw))
}

fn deserialize_last_status<'de, D>(deserializer: D) -> Result<Option<AutomationRunStatus>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.as_deref().map(AutomationRunStatus::from_wire))
}

/// `RebornTimelineResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Timeline {
    /// Oldest first.
    pub messages: Vec<Message>,
    /// Pass back as `?cursor=` to load the page before this one. `None` means
    /// the start of the thread.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

impl Timeline {
    /// The last finalized assistant message, which is the answer to the most
    /// recent turn.
    pub fn latest_assistant_reply(&self) -> Option<&Message> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.kind == MessageKind::Assistant && message.content.is_some())
    }
}

/// `ThreadMessageRecord`, trimmed. Render `content` — never the provider side
/// channel, which the gateway does not serialize anyway.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Message {
    /// Per-thread ordering.
    pub sequence: u64,
    /// Who said it.
    pub kind: MessageKind,
    /// `finalized` for a completed assistant answer.
    pub status: String,
    /// The text. `None` for messages whose body is a reference (tool results).
    #[serde(default)]
    pub content: Option<String>,
}

/// `MessageKind`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// The user.
    User,
    /// The agent.
    Assistant,
    /// System framing.
    System,
    /// A rollup of older messages.
    Summary,
    /// A kind this build does not know.
    #[serde(other)]
    Other,
}

/// The capability ids `GET /extensions` reports for one extension.
///
/// The gateway names this array **`tools`** and fills it with capability-id
/// strings (`"ic-browser.browser_click"`). An earlier version read only
/// `capabilities`, so it always returned an empty list — and the caller's "did the
/// tools actually reach the agent?" check therefore warned on *every* launch,
/// including the ones where the tools were there. A check that cannot report
/// success is worse than no check: it trains you to ignore it.
///
/// `capabilities` is still accepted as a fallback, and an entry may be either a
/// bare string or an object with an `id`, so a gateway that tightens either shape
/// keeps working.
fn capability_ids_of(response: &serde_json::Value, id: &str) -> Vec<String> {
    let Some(extensions) = response
        .get("extensions")
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };

    let Some(extension) = extensions.iter().find(|extension| {
        extension
            .get("id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                extension
                    .get("package_ref")
                    .and_then(|package| package.get("id"))
                    .and_then(serde_json::Value::as_str)
            })
            == Some(id)
    }) else {
        return Vec::new();
    };

    extension
        .get("tools")
        .or_else(|| extension.get("capabilities"))
        .and_then(serde_json::Value::as_array)
        .map(|capabilities| {
            capabilities
                .iter()
                .filter_map(|capability| {
                    capability
                        .as_str()
                        .or_else(|| capability.get("id").and_then(serde_json::Value::as_str))
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact body the running gateway returns. This is the regression: the
    /// six tools were live on the agent and the widget reported `found=0`.
    #[test]
    fn the_capability_list_is_read_from_the_tools_array_the_gateway_actually_sends() {
        let response = serde_json::json!({
            "extensions": [{
                "package_ref": { "kind": "extension", "id": "ic-browser" },
                "display_name": "Browser",
                "active": true,
                "tools": [
                    "ic-browser.browser_navigate",
                    "ic-browser.browser_get_text",
                    "ic-browser.browser_find",
                    "ic-browser.browser_fill",
                    "ic-browser.browser_click",
                    "ic-browser.browser_screenshot"
                ]
            }]
        });

        let capabilities = capability_ids_of(&response, "ic-browser");
        assert_eq!(capabilities.len(), 6, "{capabilities:?}");
        assert!(capabilities.contains(&"ic-browser.browser_click".to_string()));
    }

    #[test]
    fn an_object_shaped_capabilities_array_still_decodes() {
        let response = serde_json::json!({
            "extensions": [{
                "id": "ic-canvas",
                "capabilities": [{ "id": "ic-canvas.canvas_render" }]
            }]
        });
        assert_eq!(
            capability_ids_of(&response, "ic-canvas"),
            vec!["ic-canvas.canvas_render".to_string()]
        );
    }

    #[test]
    fn an_extension_the_gateway_does_not_list_has_no_capabilities() {
        let response = serde_json::json!({ "extensions": [] });
        assert!(capability_ids_of(&response, "ic-browser").is_empty());
    }

    #[test]
    fn an_automation_list_decodes_a_full_row_and_a_never_run_one() {
        let response: ListAutomationsResponse = serde_json::from_str(
            r#"{"automations":[
                {"automation_id":"a-1","name":"Nightly digest","source":{"kind":"schedule"},
                 "state":"scheduled","next_run_at":"2026-07-11T03:00:00Z",
                 "last_run_at":"2026-07-10T03:00:00Z","last_status":"ok",
                 "is_active":true,"created_at":"2026-07-01T00:00:00Z"},
                {"automation_id":"a-2","name":"Never ran","source":{"kind":"schedule"},
                 "state":"paused","is_active":false}
            ]}"#,
        )
        .expect("automations");

        let [first, second] = response.automations.as_slice() else {
            panic!("expected two automations");
        };

        assert_eq!(first.state, AutomationState::Scheduled);
        assert_eq!(first.last_status, Some(AutomationRunStatus::Ok));
        assert_eq!(first.next_run_at.as_deref(), Some("2026-07-11T03:00:00Z"));
        assert!(first.is_active);

        // A schedule that has never fired omits every timestamp and the status.
        // Those fields must not be required, or the whole page fails to decode.
        assert_eq!(second.state, AutomationState::Paused);
        assert_eq!(second.last_status, None);
        assert_eq!(second.next_run_at, None);
        assert!(!second.is_active);
    }

    #[test]
    fn an_unknown_automation_state_or_status_does_not_fail_the_page() {
        // One unrecognized row must not cost us the rows we *can* render.
        let response: ListAutomationsResponse = serde_json::from_str(
            r#"{"automations":[
                {"automation_id":"a-1","name":"From a newer gateway",
                 "state":"hibernating","last_status":"timed_out","is_active":true}
            ]}"#,
        )
        .expect("tolerant decode");

        let automation = &response.automations[0];
        assert_eq!(automation.state, AutomationState::Unknown);
        assert_eq!(
            automation.last_status,
            Some(AutomationRunStatus::Other("timed_out".into()))
        );
        // The raw status survives for display; the state does not, because the
        // gateway itself already collapsed anything it would not name.
        assert_eq!(
            automation.last_status.as_ref().unwrap().as_str(),
            "timed_out"
        );
        assert_eq!(automation.state.as_str(), "unknown");
    }

    #[test]
    fn a_submit_response_decodes_each_outcome_and_exposes_one_run_id() {
        let submitted: SubmitTurnResponse = serde_json::from_str(
            r#"{"outcome":"submitted","thread_id":"t","accepted_message_ref":"msg:1",
                "turn_id":"turn","run_id":"r-1","status":"Queued",
                "resolved_run_profile_id":"p","resolved_run_profile_version":1,"event_cursor":1}"#,
        )
        .expect("submitted");
        assert_eq!(SubmitOutcome::from(submitted).run_id().as_str(), "r-1");

        let busy: SubmitTurnResponse = serde_json::from_str(
            r#"{"outcome":"deferred_busy","thread_id":"t","accepted_message_ref":"msg:2",
                "active_run_id":"r-2","status":"Running","event_cursor":2}"#,
        )
        .expect("deferred_busy");
        // The Stop button must cancel the run that is actually holding the
        // thread, not a run that was never created.
        assert_eq!(SubmitOutcome::from(busy).run_id().as_str(), "r-2");

        let replayed: SubmitTurnResponse = serde_json::from_str(
            r#"{"outcome":"already_submitted","thread_id":"t","accepted_message_ref":"msg:1",
                "run_id":"r-1","status":"Running","event_cursor":3}"#,
        )
        .expect("already_submitted");
        assert_eq!(SubmitOutcome::from(replayed).run_id().as_str(), "r-1");
    }

    #[test]
    fn message_validation_matches_the_gateways_own_rules() {
        assert!(validate_message("hello").is_ok());
        // Newline and tab are explicitly allowed.
        assert!(validate_message("a\nb\tc").is_ok());
        assert!(validate_message("").is_err());
        assert!(validate_message("   \n ").is_err());
        assert!(validate_message("bell\u{7}").is_err());
        assert!(validate_message(&"x".repeat(MAX_MESSAGE_BYTES + 1)).is_err());
        validate_message(&"x".repeat(MAX_MESSAGE_BYTES)).expect("at the limit");
    }

    #[test]
    fn the_latest_assistant_reply_ignores_user_turns_and_empty_bodies() {
        let timeline = Timeline {
            next_cursor: None,
            messages: vec![
                Message {
                    sequence: 1,
                    kind: MessageKind::User,
                    status: "accepted".into(),
                    content: Some("what is 21 * 2?".into()),
                },
                Message {
                    sequence: 2,
                    kind: MessageKind::Assistant,
                    status: "finalized".into(),
                    content: Some("42".into()),
                },
                // A tool-result reference carries no renderable content.
                Message {
                    sequence: 3,
                    kind: MessageKind::Assistant,
                    status: "finalized".into(),
                    content: None,
                },
            ],
        };
        assert_eq!(
            timeline
                .latest_assistant_reply()
                .and_then(|m| m.content.as_deref()),
            Some("42")
        );
    }

    #[test]
    fn an_unknown_message_kind_does_not_fail_the_timeline() {
        let timeline: Timeline = serde_json::from_str(
            r#"{"messages":[{"sequence":1,"kind":"telepathy","status":"finalized","content":"x"}]}"#,
        )
        .expect("decode");
        assert_eq!(timeline.messages[0].kind, MessageKind::Other);
    }

    #[test]
    fn gate_resolutions_map_to_the_facades_vocabulary() {
        assert_eq!(GateResolution::Approved.wire_value(), "approved");
        assert_eq!(GateResolution::Denied.wire_value(), "denied");
        assert_eq!(GateResolution::Cancelled.wire_value(), "cancelled");
        assert_eq!(
            GateResolution::CredentialProvided {
                credential_ref: "host-ref".into()
            }
            .wire_value(),
            "credential_provided"
        );
    }

    #[test]
    fn the_bearer_token_never_appears_in_debug_output() {
        let client = GatewayClient::new("http://127.0.0.1:3000", "super-secret").expect("client");
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_does_not_double_up() {
        let client = GatewayClient::new("http://127.0.0.1:3000/", "t").expect("client");
        assert_eq!(
            client.url("/threads"),
            "http://127.0.0.1:3000/api/webchat/v2/threads"
        );
    }
}
