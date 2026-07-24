# Phase 8 spec — Surfacing the runtime

Moved verbatim from CLAUDE.md's "Build phases — execute in order" section during
the 2026-07-24 doc restructure (see `docs/desktop/PROGRESS.md` for what actually
shipped, sub-phase by sub-phase). This file is the original plan text, unedited.

### Phase 8 — Surfacing the runtime (dashboard shell, connectors, voice, gates, skill repos)

Context: an audit of the serve API route table (`gateway-api-notes.md` §3)
against what `ui/src/dashboard.tsx` / `api.ts` actually call found three tiers
of dormant capability: (1) full HTTP routes the UI never calls — extensions
manager, channels, `cancel_run`, gate resolve, `llm/test-connection`,
`llm/list-models`; (2) runtime features with no HTTP route at all — memory
browser, skills list, audit log, run history, automation create/edit/delete
(all documented in `dashboard-gaps.md`; these need upstream routes and are
BACKLOG, not sub-phases); (3) agent-side capabilities no surface acknowledges —
`builtin__spawn_subagent`, `memory_import`/`memory_seed`, `apply_patch`, the
secrets vault beyond LLM keys. Phase 8 wires the highest-value items into the
product. All work is additive; no core patch is expected — if one becomes
unavoidable, follow the `core-patch:` protocol. Build strictly in order
8a → 8g; each sub-phase ends with the full gate (fmt, clippy, test,
integration suite, manual smoke run) and a dated progress note in this file.

**Phase-wide rules (apply to every sub-phase):**

- **Contract surface.** Every serve route this phase newly consumes widens our
  exposure to upstream's beta churn. For each one, add an integration-test
  assertion (real gateway, not mocks) covering exactly the response fields the
  UI reads, and record the upstream commit the contract was verified against
  in the progress note. On the next upstream merge these tests are the alarm.
- **Restart resilience.** Several panels restart the gateway (providers,
  ambient toggle). All new UI state keyed by thread/run ids must survive that:
  refetch lists on SSE reconnect; treat 404 on a stale id as "refresh", never
  as an error dialog.
- **Feature flags.** Anything that changes agent reachability or capability
  (connectors, channels, repo import) ships behind a settings flag, default
  OFF, so a bad verify or regression can be switched off without a rebuild.
- **Verified fact to build on:** the widget's SSE/timeline parsers already
  tolerate unknown event kinds and non-terminal unknown run phases
  (`gateway_client/events.rs`) — new event types degrade gracefully. Bearer
  token handling lives in Windows Credential Manager via `keyring`
  (`secrets.rs`); connector credentials follow the same pattern, never
  settings.json.

**8a — Dashboard shell rework + chat management**

1. Restructure the dashboard into the conventional two-pane layout: a left
   sidebar (Chats, Connectors, Automations, Voice, Models & Providers,
   Settings — plus the existing unavailable-panel entries with their reasons)
   and the selected panel on the right. Keep the existing panels' logic; this
   is layout + navigation, not a rewrite. The widget/bubble remains the
   primary surface — the dashboard stays an on-demand window.
   ⚠️ REGRESSION PRECAUTION: `dashboard.tsx` is >2000 lines and several panels
   restart the gateway. Land the layout as a commit that changes NO panel
   logic, then re-verify every existing panel manually against the checklist:
   model download + cancel, model switch, provider add/test/apply (gateway
   restart + reload), onboarding card, ambient toggles, automations list,
   skill review card. Record the checklist pass in the progress note.
2. **Chats panel**: list past sessions via `GET /threads` (limit/cursor
   paging), open one to read its history via `GET /threads/{id}/timeline`
   (reuse the bubble's renderer; historical threads may contain item kinds the
   bubble never met — render unknowns as a neutral "event" row, never crash),
   continue a past conversation by pointing the bubble at that thread.
   ⚠️ Thread switching: define what happens if the user switches while a run
   is in flight — block the switch with a "finish or stop first" notice.
