# Gateway API Notes — `ironclaw-reborn serve` (WebChat v2)

> Contract reference for the Tauri desktop client (`ic_widget::gateway_client`).
> Derived by reading source on branch `reborn-integration`. Every claim cites
> `path:line`. Where source was ambiguous or not found, see **Gaps / to confirm**.

The `serve` subcommand exposes the **WebChat v2** HTTP surface. It is compiled
in only under the `webui-v2-beta` Cargo feature and is host-composed from three
crates:

- `ironclaw_reborn_cli` — the `serve` subcommand (CLI flags, env resolution, listener bind).
- `ironclaw_reborn_composition` — `webui_v2_app`: middleware stack (auth, CORS, body/rate limits, WS origin) + product-auth/SSO route mounts.
- `ironclaw_webui_v2` — the route table + axum handlers (`webui_v2_router`).
- `ironclaw_reborn_webui_ingress` — the listener/serve loop (`serve_webui_v2`) and the `EnvBearerAuthenticator`.
- `ironclaw_product_workflow` / `ironclaw_product_adapters` — the request/response and SSE event wire structs.

All routes are mounted under the base path prefix **`/api/webchat/v2`**.

---

## 1. Startup / CLI

Subcommand: `ironclaw-reborn serve` — gated behind the `webui-v2-beta` feature.

- Enum variant + gate: `crates/ironclaw_reborn_cli/src/commands/mod.rs:14-25,52-56` (`#[cfg(feature = "webui-v2-beta")] Serve(serve::ServeCommand)`).
- CLI feature wiring: `crates/ironclaw_reborn_cli/Cargo.toml:39-45` (`webui-v2-beta = ["ironclaw_reborn_composition/webui-v2-beta", ...]`).
- `webui_v2` route crate is itself off by default (`crates/ironclaw_webui_v2/CLAUDE.md`).

### Flags (`ServeCommand`) — `crates/ironclaw_reborn_cli/src/commands/serve.rs:35-60`

| Flag | Type | Meaning |
|---|---|---|
| `--host <IP>` | `Option<IpAddr>` | Listener interface. Overrides `[webui].listen_host`. |
| `--port <u16>` | `Option<u16>` | Listener port. `0` = OS-assigned ephemeral (CLI only). Overrides `[webui].listen_port`. |
| `--confirm-host-access` | bool | Confirms trusted-laptop host FS access for the `local-dev-yolo` profile. |

### Default listener address/port — **confirmed `127.0.0.1:3000`**

- `const DEFAULT_SERVE_HOST: &str = "127.0.0.1";` — `serve.rs:30`
- `const DEFAULT_SERVE_PORT: u16 = 3000;` — `serve.rs:31`

Precedence is explicit **CLI flag > config file `[webui]` > compile-time default**:

- Host resolution: `serve.rs:185-193`.
- Port resolution: `serve.rs:200-215`. A `[webui].listen_port = 0` from *config* is rejected (`serve.rs:203-211`); `--port 0` from the CLI is allowed for test harnesses.
- `listen_addr = SocketAddr::new(host, port)` — `serve.rs:239`.
- Non-loopback bind emits a loud warning (`serve.rs:319-334`) and is **refused** when the runtime policy grants trusted-laptop host access (`serve.rs:490-504`).

The actual bound port is bindable via `RebornWebuiServeOptions.bound_addr_tx`, but the `serve` command passes `bound_addr_tx: None` (`serve.rs:463-468`) — so with `--port 0` the startup banner prints `:0` and the real port is not surfaced. **For the desktop app, always pass an explicit non-zero `--port` (dynamically chosen free port) so the client knows the URL.** (banner: `serve.rs:656-671`.)

### Storage profile

- The boot profile is selected by env var **`IRONCLAW_REBORN_PROFILE`** (`crates/ironclaw_reborn_config/src/profile.rs:6`), default **`local-dev`** (`profile.rs:9-14`, `#[default] LocalDev`). Valid values: `local-dev`, `local-dev-yolo`, `production`, `migration-dry-run` (`profile.rs:56-71`). It can also be set via config `[boot].profile` (`crates/ironclaw_reborn_cli/src/runtime/mod.rs:598-606`).
- `serve` builds the runtime with `RuntimeInputCaller::Serve` (`serve.rs:70-76`); this is the local-dev substrate path (see §7).

### Listener bind + graceful shutdown

- `serve_webui_v2(RebornWebuiServeOptions { addr, router, shutdown, bound_addr_tx })` binds a `TcpListener` and runs `axum::serve` with graceful shutdown — `crates/ironclaw_reborn_webui_ingress/src/lib.rs:98-140`.
- Ctrl-C triggers graceful shutdown via a oneshot channel — `serve.rs:452-461`.

---

## 2. Auth

**Scheme: `Authorization: Bearer <token>` on every route.** A `?token=<token>`
query-string shim is honored **only** on the SSE events route (`EventSource`
cannot set headers).

### Env vars (single-operator `EnvBearerAuthenticator`)

- `IRONCLAW_REBORN_WEBUI_TOKEN` — the bearer token value. `const DEFAULT_ENV_TOKEN_VAR` — `serve.rs:32`, read at `serve.rs:105-111`. **Required**; missing = startup error.
- `IRONCLAW_REBORN_WEBUI_USER_ID` — the `UserId` a valid token maps to. `const DEFAULT_ENV_USER_ID_VAR` — `serve.rs:33`, read at `serve.rs:112-120`. **Required**.
- The variable *names* can be overridden via config `[webui].env_token_var` / `[webui].env_user_id_var` (`serve.rs:97-103`). The values themselves must come from env (inline secrets are rejected at config parse).
- When SSO is enabled, the same token doubles as the session-signing HMAC key and must be ≥ 32 bytes (`serve.rs:272-279`).

