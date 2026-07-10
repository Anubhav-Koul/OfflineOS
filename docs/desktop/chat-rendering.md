# How the widget renders a reply

The obvious design — "subscribe to the event stream, append tokens as they
arrive" — is not available. This document says why, and what the widget does
instead.

## The event stream carries no assistant text

`GET /api/webchat/v2/threads/{id}/events` never emits the assistant's answer.
Not as tokens, not as a final message. Verified in source and pinned by
`crates/ic_widget/tests/gateway_roundtrip.rs::the_event_stream_never_carries_the_assistant_text`.

Three separate facts combine:

1. **`final_reply` is not produced for browsers.** The SSE handler drains
   `RebornServices::stream_events` (`reborn_services.rs:882`), which yields only
   projection payloads. `ProductOutboundPayload::FinalReply` exists solely on the
   push-delivery path that sends answers to Telegram and Slack
   (`outbound_delivery.rs:229`).

2. **`ProductProjectionItem::Text` has no producer.** Its only construction sites
   in the entire workspace are the wire `Deserialize` impl (`outbound.rs:797`)
   and tests. The live projection builder (`projection/live_progress.rs:147`)
   emits `Thinking`, `CapabilityActivity`, `WorkSummary`, and `SkillActivation`;
   `turn_events.rs:407` emits `RunStatus`. Upstream's own SPA has a dead branch
   waiting for a `text` item that never comes
   (`ironclaw_webui_v2_static/.../useChatEvents.js:354`).

3. **There is no token stream anywhere.** The facade is drain-only, and the SSE
   handler polls it once a second (`SSE_POLL_INTERVAL`, `handlers.rs:109`). Even
   if text were projected, it would arrive in one-second steps.

So the stream is a *status* channel, not a *content* channel.

## What the widget does

```text
send_message ─────────────────────────────► run_id
                                              │
event stream:  run_status(queued)             │  status only
               run_status(running)            │
               capability_activity(shell)     │  "running shell"
               gate(…)                        │  approval prompt
               run_status(completed) ─────────┘
                                              │
                                              ▼
                                    GET /threads/{id}/timeline
                                              │
                                              ▼
                          last message with kind=assistant, content≠null
```

1. `POST /threads/{id}/messages` returns a `run_id`.
2. Watch `run_status` items on `projection_snapshot` / `projection_update` for
   that `run_id`.
3. When the phase is terminal, `GET /threads/{id}/timeline` and render the last
   `ThreadMessageRecord` with `kind: "assistant"` and non-null `content`.

Everything between steps 1 and 3 is status: a "thinking" line, the name of the
tool currently running, an approval prompt.

## Run phases

The vocabulary is closed upstream (`turn_status_wire`,
`projection/turn_events.rs:602`, and `run_status_wire`, `projection.rs:1060`):

| Group | Values | Widget behavior |
|---|---|---|
| In flight | `queued`, `running`, `cancel_requested` | show status, Stop enabled |
| Blocked on the user | `blocked_approval`, `blocked_auth`, `blocked_resource`, `blocked_dependent_run`, `recovery_required` | show the gate prompt |
| Terminal | `completed`, `cancelled`, `failed`, `killed` | fetch the timeline |

A terminal failure carries a sanitized `failure_summary`; render that rather
than inventing an error message.

`ic_widget::gateway_client::RunPhase` maps anything else to `Other`, which is
deliberately **not** treated as terminal. A status a newer gateway invents is far
more likely to be a new in-flight state than a new way of finishing, and guessing
"terminal" would make the widget stop listening while the agent kept working.

## Consequences you can feel

- **No streaming text.** A reply appears at once, when the run finishes. This is
  a property of the gateway, not of the widget.
- **Latency floor of ~1 second** on every status transition.
- **The Stop button races the answer.** `POST .../cancel` returns
  `already_terminal: true` when the reply landed first. That is a success, not a
  failure, and the widget then fetches the reply as usual.

## If upstream fixes this

`the_event_stream_never_carries_the_assistant_text` fails the moment a `text`
item appears on the stream. That is the signal to delete the timeline fetch and
render from the projection instead.