3. **Delete chat**: ⚠️ VERIFY FIRST whether the serve API exposes any thread
   delete/archive route (§3 of gateway-api-notes shows none — confirm against
   the running gateway). If none exists: implement *local archive* (hide the
   thread id in widget settings, never show it again) and label it "Hide" not
   "Delete"; record the honest behavior in the progress note. Do NOT reach
   into IronClaw's libSQL to delete rows — that couples us to internals
   (dashboard-gaps.md rationale) and violates the never-delete posture of the
   LLM-data invariant.
4. **Stop button**: wire `POST /threads/{thread_id}/runs/{run_id}/cancel` into
   both the bubble and the dashboard chat view for in-flight runs (run_id
   comes from the `accepted` ack event — it must be retained per run).
   ⚠️ VERIFY cancel semantics end-to-end: does cancelling the run actually
   abort the in-flight llama-server generation through SchemaProxy, or does
   the GPU keep generating to completion? Either way show a "stopping…" state,
   and handle 404/409 when the run went terminal before the cancel landed
   (silently refresh, no error dialog). Record the observed semantics.
5. **Providers panel extras**: add "Test connection" (`POST
   /llm/test-connection`) and populate model dropdowns via `POST
   /llm/list-models` instead of restarting the gateway to find out a key works.

**8b — Connectors panel (extensions manager over existing routes)**

1. Build the panel on the five existing routes: `GET /extensions`,
   `GET /extensions/registry`, `POST /extensions/install`,
   `POST /extensions/{id}/activate` / `remove`, and the two-way
   `GET`/`POST /extensions/{id}/setup` flow. The catalog ships in-repo
   (`registry/_bundles.json`, `registry/mcp-servers/*.json`).