### Token validation

- `EnvBearerAuthenticator::authenticate` compares the candidate against the configured token in **constant time** (`subtle::ConstantTimeEq`) — `crates/ironclaw_reborn_webui_ingress/src/lib.rs:180-201`. Empty configured token is rejected at construction (`lib.rs:164-169`).
- This authenticator returns `allows_operator_llm_config() == true` (`lib.rs:198-200`), so the operator-wide `/llm/*` routes are mounted for the standalone binary.

### Middleware that extracts + validates the bearer

`crates/ironclaw_reborn_composition/src/webui_serve.rs`:

- `authenticate_request` — resolves the token, calls `authenticator.authenticate`, returns **401** (`"Invalid or missing auth token"`) on failure, and on success inserts a `WebUiAuthenticatedCaller` extension for the handler — `webui_serve.rs:704-738`.
- `extract_bearer_token` — parses `Authorization: Bearer …` (case-insensitive, UTF-8-safe prefix check) — `webui_serve.rs:740-776`.
- `?token=` shim is accepted **only** on `GET /api/webchat/v2/threads/{id}/events` via `is_v2_sse_event_request` (method must be GET, path must match exactly) — `webui_serve.rs:772-789`; `query_token` percent-decodes and treats blank as absent — `webui_serve.rs:794-810`. On every non-SSE path (including SSE path with wrong method) the query token is ignored — tests at `webui_serve.rs:980-1018`.

### Caller identity injected into handlers

`WebUiAuthenticatedCaller` — `crates/ironclaw_product_workflow/src/webui_inbound.rs:24-62`:

```jsonc
{
  "tenant_id": "reborn-cli",       // trusted host-installation config, NOT the browser
  "user_id":   "<IRONCLAW_REBORN_WEBUI_USER_ID>",
  "agent_id":  "reborn-cli-agent", // optional; stamped from [identity].default_agent
  "project_id": null               // optional; from [identity].default_project
}
```

`tenant_id` / `agent_id` / `project_id` are host-trusted (config), never taken
from the request body (`webui_serve.rs:719-732`; tenant resolution `serve.rs:85-91`;
agent/project `serve.rs:156-168`).

### SSO / OAuth login surface (optional, public — no bearer)

When an SSO provider is configured, `ironclaw_reborn_webui_ingress::webui_v2_auth_router`
mounts (outside bearer auth, inside CORS/headers): `GET /auth/providers`,
`GET /auth/login/{provider}`, `GET /auth/callback/{provider}`,
`POST /auth/session/exchange`, `POST /auth/logout`
(`crates/ironclaw_reborn_webui_ingress/CLAUDE.md` → "WebChat v2 OAuth login surface").
For the desktop app with the single-operator env-bearer token, **SSO is not needed** — the token is generated at launch and stored in the Windows Credential Manager.

---

## 3. HTTP endpoints

Router registration: `crates/ironclaw_webui_v2/src/router.rs:96-189`.
Path patterns: `crates/ironclaw_webui_v2/src/descriptors.rs:44-73`.
Handlers: `crates/ironclaw_webui_v2/src/handlers.rs`.

All routes require `Authorization: Bearer`; CORS is `SameOriginOnly`; rate limits
are per-caller (mutation 60/60s, read 120/60s, stream 30/60s — `descriptors.rs:585-614`).
JSON bodies. Error body shape is uniform (see §8).

