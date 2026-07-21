# Dashboard panels with no backend

`CLAUDE.md` Phase 2 asks for seven dashboard panels. Three of them cannot be
built against `ironclaw-reborn serve`, because the routes do not exist.

This is recorded rather than worked around. Reading IronClaw's libSQL database or
its audit-log JSONL sink directly would couple the widget to internals upstream
is free to change without notice, and adding routes to `ironclaw_webui_v2` would
be a large core edit to replay on every merge — against golden rule #1.

> **Phase 8c update (2026-07-21).** The "skills list" gap below was **wrong** and
> is now **built**. The 8c VERIFY drove the running gateway and the on-disk state
> and established a sharp line: user skills are **plain files** at
> `<reborn-home>/local-dev/skills/<name>/SKILL.md` — a directory the widget itself
> writes (7b reflection installs, 7c folder imports) — so listing/removing them is
> a filesystem read the widget already co-owns, no route and no libSQL coupling.
> **Memory and audit are the opposite**: both live in the gateway's *private*
> libSQL store (memory in `root_filesystem_entries WHERE path LIKE '/memory/%'`;
> audit in `root_filesystem_events WHERE path LIKE '/events/audit/%'`, as an
> unversioned `ironclaw_host_api::AuditEnvelope`). Surfacing either means reading
> the private DB directly — exactly the coupling this file refuses — so they stay
> unavailable, per the deliberate 8c decision to hold golden rule #1. **Run
> history** has no cross-thread enumeration at all; a conversation's own history is
> the 8a Chats timeline. The rows below are kept for the record; the table's
> "Skills list — no route" line is superseded.

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
| **Memory browser** | **no route** (private libSQL) | — |
| ~~Skills list~~ **built (8c)** | on-disk `local-dev/skills` | widget-owned files, no route |
| **Audit log viewer** | **no route** (private libSQL) | — |

## The three gaps in detail

**Memory browser.** No route reads or searches agent memory. Memory is reachable
only from inside the agent loop, through its `memory_*` tools.

**Skills list.** ~~`__list_skills__()` is an in-agent tool, not an HTTP
endpoint.~~ **Superseded (8c):** there is still no HTTP route, but there does not
need to be one — user-installed skills are plain `SKILL.md` files under
`local-dev/skills`, which the widget already reads and writes. `ic_widget::skills`
lists and removes them directly. Only user skills appear; the runtime's own
bundled skills live in a separate, runtime-managed `local-dev/system/skills` tree
that the widget deliberately leaves alone.

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