2. ⚠️ VERIFY FIRST — and the bar is one REAL tool call, not a green install:
   pick the simplest registry connector and drive install → setup → the agent
   successfully *using* one of its tools, against the running gateway, before
   building any UI. This flushes out three separately-fatal unknowns:
   (a) whether external MCP registration is composed under our local profile
   at all (our browser/canvas MCPs are in-process first-party extensions,
   which sidesteps that path); (b) whether the gateway's outbound network
   policy allows the connector's endpoints — a blocked egress looks like
   "tools registered but every call fails"; (c) where credentials actually
   land (the setup projection should drive them into the secrets vault — if
   there is no working credential path, that's a blocker to report).
   **Fallback if (a) fails:** wrap the 2–3 priority connectors (email first)
   as in-process first-party extensions using the exact ic_browser_mcp /
   ic_canvas_mcp pattern — additive, already proven — and record that the
   registry path is upstream-blocked.
3. ⚠️ OAuth trap: Google (Gmail) requires a FIXED redirect URI; our gateway
   port is OS-assigned. Connector OAuth needs a dedicated fixed-port loopback
   listener owned by the widget solely for OAuth callbacks (or a device-code
   flow where the provider offers one). Do not let this surface as a
   mysterious redirect_uri_mismatch for users. Gmail also requires the user
   to create their own Google OAuth client — link instructions, don't pretend
   it's one click.
4. Curate, don't flood: small local models degrade when many tools are
   registered at once. Give each connector an enable/disable toggle (cheaper
   than uninstall), show a warning past N active connectors (pick N from
   testing, likely 3–5 with a 4B model), and record the observed degradation
   in the progress note.

**8c — Voice picker (and the honest path to custom voices)**

1. The TTS voice is hardcoded: `ic_voice/src/assets.rs` downloads exactly one
   Piper model (`en_US-amy-medium.onnx`). Turn this into a setting: a
   **curated, individually smoke-tested** voice list from
   `rhasspy/piper-voices`, downloaded on selection through the existing
   asset-download machinery (manifest pattern, progress UI), applied on next
   TTS session. Dashboard home: the Voice panel.
   ⚠️ Three traps to test per curated voice, not assume: (a) non-English
   voices need espeak-ng phoneme data our Piper bundle may not ship — verify
   before listing any; (b) voices differ in sample rate — confirm the
   resample stage (`resample.rs`) handles each and lip-sync amplitude scaling
   still looks right (it was tuned against amy-medium); (c) switching voices
   while a TTS session is speaking must tear down and restart the pipeline
   cleanly, not deadlock the audio device.
2. Voice *cloning* ("upload a sample") is explicitly OUT of this sub-phase:
   it requires a zero-shot engine (F5-TTS / GPT-SoVITS / XTTS-class), which is
   GPU-heavy (competes with llama for VRAM — the Phase 3 perf rule), slower to
   first audio, and license-encumbered (XTTS is non-commercial). Since Piper
   already runs as a subprocess behind the `tts.rs` stage, a second engine
   behind the same interface is architecturally clean — write a short design
   note (`docs/desktop/voice-cloning.md`) recording the engine candidates,
   VRAM budget, and license constraints, and stop there. Training a dedicated
   Piper voice (~30 min of speech + GPU training) is an offline pipeline, not
   an in-app feature — one paragraph in the same note.

**8d — Universal approval gates**

1. The wire protocol has a generic tool-approval mechanism: the `gate` SSE
   event + `POST /threads/{t}/runs/{r}/gates/{gate_ref}/resolve`. Our consent
   gate today is browser-sidecar-only.
2. ⚠️ VERIFY FIRST which capabilities actually emit `gate` events under the
   local profile — drive the real gateway and enumerate (the Phase 4 lesson:
   `default_permission` was never read; the LIKELY outcome is that gates fire
   only for extension auth/setup, not tool approvals). Specifically check
   `apply_patch` and `builtin__skill_install`, both consent-sensitive, and
   check whether `apply_patch` is even active in our profile.
3. If gates fire where we need them: render every gate as the red consent
   card in the bubble (the Phase 4 pattern), making it the universal approval
   surface, and reconcile with the browser sidecar gate so that flow doesn't
   double-prompt (one owner per decision: if a capability gates at the
   runtime, the sidecar defers). If gates do NOT fire for the capabilities
   that matter — the expected case — record exactly which are uncovered,
   keep the Phase 7b two-step approval pattern as the standing mechanism, and
   check whether the local capability policy
   (`local_dev_capability_policy.rs`) can deny consent-sensitive capabilities
   by default as a backstop. Do not build UI over a mechanism that doesn't
   trigger.

**8e — Skills from git repos + "Study this repo"**

1. Extend the Phase 7c import: accept a git URL → shallow clone → scan for
   `SKILL.md` folders → validate/normalize frontmatter → per-skill review
   card → install via the approved 7b/7c path (install-bundle limits apply).
   ⚠️ HARD CONSTRAINTS on the clone step:
   - Do NOT shell out to `git.exe` — users' machines may not have git. Use a
     Rust git library (`gix` preferred, `git2` acceptable); neither is
     currently in Cargo.toml, so this is a new vetted dependency (deny.toml
     applies).
   - Depth-1, no submodules, hard size cap (default 50 MB), hard timeout,
     and REJECT symlinks in the tree (symlink extraction can write outside
     the target directory).
   - Namespace imported skills by repo (repo-slug/skill-name) — two repos
     will eventually ship the same skill name.
2. ⚠️ REVIEW-CARD PRECAUTIONS (third-party skill text is untrusted input —
   prompt injection with persistence): render skill text as PLAIN TEXT in the
   review card, never as markdown/HTML (rendering can visually hide
   instructions); flag zero-width/bidi control characters; on re-sync show a
   DIFF against the installed version and re-review only what changed. Never
   bulk-silent-install.
3. "Study this repo": a flow where the agent clones and analyzes a repo, then
   produces (a) a draft SKILL.md distilling its *procedures* (goes through the
   standard review/install gate) and, where applicable, (b) a proposal to
   register the repo's tool surface (an MCP server or CLI it ships) as a
   connector via 8b. ⚠️ Context-budget reality: a small local model cannot
   read a repo. The flow must cap files read (README, docs, manifests first),
   summarize incrementally, and surface the 7b model-quality hint — recommend
   running it on the strongest configured model. Scope honestly: the app
   cannot rewrite its own compiled Rust, and agent-authored WASM extensions
   are research territory — both are explicitly out. This flow captures the
   practical 70% (knowledge + tool use) at a fraction of the risk.
   NOTE: 8e(b) depends on 8b's verified install path; if 8b fell back to
   in-process wrappers, (b) downgrades to "tell the user what could be
   wrapped", not an automatic registration.

**8f — Channels (companion on your phone) — feature-flagged, security-first**

1. The runtime compiles real Slack/Telegram/WhatsApp adapters and serve
   exposes `GET /channels/connectable`. Surface Telegram ONLY in this phase —
   ⚠️ VERIFY it uses long-polling (getUpdates), which works behind NAT; Slack
   and WhatsApp need publicly reachable endpoints and are OUT of scope for a
   desktop machine.
2. ⚠️ SECURITY — this is the one non-negotiable in the phase: anyone on
   Telegram can message any bot. An unpaired channel is a stranger commanding
   an agent that holds your files, browser, and connectors. Required design:
   on connect, the DESKTOP shows a one-time pairing code; the first Telegram
   message must present it; only that chat_id is allowlisted; every message
   from any other chat_id is dropped and logged, never answered. Default
   deny. No pairing, no channel.
3. Consent-sensitive actions triggered from the phone cannot be approved on
   the phone: define the policy explicitly — gated/consent actions initiated
   via a channel are auto-denied with a reply ("needs approval at the
   desktop"), and the desktop surfaces the pending request via the ambient
   popup. Runs must never hang waiting for an approval surface that isn't
   there.
4. ⚠️ VERIFY FIRST that channel adapters are composed/activatable under the
   local profile at all, and how inbound channel messages map onto threads
   (does a Telegram chat land on its own thread?). Ambient guardrails (quiet
   hours, rate caps) apply to outbound channel messages too. Behind
   `settings.channels_enabled`, default OFF.

**8g — Small surfacings: memory seeding + subagent visibility**

1. Onboarding via `memory_import`/`memory_seed`: a first-run (and Settings)
   flow — "tell me about yourself" free text and/or import a notes file —
   seeded into agent memory. ⚠️ VERIFY the exact seed/import capability
   contract in-agent before building UI. ⚠️ Two disclosures the UI must make
   before seeding: (a) seeded memory is permanent (the never-delete
   invariant — there is no unsend); (b) if a CLOUD provider is active,
   memories get sent to that provider inside prompts — say so at seed time.
   Enforce a size cap on imports.
2. Subagent visibility: when `builtin__spawn_subagent` runs (⚠️ verify how it
   appears in the event stream — likely `capability_progress`), show a
   distinct character/bubble treatment ("my assistant is on it") instead of
   generic progress.

**Backlog (upstream contributions, not sub-phases):** routes for memory
browser, skills list, audit log, run history, and automation create/edit —
each a `RebornServicesApi` facade method + `ironclaw_webui_v2` route, per
`dashboard-gaps.md` "What would unblock them". Track upstream; do not fork
these in.

**Definition of done (Phase 8):** dashboard has sidebar navigation with a
working Chats panel (list, read history, continue, hide, stop button) and
every pre-existing panel re-verified; installing a registry connector
end-to-end gives the agent tools proven by a successful real tool call (or
the documented fallback is in place); the voice is switchable from a curated
picker and survives restart; gate coverage is enumerated with the fallback
recorded; a git repo of skills imports via a Rust git library with per-skill
plain-text review and diffed re-sync; Telegram connects behind its flag with
pairing enforced (or the blocker is documented); onboarding can seed memory
with both disclosures; all gates green and a manual smoke run on Windows.