| Method | Path | Handler | Purpose | Request body | Response body |
|---|---|---|---|---|---|
| POST | `/api/webchat/v2/threads` | `create_thread` | Create a thread | `WebUiCreateThreadRequest` | `RebornCreateThreadResponse` |
| GET | `/api/webchat/v2/threads` | `list_threads` | List caller-scoped threads (`?limit=&cursor=`) | — | `RebornListThreadsResponse` |
| POST | `/api/webchat/v2/threads/{thread_id}/messages` | `send_message` | **Send a chat message / start a turn** | `WebUiSendMessageRequest` | `RebornSubmitTurnResponse` |
| GET | `/api/webchat/v2/threads/{thread_id}/timeline` | `get_timeline` | Paginated message history (`?limit=&cursor=`) | — | `RebornTimelineResponse` |
| GET | `/api/webchat/v2/threads/{thread_id}/events` | `stream_events` | **SSE event stream** (accepts `?token=` + `?after_cursor=`) | — | `text/event-stream` (see §4) |
| GET | `/api/webchat/v2/threads/{thread_id}/ws` | `stream_events_ws` | WebSocket event stream | — | WS text frames (see §5) |
| POST | `/api/webchat/v2/threads/{thread_id}/runs/{run_id}/cancel` | `cancel_run` | **Cancel/stop an in-progress run** | `WebUiCancelRunRequest` | `RebornCancelRunResponse` |
| POST | `/api/webchat/v2/threads/{thread_id}/runs/{run_id}/gates/{gate_ref}/resolve` | `resolve_gate` | Resolve a tool-approval / auth gate | `WebUiResolveGateRequest` | `RebornResolveGateResponse` |
| GET | `/api/webchat/v2/automations` | `list_automations` | List caller schedules (`?limit=`) | — | `RebornListAutomationsResponse` |
| GET | `/api/webchat/v2/channels/connectable` | `list_connectable_channels` | Connectable channel metadata | — | `RebornConnectableChannelListResponse` |
| GET | `/api/webchat/v2/extensions` | `list_extensions` | Installed extensions | — | `RebornExtensionListResponse` |
| GET | `/api/webchat/v2/extensions/registry` | `list_extension_registry` | Available extensions registry | — | `RebornExtensionRegistryResponse` |
| POST | `/api/webchat/v2/extensions/install` | `install_extension` | Install by `package_ref` | `{ "package_ref": LifecyclePackageRef }` | `RebornExtensionActionResponse` |
| POST | `/api/webchat/v2/extensions/{package_id}/activate` | `activate_extension` | Activate | — | `RebornExtensionActionResponse` |
| POST | `/api/webchat/v2/extensions/{package_id}/remove` | `remove_extension` | Remove | — | `RebornExtensionActionResponse` |
| GET | `/api/webchat/v2/extensions/{package_id}/setup` | `get_extension_setup` | Fetch setup projection | — | `RebornSetupExtensionResponse` |
| POST | `/api/webchat/v2/extensions/{package_id}/setup` | `setup_extension` | Drive setup | `WebUiSetupExtensionRequest` | `RebornSetupExtensionResponse` |
| GET | `/api/webchat/v2/llm/providers` | `get_llm_config` | LLM config snapshot | — | `LlmConfigSnapshot` |
| POST | `/api/webchat/v2/llm/providers` | `upsert_llm_provider` | Add/update provider | `UpsertLlmProviderRequest` | `LlmConfigSnapshot` |
| POST | `/api/webchat/v2/llm/providers/{provider_id}/delete` | `delete_llm_provider` | Delete provider | — | `LlmConfigSnapshot` |
| POST | `/api/webchat/v2/llm/active` | `set_active_llm` | Set active provider/model | `SetActiveLlmRequest` | `LlmConfigSnapshot` |
| POST | `/api/webchat/v2/llm/test-connection` | `test_llm_connection` | Probe provider connectivity | `LlmProbeRequest` | `LlmProbeResult` |
| POST | `/api/webchat/v2/llm/list-models` | `list_llm_models` | List provider models | `LlmProbeRequest` | `LlmModelsResult` |
| POST | `/api/webchat/v2/llm/nearai/login` | `start_nearai_login` | Begin NEAR AI login | `NearAiLoginRequest` | `NearAiLoginStart` |
| POST | `/api/webchat/v2/llm/nearai/wallet` | `complete_nearai_wallet_login` | Complete NEAR wallet login | `NearAiWalletLoginRequest` | `NearAiWalletLoginResult` |
| POST | `/api/webchat/v2/llm/codex/login` | `start_codex_login` | Begin Codex device login | — (no body) | `CodexLoginStart` |

The `/llm/*` routes are operator-wide and are only mounted when the authenticator
opts in (`WebUiV2RouteOptions.mount_llm_config_routes`, `router.rs:151-187`;
the env-bearer authenticator opts in). There is **no dedicated memory-browser,
skills, or audit-log HTTP endpoint** in this route set — see **Gaps**.

### Key request bodies (`crates/ironclaw_product_workflow/src/webui_inbound.rs`)

`WebUiCreateThreadRequest` (`webui_inbound.rs:65-71`):
```json
{ "client_action_id": "uuid-or-token", "requested_thread_id": "optional" }
```

`WebUiSendMessageRequest` (`webui_inbound.rs:74-82`) — path `thread_id` overrides body (`handlers.rs:62-71`):
```json
{ "client_action_id": "unique-per-send", "content": "user message text" }
```
`content` is required, ≤ 64 KiB, control chars rejected except `\n`/`\t` (`webui_inbound.rs:237-257,442-471`). `client_action_id` is the idempotency key (required, ≤ 256 bytes).

`WebUiCancelRunRequest` (`webui_inbound.rs:85-95`) — path `thread_id`/`run_id` override body (`handlers.rs:327-337`):
```json
{ "client_action_id": "unique", "reason": "user_requested" }
```
`reason` ∈ {`user_requested`(default), `superseded`, `timeout`, `operator_requested`, `policy`} (`webui_inbound.rs:376-394`).

`WebUiResolveGateRequest` (`webui_inbound.rs:134-150`) — path overrides `thread_id`/`run_id`/`gate_ref` (`handlers.rs:349-364`):
```json
{
  "client_action_id": "unique",
  "resolution": "approved",          // approved | denied | credential_provided | cancelled
  "always": false,                    // only for approved
  "credential_ref": "host-ref"        // only for credential_provided (never a raw secret)
}
```
Resolution parsing: `webui_inbound.rs:396-421`. Note: persistent (`always: true`) approvals are currently **refused** by the facade until an approval-policy port lands (see `ironclaw_product_workflow/CLAUDE.md`).

### Key response bodies (`crates/ironclaw_product_workflow/src/reborn_services/types.rs`)

`RebornCreateThreadResponse` (`types.rs:49-52`): `{ "thread": SessionThreadRecord }`.

`RebornSubmitTurnResponse` — tagged by `outcome` (`types.rs:54-81`):
```json
{
  "outcome": "submitted",            // submitted | deferred_busy | already_submitted
  "thread_id": "...",
  "accepted_message_ref": { ... },
  "turn_id": "...",
  "run_id": "<uuid>",                 // use this as {run_id} for cancel
  "status": "...",                    // TurnStatus
  "resolved_run_profile_id": "...",
  "resolved_run_profile_version": 1,
  "event_cursor": { ... }
}
```
`deferred_busy` carries `active_run_id` instead of `run_id`; `already_submitted` carries `run_id` (idempotent replay).

`RebornCancelRunResponse` (`types.rs:127-133`):
```json
{ "run_id": "<uuid>", "status": "...", "event_cursor": {...}, "already_terminal": false }
```

