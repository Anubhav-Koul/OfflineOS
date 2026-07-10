# Dashboard panels with no backend

`CLAUDE.md` Phase 2 asks for seven dashboard panels. Three of them cannot be
built against `ironclaw-reborn serve`, because the routes do not exist.

This is recorded rather than worked around. Reading IronClaw's libSQL database or
its audit-log JSONL sink directly would couple the widget to internals upstream
is free to change without notice, and adding routes to `ironclaw_webui_v2` would
be a large core edit to replay on every merge — against golden rule #1.

## What the serve API actually mounts

The complete route table is in `gateway-api-notes.md` §3. `ironclaw_gateway`
(the v1 gateway, `src/channels/web/`) is **not** compiled into or served by
`ironclaw-reborn serve` — it is a separate legacy binary — so there is no second
router to borrow from.

| Panel | Status | Route |
|---|---|---|
| Sessions | buildable | `GET /threads` |
| Tool-approval prompts | buildable | `gate` SSE event + `POST .../gates/{ref}/resolve` |
| Provider keys | buildable | `GET`/`POST /llm/providers`, `POST /llm/active` |
| Model picker, GGUF download, VRAM | buildable | local, via `ic_llama` |
| Jobs | partial | `GET /automations` — *scheduled* automations only |
| **Memory browser** | **no route** | — |
| **Skills list** | **no route** | — |
| **Audit log viewer** | **no route** | — |

## The three gaps in detail

**Memory browser.** No route reads or searches agent memory. Memory is reachable
only from inside the agent loop, through its `memory_*` tools.

**Skills list.** `__list_skills__()` is an in-agent tool, not an HTTP endpoint.
Nothing enumerates installed skills over the wire.

**Audit log viewer.** Audit records are written to scoped JSONL sinks and to
`DurableAuditLog` in the host runtime. No handler exposes them.

## "Jobs" is not what it sounds like

`GET /automations` returns `RebornListAutomationsResponse`
(`reborn_services/types.rs:236`): a list of **scheduled automations**, where
`RebornAutomationSource` currently has one variant, `Schedule { cron }`. Each row
carries the next scheduled run, the last run's timestamp, and a coarse
`ok`/`error` status.

There is no run history, no run ids, no logs, and no way to enumerate individual
executions. A dashboard can render "scheduled automations", not "jobs".

## What would unblock them

Each needs a route in `ironclaw_webui_v2` backed by a facade method on
`RebornServicesApi`, since handlers may consume nothing else (that crate's
boundary rules are enforced by an architecture test):

- `GET /api/webchat/v2/memory?query=` → a redacted memory projection
- `GET /api/webchat/v2/skills` → installed skills, name + description + state
- `GET /api/webchat/v2/audit?after=` → paginated, sanitized audit records
- `GET /api/webchat/v2/runs?thread_id=` → run history with ids and statuses

All four are plausible upstream contributions. Until one lands, the dashboard
lists these panels as unavailable, with the reason, rather than showing an empty
box that looks like a bug.
