# Channels (Phase 8f) — why Telegram is not connected

**Status: blocked upstream, documented per the sub-phase's own definition of
done** (*"Telegram connects behind its flag with pairing enforced — **or the
blocker is documented**"*). Verified against the pinned upstream commit
`a492857`. Canary: `ic_integration_tests/tests/channels_verify.rs`.

Nothing was built. No settings flag, no panel, no pairing UI — the Phase 8d rule
applies unchanged: *do not build UI over a mechanism that does not exist.*

## The one-line finding

8f put Telegram in scope, and Slack and WhatsApp out of it, on a single
technical premise: **Telegram long-polls (`getUpdates`), which works behind
NAT**, while the others need a publicly reachable endpoint that a desktop
machine does not have.

In the Reborn stack that premise is false. Telegram is a **webhook** adapter. It
needs exactly the property that ruled the other two out, so the spec's own
scoping logic — applied to the verified facts instead of the assumed ones — puts
Telegram out of scope too.

## What the VERIFY actually found

The sub-phase's step 4 says to verify *first* that channel adapters are composed
and activatable under the local profile at all. Four independent checks, all
negative:

### 1. `GET /channels/connectable` answers `{"channels":[]}`

The route exists and is honest — it lists nothing. Its facade
(`connectable_channels_facade`) is only wired when Slack host-beta mounts are
configured, and even then it is a `StaticConnectableChannelsProductFacade`
holding exactly one hardcoded entry: Slack
(`ironclaw_reborn_composition/src/slack_connectable_channel.rs`). **There is no
Telegram entry to enable.** `serve` without the `slack-v2-host-beta` feature
takes `build_webui_services(&runtime, None)`, which passes no facade at all.

### 2. The pairing route is not mounted

`POST /api/webchat/v2/extensions/pairing/redeem` → **404**. It ships as part of
the Slack host-beta mount, and composition's own docs say it "currently resolves
the supported Slack channel aliases to the Slack personal-binding pairing
service". A `channel: "telegram"` body has nothing to resolve to.

### 3. 8b's connector lane offers no Telegram package

`GET /extensions` and `GET /extensions/registry` both answer 200 with real
content, and neither mentions Telegram. There is no first-party Telegram
extension in `ironclaw_first_party_extensions`.

### 4. The Reborn Telegram adapter is webhook-shaped by construction

`crates/ironclaw_telegram_v2_adapter` exists and is real code — but it is a
self-described **tracer-bullet** ("the wasmtime component-model binary build
lands in a follow-up"), and it is referenced from production code **nowhere**:
the only mention outside its own crate is a test in
`ironclaw_product_workflow/tests/outbound_delivery_contract.rs`.

More decisive than that, it is webhook-only *by type*:

- `parse_telegram_update` takes "raw webhook update bytes" and refuses any
  payload whose `ProtocolAuthEvidence` is not host-verified
  (`PayloadParseError::UnauthenticatedPayload`, whose message reads *"host MUST
  verify the webhook before calling parse_telegram_update"*).
- Verified evidence **cannot be minted from outside the host**:
  `ProtocolAuthEvidence::host_verified` is `pub(crate)` behind a
  `host-auth-mint` feature, and `test_verified` needs `test-support`. The struct
  is sealed precisely so components cannot fabricate one.

A long-polling client has no inbound request to verify. So there is no shape in
which it could hand this adapter an acceptable payload — the webhook is not one
of two transport options, it is the only door.

### Where `getUpdates` actually lives

In the **legacy v1** WASM channel wrapper, `src/channels/wasm/wrapper.rs`, which
special-cases Telegram's `getUpdates` as inbound polling data. That is the v1
`ironclaw` binary's path. `ironclaw_gateway` v1 is not compiled into
`ironclaw-reborn`, so it is not reachable from the runtime this fork supervises.

## Why we are not routing around it

Three routes exist in principle. None is worth taking now.

**Expose a public webhook endpoint from the desktop.** This is what the adapter
wants and what the spec explicitly rejected for Slack and WhatsApp. It means a
tunnel or a hosted relay, an inbound HTTPS listener on a user's laptop, and a
public URL that must stay reachable — against an agent that holds files, a
browser, and connectors. The spec's security section calls unpaired channel
access "a stranger commanding an agent"; a public ingress makes that the default
posture rather than an edge case.

**Run the legacy v1 binary alongside Reborn for its Telegram channel.** A second
runtime, a second storage substrate, and a second security model to keep in
sync — against the fork's additive-crate policy and its single-supervised-child
architecture. The cost is structural, not incidental.

**Write our own long-poll bridge as a fork crate** (`ic_telegram`, in the shape
of `ic_voice` / `ic_browser_mcp`): poll `getUpdates` ourselves, enforce 8f's
pairing design in our own code, and inject messages through the gateway HTTP API
we already speak. This is genuinely feasible and is the only option that
delivers the sub-phase's actual intent ("companion on your phone") on a desktop
machine. It is also a **new ingress into the agent** with the highest security
weight in Phase 8, and it is a different design from the one 8f describes — the
spec is written around surfacing the runtime's channel, not building one. It
needs an explicit decision, not an assumption. **Recorded as the open option.**

## What would unblock the spec's own version

Any one of:

- A `getUpdates` long-poll ingress in the Reborn stack (host-side poller minting
  the verified evidence the adapter already demands), or
- A Telegram entry in the connectable-channels facade plus a pairing service
  behind `extensions/pairing/redeem`, the way Slack has one, or
- A Telegram channel shipped as an installable extension package that 8b's
  connector lane can already install.

The canary fails on the first of these to land, and says so in those words.

## The pairing design, kept on file

Not built, but it is the non-negotiable whenever a channel does arrive, and it
should not have to be re-derived:

- On connect, the **desktop** shows a one-time pairing code. The first inbound
  message must present it.
- Only that `chat_id` is allowlisted. Every message from any other `chat_id` is
  **dropped and logged, never answered** — silence, not an error reply, so the
  bot does not confirm its own existence to a stranger.
- Default deny. No pairing, no channel. Behind `settings.channels_enabled`,
  default OFF.
- Consent-sensitive actions initiated from the phone are **auto-denied** with a
  reply ("needs approval at the desktop") and surfaced on the desktop via the
  ambient popup. Runs must never hang waiting for an approval surface that is
  not there. Note that Phase 8d established there is no firing approval gate
  under our profile at all (`docs/desktop/approval-gates.md`), so today this
  rule would bind on our own consent surfaces rather than on runtime gates.
- Ambient guardrails (quiet hours, rate caps) apply to **outbound** channel
  messages too.

## The canary

`ic_integration_tests/tests/channels_verify.rs`, two halves:

- **Live** — drives a real `serve` and asserts the channels lane lists nothing,
  the pairing route 404s, and neither extensions listing mentions Telegram. Ends
  on a control request to a route that does exist, so a wrong base URL cannot
  make it pass vacuously.
- **Structural** — calls the real adapter's parser twice with the same bytes:
  refused without host-verified webhook evidence, accepted with it. That pins
  *why* it is webhook-only rather than grepping for a string.

A failure is good news: a channel became reachable. Read what appeared and build
the pairing flow above.