`RebornResolveGateResponse` — tagged by `outcome` (`types.rs:163-168`): `{ "outcome": "resumed", ... }` (`RebornResumeGateResponse`) or `{ "outcome": "cancelled", ... }` (`RebornCancelRunResponse`).

`RebornTimelineResponse` (`types.rs:103-113`): `{ "thread": SessionThreadRecord, "messages": [ThreadMessageRecord], "summary_artifacts": [...], "next_cursor": "opaque | null" }`. `limit` clamps to `[1,200]`.

`RebornListThreadsResponse` (`types.rs:223-228`): `{ "threads": [SessionThreadRecord], "next_cursor": "opaque | null" }`.

---

## 4. SSE — the streaming contract

**Path:** `GET /api/webchat/v2/threads/{thread_id}/events`
Handler: `crates/ironclaw_webui_v2/src/handlers.rs:146-310`.

### How a client subscribes

- Auth: `Authorization: Bearer <token>` **or** `?token=<token>` (browser `EventSource` uses the query form).
- Resume cursor precedence: `Last-Event-ID` request header (browser auto-sends on reconnect) **>** `?after_cursor=<cursor>` query param **>** projection origin (`handlers.rs:158-165`, doc `handlers.rs:121-145`). The SSE `id:` of each event is the JSON-serialized projection cursor, so `Last-Event-ID` round-trips verbatim.
- Keep-alive: an SSE comment every 15 s (`SSE_KEEPALIVE_INTERVAL`, `handlers.rs:113,167`).
- Poll cadence: the facade is currently drain-only; the handler drains then polls every 1 s (`SSE_POLL_INTERVAL`, `handlers.rs:109`).

### Resource caps (client must reconnect)

- **Per-`(tenant,user)` concurrency cap: 3 concurrent streams** (SSE + WS share this budget). Exceeding it returns **429** with `retryable: true` (`handlers.rs:153-156,173-182`; `DEFAULT_SSE_MAX_CONCURRENT_PER_CALLER`, `sse_capacity.rs`).
- **Max stream lifetime: 5 minutes** (`SSE_MAX_LIFETIME`); the server closes the stream and the client must reconnect with `Last-Event-ID` (`handlers.rs:218-227,281-286`).

### Event shape

Each SSE event: `event: <name>`, `id: <json-cursor>`, `data: <json>`.
Serialization: `handlers.rs:255-278`. The payload is a `WebChatV2EventFrame`
(`crates/ironclaw_webui_v2/src/schema.rs:16-49`):

```jsonc
// data field, per event:
{
  "cursor": "<projection cursor string>",   // flattened; also the SSE id:
  "type": "final_reply",                     // discriminator (see table)
  "reply": { ... }                           // variant-specific payload key
}
```

`WebChatV2Event` is `#[serde(tag = "type", rename_all = "snake_case")]`
(`schema.rs:51-111`). The SSE `event:` name equals the `type` value
(`schema.rs:93-110`). Variant → payload key → payload struct:

| `event:` name / `type` | Payload key | Payload struct | Meaning |
|---|---|---|---|
| `accepted` | `ack` | `RebornSubmitTurnResponse` | Turn admitted |
| `running` | `progress` | `ProgressUpdateView` | Generic progress (typing/reflecting) |
| `capability_progress` | `progress` | `ProgressUpdateView` (`kind == tool_running`) | Tool is running |
| `capability_activity` | `activity` | `CapabilityActivityView` | Tool/capability lifecycle metadata |
| `capability_display_preview` | `preview` | `CapabilityDisplayPreviewView` | Sanitized tool-output preview |
| `gate` | `prompt` | `GatePromptView` | **Tool-approval prompt** |
| `auth_required` | `prompt` | `AuthPromptView` | Auth/credential prompt |
| `final_reply` | `reply` | `FinalReplyView` | Assistant final answer |
| `cancelled` | `response` | `RebornCancelRunResponse` | Run cancelled |
| `failed` | `run_state` | `RebornGetRunStateResponse` | Run failed |
| `projection_snapshot` | `state` | `ProductProjectionState` | Full thread projection snapshot |
| `projection_update` | `state` | `ProductProjectionState` | Incremental projection update |
| `keep_alive` | — | (none) | Liveness |

Mapping from backend payload → event: `schema.rs:113-138`.

#### Payload structs (`crates/ironclaw_product_adapters/src/outbound.rs`)

`FinalReplyView` (`outbound.rs:185-190`):
```json
{ "turn_run_id": "<uuid>", "text": "assistant answer", "generated_at": "2026-07-08T..." }
```

`ProgressUpdateView` (`outbound.rs:192-205`): `{ "turn_run_id": "<uuid>", "kind": "typing|tool_running|reflecting", "generated_at": "..." }`.

`GatePromptView` — the tool-approval prompt (`outbound.rs:561-567`):
```json
{ "turn_run_id": "<uuid>", "gate_ref": "<opaque>", "headline": "...", "body": "..." }
```
The client resolves it via `POST .../runs/{run_id}/gates/{gate_ref}/resolve` (§3).

`AuthPromptView` (`outbound.rs:588-616`):
```json
{
  "turn_run_id": "<uuid>", "auth_request_ref": "<opaque>",
  "headline": "...", "body": "...",
  "challenge_kind": "oauth_url|manual_token|other",   // optional
  "provider": "google", "account_label": "...",        // optional
  "authorization_url": "https://...",                  // optional, OAuthUrl only
  "expires_at": "..."                                   // optional
}
```

`CapabilityActivityView` (`outbound.rs:216-229`, wire via custom `Serialize`): `invocation_id`, `turn_run_id?`, `thread_id?`, `capability_id`, `status` (`started|running|completed|failed|killed`), `provider?`, `runtime?`, `process_id?`, `output_bytes?`, `error_kind?`, `updated_at`. Metadata only — never raw args/results.

`CapabilityDisplayPreviewView` (`outbound.rs:361-379`): bounded display artifact — `title`, `subtitle?`, `input_summary?`, `output_summary?` (≤2 KiB each), `output_preview?` (≤16 KiB), `output_kind?`, `result_ref?`, `truncated`, etc.

`ProductProjectionState` (`outbound.rs:847-851`): `{ "thread_id": "...", "items": [ProductProjectionItem] }`. `ProductProjectionItem` is a snake_case tagged enum: `text`, `thinking`, `capability_activity`, `work_summary`, `run_status`, `gate`, `skill_activation` (`outbound.rs:618-663`).

#### SSE error event

On a facade error the stream emits one event with `event: error` then closes
(`handlers.rs:288-306`). Payload `SseErrorPayload` (`handlers.rs:196-201`):
```json
{ "error": "unavailable", "kind": "service_unavailable", "retryable": true }
```
(`error` = `RebornServicesErrorCode`, `kind` = `RebornServicesErrorKind` — see §8.)

---

## 5. WebSocket

**Path:** `GET /api/webchat/v2/threads/{thread_id}/ws`
Handler: `crates/ironclaw_webui_v2/src/handlers.rs:685-851`.

- **Bearer header required** — no `?token=` shim on WS (that shim is SSE-only). WS also enforces **same-origin** (`WebSocketOriginPolicy::SameOriginRequired`, `descriptors.rs:496-517`); the composition layer rejects mismatched `Origin` with 403 before upgrade (`ironclaw_reborn_composition/CLAUDE.md` middleware step 6).
- Same resume semantics as SSE: `Last-Event-ID` header or `?after_cursor=` (`handlers.rs:698-702`).
- **Framing:** each backend event is sent as **one JSON text frame** — but note it serializes the **raw `ProductOutboundEnvelope`** (`serde_json::to_string(&envelope)`, `handlers.rs:774-776`), **not** the `WebChatV2EventFrame` the SSE path renders. So WS frames carry adapter routing metadata (adapter_id, installation_id, target, delivery_attempt_id) alongside `projection_cursor` + `payload`; the SSE frame is the trimmed browser-facing shape. `ProductOutboundEnvelope`: `outbound.rs:938-946`; `payload` = `ProductOutboundPayload` (`outbound.rs:889-901`, snake_case tagged: `final_reply`, `progress`, `capability_activity`, `capability_display_preview`, `gate_prompt`, `auth_prompt`, `projection_snapshot`, `projection_update`, `keep_alive`).
- Shares the same 3-stream concurrency cap and 5-min lifetime as SSE; pre-upgrade `try_acquire` returns 429 when exhausted (`handlers.rs:693-696`).
- Server ignores inbound Text/Binary frames; a `Close` frame or socket error frees the slot immediately (`handlers.rs:753-761`).

**Recommendation for `ic_widget`: prefer SSE** — its `WebChatV2EventFrame` schema is the stable browser contract; the WS path emits the un-trimmed envelope and is a thinner transport.

---

## 6. Cancellation (the "Stop button")

**`POST /api/webchat/v2/threads/{thread_id}/runs/{run_id}/cancel`** — handler
`cancel_run` (`handlers.rs:327-337`), body `WebUiCancelRunRequest`.

Flow:
1. From the send response (`RebornSubmitTurnResponse`) capture `run_id` (or `active_run_id` on `deferred_busy`).
2. `POST .../runs/{run_id}/cancel` with `{ "client_action_id": "<unique>", "reason": "user_requested" }`.
3. Response `RebornCancelRunResponse` includes `already_terminal` (`types.rs:127-133`) — `true` if the run had already finished.
4. The SSE/WS stream then emits a `cancelled` event carrying the `RebornCancelRunResponse` (`schema.rs:78-80`).

`reason` defaults to `user_requested` when omitted (`webui_inbound.rs:379-381`).
Body cap 4 KiB (`descriptors.rs:189-201`).

---

## 7. libSQL profile (local-desktop, no Postgres)

**Use profile `local-dev` (the default) with the composition `libsql` feature.**

- Default profile is `LocalDev` (`crates/ironclaw_reborn_config/src/profile.rs:9-14`); selectable via `IRONCLAW_REBORN_PROFILE=local-dev` (`profile.rs:6`) or config `[boot].profile`.
- The `serve` command persists to a libSQL DB file on the local-dev root:
  `~/.<home>/local-dev/reborn-local-dev.db` — `serve.rs:284-288` (also the user/identity + trigger-access store, one durable source).
- The composition `libsql` Cargo feature pulls libSQL-backed event store, run-state, resources, triggers, filesystem, host-runtime (`crates/ironclaw_reborn_composition/Cargo.toml:59-67`; `libsql = { version = "0.6", ... }` at line 127). Per `ironclaw_reborn_composition/CLAUDE.md` → "How the standalone serve consumes this": *"In local-dev builds with `libsql` enabled, the log and runtime state stores sit behind the composed local-dev root filesystem (`reborn-local-dev.db` for durable records, `/projects` for workspace files)."*
- `local-dev-yolo` = same libSQL substrate but grants trusted-laptop host access (host shell/FS/network); requires `--confirm-host-access` (`serve.rs:57-59`) and **refuses non-loopback binds** (`serve.rs:490-504`). For the desktop app, `local-dev` is the safe default; `local-dev-yolo` only if unrestricted shell/host access is needed.
- `production` targets PostgreSQL and must fail closed without durable services (`profile.rs:18-20`); **not** for desktop.

The `RebornProfile` enum has exactly four variants — `local-dev`, `local-dev-yolo`,
`production`, `migration-dry-run` (`profile.rs:9-24`). The
`hosted-single-tenant-volume` profile named in the project `CLAUDE.md` does
**not** exist as a `RebornProfile` variant on this branch (see Gaps).

---

## 8. Error response shape (uniform across all HTTP routes)

Body: `WebUiV2HttpErrorBody` (`crates/ironclaw_webui_v2/src/error.rs:64-78`):
```json
{
  "error": "invalid_request",     // RebornServicesErrorCode (snake_case)
  "kind": "validation",           // RebornServicesErrorKind (snake_case)
  "retryable": false,
  "field": "content",             // optional; on validation errors
  "validation_code": "missing_field"  // optional; WebUiInboundValidationCode
}
```
Status code mapped from `RebornServicesError.status_code` (`error.rs:27-48`), from the fixed table `{400,401,403,404,409,429,500,503}`.

- `RebornServicesErrorCode` (`reborn_services/error.rs:5-16`): `invalid_request`, `unauthenticated`, `forbidden`, `not_found`, `conflict`, `rate_limited`, `unavailable`, `internal`.
- `RebornServicesErrorKind` (`error.rs:23-39`): `validation`, `duplicate`, `busy`, `participant_denied`, `blocked_approval`, `blocked_authentication`, `blocked_resource`, `replay_unavailable`, `timeline_unavailable`, `service_unavailable`, `not_found`, `conflict`, `internal`.
- `WebUiInboundValidationCode` (`webui_inbound.rs:305-314`): `missing_field`, `blank`, `too_long`, `invalid_control_character`, `invalid_id`, `invalid_value`.

Malformed path ids (e.g. bad `package_id`) 400 with `field` + `validation_code: "invalid_id"` before the facade is reached (`handlers.rs:657-669`).

---

## Minimal chat + SSE round-trip (the contract `ic_widget` builds against)

```
Base URL:  http://127.0.0.1:<port>          (default port 3000)
Auth:      Authorization: Bearer <IRONCLAW_REBORN_WEBUI_TOKEN>

1. POST /api/webchat/v2/threads
   { "client_action_id": "<uuid-1>" }
   → 200 { "thread": { "id": "<thread_id>", ... } }

2. GET  /api/webchat/v2/threads/<thread_id>/events?token=<token>
   (open SSE first, or in parallel; resume with Last-Event-ID on reconnect)

3. POST /api/webchat/v2/threads/<thread_id>/messages
   { "client_action_id": "<uuid-2>", "content": "hello" }
   → 200 RebornSubmitTurnResponse { "outcome":"submitted", "run_id":"<run>", ... }

4. Consume SSE:  accepted → running/capability_* → [gate → resolve_gate] → final_reply
   (each event: `event:<type>`, `id:<cursor>`, `data:<WebChatV2EventFrame>`)

5. Stop button:  POST /api/webchat/v2/threads/<thread_id>/runs/<run>/cancel
   { "client_action_id": "<uuid-3>", "reason": "user_requested" }
   → SSE emits `cancelled`.
```

---

## Gaps / to confirm

- **No memory / skills / audit-log HTTP endpoints** in the v2 route set. The task asked for these; the mounted routes (`descriptors.rs:44-73`, `router.rs`) cover threads, messages, timeline, events/ws, cancel, gate-resolve, automations, connectable-channels, extensions, and llm-config only. Memory browsing, skills listing, and audit-log viewing (Phase 2 dashboard panels in project `CLAUDE.md`) have **no v2 gateway route** on this branch — they may be v1-gateway-only, surfaced through a different crate, or not yet ported. **Confirm before building those dashboard panels.** `list_automations` is the closest thing to a "jobs" list; sessions = `list_threads`.
- **`hosted-single-tenant-volume` profile** named in the project `CLAUDE.md` does not exist as a `RebornProfile` variant (`profile.rs:9-24` has only local-dev / local-dev-yolo / production / migration-dry-run). The libSQL local-desktop path is `local-dev` + the composition `libsql` feature, not a distinct "hosted-single-tenant-volume" profile. Re-verify against the pinned upstream release tag the desktop fork targets.
- **WS emits the raw `ProductOutboundEnvelope`, not `WebChatV2EventFrame`** (`handlers.rs:774-776`) — an asymmetry with SSE. If the client needs one schema, use SSE. Confirm whether upstream intends to align WS onto the frame schema.
- **Bound-port discovery with `--port 0`:** `serve` passes `bound_addr_tx: None` (`serve.rs:467`), so an ephemeral port is not reported (banner prints `:0`). The desktop supervisor must pick a concrete free port itself and pass `--port <n>`; it cannot ask the server which port it bound.
- **`LlmConfigSnapshot` / `UpsertLlmProviderRequest` / `LlmProbeRequest` / `SetActiveLlmRequest` / `LlmModelsResult` field shapes** were not expanded here (they live in `ironclaw_product_workflow`, re-exported at `handlers.rs:24-38`). Read those structs when implementing the model-picker / provider-keys dashboard panels.
- **`SessionThreadRecord`, `ThreadMessageRecord`, `TurnStatus`, `EventCursor`, `AcceptedMessageRef`** field shapes (from `ironclaw_threads` / `ironclaw_turns`) were not expanded; expand when rendering the timeline and thread list.
- **Streaming is drain+poll (1 s), not push.** The facade has no true subscription API yet (`handlers.rs:139-141`); latency floor ≈ 1 s per poll. Upstream may add push later without changing the event schema.

---

## Verified end-to-end on Windows (Phase 0 smoke, port 38080)

Observed live against `ironclaw-reborn serve` (profile `local-dev`, provider `ollama`), token via `?token=` shim on the SSE route:

- `GET /api/webchat/v2/threads` unauth → **401**; with bearer → **200** `{"threads":[]}`.
- `POST /api/webchat/v2/threads` **requires** `client_action_id` (400 `validation/missing_field` otherwise). Success body: `{"thread":{"scope":{tenant_id,agent_id,owner_user_id},"thread_id","created_by_actor_id","title","metadata_json"}}`. `scope` is derived from `[identity].default_owner` (tenant/agent/owner all `reborn-cli`), **not** the request.
- `POST /api/webchat/v2/threads/{id}/messages` body `{client_action_id, content}` → **200** `{"outcome":"submitted","thread_id","accepted_message_ref":"msg:<uuid>","turn_id","run_id","status":"Queued","resolved_run_profile_id":"reborn-planned-default","resolved_run_profile_version":1,"event_cursor":1}`.
- **SSE actual event types** (this is the ground truth — supersedes the source-inferred `accepted/running/final_reply` names): the stream is a **projection model**:
  - `keep_alive` — `{cursor, type}` heartbeat, carries the current cursor.
  - `projection_snapshot` — full state: `{type, cursor, state:{thread_id, items:[{run_status:{run_id, status}}]}}`.
  - `projection_update` — incremental state deltas, same `state` shape; `run_status.status` transitions `queued`→`running`→(terminal).
  - Each frame's SSE `id:` is the JSON **cursor** (opaque `{runtime, turn:{event, scope}}`), so `Last-Event-ID` / `?after_cursor=` resumption works as documented.
- **Client contract for `ic_widget`:** render from `projection_snapshot`, then apply `projection_update` deltas keyed by `run_id`; treat `keep_alive` as liveness only. The Stop button posts to `.../runs/{run_id}/cancel`.
- **Boot preconditions confirmed:** active LLM provider must be resolvable at boot (key present, or keyless like `ollama`); `IRONCLAW_REBORN_WEBUI_USER_ID` must equal `[identity].default_owner`; required core-patch CP-1 (Windows dir-fsync) must be applied or `serve` cannot write bundled skills.

---

## Corrections — verified in Phase 2a against a running `serve`

Four claims above are wrong or incomplete. They are left in place so the
diff-against-source stays honest; these are the facts. Each is pinned by a test
in `crates/ic_widget/tests/gateway_roundtrip.rs`.

### C1 — Most `WebChatV2Event` variants are unreachable

The event table in §4 lists `accepted`, `running`, `final_reply`, `cancelled`,
and `failed`. **None of them are ever emitted over SSE.** `stream_events` drains
`RebornServices::stream_events`, which yields only projection payloads
(`reborn_services.rs:882-918`). `ProductOutboundPayload::FinalReply` is produced
solely on the push-delivery path for Telegram/Slack
(`outbound_delivery.rs:229`) and never reaches a browser.

A client sees exactly: `keep_alive`, `projection_snapshot`, `projection_update`,
`capability_activity`, `capability_display_preview`, `gate`, `auth_required`,
and a terminal `error`.

### C2 — The assistant's reply text is not on the event stream at all

`ProductProjectionItem::Text` has **no producer anywhere in the workspace**. Its
only construction sites are the wire `Deserialize` impl
(`outbound.rs:797`) and tests. The live projection builder
(`projection/live_progress.rs:147`) emits only `Thinking`, `CapabilityActivity`,
`WorkSummary`, and `SkillActivation`; `turn_events.rs:407` emits `RunStatus`.

Upstream's own SPA has a dead branch for it
(`ironclaw_webui_v2_static/static/js/pages/chat/lib/useChatEvents.js:354`).

**A client must:** watch `run_status` on the projection stream until the run is
terminal, then `GET /threads/{id}/timeline` and read the last
`ThreadMessageRecord` with `kind: "assistant"` and non-null `content`.

The complete `run_status` vocabulary (union of `turn_status_wire`,
`projection/turn_events.rs:602`, and `run_status_wire`, `projection.rs:1060`):

| Group | Values |
|---|---|
| In flight | `queued`, `running`, `cancel_requested` |
| Blocked on the user | `blocked_approval`, `blocked_auth`, `blocked_resource`, `blocked_dependent_run`, `recovery_required` |
| Terminal | `completed`, `cancelled`, `failed`, `killed` |

Terminal failures carry a sanitized `failure_summary`.

### C3 — `401` does not use the documented JSON error body

§8 says every route returns `WebUiV2HttpErrorBody`. The auth middleware does
not: `unauthorized()` returns a bare text body,
`(StatusCode::UNAUTHORIZED, "Invalid or missing auth token")`
(`webui_serve.rs:736-738`). A client must fall back to the status code and must
not require a parsed `code`/`kind` on `401`.

### C4 — A replayed `client_action_id` is only idempotent inside a window

§3 implies `already_submitted` is the general idempotent-replay answer. It is
only returned while the original message is still in `MessageStatus::Submitted`
(`reborn_services.rs:711-729`). Once the turn reaches a terminal state, the
identical replay falls through to `_ =>` and is refused with **409 conflict**
(`reborn_services.rs:741-747`). A replay onto a *different* thread is a 409 with
kind `duplicate` (`reborn_services.rs:703-710`).

Both outcomes mean "the message was accepted exactly once". A client retrying a
send whose response it never saw must treat `409` as success, not as an error to
show the user. `ic_widget::Error::is_duplicate_action()` exists for this.

---

## Corrections — verified in Phase 8b (connectors), 2026-07-15

### C5 — `serve` reads `IRONCLAW_REBORN_LOG`, **not** `RUST_LOG`

`ironclaw_reborn_cli/src/runtime/mod.rs:34` —
`EnvFilter::try_from_env("IRONCLAW_REBORN_LOG")`. Set `RUST_LOG` and the binary
emits nothing but its startup banner.

This is not cosmetic. An empty log next to a stalled run reads as a **wedged
runtime**, and on that basis the Phase 8b probe was one step away from filing a
false "WASM tool calls hang forever" bug upstream. The run was not wedged; it was
parked on an auth gate (C6), and the log would have said so. **Turn the right
variable on before concluding anything is broken.**

### C6 — a failing credential parks the run on an auth gate; it does not fail it

When a connector's tool call gets a `401` from its vendor, the runtime's answer is
**not** to fail the run. It raises an `auth_required` gate and parks the run, so
the user can supply a better credential and the turn can continue.

A client that waits only for `"status":"completed"` therefore **waits forever**,
and will render an endless spinner over a run that is quietly asking a question.

The `auth_required` SSE payload, verified live:

```jsonc
{ "type": "auth_required",
  "prompt": {
    "turn_run_id":      "55b7f1e5-…",
    "auth_request_ref": "gate:auth-auth-05aa915f…",   // ← this is the gate_ref
    "headline":         "Authentication required",
    "body":             "Authenticate to continue this run."
  } }
```

**The prompt does not name the provider.** `headline` is a generic
"Authentication required" — there is no `provider`, no `challenge_kind`, no
`setup_url`, despite §4's table suggesting those fields exist. A client must infer
*which* connector is asking from the `capability_activity` events on the same
stream: the last `capability_id` before the gate (`github.search_repositories`)
is the one that raised it. `ic_widget` does exactly this.

### C7 — three manual-token routes, and picking the wrong one costs an afternoon

Credentials for a connector do **not** go through `POST /extensions/{id}/setup`.
They go through the product-auth lane, and there are three routes whose names
invite confusion:

| Route | Body | When |
|---|---|---|
| `POST /api/reborn/product-auth/manual-token/setup` | `{provider, account_label}` | **Step 1 always.** Mints an interaction. Returns `{interaction_id, invocation_id, expires_at}`. |
| `POST /api/reborn/product-auth/manual-token/secret-submit` | `{interaction_id, invocation_id, token}` | **Step 2, from a settings page.** The standalone path. Returns `{credential_ref, status: "configured", continuation: {type: "setup_only"}}`. |
| `POST /api/reborn/product-auth/manual-token/submit` | `{provider, account_label, token, run_id, gate_ref}` | **Step 2, answering an auth gate** (C6). Requires the parked run's ids — which is *why* it demands them. |

Send a settings-page credential to `/submit` and it answers **422 `missing field
run_id`**. Send a gate answer to `/secret-submit` and the parked run is never
resumed. The `invocation_id` from step 1 must be carried back into step 2's scope
or the host cannot re-derive the pending interaction.

After `/submit` answers a gate, resolve it with
`POST /threads/{t}/runs/{r}/gates/{gate_ref}/resolve` and
`resolution: "credential_provided"` + the returned `credential_ref` — the raw
token never goes near gate resolution.

Whether a secret actually landed is readable from the setup projection:
`GET /extensions/{id}/setup` → `secrets: [{ name: "github_runtime_token",
provided: true|false, setup: { kind: "manual_token" } }]`.

### C8 — the extension list and registry hide their ids one level down

`GET /extensions/registry` → `{ "entries": [ { "package_ref": { "id": "github" },
"kind": "wasm_tool" | "mcp_server" | "first_party", "display_name", "version",
"installed" } ] }`. The id is under **`package_ref.id`**, not a top-level `id` —
reading the wrong key reports an empty registry when ten entries are present.

`GET /extensions` → `{ "extensions": [ { "package_ref": {"id"}, "display_name",
"kind", "description", "active": bool, "authenticated": bool, "needs_setup": bool,
"has_auth": bool, "tools": [ "github.get_repo", … ], "activation_status",
"version", "onboarding": { … } } ] }`. **Flat**, and the capability ids are the
**`tools`** array.

> ⚠️ **This paragraph said the opposite when it was first written** — that the
> entries were `{ phase, summary: { visible_capability_ids, … } }`. That is the
> shape of the *internal* `LifecycleInstalledExtensionSummary`, not of the wire;
> the route projects it into a flat `RebornExtensionInfo`
> (`ironclaw_product_workflow/src/reborn_services/types.rs:363`) on the way out.
> The widget's typed parser was written from this note, so `serde` rejected every
> response and the Connectors panel would have listed nothing while blaming the
> gateway. Nothing caught it because the probe that "verified" the shape *printed*
> its parse instead of asserting it, and hand-walked the JSON — so a wrong belief
> was held in two places that agreed with each other and never met the wire.
> `connector_verify::the_panels_parser_decodes_the_live_extensions_route` now
> decodes a live response **through the type the widget ships**, which is the only
> version of this check that can fail for the right reason.

`POST /extensions/install` returns the onboarding copy the UI should render:
`{ awaiting_token, onboarding_state: "setup_required", instructions,
onboarding: { credential_instructions, credential_next_step, setup_url } }`.
