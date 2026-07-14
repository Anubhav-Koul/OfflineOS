# CLAUDE.md — IronClaw Fork: Desktop Widget Agent

Project instructions for Claude (Code) working in this repository. Read fully before making changes. Dated progress notes at the bottom of this file are authoritative over the phase descriptions when they conflict — they record what the running system actually does.

## What this project is

A fork of [nearai/ironclaw](https://github.com/nearai/ironclaw) (Rust agent OS, MIT OR Apache-2.0) extended into a **Windows-first desktop app**: an animated character companion widget + dashboard (Tauri 2), **llama.cpp local inference** alongside cloud LLMs, plus browser automation, voice (wake word/STT/TTS), and a canvas window. Target capability: on par with OpenClaw for agent core + browser + voice/canvas (messaging channels are out of scope for v1).

## Golden rules

1. **Additive fork policy — do NOT edit IronClaw core crates unless unavoidable.** All new functionality lives in new crates (`crates/ic_widget`, `crates/ic_llama`, `crates/ic_voice`, `crates/ic_browser_mcp`). Integrate through existing extension points: the gateway HTTP/SSE/WS API, `LLM_BACKEND=openai_compatible`, MCP servers, and WASM tools. This keeps upstream merges cheap.
2. **Never commit secrets.** API keys via env vars / OS keychain only. IronClaw already rejects inline secrets in config — preserve that behavior.
3. **Windows is the primary target.** Every phase must build and run on Windows (MSVC toolchain). Don't introduce Unix-only code paths without a Windows equivalent.
4. **After every phase: `cargo fmt`, `cargo clippy --all-targets`, `cargo test`, and a manual smoke run.** Do not mark a phase done with failing checks.
5. When IronClaw internals are unclear, read the upstream docs in-repo first: `README.md`, `FEATURE_PARITY.md`, `AGENTS.md`, `docs/`, and `CLAUDE.upstream.md` — do not guess API shapes.

## Precedence & inherited invariants

This file governs the fork crates (`ic_*`, `ui/`). `CLAUDE.upstream.md` governs any work inside IronClaw core code — its full clippy gate, dual-backend persistence rule ("all new persistence features must support PostgreSQL AND libSQL"), and code style apply there in full. Our scoped clippy/CI is a deliberate fork-gate decision (see Phase 0 notes), not a license to lint core code loosely.

Two upstream invariants bind the fork crates too, even though they originate in `CLAUDE.upstream.md`:

- **LLM data is never deleted by code or agents.** Context, reasoning, tool calls, messages, events — retain, timestamp, filter; never strip or drop rows. "Cleanup" means evicting in-memory caches only. **Fork scoping decision:** an explicit *user-initiated* wipe in the dashboard (their machine, their data) is permitted — it must be a deliberate UI action with confirmation, never automatic, never agent-callable.
- **`credential_name` ≠ `extension_name`** (backend secret identity vs user-facing extension identity). Never conflate them; never route setup UI from `credential_name`. This binds Phase 4+ when `ic_browser_mcp`/`ic_voice` surface auth or setup flows through the dashboard.

Also inherited and still enforced by tooling: every gateway/CLI/routine mutation goes through `ToolDispatcher::dispatch()` (pre-commit hook `scripts/pre-commit-safety.sh`, full rule in `.claude/rules/tools.md`).

## Repository origin & sync

```bash
git clone https://github.com/<our-org>/ironclaw.git && cd ironclaw
git remote add upstream https://github.com/nearai/ironclaw.git
git fetch upstream --tags
```

- Pinned to `reborn-integration` (see Phase 0 notes below for why, and the exact commit).
- Monthly sync: `git fetch upstream && git merge` on a branch; the integration test suite (Phase 0) is the merge gate.
- Keep `LICENSE-MIT`, `LICENSE-APACHE`, and attribution intact.

## Key IronClaw facts (verified July 2026 — re-verify on upgrade)

- **Toolchain:** Rust 1.96+. Windows MSI installer exists upstream, so Windows builds are supported.
- **Two runtimes:** legacy `ironclaw` binary, and **Reborn** (`ironclaw_reborn_cli` → `ironclaw-reborn` binary, `reborn-integration` branch). Reborn profiles: `local-dev`, `local-dev-yolo` (host access, needs `--confirm-host-access`), `hosted-single-tenant-volume` (**libSQL embedded storage** — no Postgres!), `production` (PostgreSQL 15+ + pgvector).
- **Storage decision for us:** desktop users must not need Postgres. Primary path: Reborn **libSQL substrate**. Fallback: bundle [postgresql_embedded](https://crates.io/crates/postgresql-embedded). Verify libSQL profile supports everything we need (memory hybrid search); if pgvector-only features block us, evaluate before writing code.
- **LLM providers built in:** NEAR AI (default), Anthropic, OpenAI, Gemini, Ollama, `openai_compatible` (`LLM_BASE_URL`/`LLM_API_KEY`/`LLM_MODEL`). **llama.cpp needs no core changes**: run `llama-server` and set `LLM_BACKEND=openai_compatible`, `LLM_BASE_URL` pointing at the `ic_llama` SchemaProxy (see Phase 1 notes).
- **Gateway/WebUI:** `serve` command behind `webui-v2-beta` cargo feature (requires Node 22 + pnpm at build time). Listener default `127.0.0.1:3000`; auth via `IRONCLAW_REBORN_WEBUI_TOKEN` (bearer) + `IRONCLAW_REBORN_WEBUI_USER_ID`.
- **The serve event stream never carries assistant text** — no token streaming exists; the projection SSE carries `run_status` only (1-second poll granularity). Read replies from `GET /threads/{id}/timeline` after the run goes terminal. The canary test `the_event_stream_never_carries_the_assistant_text` fails the day upstream fixes this — that is the signal to delete the timeline fetch. See `docs/desktop/chat-rendering.md`.
- **Memory browser, skills list, audit log, and run history have no HTTP route** in `ironclaw-reborn serve` (`ironclaw_gateway` v1 is not compiled into that binary; "jobs" is only `GET /automations`). The dashboard shows these as unavailable-with-reason. See `docs/desktop/dashboard-gaps.md` for the four routes upstream would need.
- **Gateway contract quirks:** `401` returns a bare text body (not the documented JSON error shape); a replayed `client_action_id` yields `already_submitted` while the original is still `Submitted`, then **409** after the turn finalizes — both mean "accepted exactly once" (`Error::is_duplicate_action()`).
- **Security model:** WASM sandbox (capability permissions, endpoint allowlist, credential injection at host boundary, leak scan), prompt-injection defense layers, AES-256-GCM secrets, audit log. **Do not weaken any of it.** Tools needing native host access (CDP browser, audio) run as MCP servers on the host, not as WASM tools.
- **Docker sandbox is optional** — assume no Docker on user machines; use local in-proc workers.
- **Gaps vs our goal** (what we're building): desktop character widget UI, llama.cpp model management, browser automation tool, voice pipeline, canvas window.

## Architecture

```
┌─ Tauri app: crates/ic_widget ──────────────────────────────────┐
│ Character widget (always-on-top, transparent) + speech bubble  │
│ Dashboard · Canvas window · Tray                               │
│   │ HTTP + SSE client via gateway_client (bearer token)        │
└───┼────────────────────────────────────────────────────────────┘
    ▼
ironclaw-reborn serve  (127.0.0.1:<port>, libSQL storage)  ← supervised child of ic_widget
    │ agent loop · tools · memory · skills · safety layer
    ├── LLM: openai_compatible → ic_llama SchemaProxy → llama-server sidecar
    │        or Anthropic/OpenAI cloud (config switch, failover)
    ├── MCP: ic_browser_mcp (chromiumoxide/CDP on host)
    └── channel/HTTP: ic_voice (wake word → whisper-rs → agent → Piper TTS)
```

The Tauri app is the single user-facing process; it supervises `ironclaw-reborn` and `llama-server` as children (restart with backoff, health checks, kill-tree on exit via Windows Job Objects with `KILL_ON_JOB_CLOSE`).

## Build phases — execute in order

### Phase 0 — Fork bootstrap ✅ (see notes below; Windows build friction log: `docs/desktop/windows-build.md`, serve API contract: `docs/desktop/gateway-api-notes.md` + its C1–C4 corrections)

### Phase 1 — llama.cpp integration (`crates/ic_llama`) ✅ (see notes below)

### Phase 2 — Widget + dashboard (`crates/ic_widget` + `ui/`) — 2a ✅, 2b in progress
Split into **2a** (shell + supervision + chat — done, see notes) and **2b** (remaining dashboard panels).

Phase 2b scope:
1. Dashboard panels with a live route: sessions (`GET /threads` — threads survive gateway restarts via the libSQL-backed root filesystem), automations (`GET /automations`), model picker + GGUF download UI + token/s + VRAM stats from `ic_llama`, provider keys (Credential Manager).
2. Panels without a route (memory browser, skills, audit log, run history) stay visible as unavailable-with-reason per `docs/desktop/dashboard-gaps.md`. Do not fake them.
3. Tool-approval prompts surfaced from the safety layer (also mirrored in the speech bubble).
4. Keep the speech-bubble chat UI as the Phase 2 face of the app — the character lands in Phase 3. Transparency, undecorated frame, and always-on-top landed in 2a (`WebviewWindowBuilder` in `crates/ic_widget/src/main.rs`). **Per-pixel hit testing did not** — clicks do not yet pass through the transparent regions. Build that plumbing in 2b or at the start of Phase 3.

### Phase 3 — Animated character companion (in `ui/`) ✅ (see notes below; pipeline doc: `docs/desktop/character-pipeline.md`)

The widget becomes an **animated anime-style character** standing on the desktop; the Phase 2 speech bubble anchors beside it.

1. **Dev model (already in-repo): `ren_en/`** — Live2D's official "Ren Foster – PRO" sample (Cubism 5.3). Copy `ren_en/runtime/` into `assets/characters/ren/live2d/`. It ships with everything we drive: `LipSync` group → `ParamMouthOpenY`, `EyeBlink` group, Idle motion + 2 motions, 5 expressions, Head/Body hit areas. License: Live2D Free Material License — commercial use permitted for general/small-scale users, so it may ship in v1.
2. **FIRST TASK — compatibility check:** Ren Foster uses Cubism 5.3 drawing features (alpha-blend masks, offscreen rendering). Verify `pixi-live2d-display` with the **Cubism 5 web core** renders it correctly in the Tauri webview (WebView2). If it misrenders, fall back to an official Cubism 4 sample (Hiyori/Mao) for dev and revisit.
3. **`CharacterRenderer` interface, two backends:**
   - `Live2DRenderer` (primary): loads `.model3.json` from `assets/characters/<name>/live2d/`. Drive: blink cycles, cursor-following eye tracking, head sway, expressions, `ParamMouthOpenY` lip sync from TTS amplitude (stub with a test tone until Phase 5 wires Piper).
   - `SpriteRenderer` (fallback/simple): flat PNG art from `assets/characters/<name>/sprite/` with cheap-puppet transforms (idle bob, tilt, squash). User-supplied copyrighted art is dev-only — **must not ship in public releases**.
   - Character = asset folder + `character.json` (name, renderer type, scale, anchor, param/expression → state mappings). A commissioned model later drops in with zero code changes; character choice is a settings toggle.
4. **Animation state machine** driven by supervisor/gateway events: `idle` → `listening` (input focus / wake word) → `thinking` (run in flight) → `speaking` (reply rendering / TTS) → `concerned` (approval pending) → `error` (child unhealthy, e.g. `GatewayState::Unhealthy`). Map Ren's expressions + motions to these states in `character.json`. Transitions interruptible; Stop always returns to `idle`. Note: because there is no token streaming, `thinking` runs until the timeline fetch returns — `speaking` is entered on reply render, not first token.
5. Use the model's hit areas for click handling (click head = summon input, drag body = move window); clicks outside the character pass through (per-pixel hit testing).
6. Performance: Ren is heavyweight (4096px texture, offscreen rendering). Cap FPS ~30, pause animation when hidden or a fullscreen app is foreground, measure GPU alongside llama.cpp on iGPU-only machines.
7. Licensing gates before public release: verify Live2D Cubism Web SDK license tier for our distribution size; check any marketplace model's redistribution terms; drop dev-only sprite art.

### Phase 4 — Browser automation (`crates/ic_browser_mcp`) ✅ (see notes below)

⚠️ **Step 1 and step 3 below were wrong** — stdio MCP does not work against Reborn. Corrected in the Phase 4 notes at the bottom; read those, not this list.

1. ~~Standalone MCP server (stdio)~~ → **streamable-HTTP MCP server on loopback** wrapping `chromiumoxide`: `browser_navigate`, `browser_get_text`, `browser_find`, `browser_fill`, `browser_click`, `browser_screenshot`.
2. Launch a dedicated browser profile (probe registry: Chrome → Edge; Edge is guaranteed on Win10+). Never attach to the user's running profile by default.
3. ~~Register with IronClaw through its MCP config.~~ → **host-bundled extension manifest + CP-4**. Sensitive actions route through the approval flow (every discovered MCP tool is `default_permission: Ask`, hardcoded upstream).
4. CAPTCHAs/logins: pause, notify via widget (character `concerned` state), let user complete, resume. Selector failures: screenshot → vision-capable model fallback.

### Phase 5 — Voice (`crates/ic_voice`) + Canvas — **Canvas ✅, Voice ✅ (see notes below; full write-up: `docs/desktop/voice-notes.md`)**

⚠️ **The voice pipeline plan below has two dead ends and several Windows landmines — verified before any code. Read the Phase 5 notes at the bottom before building voice.**

1. Pipeline: `cpal` capture → ring buffer → wake word (~~openwakeword ONNX via `ort`~~ — **models are CC-BY-NC, non-commercial; use `rustpotter` or self-trained**) → silero VAD gate (`voice_activity_detector`, MIT ✓) → `whisper-rs` transcribe (**CPU first — Vulkan silently no-ops on Windows static builds**) → post to gateway → reply → ~~Piper TTS playback (bundled piper1-gpl binary)~~ **the piper1-gpl binary no longer exists (it's a Python package now); use the archived MIT `rhasspy/piper` exe or run the VITS ONNX in-proc**. Barge-in: stop TTS when VAD triggers.
2. Wire TTS playback amplitude into `CharacterRenderer` lip sync (`ParamMouthOpenY`), replacing the Phase 3 test-tone stub (the seam is `patchLipSync` in `ui/src/character.ts`; Piper emits no timing, so compute RMS from the PCM yourself); barge-in returns the character to `listening`.
3. WASAPI device-change handling (**cpal exposes no device-change API — hand-write an `IMMNotificationClient` via the `windows` crate**), mic-live indicator on the character/bubble, tray mute toggle, audio never written to disk.
4. ~~Canvas: dedicated Tauri window; agent emits HTML/SVG via a `canvas_render` tool (register as WASM tool or MCP); render in sandboxed iframe, sanitize output.~~ **✅ Done — see Phase 5 canvas notes. Route: in-process loopback MCP (reusing CP-4), not a WASM tool — WASM/first-party would strand the HTML in the wrong process behind a sanitizing, 16 KiB-capped channel.**

### Phase 6 — Packaging & hardening — **config + hardening ✅, real MSI build gated on external inputs (see notes below; full write-up: `docs/desktop/packaging.md`)**
1. Single MSI (Tauri bundler): our app + `ironclaw-reborn` + llama.cpp binaries + Piper + bundled character assets. First-run wizard: GPU probe → model recommendation → provider keys → storage init (libSQL — no Postgres install!).
2. Uninstaller must remove the Credential Manager entry ("IronClaw Desktop" / "gateway-token") and `%LOCALAPPDATA%\IronClaw Desktop\`.
3. Tauri auto-updater; code-sign (unsigned + mic capture + child processes = SmartScreen/AV flags).
4. Failure drills before ship: kill llama-server mid-generation; kill ironclaw-reborn mid-job; sleep/resume; monitor unplug; disk-full during GGUF download; occupied ports (ports are already dynamic — verify end to end).

### Phase 6 — done to the limit of what the repo can hold (recorded 2026-07-13)

`ic_widget` (`main.rs`, `secrets.rs`, `settings.rs`, `tauri.conf.json` +
`tauri.release.conf.json` + `wix/uninstall-cleanup.wxs`) + `ui/`. **Full write-up:
`docs/desktop/packaging.md`.** Everything buildable and verifiable *without a
certificate, an updater keypair, or an update endpoint* is done; those three (plus
the real MSI build, which needs WiX + the staged sidecar on a clean VM) are
documented with config templates rather than committed as half-configs or secrets.

- **Bundle config** (`tauri.conf.json`): MSI target, WebView2 `embedBootstrapper`
  (offline), publisher/metadata, and the WiX uninstall fragment. `externalBin`
  (the `ironclaw-reborn` sidecar) lives in a **release-only overlay**
  (`tauri.release.conf.json`, applied via `cargo tauri build --config …`) because
  tauri-build validates `externalBin` existence on *every* build — committing it in
  the base config broke `cargo run`. Character assets ride in the embedded frontend,
  so they need no resource bundling.
- **Offline strategy:** ship the lean MSI (llama-server / models / Piper / whisper
  download on first run — each runtime already prefers a present file, so a fully
  offline MSI is a staging exercise, no code change) and gate the first launch behind
  the wizard. `ironclaw-reborn` is the one binary that *must* ship (not downloadable).
- **Uninstall cleanup:** `ic-widget.exe --uninstall-cleanup` (`SecretStore::clear_all`
  + remove `%LOCALAPPDATA%\IronClaw Desktop\`), invoked by the WiX custom action.
  Two real caveats (Tauri's main-exe `FileKey`; per-user data under a per-machine
  MSI) are documented in the `.wxs` and packaging doc — they need the real generated
  WiX to reconcile.
- **First-run wizard:** `settings.setup_complete` + `needs_setup`/`complete_setup`
  commands; a `SetupWizard` overlay in the dashboard (orients toward the existing
  model/provider panels rather than duplicating them, offers a voice opt-in); the
  widget opens the dashboard on first launch so it's seen. Storage is *not* a step —
  the gateway inits libSQL on boot.
- **Auto-updater + code-signing:** templated in the packaging doc (keypair gen, config
  block, plugin registration; cert thumbprint + timestamp). **Not wired live** — a
  placeholder pubkey would fail the build, and the repo must never carry a private key.
- **Failure drills:** the checklist is in the packaging doc with each mode marked
  covered-by-design / needs-manual-drill / gap. No known uncovered failure; the
  manual pass runs on the first signed build on a clean VM.

Next: **Phase 7 — ambient companion** (7a ✅ — see its notes at the bottom; 7b → 7c →
7d remain). Phase 6's remaining work is gated on external
inputs and stays open alongside it: obtain a code-signing cert + updater keypair +
update endpoint, produce the first real MSI on a clean VM, reconcile the WiX caveats,
run the manual drills, and record + bundle rustpotter wakeword models (then wake word
replaces push-to-talk).

### Phase 7 — Ambient companion (proactive suggestions + self-learning & imported skills) — 7a ✅ (see notes below)

Goal: the character *initiates* and *learns*. The dashboard stays an occasional
settings/inspection surface — everything in this phase surfaces through the
character and bubble. All work is additive (modules in `ic_widget`, or a new
`crates/ic_ambient` if it grows); no core patch is expected — if one becomes
unavoidable, follow the `core-patch:` protocol in `docs/desktop/core-patches.md`.
Build strictly in order 7a → 7b → 7c → 7d; each sub-phase ends with the full
gate (fmt, clippy, test, integration suite, manual smoke run) and a dated
progress note appended to this file in the established style.

**7a — Proactive surfacing plumbing (shared by everything below)**

1. A dedicated **ambient thread**, separate from the chat thread, created via
   `gateway_client`. Replies follow the existing contract: watch `run_status`
   to terminal, then read `GET /threads/{id}/timeline` (no token streaming
   exists — see `docs/desktop/chat-rendering.md`).
2. A new character state **`suggesting`** (interruptible, mapped to an
   expression/motion in `character.json`) + a bubble popup rendering the
   suggestion with **Accept / Not now**. "Not now" responses are recorded with
   timestamps (never deleted — LLM-data invariant) and feed the rate limiter.
3. A **guardrail module**: hard rate cap (default ≤ 2 unsolicited surfacings
   per hour), quiet hours, master toggle (`settings.ambient_enabled`, default
   **OFF**) in tray + dashboard. Everything runs locally.
4. First consumer: **scheduled proactivity through the existing
   triggers/automations lane** (`GET /automations` is already surfaced; min
   fire cadence is 60 s). A completed automation run surfaces via the popup.
   ⚠️ VERIFY FIRST, against the running gateway, how a trigger-fired run lands
   (thread routing, projection SSE visibility) versus a webchat run — read
   `ironclaw_triggers` (`trusted_submit.rs`, `worker.rs`) and the reborn
   composition before writing widget code. Do not assume it matches webchat.

**7b — Self-learning skills (Hermes-style reflection, fail-closed)**

1. After a *user-initiated* run reaches terminal, fire **one reflection turn**
   on the ambient thread (behind `settings.reflection_enabled`): "Did this
   task teach a reusable procedure? If yes, output a draft SKILL.md
   (frontmatter: name / description / activation keywords, matching
   `skills/*/SKILL.md` in-repo). If not, say no. Do NOT install anything."
2. **A draft is never auto-installed.** The runtime has no tool-approval
   prompt (Phase 4 finding: `default_permission` is never read;
   `builtin__skill_install` would execute unprompted). Installation happens
   only after the user approves in the bubble — red prompt, full skill text
   shown, consent-gate pattern from Phase 4, default answer No. On approval
   the widget sends an explicit install turn (or dispatches the install
   capability directly if a cleaner seam exists).
   ⚠️ VERIFY FIRST: is `builtin.skill_install` actually composed and
   activatable in `ironclaw-reborn serve` under our local profile? Skills have
   no HTTP route (`docs/desktop/dashboard-gaps.md`), so confirm the install
   path end-to-end against the running gateway before building UI. The Phase 4
   lesson applies: a green suite can coexist with a capability the agent
   cannot actually reach — drive the real runtime.
3. **Dedupe and cap.** Before proposing, list installed skills (find the
   listing seam — `skill_listing.rs` in the composition); a rejected draft is
   remembered so the same skill is not re-proposed every run; cap total
   self-learned skills (default 50).
4. **Model-quality constraint, documented not solved:** `LLM_BACKEND` is
   single-valued, so reflection runs on the same model as chat. Small local
   models draft poor skills. Surface a "skills learn better with a stronger
   model" hint when the active model is small; do NOT build multi-model
   routing now (`docs/desktop/llm-provider-selection.md` tracks that).

**7c — Skill import (third-party SKILL.md)**

1. Import from a local folder (URL import later): parse + validate
   frontmatter, normalize to the `ironclaw_skills` format, enforce the
   install-bundle limits (`MAX_INSTALL_BUNDLE_*`), show the full skill text
   for review, and install through the same approved path as 7b. Never
   install silently.
2. Treat skill text as **untrusted input** (prompt-injection surface).
   ⚠️ VERIFY what `ironclaw_skills/src/gating.rs` and the safety layer
   already do with skill content on install/activation, and state in the
   progress note what is and is not scanned.
3. Entry point: dashboard (settings surface is the right home for imports) +
   an "install this skill?" bubble confirmation.

**7d — Ambient watchers (event-driven proactivity)**

1. Opt-in signals, each individually toggled, all default OFF: foreground
   app/window title (extend the existing interaction/fullscreen poller from
   Phase 3), watched folders (`notify` crate), time-of-day patterns. No
   screen content capture, no audio persistence, nothing leaves the machine.
2. **v1 gate is rule-based**, not LLM-based: user-defined "when X happens,
   ask the agent to Y" rules that materialize a prompt on the ambient thread.
   An LLM "is this worth suggesting?" gate is v2 — on iGPU machines constant
   gate inference competes with the chat model (the Phase 3 perf rule
   applies).
3. Wake-word models (the outstanding Phase 6 item) later make voice a third
   sense; not a blocker — push-to-talk remains until they are recorded.

**Definition of done (Phase 7):** with ambient enabled, a scheduled automation
surfaces as a character suggestion; completing a multi-step chat task yields a
"want me to remember this?" prompt whose approval installs a skill that is
active in the next session; a third-party SKILL.md folder imports after
review; every surfacing respects the rate cap and quiet hours; a "Not now"
suppresses re-surfacing; all gates green and a manual smoke run on Windows.

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

## Edge cases checklist (apply throughout)

- **Upstream merge conflicts** → additive-crate policy; if a core patch is truly unavoidable, isolate it in one commit prefixed `core-patch:` and list it in `docs/desktop/core-patches.md` for replay after merges.
- **Reborn is beta** → pinned commit; `ic_widget::gateway_client` is the only place we speak to the gateway, so protocol drift is fixed in one place; integration tests catch breakage.
- **NEAR AI default auth** → our onboarding always writes an explicit `[llm.default]`; never depend on NEAR onboarding flow.
- **Local models fumble tool-call JSON** → keep agentic routing on cloud or ≥14B local models by default; expose per-task routing in settings. (See also CP-3/SchemaProxy in Phase 1 notes — llama.cpp rejects oversized schema bounds.)
- **Context overflow, runaway loops, cost** → rely on IronClaw's job limits; surface budget/iteration settings in dashboard; always-visible Stop.
- **libSQL profile disables process-backed tools (e.g. shell) in hosted profile** → we run `local-dev` semantics with libSQL storage; shell exec must work on the user's machine with the approval flow intact.
- **Two instances / port clashes / orphaned children** → single-instance mutex; dynamic ports; Job Objects so children die with the parent (verified: survives `TerminateProcess`).
- **DB integrity** → libSQL WAL, backup-before-migrate, integrity check on start.
- **Transparent character window quirks** → per-pixel hit testing so clicks pass through empty regions but land on the character; GPU compositing of transparent always-on-top windows varies by driver — test on Intel iGPU too; cap renderer FPS (~30) and pause animation when hidden or a fullscreen app is detected, so idle GPU use never competes with llama.cpp.

## Commands reference

```bash
cd ui && npm install && npm run build && cd ..      # MUST run before the app build (frontend embeds at compile time)
cargo build -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta
cargo run -p ic_widget --features app --bin ic-widget
cargo run -q -p ironclaw_reborn_cli --bin ironclaw-reborn -- doctor   # safe diagnostics (home, profile, config)
cargo build --release                                # core release build (no webui feature)
cargo test -p ic_integration_tests --features webui-v2-beta   # merge gate (needs serve binary above)
cargo fmt && cargo clippy --all-targets              # before every commit
```

## Definition of done (v1)

Offline on a clean Windows machine with one MSI install: an animated character stands on the desktop; wake it by hotkey or voice, ask it to research something in the browser, have it read/write files in its workspace with approval prompts, remember facts across restarts, render a chart on the canvas, and answer with a local GGUF model — with cloud failover when a key is configured.

---

## Fork bootstrap notes (Phase 0 — recorded during setup)

- **Upstream source pinned to branch `reborn-integration`** (commit `a492857`), *not* a release tag: the Reborn runtime (`ironclaw_reborn`, `ironclaw_reborn_cli`, libSQL storage) that this entire desktop architecture depends on exists **only** on `reborn-integration`. The latest release tag (v0.21.0) contains none of it. This is a deliberate, necessary deviation from the "pin to a release tag" rule above — revisit if/when Reborn lands in a tagged release. Pin the exact commit for reproducibility; unshallow before the first upstream sync (current clone is `--depth 1 --filter=blob:none`).
- Cloned upstream directly into this repo (`git remote upstream = https://github.com/nearai/ironclaw.git`); no fork org yet. Add your fork as `origin` when ready.
- Upstream's own `CLAUDE.md` is preserved as **`CLAUDE.upstream.md`** (it holds useful IronClaw dev conventions — code style, extension/auth invariants, crate layout). This file (the desktop-fork guide) is the authoritative root `CLAUDE.md`.
- The repo also ships its own `.claude/` skills (`reborn-feature`, `add-tool`, `ship`, `trace`, `mintlify-docs`, `architecture-video`) and rules — use them.

### Phase 0 steps 5 & 6 — done (recorded 2026-07-09)

- **Step 5 — merge-gate crate `crates/ic_integration_tests`** (added to workspace members). It spawns `ironclaw-reborn serve` (libSQL `local-dev` profile) against a **hermetic mock LLM** and drives the WebChat v2 chat contract end-to-end: `401` without a bearer → create thread → send message → stream SSE to `run_status: completed` → assert the assistant reply round-tripped into the timeline. Verified passing on Windows (see the crate `README.md` for the two-step run recipe). Key facts baked in: `openai_compatible` → `RigAdapter` → **non-streaming** Chat Completions, so the mock only answers `POST /v1/chat/completions`; the assistant reply surfaces in the **timeline**, not as a `text` item in the projection SSE (which carries only `run_status`); `IRONCLAW_REBORN_WEBUI_USER_ID` must equal the runtime default owner (`reborn-cli`); a separate crate can't use `CARGO_BIN_EXE_*`, so the binary is located from the target dir (override `IRONCLAW_REBORN_BIN`).
- **Step 6 — Windows CI** at `.github/workflows/desktop-ci.yml` (three `windows-latest` jobs: `quality` = fmt + scoped clippy `-D warnings`; `gate` = build the `serve` binary then run the merge gate; `release-build` = `cargo build --release` core, no webui). Scoped to fork crates + the serve path so pre-existing upstream lints don't fail the fork gate. Pinned action SHAs match the upstream workflows.
- Phase 0 is complete.

## Phase 1 notes — llama.cpp integration (done, recorded 2026-07-10)

`crates/ic_llama` (+ `docs/desktop/llama-cpp-pin.md`,
`docs/desktop/llama-cpp-tool-grammar.md`, crate `README.md`). No IronClaw core
crate was touched. Verified on this machine: Qwen3-4B-Q4_K_M, full GPU offload
(`-ngl 37`) on an RX 7900 XTX, agent round-trip through `ironclaw-reborn serve`
in 7.5 s, fully offline.

- **Pinned llama.cpp `b9948`**, digests from the GitHub release API's `digest`
  field. Vulkan is the default (31 MiB, covers all vendors); CUDA is opt-in only
  (628 MiB with the cudart archive); CPU is the fallback. Bumping the pin has a
  checklist — read `llama-cpp-pin.md` before touching `release.rs`.
- **The archive layout is not assumed.** `runtime.rs` extracts, then *searches*
  for `llama-server.exe` (upstream has moved it between `/` and `build/bin/`).
  It launches in place because the backend DLLs are its siblings.
- **Per-layer sizes come from GGUF tensor *offsets*, not a `ggml` type table.**
  Tensors are written back to back, so a tensor's size is the distance to the
  next offset. This is why `placement.rs` can size the VRAM budget exactly
  without tracking 40 quantization block sizes that drift upstream.
- **`-ngl N` offloads the LAST N blocks**, and `N > block_count` also offloads
  the output tensors — the planner fills the budget from the last block backwards
  and accounts for each layer's KV-cache slice, which at long contexts exceeds
  the layer itself.
- **A nonzero DXGI `DedicatedVideoMemory` does not mean a discrete GPU.** AMD
  APUs report a BIOS carve-out of system RAM (485 MiB here). `is_discrete()` uses
  a 1 GiB floor; without it the planner offloads to the iGPU.
- **`llama-server` answers `503 Loading model` before it is ready.** The
  supervisor treats `503` as loading, not as a crash; a server that is up but
  never reaches `200` within the startup budget is killed and counted as a
  failure. Two consecutive failures mark the model *suspect* (a marker beside the
  weights, surviving restarts) rather than restarting forever.
- **The sidecar port is reserved once and reused across restarts**, because
  IronClaw reads `LLM_BASE_URL` at startup and never re-reads it.
- **CP-3 (route-around, not a patch): llama.cpp cannot compile IronClaw's tool
  schemas.** Tool calls require `--jinja`, which compiles tool schemas into a
  GBNF grammar; llama.cpp rejects repetition counts >= 2000, and
  `builtin__spawn_subagent` declares `maxLength: 65536`, so *every* agent turn
  failed. Fixed additively: `ic_llama::proxy::SchemaProxy` sits in front of the
  sidecar (`LLM_BASE_URL` points at it) and strips oversized bounds from
  `tools`/`response_format` in flight. Patching the one core schema was rejected
  because any user-installed MCP tool can declare a bound of its own. See
  `docs/desktop/llama-cpp-tool-grammar.md`.
- **Tests are hermetic** (`cargo test -p ic_llama`, wired into CI): synthetic
  GGUF files, a mock HTTP origin for `206`/`200`/`416` + digest mismatch, and
  `testsupport/fake_llama_server.rs` — a real subprocess reproducing a slow load,
  a crash on startup, a crash after being healthy, and a wedged server. The
  real-weights round-trip is `#[ignore]`d in
  `crates/ic_integration_tests/tests/local_model_roundtrip.rs`.

## Phase 2a notes — widget shell, supervision, chat (done, recorded 2026-07-10)

`crates/ic_widget` + `ui/` (+ `docs/desktop/chat-rendering.md`,
`docs/desktop/dashboard-gaps.md`, and four **corrections** appended to
`gateway-api-notes.md`). No IronClaw core crate was touched. Verified by
launching the app: it spawns `ironclaw-reborn`, and a **hard kill** of the widget
(`TerminateProcess`, no `Drop`) takes the gateway down with it.

Phase 2 was split. **2a** = shell + supervision + chat. **2b** = the remaining
dashboard panels.

### Two findings that reshaped the plan

- **The event stream carries no assistant text — at all.**
  `ProductProjectionItem::Text` has *no producer* in the workspace, and
  `final_reply` is only produced on the Telegram/Slack push path. Upstream's own
  SPA has a dead branch waiting for it. So the widget watches `run_status` until
  the run is terminal, then reads the reply from `GET /threads/{id}/timeline`.
  There is no token streaming, and the stream is a 1-second poll. See
  `docs/desktop/chat-rendering.md`; pinned by
  `the_event_stream_never_carries_the_assistant_text`.
- **Memory browser, skills list, and audit log have no HTTP route**, and
  `ironclaw_gateway` (v1) is not compiled into `ironclaw-reborn serve`. "Jobs" is
  only `GET /automations` (scheduled cron entries, no run history). The dashboard
  lists these as unavailable with the reason rather than faking them. See
  `docs/desktop/dashboard-gaps.md`.

### Design decisions worth keeping

- **`gateway_client` is the only place we speak to the gateway.** Newtyped ids
  (`ThreadId`/`RunId`/`GateRef` all appear in one URL path, so `String` would make
  transposition a 404 rather than a compile error), tolerant decoding (unknown SSE
  events and projection items become `Unknown` instead of failing), and a
  spec-correct SSE decoder.
- **`RunPhase::Other` is deliberately non-terminal.** An unknown status from a
  newer gateway is far more likely a new in-flight state than a new way of
  finishing; assuming terminal would make the widget stop listening mid-turn.
- **A `401` from the gateway is fatal, not a retry.** The process is healthy and
  simply rejects our token; restarting cannot help. The supervisor stops and says
  so. (`GatewayState::Unhealthy`, not a restart loop.)
- **Windows Job Object with `KILL_ON_JOB_CLOSE`** owns every child. This is the
  only thing that survives a hard kill of the widget, and it is what stops an
  orphaned gateway from holding the libSQL write lock and the port.
- **Widget position is keyed by a hash of the monitor arrangement**, and a saved
  point that is no longer on any monitor is discarded on read. An always-on-top
  widget stranded offscreen cannot be dragged back.
- **`EventStream` must be `Send`.** An earlier version awaited `self.open()`
  inside `reconnect(&mut self)`, holding a `&EventStream` across the await;
  `&T: Send` requires `T: Sync`, which a boxed response body never is. The stream
  would not compile inside `tokio::spawn`. Pinned by
  `a_pump_task_over_the_event_stream_is_send`.
- **Tauri sits behind an optional `app` feature**, so `cargo test -p ic_widget`
  never builds a WebView. The frontend (`ui/`, SolidJS + Vite) is built before
  `tauri-build` runs, not through `beforeBuildCommand`.
- **The gateway is stored into app state *before* `Ready` is emitted.** The UI
  falls back to reading `gateway_state` when it misses the event; emitting first
  left a window where that read still said `Starting` and the event had already
  gone, stranding the widget on "starting" forever.
- **The widget creates its thread on the first `ready` it observes**, not on
  mount. The gateway takes ~500 ms to boot (much longer on a first run), so
  creating the thread on mount failed with "still starting" and never retried.

### Corrections to `gateway-api-notes.md` (C1–C4)

Most `WebChatV2Event` variants are unreachable; the assistant's text is not on the
stream; `401` returns a bare text body rather than the documented JSON error
shape; and a replayed `client_action_id` only yields `already_submitted` while the
original message is still `Submitted` — after that the identical replay is a
**409**. Both mean "accepted exactly once"; `Error::is_duplicate_action()` exists
so the UI does not show a failure for either.

### Running it

```bash
cd ui && npm install && npm run build && cd ..     # MUST run before the app build
cargo build -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta
cargo run -p ic_widget --features app --bin ic-widget
```

**The frontend is embedded into the binary at compile time.** `generate_context!`
reads `ui/dist`, so a change to `ui/src` needs `npm run build` *and then* a Rust
rebuild. There is no dev server: `tauri.conf.json` deliberately sets **no
`devUrl`**. It used to, and because `tauri-build` sets `cfg(dev)` for a plain
`cargo run`, the webview navigated to `http://localhost:1420` — which only the
`tauri` CLI's `beforeDevCommand` would have started. The result was WebView2's
"can't reach this page" error inside the widget. Hot reload would need
`cargo install tauri-cli` and `cargo tauri dev`; it is not wired up.

`ironclaw-reborn` is found via `IRONCLAW_REBORN_BIN`, then beside the widget
binary. The bearer token is minted into the Windows Credential Manager under
**"IronClaw Desktop" / "gateway-token"**; state lives in
`%LOCALAPPDATA%\IronClaw Desktop\`.

Logs go to stderr; `RUST_LOG=ic_widget=debug` for more. A healthy first launch
prints `ironclaw-reborn is ready`, `gateway is ready`, and `the widget created a
thread and started its event pump` — that last line is the proof the webview
loaded and its IPC reached Rust.

The summon hotkey (Ctrl+Alt+Space) is registered best-effort. If another app owns
it you get a warning and the tray still works; nothing else is affected.

### Verified facts that are easy to get wrong

- **Threads survive a gateway restart.** They are persisted through the
  libSQL-backed root filesystem, *not* a `threads` table — a schema dump of
  `reborn-local-dev.db` shows no thread rows, which looks like they are in-memory.
  They are not. The Phase 2b sessions panel can rely on `GET /threads`.
- **Nothing about the widget's own behavior is observable in that database.** Use
  the stderr log, not the DB, to check whether the UI is alive.

## Phase 2b notes — dashboard panels + LLM wiring (recorded 2026-07-10)

`crates/ic_widget` + `ui/` + `crates/ic_llama` (+
`docs/desktop/llm-provider-selection.md`). No IronClaw core crate was touched.
The interactive panels are done and **verified by a manual smoke run on
2026-07-10**: `cargo run -p ic_widget --features app` launched the widget, the
supervised gateway reached ready, and the dashboard panels rendered. The two
remaining pieces are **explicitly-deferred follow-ups — GGUF download UI and
tokens/sec** (see below); with those outstanding, 2b is functionally complete
but not feature-complete.

> ⚠️ **Superseded (2026-07-14).** Both follow-ups are done, and so is the failover
> decision this section leaves open — see **"Closing the open items"** at the
> bottom of this file. The paragraphs below are kept as the record of what was
> true in July; where they conflict with that note, the note wins.

### The seam that had to be closed first

- **The widget never started a local model.** `ic_widget` depended on `ic_llama`
  but `main.rs` used none of it: a launched app supervised the gateway with an
  empty `llm_env`, so the agent had no LLM. Phase 1's round-trip only ran through
  `ic_integration_tests`. `spawn_gateway` now brings up the model *before* the
  gateway (which reads `LLM_BASE_URL` once at boot) and keeps the `LocalLlm` in
  app state so its `Drop` stops the sidecar and proxy.
- **The sidecar now dies with the widget under a hard kill.** This needed an
  `ic_llama` addition: `SpawnHook`, an optional callback the sidecar runs against
  its `llama-server` child on *every* (re)spawn. The widget passes one that
  enlists the child in the same Windows Job Object the gateway rides in; a hook
  error fails the spawn and kills the child. Without it, `TerminateProcess` of the
  widget would orphan `llama-server` holding VRAM and a port. Pinned by
  `the_spawn_hook_runs_on_every_spawn_including_restarts` and
  `a_spawn_hook_error_fails_the_attempt_and_kills_the_child`.

### Panels

- **Sessions** (`GET /threads`) and **automations** (`GET /automations`) —
  live routes. Automations are schedule entries, *not* run history; "Run history"
  joins memory/skills/audit in the unavailable-with-reason list.
- **Local model** — read-only: model id, backend, live sidecar state, GPU-layer
  offload, estimated VRAM/RAM, placement warnings. `None` when running without
  local inference. **tokens/sec is deferred** (needs live metrics polling, not the
  static placement).
- **Provider** — the active-provider switch and cloud key manager. See below.

### Provider selection is single-valued, and failover is unbuilt

- `LLM_BACKEND` holds one value, so the local sidecar and a cloud provider are
  mutually exclusive. The dashboard drives a `ProviderSelection` (`Local` |
  `Cloud{id, model}`) persisted in `settings.json`; `apply_provider` persists it,
  tears down the running gateway + model, brings the gateway back up on the new
  selection, and reloads the webviews so they re-establish their client and
  thread. The provider list is read from the **same `providers.json` the gateway
  resolves against** (`ic_widget::providers`), so the dashboard cannot drift from
  the runtime; OAuth/subscription providers with no `api_key_env` are filtered
  out of the key UI. Keys live in the credential store under
  `provider-key/<id>`; `has_provider_key()` is the only thing the UI sees, never
  the key.
- **The v1 "cloud failover when a key is configured" promise is not implemented.**
  `FailoverProvider` exists but is only ever constructed same-backend in one
  production site. The three ways to close it (a core patch, a route-around in the
  `ic_llama` SchemaProxy à la CP-3, or cutting the promise) are written up in
  `docs/desktop/llm-provider-selection.md`. Decide before Phase 6.

Next: finish 2b's deferred follow-ups (**GGUF download UI**, **tokens/sec**) and
do the **manual smoke run**, then **Phase 3 — animated character companion**
(Live2D, `ren_en/` model), then **Phase 4 — browser automation**.
*(All three follow-ups landed — see "Closing the open items" at the bottom.)*

## Phase 3 notes — animated character companion (done, recorded 2026-07-11)

`crates/ic_widget` (`character.rs`, `hit_test.rs`, `settings.rs`, `main.rs`) +
`ui/` (+ `docs/desktop/character-pipeline.md`, which holds the full pipeline
architecture, the licensing gates for Phase 6, and the open follow-ups). No
IronClaw core crate was touched.

- **The Phase 3 compatibility check failed exactly as CLAUDE.md warned, but
  one layer deeper than expected.** The Cubism 5 web core (SDK 5-r.5, core v6)
  moved render orders from `drawables.renderOrders` to
  `model.getRenderOrders()`; pixi-live2d-display 0.4.0 bundles the Cubism 4
  framework, which reads the old field. The crash surfaced as ONE console
  error and a forever-blank canvas, because a throw inside a Pixi ticker
  callback ends the rAF chain — the render ticker died while the model's
  updates kept ticking on the shared ticker. Fixed with a core API bridge plus
  a guarded render loop (`ui/src/character.ts`); the earlier "WebView2 uploads
  blank textures" theory was probably this bug wearing a costume (retest noted
  in the pipeline doc). **Hiyori (Cubism 4) is the dev model and renders;
  Ren (5.3, offscreen blending) stays parked until a Cubism 5 framework.**
- **Per-pixel click-through is split across the IPC boundary** because a
  click-through window's webview receives no mouse events at all: the UI
  rasterizes the chat panel rect + the character's alpha silhouette (GPU
  readback at one texel per 8-px cell, dilated) into `ic_widget::hit_test::
  HitMask`; a Rust poller (`spawn_interaction_watch`) tests the global cursor
  against it and flips `set_ignore_cursor_events`. No mask yet = fully
  interactive (fail-safe). The same poll feeds eye tracking (`cursor://pos`)
  and pauses animation while a fullscreen app is foreground
  (`character://active`, with the Progman/WorkerW desktop-shell exclusion).
- **Layout**: the character stands at the window's bottom edge over the
  transparent desktop; the Phase 2 chat card became a collapsible bubble panel
  above it. Head click (model hit areas, bounding-box top-third fallback)
  toggles the panel; body drag moves the window; an incoming tool gate forces
  the panel open alongside the `concerned` face.
- **The state machine's Phase 5 hooks are live**: `set_character_signals`
  drives `listening` (composer focus) and `speaking` (reply render, ended on a
  reading-time estimate). Lip sync runs off a test-tone stub patched into
  `motionManager.update` — the exact seam Phase 5's TTS amplitude replaces.
  State-entry motions play at FORCE priority so transitions interrupt; idle is
  the exception (the motion manager already loops it).
- **Character choice is a settings toggle** (`settings.json` → `character`,
  `CharacterId` newtype validates the path segment; dashboard picker reads the
  bundled `characters/manifest.json`). `apply_provider` now load-modify-saves
  settings — a rebuilt literal would have wiped the new field.
- **Perf rules landed**: both tickers capped at 30 fps; animation pauses when
  the window is hidden (WebView2 stops rAF) and under fullscreen apps.
- `SpriteRenderer` (PNG poses + CSS cheap-puppet transforms) exists and ships
  no art; the placeholder remains the no-assets fallback.

Next: **Phase 4 — browser automation** (`crates/ic_browser_mcp`).

## Phase 4 notes — browser automation (done, recorded 2026-07-11)

`crates/ic_browser_mcp` (new) + `crates/ic_widget` (`browser.rs`, `gateway_client`,
`main.rs`) + **one core patch, CP-4** (`docs/desktop/core-patches.md`). Verified
against real Chrome: navigate, read, find, click, screenshot, and a recoverable
missing-selector error all round-trip through the MCP server.

### The plan above was wrong: stdio MCP does not work in Reborn

`ironclaw_mcp` **hard-rejects `transport = "stdio"`** — *"unsupported until
process-level egress controls land"* — and spawns no processes at all. The stdio
docs in `docs/capabilities/mcp.md` describe the **legacy v1 binary**, not Reborn.
The trap is that a stdio manifest **parses, installs, and activates cleanly**, then
fails at *every* `tools/call`. It looks wired, then dies at runtime.

Loopback HTTP — the obvious fallback — was blocked too: the hosted-MCP lane forced
`https` (impossible for a sidecar, which cannot hold a publicly-trusted cert) and
planned egress with `deny_private_ip_ranges: true` (which denies `127.0.0.1`). No
config, env var, or profile — **including `local-dev-yolo`** — relaxes either.

So **some** core edit was unavoidable. Every alternative was measured first: a WASM
shim hits the same wall one seam over (`extension_surface.rs` hardcodes the same
flag for *every* extension capability — wider blast radius); a native first-party
tool means patching `factory.rs` *and* `ironclaw_first_party_extensions` and pulling
`chromiumoxide` into core; executing browser tools inside the `ic_llama` SchemaProxy
(CP-3 style) would bypass the safety layer and approval flow entirely. **CP-4** is
the smallest cut and lands on the lane upstream already supports: `http` is accepted
for, and only for, a **literal loopback IP** (not `localhost` — a DNS name is
rebindable), and private-range denial is waived for exactly that one endpoint.

### ⚠️ Correction (2026-07-13): CP-4 alone never worked. See CP-5 + the discovery break.

The three claims below marked "free" were read off the *code contract*, not off a
running agent. Driving a real browse end to end on 2026-07-13 found the browser tools
had **never actually worked through the gateway** — two further breaks sat behind
CP-4, both silent:

1. **Tool discovery can never succeed** (`network_policy_missing` — the egress wants a
   staged policy that only a *dispatch* stages, and discovery runs at *activation*).
   Reborn then falls back to the bundled manifest **while reporting `activated: true`**.
   So the manifest is not a template — it is the whole tool list. `ic_browser_mcp::manifest`
   now declares all six capabilities, generated from `protocol::Tool`.
2. **The capability is never granted its own endpoint** (`extension_network_policy`
   builds the allowlist only from credential audiences → empty for a credential-less
   sidecar → rejected in obligation preflight). Fixed by **CP-5**.

Both are written up in `docs/desktop/core-patches.md`. Verified after the fix: the
agent calls `browser_navigate` + `browser_get_text` against real Chrome and answers
from the page. **The lesson worth keeping: the Phase 4 gate drove the sidecar and the
runtime's discovery *code*, but never the runtime's *egress* — so a green suite
coexisted with a browser the agent could not reach. The same shape of gap as "the bug
every unit test passed" below, one layer out.**

### What we got for free by riding the supported lane

- ~~**Schemas come from the live `tools/list`.**~~ **False in practice — see the
  correction above.** Reborn *would* discard the manifest's capability declarations and
  rebuild every capability from our `inputSchema` (`hosted_mcp_discovery.rs`), but that
  discovery call never reaches us. The manifest's declarations are what the agent gets,
  so `protocol::Tool` generates them (schemas included) rather than a single template.
- **Approval gating is enforced in the sidecar, not the runtime.** The runtime's
  own approval flow is a no-op — `default_permission: Ask` is hardcoded for every
  discovered MCP tool and **nothing reads it** (see the security section below). So
  `browser_fill` routes sensitive fields through a human via the sidecar's consent
  gate. This is the one claim here that is *not* free from the hosted-MCP lane; we
  built it.
- **Annotations are load-bearing, not cosmetic.** `readOnlyHint`/`destructiveHint`
  are what promote a tool to `EffectKind::ExternalWrite`. `browser_fill` and
  `browser_click` are annotated destructive — that is what makes "submit this form"
  a write effect rather than a read.

### 🟠 SECURITY — Reborn has no tool-approval prompt; we gate in the sidecar (closed)

**The runtime never prompts before a tool runs.** Left alone, `browser_fill` would
type into password and payment fields with no user prompt, on the default profile,
with nothing switched on. That would break Phase 4 step 3 ("sensitive actions must
route through the approval flow"). **The gap is closed in the sidecar** — see "The
consent gate" below — but the underlying runtime finding stands and is worth
recording, because anything else we build on discovered MCP tools inherits it.
Verified, not inferred:

- `default_permission` is **never read** anywhere in the workspace. Every occurrence
  is a manifest literal, a struct-literal write, a field-to-field copy, or a test
  assertion. There is no `match`/`==`/`if` on it in any decision path.
- `Decision::RequireApproval` has **zero production producers**. The only authorizer
  wired into Reborn composition is `GrantAuthorizer` (`factory.rs:648`, and 4 more
  sites), and it returns only `Allow` or `Deny` (`ironclaw_authorization/src/lib.rs`,
  `authorize_from_grants_with_authority_ceiling`). `LeaseBackedAuthorizer` — the one
  that *could* require approval — exists but is **never wired in**.
- `ActiveExtensionCapability` (`extension_lifecycle.rs:54`) has **no
  `default_permission` field at all**; `from_descriptor` discards it. Then
  `extension_surface.rs:55` mints every active capability a standing grant with
  `expires_at: None`, `max_invocations: None`, and `allowed_effects` = its own
  declared effects. So `ExternalWrite` clears its own ceiling.

The approval machinery downstream (`CapabilityHost`'s `RequireApproval` arm,
`ironclaw_approvals`, `resolve_gate`) is fully built and simply **unreachable**. The
gates the widget *does* handle are **auth** gates (product-auth / credential setup),
not tool approvals — which is why this was easy to mistake for a working gate.

So there is no "prompt fatigue" switch to worry about, because **there is no prompt
to lose**, and no per-capability policy surface in which to pin one. `ExternalWrite`
only widens the effect list; it forces nothing. `ironclaw_safety` contains no
approval-forcing code. `local-dev-yolo` changes filesystem/network/secrets, not
approvals — there are none to change.

**Consequence for us:** the approval requirement cannot be met by riding the
runtime — see the consent gate below for how it is met instead. And it was reported
upstream (privately-first: [#6000](https://github.com/nearai/ironclaw/issues/6000)
asks how to disclose, since the repo has no `SECURITY.md` and private reporting is
disabled). If upstream wires `Decision::RequireApproval` for `Ask` capabilities, the
sidecar gate becomes defence-in-depth rather than the only line.

### The consent gate (how step 3 is actually met)

`ic_browser_mcp::consent` + `::classify`. Sensitive fills route through a human
**in the sidecar** — the last boundary the model cannot route around, because the
sidecar decides, not the prompt (the same move as CP-3). Enforced in
`BrowserSession::fill`: classify → ask → *then* type, so a denied fill never touches
the page.

- **Fail closed, everywhere.** The classifier has three outcomes, not two —
  `Sensitive`, `Benign`, `Unknown` — and **both `Sensitive` and `Unknown` ask**.
  Only a positively-ordinary field types without a prompt. A probe that throws, a
  selector that matches nothing, a shadow-DOM/custom element, an unrecognised input
  type, or a `type="text"` field inside a form that holds a password → all ask. An
  unnecessary prompt is annoying; a missed one is the whole gap back. The hinge is
  `Sensitivity::needs_approval` (`Unknown → true`); flipping it reopens the gap.
- **The channel is the sidecar's stdout/stdin**, which the widget already owns as
  the parent process — no new port, no auth to get wrong. `IC_BROWSER_MCP_APPROVAL`
  out, `IC_BROWSER_MCP_DECISION` in. Every non-yes is a no: no channel (standalone
  sidecar → `DenyAll`), timeout, closed pipe, malformed answer, answer for a
  different request.
- **The prompt shows what will be typed and where** (field label + URL + the value,
  and flags non-HTTPS) — a consent prompt the user can't evaluate isn't consent. The
  widget surfaces it as a red prompt distinct from a normal amber gate, defaulting to
  the safe answer, and the character goes `concerned`.
- **The value never reaches a log** (`FillApproval::redacted`).
- Verified against real Chrome (`tests/real_browser.rs`, `--ignored`) *and* end to
  end through the real sidecar binary's stdout/stdin channel: ordinary field silent,
  password field prompts, deny types nothing, approve types.
- A denial is a **recoverable** `isError`, not a crash — the agent reports it and
  moves on.

### Three timing rules, or you get an extension with no tools

Each one is silent when violated. All three are encoded in `ic_widget::browser`:

1. **The extension catalogue is scanned once, at `serve` boot.** The manifest must
   be on disk *before* the gateway starts — so the sidecar launches first and its
   live port is written into the manifest's `url`.
2. **Discovery runs at *activation*, not at boot.** The gateway calls our
   `tools/list` when the extension is activated, so the sidecar must be listening
   then, and activation is driven against the **running** gateway.
3. **A restart does not re-discover.** `restore_extension_lifecycle_state`
   republishes the *bundled manifest*, which carries only a capability **template**
   — not the six tools. So the widget re-activates on **every** launch.

And a discovery failure makes the gateway **silently** fall back to that template
*while still reporting `activated: true`*. So the widget verifies the capability
count rather than trusting the activation response.

### The bug every unit test passed

The unit tests fill the `ToolExecutor` seam with a fake, so they exercise the MCP
transport and never touch CDP. The CDP layer was dead on arrival against current
Chrome: it emits events `chromiumoxide` 0.7 cannot deserialize, the event pump
treated the first one as fatal and exited, the handler dropped — and every tool call
failed with *"send failed because receiver is gone"*, ~60 ms after a launch that
reported success. **A green suite and a browser that never worked.** The pump now
logs an unparseable event and keeps pumping. Pinned by
`tests/real_browser.rs` (`#[ignore]`d — run with `--ignored`; it needs a browser).

### Other decisions worth keeping

- **Screenshots are viewport JPEGs, not full-page PNGs.** The host caps an MCP
  result at 1 MiB and base64 inflates by a third; a full-page PNG of a real site
  blows the budget and fails with an opaque `response_error`. A q70 viewport JPEG of
  example.com is ~15 KiB.
- **A missing selector is a *recoverable* `isError` result, not a JSON-RPC error.**
  The model must see it and try another selector; a JSON-RPC error fails the whole
  capability. Only a genuinely broken browser is an error.
- **The sidecar rides in the widget's Job Object**, so a hard kill takes the
  automation browser down too — an orphaned Chrome would hold a profile lock and a
  port. `BrowserSession`'s `Drop` deliberately does *not* call
  `Browser::close()`/`kill()`: both are `async`, and the discarded future closed
  nothing while reading like a graceful shutdown.
- **The automation browser never touches the user's real profile** — a dedicated
  user-data dir, so the agent only ever has access to what the user logs into inside
  the automation window themselves.

`cargo test -p ic_integration_tests --test browser_mcp_contract` is the gate: it
drives the real sidecar over HTTP and feeds its `tools/list` through the runtime's
**own** discovery code, so it fails if IronClaw would refuse us — rather than
re-asserting our own beliefs about IronClaw back at ourselves.

**Known-unrelated failures:** three `ironclaw_reborn_composition` tests
(`local_yolo_policy_*`, `local_dev_yolo_shell_*`) fail on Windows with *"backslashes
are not allowed"*. Pre-existing upstream bug, confirmed failing with our core changes
stashed, and filed as
**[nearai/ironclaw#5999](https://github.com/nearai/ironclaw/issues/5999)**. Not caused
by CP-4. Root cause: `build_workspace_filesystems` passes a **host path** where a
`MountAlias` (a `/`-rooted POSIX string) is expected — so it is not really a
UNC/`\\?\` issue at all; a plain `C:\...` fails the same two checks. It bites only
the yolo path, because the ambient alias is built only under `--confirm-host-access`.
**`local-dev-yolo` is therefore completely unusable on Windows** (`ironclaw-reborn
serve --confirm-host-access` fails during runtime assembly) — we don't use that
profile, so it does not block us.

### Upstream issues filed from this phase

| Issue | What | Why we care |
|---|---|---|
| [#5998](https://github.com/nearai/ironclaw/issues/5998) | No transport for a local MCP server (stdio rejected, loopback denied) | **CP-4 gets deleted when this lands** — see `core-patches.md` |
| [#5999](https://github.com/nearai/ironclaw/issues/5999) | `local-dev-yolo` can't start on Windows (host path used as `MountAlias`) | Explains 3 red tests in our baseline; not ours |

Next: **Phase 5 — voice (`crates/ic_voice`)**. Canvas is done (below). The Phase 3
lip-sync test-tone stub is still the seam TTS amplitude replaces.

## Phase 5 notes — canvas (done, recorded 2026-07-11)

`crates/ic_canvas_mcp` (new) + `crates/ic_widget` (`canvas.rs`, `main.rs`) + `ui/`
(`canvas.tsx`, `canvas.html`). **No new core patch** — it reuses CP-4. Verified: the
contract gate (`ic_integration_tests/tests/canvas_mcp_contract.rs`) drives the real
canvas server through the runtime's own discovery code and confirms `canvas_render`
becomes the capability `ic-canvas.canvas_render`.

### The data path is the whole design

The agent runs in `ironclaw-reborn serve`; the render must reach a Tauri window. The
decisive fact: **every gateway→widget channel is content-hostile.** The SSE
`CapabilityDisplayPreview` is `sanitize_text`'d and capped at 16 KiB
(`ironclaw_reborn_composition/src/projection/display_preview.rs`), and a tool
result in the timeline has `content: None`. So HTML routed back through the gateway
would arrive corrupted and truncated. The only way to get raw markup to the window
is to **produce it inside the widget process.**

So `canvas_render` is served by an MCP server that runs **in-process in `ic_widget`**
(not a child, unlike the browser sidecar). When the agent calls it, Reborn POSTs the
arguments to the loopback URL (CP-4), the in-proc `tools/call` handler holds the raw
HTML, and hands it to a `CanvasSink` → `app.emit("canvas://render", …)` to the canvas
window. The markup never touches the gateway. A WASM tool or a native first-party
tool would both strand the HTML in the *runtime* process and each need a new
unsanitized projection channel — the exact core surface the fork avoids.

### Rendering safety: isolation, not sanitization

Agent markup is untrusted (a prompt-injected agent could emit hostile HTML). It
renders in an iframe with an **empty `sandbox`** (no scripts, no same-origin, no
forms, no navigation) under an injected `default-src 'none'` CSP (inline styles +
`data:` images only). Static HTML and inline SVG render; scripts and every network
fetch are inert. We deliberately do **not** strip-sanitize (ammonia/DOMPurify): the
sandbox+CSP already bound what the content can do, and stripping would break
legitimate inline SVG. The shell assigns `iframe.srcdoc`, never `innerHTML`, so the
markup is always parsed as a separate isolated document. The global app CSP gained
one token, `frame-src 'self'`, to permit the srcdoc frame (no other window uses
frames, so the widening is inert for them).

### Reused from Phase 4, exactly

The `ic-canvas` manifest, install/activate, and the three timing rules (manifest on
disk before boot; discovery at activation; re-activate every launch and verify the
capability count) are the browser's, one more extension. `GatewayClient::install_extension`
/`activate_extension`/`extension_capabilities` already existed. First-open race (an
event before the shell's listener attaches) is covered by storing the last render in
app state and having the shell fetch it via the `canvas_content` command on mount.

### Voice — decisions made (2026-07-11), build in progress

The plan was pressure-tested and the user made the three genuine calls:

- **Wake word: `rustpotter`** (MIT/Apache, pure Rust, **no ort/ONNX dependency**).
  Ship our own reference models (trained from recordings of the wake phrase) so the
  license is clean. openWakeWord is out (its pretrained models are CC-BY-NC).
- **TTS: the archived `rhasspy/piper` MIT `piper.exe`** — a self-contained binary,
  bundled and invoked as a Job-Object-supervised subprocess like `llama-server`. **Not**
  `piper1-gpl` (no binary; GPL). Repo is archived/unmaintained but functional. Pin a
  specific build and a **CC-BY-4.0 voice** (verify its MODEL_CARD; some voices are
  non-commercial). Piper emits no timing → compute an **RMS envelope from the output
  PCM** for lip sync.
- **VAD: `voice_activity_detector`** (Silero v5 ONNX, MIT). This *does* pull in `ort`
  even though rustpotter dropped it — so `ort` returns for VAD alone; bundle the ORT
  DLL beside the exe (`copy-dylibs`) to dodge the System32 clash.
- **STT: `whisper-rs`** 0.16 (MIT), `base.en` (q5_1), **CPU first** — Vulkan silently
  no-ops on Windows static builds (whisper.cpp #3750).
- **Sequencing: build voice fully now** (not a thin slice, not Phase 6 first).

Settled architecture: `crates/ic_voice` is a **library crate linked into `ic_widget`**
(needs `AppHandle` for lip-sync events, tray mute, and the existing `GatewayClient`) —
voice is an alternate *input* to the same chat path, not a new gateway channel — with
**only `piper.exe` as a supervised subprocess**. Reuse `ic_llama::download::Downloader`
and `SpawnHook` directly for models + the TTS child; `Sidecar`/`release.rs`/`ModelStore`
are blueprints. Device-change needs a hand-written `IMMNotificationClient` (cpal has no
API for it). Capture is cpal + `rubato` (downmix mono, resample 48k→16k f32).

### Voice — done (recorded 2026-07-12)

`crates/ic_voice` (new, 64 tests) + vendored `crates-src/rustpotter` + `ic_widget`
(`voice.rs`, `job_object.rs`, `main.rs`, settings) + `ui/`. No IronClaw core crate
touched. **Full write-up + every trap: `docs/desktop/voice-notes.md`** — read it
before touching voice. The load-bearing surprises, in one breath:

- **The wake-word library from the plan is unusable as published.** rustpotter's only
  non-yanked crate (3.0.2) won't compile (candle 0.2 vs modern `rand`/`half`); every
  2.x is yanked. **User's call: vendor rustpotter 2.0.1** (Apache-2.0, pure DSP, the
  reference-model spotter we actually want) at `crates-src/rustpotter`, path-dep +
  workspace-`exclude`d. No wake models are recorded yet → `NullWakeWord` + the
  **summon hotkey as push-to-talk** until they are.
- **STT now needs two build prerequisites: CMake + LLVM/libclang** (whisper.cpp build
  + bindgen). `winget install Kitware.CMake LLVM.LLVM`; build with `LIBCLANG_PATH` set
  and CMake on `PATH`. The `WHISPER_DONT_GENERATE_BINDINGS` shortcut is a dead end
  (its committed bindings fail layout asserts). Users need neither — Phase 6 ships
  built artifacts. This is the third build-env doc after `windows-build.md`.
- **Pins the plan didn't foresee:** `rubato = 0.16` (3/4 rewrote the API), cpal 0.18's
  `SampleRate` is a `u32` alias with by-value stream configs, `whisper-rs = 0.16` CPU.
- **A regression the voice deps caused elsewhere:** linking ONNX Runtime made the
  `job_object` kill-on-close test's `ping` sleeper fail (exit 1). Fixed by switching
  the test sleeper to PowerShell `Start-Sleep` (network-free). Kill-on-close still
  verified.
- **Everything model/hardware-bound is behind a trait with a fake**, so the whole
  loop — wake → VAD/endpoint → STT → gateway turn → TTS → playback, plus barge-in and
  mute — is tested with no mic and no models. Real impls have `#[ignore]`d asset tests.
- **The reply path reuses the typed chat exactly** (`voice::drive_turn`: send → await
  terminal run → read timeline, since the reply text isn't on the event stream), on
  voice's own thread. **Lip sync** feeds Piper's PCM RMS into `ParamMouthOpenY`
  (`voice://amplitude` → `setMouthOpen`), with the Phase 3 test tone kept as the
  no-audio fallback. Voice is **opt-in** (`settings.voice_enabled`, default off — first
  enable downloads ~210 MB); tray mute + `voice_status`/`set_voice_*` commands exist.

Next: **Phase 6 — packaging & hardening** (bundle piper/whisper/voice/ORT binaries,
verify the amy voice licence, record + bundle wake models, first-run wizard).

## Phase 7a notes — proactive surfacing plumbing (done, recorded 2026-07-14)

`crates/ic_widget` (new `ambient/` module: `mod.rs`, `guardrail.rs`, `log.rs`,
`automations.rs`; plus `character.rs`, `settings.rs`, `supervisor.rs`, `main.rs`) +
`ui/` + a new gate, `ic_integration_tests/tests/ambient_surfacing.rs`. **No core
patch** — nothing in 7a needed one. Verified by a manual smoke run on 2026-07-14
(ambient off: silent, no poller; ambient on: ambient thread opened and persisted,
watcher primed) and by the new gate, which drives a **real** trigger fire through a
real `serve` and runs the shipping watcher over the result.

### The ⚠️ VERIFY item, answered — and the plan was wrong in one load-bearing way

The spec said "scheduled proactivity through the existing triggers/automations lane
(`GET /automations` is already surfaced)". It is surfaced. It also **never fires**.

- **The trigger poller is off by default.** `TriggerPollerSettings::default()` has
  `enabled: false` (`runtime_input.rs`), so `ironclaw-reborn serve` composes the
  poller and never starts it. A schedule the agent creates is listed by
  `GET /automations`, shows a `next_run_at`, and sits there forever. The switch is
  `IRONCLAW_TRIGGER_POLLER_ENABLED=1|true` (env beats config; `_INTERVAL_SECS` tunes
  the poll, min 1 s — the *fire* cadence floor is still cron's 60 s). Read once at
  boot, like `LLM_BASE_URL` — **so the ambient toggle restarts the gateway**, and
  `apply_provider`'s teardown/bring-up became the shared `restart_gateway`.
- **We tie the poller to `settings.ambient_enabled` deliberately, and that is a
  security decision, not a convenience one.** `builtin__trigger_create` is declared
  `PermissionMode::Ask` and **runs with no prompt** — the Phase 4 finding
  (`default_permission` is read by nothing), now confirmed live in this lane: the mock
  agent armed a recurring schedule and the run just completed. An agent talked into
  arming a minute-by-minute trigger would otherwise own a heartbeat the user never
  granted. Ambient off ⇒ the poller is absent ⇒ **no unprompted run can exist at
  all.** Pinned by `a_schedule_never_fires_while_the_trigger_poller_is_off`.
- **A fire lands in a brand-new thread, one per fire** — not the ambient thread, not
  the chat thread. `TriggerFireIdentity` hashes `(domain, tenant, trigger, fire_slot)`
  into a `route_thread_id`, which is a *binding key*, not a `ThreadId`; the binding
  misses and mints a fresh UUID thread (`conversations/memory.rs`). It is owned by the
  same scope, so `GET /threads` lists it, its timeline reads normally, and its
  projection SSE works. Ordinary in every way except that nobody told you it exists.
- **Nothing correlates an automation to the thread its run landed in.**
  `RebornAutomationInfo` carries no `thread_id` and no `run_id` (`TriggerRecord` *has*
  an `active_run_ref`; `trigger_output` does not emit it). So the watcher pairs them
  **by timing**: between two polls, one completed automation + one new thread = the
  same event; anything else surfaces the automation's name and status with **no body**,
  because a wrong body is worse than none. This is the one honest weak spot in 7a, and
  it is a missing field upstream, not a design choice here — filed as
  [nearai/ironclaw#6076](https://github.com/nearai/ironclaw/issues/6076); the timing
  pairing gets deleted when it lands.

### What "the character speaks first" is made of

- **`ambient::guardrail::check` is pure and fails closed.** Four ways to be suppressed
  (`Disabled` → `QuietHours` → `AlreadySurfaced` → `Dismissed` → `RateCap`), one way
  through. Defaults: **off**, ≤ **2/hour**, quiet **22:00–08:00 local**. Quiet hours
  are a *local* wall-clock question and the cap is an *elapsed-time* one, so both come
  off one `chrono` instant. `start == end` is an **empty** window, not a full day — a
  quiet period that silenced everything forever is indistinguishable from the feature
  being broken.
- **The log is the rate limiter's memory, and it is append-only JSONL**
  (`%LOCALAPPDATA%\IronClaw Desktop\ambient-log.jsonl`). "Not now" is the user's answer
  to the agent, so it is retained and timestamped, never rewritten — the inherited
  LLM-data invariant. A line this build cannot read is skipped **and left on disk**;
  rewriting the file to "clean" it would be the deletion the invariant forbids. If the
  log cannot be written, `propose` **declines to surface**: no memory ⇒ no cap ⇒ stay
  quiet.
- **Two keys, not one.** `key` = `automation:<id>:<last_run_at>` (this exact run, shown
  once ever); `source` = `automation:<id>` (what a "Not now" quiets, for an hour). Share
  them and a dismissal either silences the automation forever or not at all.
- **Priming.** The watcher's first tick records and surfaces **nothing**. A run that
  finished while the app was closed is not news, and greeting the user with last
  night's digest every morning is how a companion gets switched off for good.
- **`suggesting`** joins the character states, ranked **below** work/gates and **above**
  speech: what the user asked for always beats what the character volunteered, but an
  unanswered question it asked outranks it finishing a sentence. The bubble card is the
  calmest of the three (blue, not amber/red) — an offer must never read as an alarm.
  **Accept** means "show me": it repoints both windows at the run's thread. Both answers
  are recorded.
- **The ambient thread is real plumbing, and in 7a only `ensure_thread` has an in-app
  caller.** It is created on first enable, **persisted** (`settings.ambient.thread_id`),
  reused across launches, and replaced if the store was wiped. `AmbientService::ask`
  (send → await terminal → read timeline, reusing `voice::drive_turn`) is what 7b's
  reflection turn and 7c's skill review send down; in 7a it is exercised by the gate
  test against the real gateway, not by app code. That is the "shared plumbing" the
  spec asked for, stated plainly rather than given a fake consumer.

### The gate

`cargo test -p ic_integration_tests --features webui-v2-beta --test ambient_surfacing`
(3 tests, ~115 s — the cron floor is 60 s). It spawns `serve`, has the agent create a
**real** trigger through `builtin__trigger_create` (the mock LLM now emits tool calls —
`MockReply::ToolCall`, content-conditioned because chat, ambient, and the fired run are
all in flight against one mock), waits for the gateway's **own** poller to fire it, then
runs the shipping `AutomationWatch` over the result and asserts one suggestion carrying
the agent's actual answer and pointing at the right thread. `ic_integration_tests` now
dev-depends on `ic_widget` (a dev-dep cycle, which Cargo permits) so the gate drives the
code that ships instead of a re-implementation that agrees with us. **This is the Phase 4
lesson applied up front:** the earlier probe of this lane was written *before* any feature
code, and it is what caught the disabled poller — a green suite over our own beliefs would
have shipped a feature whose automations never run.

### Incidental fix

`cargo fmt --all -- --check` (the CI quality gate) has been **failing since Phase 5** on
the *vendored* `crates-src/rustpotter`, which `--all` reaches through the path dep.
Formatted it (whitespace only; rustfmt's `ignore` key is nightly-only, so a config fix
was not available on stable). The fork crates were already clean.

Next: **Phase 7b — self-learning skills** (reflection turn on the ambient thread, draft
SKILL.md, consent-gated install). Its first job is the ⚠️ VERIFY item: `builtin__skill_install`
and `builtin__skill_list` **are** model-visible in `serve` under `local-dev` (seen in the
28-tool list during the 7a probe) — so confirm the *install path* end to end against the
running gateway before building any UI, and remember that `Ask` is not enforced, so the
consent gate is ours to build (as in Phase 4).

## Phase 7b notes — self-learning skills (done, recorded 2026-07-14)

`crates/ic_widget` (new `ambient/reflection.rs`; `ambient/mod.rs`, `automations.rs`,
`settings.rs`, `main.rs`) + `ui/` + two new gates
(`ic_integration_tests/tests/skill_install.rs`, `tests/skill_reflection.rs`) and one
harness affordance (`RebornServer::start_scripted_in_home` — consecutive servers over
one caller-owned home, the only way a test can watch a restart). **No core patch.**
Related upstream filing from this phase: [nearai/ironclaw#6076](https://github.com/nearai/ironclaw/issues/6076)
(automations carry no thread/run correlation — 7a's weak spot, filed with approval).

### The ⚠️ VERIFY item, answered by a probe before any feature code

`tests/skill_install.rs` drives `builtin__skill_install` through a real `serve`
(the Phase 4 lesson applied: a listed capability is not a reachable one). Findings,
each pinned by an assertion:

- **Install is reachable — the opposite of the Phase 4 egress gap.**
  `local_dev_capability_policy.toml` grants `skill_install` exactly the effects it
  declares. The agent installed a skill from inline `content` end to end.
- **`PermissionMode::Ask` ran unprompted** — third confirmation of the Phase 4
  finding. The consent gate is ours, as the plan assumed.
- **Skills are plain files**, not libSQL rows:
  `<IRONCLAW_REBORN_HOME>/local-dev/skills/<name>/SKILL.md`, re-read lazily. They
  survive a restart iff the home does (ours is stable).
- **Install ≠ in-context.** The local-dev selector is `ExplicitOnly`; the skill
  reaches the model only when the agent calls `builtin__skill_activate`. And the
  decisive fact: an inline-content install lands as **`source: user` — the trusted
  tier — so activation injects the FULL body**, not the description-only fate of
  URL-provenance installs. Without that, a learned skill would be a name, not a
  procedure. Pinned; if upstream changes the trust assignment, the gate says so.

### What 7b is, mechanically

- **`RunWatch` (edge, not level).** The projection stream repeats `run_status`
  every poll and replays it on every snapshot, so "completed" must be detected as
  an in-flight → `Completed` **transition**. A run already terminal at first sight
  is history, not news (the automations watcher's priming rule, same shape).
  Failed/cancelled runs never fire. Hooked in `pump_events`; both toggles
  (`ambient_enabled` + the new `settings.reflection_enabled`, default **off**) are
  read at fire time, so no restart.
- **The reflection turn rides 7a's plumbing exactly**: `AmbientService::ask` on the
  ambient thread — and the transcript travels *in the prompt*, because the ambient
  thread knows nothing of the chat thread (separate conversations, by design).
  Tail-truncated, whole messages only.
- **Parsing fails closed.** A draft is a fenced (or bare) SKILL.md with top-level
  `name:` + `description:` frontmatter and a non-empty body; the name is validated
  kebab-case, bounded, and not a Windows reserved device name. "NO", prose, or
  anything malformed proposes nothing.
- **Dedupe is three layers, cheapest first**: the cap needs no LLM turn (accepted
  `reflection:*` sources ∩ disk — removing a skill frees its slot, default 50);
  a name already on disk declines; and the guardrail's exact-key memory makes
  `skill:<name>` (no per-run component, deliberately) a **once-ever** offer — that
  existing 7a mechanism *is* "a rejected draft is never re-proposed".
- **Consent is the red card** (the Phase 4 `ask-fill` pattern, not the blue offer):
  full draft text in the bubble, No is the default and the focused button. An
  Accept installs **deterministically — a validated file write** to the skills
  root, no LLM between the yes and the write. This is the "cleaner seam" the spec
  allowed: a user-placed skill is *identical* in trust and effect to an
  inline-content capability install (verified above), and it cannot mangle the
  text the user just approved. `main.rs` `respond_suggestion` routes
  `SuggestionKind::SkillDraft` accepts through it and reports via
  `ambient://install-result`.
- **The model-quality constraint is surfaced, not solved** (per spec): the ambient
  panel shows "skills learn better with a stronger model" while a local model is
  active and reflection is on. Multi-model routing stays tracked in
  `docs/desktop/llm-provider-selection.md`.

### Two honest limitations, stated rather than papered over

- **The reflection turn runs with the agent's full tool surface and no runtime
  gate** (Phase 4). A model that disobeys "do NOT install anything" could call
  `builtin__skill_install` mid-reflection and nothing would stop it. Not
  preventable from the widget; it **is detected** — `reflect` snapshots the skills
  root around the turn and logs an error naming any skill that appeared without
  consent. If upstream ever wires `Decision::RequireApproval`, this becomes
  defence-in-depth.
- **Dedupe cannot see bundled system skills** (embedded in the binary, not on
  disk), so a draft could shadow one by name; `skill_listing` keys on
  `(name, source)`, so both would list. Low stakes, worth knowing.

### A finding that belongs to 7c

Nothing scans skill text on install: `ironclaw_skills/src/gating.rs` checks only
*environment requirements* (bins/env/config), and the context-build scan
(`skill_context.rs`) is structural (control chars, host paths, handle markers,
byte budgets — fail-closed) not semantic. Combined with content installs landing
**trusted with full-body injection**, 7c's review-before-install UI is not a
nicety — it is the only gate a third-party skill passes through.

### The gates

`cargo test -p ic_integration_tests --features webui-v2-beta` now includes
`skill_install` (install → restart → list → activate → body-injected) and
`skill_reflection` (task completes → reflection drafts → consent card → install
into the real skills root → never re-proposed → cap declines before the LLM
turn). Together they are the 7b definition-of-done chain. Unit tests cover the
parser's hostile cases, `RunWatch`, the cap arithmetic, and the installer's
refusals.

Next: **Phase 7c — skill import** (local folder, frontmatter validation,
review-before-install through the same consent card). Its ⚠️ VERIFY item is
answered above: treat skill text as untrusted, because the runtime does not.

## Phase 7c notes — skill import (done, recorded 2026-07-14)

`crates/ic_widget` (new `skill_import.rs`; `ambient/mod.rs`, `ambient/reflection.rs`,
`main.rs`, `lib.rs`) + `ui/` + a new gate
(`ic_integration_tests/tests/skill_import_gate.rs`). **No core patch.**

- **The ⚠️ VERIFY item was answered in 7b and it is the whole design**: the runtime
  scans skill text for *nothing* on install (`gating.rs` = environment requirements
  only; the context-build scan is structural, not semantic), and an installed skill
  is the trusted tier with full-body injection. So the review is the only gate a
  third-party skill ever passes through — which is why the dashboard shows the
  **entire SKILL.md**, not a summary, under a warning that says exactly that.
- **Two steps, both explicit.** The dashboard is the review (path in → `preview`,
  a pure read: name, description, full text, bundle file list with sizes); the
  final consent is the **red card on the character's bubble** — the same
  `SkillDraft` machinery, a new `SuggestionKind::SkillImport`. An import is
  *solicited*, so it lives outside the ambient service on purpose: it works with
  ambient off, never consults the guardrail, and never spends a rate-cap slot.
  Its consent trail is its own append-only `skill-import-log.jsonl` — writing it
  into the ambient log would desync the running service's in-memory view and
  let solicited imports eat unsolicited-surfacing slots on the next launch.
- **What installs is what was reviewed, verbatim.** `install` takes the reviewed
  SKILL.md *text* back in and writes it — never a re-read of the folder, which
  could have changed between the review and the yes (pinned by
  `install_writes_the_reviewed_text_not_the_folder`). Bundle data files are
  copied fresh (their names and sizes were the reviewed part) with caps
  re-checked, and a failed install removes the half-written directory.
- **The runtime's own bundle limits are mirrored and enforced** —
  `MAX_INSTALL_BUNDLE_*` (256 files / 2 MiB per file / 20 MiB total), so an
  import can never admit more than `builtin__skill_install` would have. Path
  rules per component (no control chars, no `:`, no Windows reserved device
  names), and **symlinks are refused outright**: a link inside the folder can
  point anywhere on the machine, and "import this folder" must never quietly
  become "import whatever this folder points at".
- **A parser fix this surfaced (also benefits 7b):** `parse_draft` shredded a
  bare SKILL.md whose *body* contained fenced code blocks — real skills carry
  code examples — into inner candidates and never tried the whole. The whole
  reply is now always the last candidate, and imports use the stricter
  `parse_skill_md` (the file *is* the candidate; a prose file with a fenced
  skill inside is not itself a skill).
- **No new plugin for folder picking**: the panel takes a typed/pasted path. A
  native dialog means adding `tauri-plugin-dialog`; worth it later, not
  load-bearing now.

The gate (`skill_import_gate`) drives the shipping `preview` + `install` pair
against a **running** `serve` and asserts the agent's very next `skill_list`
names the import — no restart, because the skills root is read lazily. Together
with the 7b chain (root → listed → activatable → body injected) that closes the
import path end to end. Unit tests cover the refusals: no SKILL.md, invalid
frontmatter, oversized file/bundle, reserved names, symlinks, overwrite, and
the half-install cleanup.

Next: **Phase 7d — ambient watchers** (opt-in signals, each defaulting OFF;
v1 gate is rule-based, not LLM-based; wake-word models remain the outstanding
Phase 6 item).

## Phase 7d notes — ambient watchers (done, recorded 2026-07-14)

`crates/ic_widget` (new `ambient/watch.rs`; `ambient/mod.rs`, `automations.rs`,
`settings.rs`, `main.rs`) + `ui/` + a new gate
(`ic_integration_tests/tests/watcher_fire.rs`) + one new dependency (`notify`,
for folder events). **No core patch.**

- **The engine is a pure state machine** (`WatchEngine`), fed signals by the
  app, so every firing rule is a unit test rather than a wait. Three kinds:
  foreground window title (extends the Phase 3 Win32 machinery with a
  `GetWindowTextW` sampler — a *title*, never screen content), watched folders
  (`notify`, recursive, recreated when the rule set's paths change), and the
  local clock. Each kind is its own opt-in, all default OFF, honoured **inside
  the engine** as well as by the caller — an engine that trusts its caller is
  one settings refactor away from watching something the user switched off.
- **Edges, priming, cooldown — the anti-nag rules.** A foreground rule fires on
  the match *transition* (the first sample only primes); a time rule fires once
  a day and a mark that already passed at startup is primed as spent (the 9 am
  rule must not greet a 3 pm launch); every rule has a 30-min cooldown so one
  noisy folder mid-build cannot spend the hourly guardrail cap alone.
- **v1 is rule-based, not LLM-based, per spec** — the only inference a watcher
  runs is the prompt the user wrote. And it runs *after* the guardrail:
  `AmbientService::would_allow` (a non-recording pre-check) means a firing
  under quiet hours or a spent cap costs **no thread and no turn** — pinned by
  `a_suppressed_firing_spends_no_llm_turn`, which asserts the mock saw no
  request. `propose` still re-checks and records when the answer arrives.
- **A firing lands in a fresh thread**, like the gateway's own trigger fires —
  the ambient thread stays the app's private conversation, and Accept opens a
  transcript holding exactly this rule's question and answer. Surfaced as
  `SuggestionKind::Watcher`, a calm blue offer (the widget's card predicate now
  names the two skill kinds as the only red consent prompts).
- **Rules are config, not LLM data** — editable and deletable in the dashboard
  panel, unlike the logs. Rule edits and kind toggles are read every 3 s
  sample, no restart; only the ambient master switch restarts (the gateway's
  boot-time constraint, not this loop's). Rules are validated on save (a
  non-folder path, an empty needle, an impossible time are refused with
  reasons; an empty needle would otherwise fire on every window change).

### Phase 7 definition of done — accounting

- ✅ a scheduled automation surfaces as a character suggestion (7a, gated)
- ✅ a completed task yields a "want me to remember this?" prompt whose approval
  installs a skill active in the next session (7b, gated end to end)
- ✅ a third-party SKILL.md folder imports after review (7c, gated)
- ✅ every surfacing respects the rate cap and quiet hours (one guardrail path;
  7d additionally pre-checks before spending turns)
- ✅ a "Not now" suppresses re-surfacing (7a log; exact-key = once ever)
- ✅ all gates green + manual smoke on Windows, per sub-phase
- Outstanding from Phase 6, unchanged: wake-word models (voice as a third
  sense; push-to-talk remains), certificate/updater/MSI externals.

Next: **Phase 8 — surfacing the runtime** (dashboard shell + chat management,
connectors, voice picker, universal gates, skill repos, channels, memory
seeding). Phase 7 is complete; the 2b follow-ups and the failover call are
closed too (see "Closing the open items" below). What remains outside Phase 8
is Phase 6's external-input gates: cert, updater keypair, MSI on a clean VM,
and the wake-word recordings.

## Closing the open items (recorded 2026-07-14)

Everything that was outstanding except the **installer itself**. Two of the four
turned out to be *mostly built already* — the notes above were stale, which is
its own lesson: check the code before believing a TODO.

`crates/ic_llama` (`proxy.rs`, `local_llm.rs`, `lib.rs`) + `crates/ic_widget`
(`providers.rs`, `settings.rs`, `main.rs`) + `ui/` + `docs/desktop/llm-provider-selection.md`.
**No core patch.**

### 1. Cloud failover — decided (option 2) and built

The v1 promise ("answer with a local GGUF model — with cloud failover when a key
is configured") is now kept, without touching a core crate. **The proxy owns the
retry**, exactly as `llm-provider-selection.md` recommended: `CloudFallback` names
an OpenAI-shaped endpoint + key + model, and `relay()` retries a **chat completion**
there when the sidecar refuses the connection or answers `5xx`. The full write-up
is in that doc; the load-bearing findings:

- **The gateway never learns it happened.** One `openai_compatible` endpoint, no
  second `LLM_BACKEND`, no restart, no core patch.
- **The cloud key never enters the gateway's environment** — it is read from the
  credential store into the widget's own process. The predicted security win, real.
- **The sidecar's throwaway bearer is stripped** before the cloud call: forwarding
  it would leak a local secret to a third party and authenticate nothing.
- **Only a chat completion fails over.** A failing `/v1/models` probe means the
  local server is down; answering it from the cloud would advertise models the
  sidecar does not have.
- **Not every provider can serve.** The proxy forwards the body the gateway already
  built, so a fallback must speak OpenAI Chat Completions. `providers.json` says
  `anthropic` speaks `anthropic` and `deepseek` speaks `deep_seek` — so the picker
  offers only `open_ai_completions` providers **plus Anthropic via its documented
  OpenAI-compatible layer**. `Provider::can_fail_over()` is the one place that
  decides, and `set_cloud_fallback` refuses a keyless or incompatible choice at the
  command boundary rather than storing a setting that silently never fires.
- **With no fallback, a dead local model is still an honest `502`.**

Pinned by three end-to-end proxy tests (dead sidecar → cloud answers; no fallback
→ 502; healthy sidecar → the cloud never sees the request *or* the key).

### 2. tokens/sec — built, in the only place that can see it

The gateway's event stream carries **no token usage at all**, so nothing downstream
of it can count. The proxy can: it reads `llama-server`'s `timings.predicted_per_second`
off each non-streamed completion (falling back to tokens-over-wall-clock, which reads
a little low because it includes the prompt pass). Surfaced in the model panel as
Speed / Last turn, polled every 3 s **only while a model is loaded**. The panel also
flags when the last answer came from the cloud — a silent failover would otherwise
look like a suspiciously fast GPU.

### 3. GGUF download UI — was already built; the missing half added

The panel (catalog, progress, cancel, resume, digest verify, custom repo/file,
remove) shipped in `de702a9`; the notes above never got updated. What was actually
missing: **nothing let the user choose which installed model runs.** `launch_local_model`
took "the first one that isn't suspect". Added `settings.default_model` + a **Use this**
button per installed model, which pins it and restarts the sidecar onto it (a model is
chosen at launch, so a new choice needs a new sidecar — and the gateway behind it,
which holds the proxy URL). A pin that no longer resolves (removed, or gone suspect)
falls back to the old rule rather than refusing to start.

### 4. Wake word — was already built; the last hop closed

Record → train → `.rpw` → `RustpotterWake` all existed, and `voice.rs` already swaps
`NullWakeWord` for the real spotter automatically when a model is present. The gap:
**the pipeline reads its models once, at start**, so a word trained under a running
pipeline did nothing until the next launch — the feature looked broken at exactly the
moment the user tried it. `train_wake_word` now restarts voice itself (new
`restart_voice` helper) and clears the banked takes, so a retrain is not trained on
both voices. Still outstanding and **genuinely external**: the user must speak the
recordings, and the `#[ignore]`d detection test needs a real `.rpw` + WAV.

### What remains

**Only the installer**, and what it needs from outside the repo: a code-signing
certificate, an updater keypair + endpoint, and a clean-VM MSI build with the manual
failure drills (`docs/desktop/packaging.md` has the checklist and the config
templates). No code is blocked on any of it.

Next: **Phase 8 — surfacing the runtime.** An audit against the serve route table
found whole tiers of runtime capability the product never surfaces; Phase 8 wires
the highest-value ones in, starting with 8a (dashboard shell + chat management).

## Phase 8a notes — dashboard shell + chat management (recorded 2026-07-14)

`crates/ic_widget` (`main.rs`, `gateway_client/mod.rs`, `error.rs`, `settings.rs`) +
`ui/` (`dashboard.tsx`, `chat.ts`, `widget.tsx`, `api.ts`, `styles.css`) + a new gate
(`ic_integration_tests/tests/chat_control.rs`). **No core patch.** Every contract
below was verified against the pinned upstream commit **`a492857`**
(`reborn-integration`) by driving a real `serve` — on the next upstream merge those
assertions are the alarm.

### The three ⚠️ VERIFY items, answered before any feature code

**1. Nothing deletes a thread.** All five plausible spellings 404
(`DELETE /threads/{id}`, `POST .../delete`, `.../archive`, `.../hide`;
`DELETE .../messages` is a 405), and the thread survives every attempt. So the
button says **Hide**, and means it: the conversation stays in the gateway exactly
as the agent left it, and the widget keeps a local `settings.hidden_threads` list
of what not to show. No libSQL was touched — that would couple us to internals and
break the never-delete-LLM-data invariant.

**2. Stop does not stop the model.** This is the finding that reshapes the feature.
`cancel_run` answers `status: "CancelRequested"` — a *request*, not a completion —
and a slow-provider mock proves the gateway **leaves its in-flight HTTP request to
the LLM open**: three seconds after the cancel, the provider request was neither
aborted nor answered. The run does go terminal on the projection stream, so the UI
recovers correctly; but with a local model, `llama-server` keeps generating to
completion and the GPU burns tokens nobody will read. **Stop means "stop showing me
this", not "stop computing this."** Pinned by
`cancelling_an_in_flight_run_reports_terminal_and_what_it_does_to_the_provider`,
whose verdict line prints which way it went — if upstream ever wires cancellation
through to the provider request, that test says so.

**3. 🚨 `POST /llm/test-connection` cannot test a connection — 8a.5 is NOT built.**
The spec asked for a Test-connection button "instead of restarting the gateway to
find out a key works". The route exists and is inert: `LlmProvider::list_models()`
has a **default impl returning an empty list** (`ironclaw_llm/src/provider.rs:494`),
and **`RigAdapter` — which serves OpenAI, Anthropic, Ollama and `openai_compatible`,
i.e. every provider our picker can configure — never overrides it**.
`test_connection` (`llm_config_service.rs:536`) asks the adapter for a model list,
gets an empty one, and reports **`ok: true`** without opening a socket. Proven by
probing a **port with nothing listening on it, using a junk key**: it reports
"connection ok". A button over this would show a green tick for a bogus key against
a dead endpoint — worse than no button — and a model dropdown fed by
`/llm/list-models` would always be empty. **Reported rather than adapted, per the
rules of engagement; awaiting a decision.** Pinned by
`the_provider_probe_reports_ok_for_an_endpoint_that_does_not_exist`, which starts
failing the day upstream implements `list_models` for `RigAdapter` — that is the
signal the button can finally exist.

### What shipped

- **The sidebar landed as its own logic-free commit** (`19d9d66`), as required.
  Each existing panel is *wrapped* in a nav `Show` rather than extracted into a
  routed component, so there is no seam through which a panel's behaviour could
  have shifted — the diff is a section registry, a signal, and moved JSX.
  **Connectors is deliberately absent** from the sidebar: it arrives with 8b, and a
  nav entry that opens an empty panel is worse than no entry.
- **Chats panel**: `GET /threads` with cursor paging (the widget's client gained
  `list_threads_page`), read any conversation's history through the timeline,
  **Continue** to point both windows at it, **Hide/Unhide**. Switching conversations
  while a run is in flight is **blocked** with "finish or stop that answer first" —
  otherwise the reply lands in a thread nobody is watching. A message kind this
  build has never met (a summary, a tool result with `content: null`) renders as a
  neutral event row; an unknown kind must never be able to crash the panel.
- **Stop is now in the bubble**, which is where the user actually is — it only ever
  existed in the dashboard, which is closed most of the time. It says "Stopping…"
  from the moment it is clicked rather than waiting for the gateway to admit the
  cancel (which takes a poll or two).
- **The two Stop races no longer show a dialog.** An already-finished run
  (`already_terminal`, the common case — the reply lands as the click flies) and an
  unknown run id (a clean `404`) both now mean "collect the reply and move on".
  Before this, a stale run id produced a red "Could not stop" the user could do
  nothing about.
- **One chat per window.** The Dashboard now owns the `createChat()` and hands it to
  both the chat pane and the Chats panel: the panel has to know whether a run is in
  flight, and a second `createChat()` would open a second event pump against the
  same thread — the gateway caps a caller at three concurrent streams.

### Two contract quirks worth remembering

- The cancel response's `status` is **PascalCase** (`CancelRequested`, `Completed`),
  unlike the projection stream's snake_case `run_status`. Two spellings of the same
  vocabulary on one API.
- **`next_cursor` is omitted, not null**, when there is no next page. A client that
  reads it as a required field breaks on the common case; both spellings normalize
  to "no more" in `gateway_client`.

### The regression checklist (8a.1)

The layout commit changes no panel logic *by construction* (the diff is wrapping),
the frontend type-checks and builds, and the app boots with the dashboard webview
mounting every panel **with no console error**. What could not be automated: a
human clicking each interactive flow (model download + cancel, model switch,
provider apply-and-restart, ambient toggles, skill review card). Those call Tauri
commands this phase did not touch, but a visual pass is still worth one minute of
someone's time.

Next: **8b — connectors**, whose first job is its own ⚠️ VERIFY: drive a registry
connector to a *real tool call* before building any UI.

### 8a.5 resolution — the provider directory, and a probe that tells the truth (2026-07-15)

The blocked item is closed **widget-side, additively, with no core patch** — and the
⚠️ VERIFY turned up a second inert route, which settles the design rather than merely
constraining it.

**⚠️ VERIFY answered: `POST /llm/providers` accepts *no* protocol under our profile.**
All eleven come back **`503 service_unavailable`**; only a bogus adapter name gets a
`400`, which proves the route parses and the *service* is simply not composed under
`local-dev`. So the gateway's provider-config lane is unusable, on top of its probe
lane being inert (the 8a finding above). Everything — directory, keys, probe — is
ours. Pinned by `provider_protocols.rs`, which turns green→red the day upstream
composes the service.

- **Provider directory** from the runtime's own `providers.json` (26 entries): each
  row shows the vendor's own name, a **"get a key" link** (20 of 26 carry one), the
  endpoint it will be reached at, and whether it can be tested at all. The panel
  features **OpenRouter** explicitly — one key there reaches most models online, and
  you can change model without changing provider. `openai_compatible` remains the
  escape hatch; **`cloudflare` joins it** as bring-your-own-endpoint (its URL embeds
  an account id), which a test caught rather than a user.
- **The probe is ours** (`ic_widget::probe`), by **protocol family, not brand**:
  OpenAI-shaped (16 of 26, plus OpenRouter) → authenticated `GET /models`;
  Anthropic → `x-api-key` + version header; Gemini → key as a query parameter.
  **The trap, and why the fallback exists:** plenty of OpenAI-compatible servers
  serve `/models` to *anyone*, so a `200` there proves the endpoint exists, not that
  the key is good — exactly the lie the gateway's own probe tells. So the probe asks
  again with a deliberately invalid key; if that also passes, it falls through to a
  **one-token completion**, the only probe that truly validates. Pinned by
  `an_endpoint_that_does_not_check_the_key_falls_back_to_a_completion`.
- **Failures are told apart**: unreachable (typo'd URL, no network) vs key rejected
  vs rate-limited vs "authenticates out of band, so a pasted key cannot be tested"
  (Bedrock, Codex, NEAR AI, Copilot's token exchange). A green tick that means
  nothing is worse than no button.
- **Model dropdown** is fed by the same probe, with **free text when the provider
  lists nothing** — which is most of them, and is not an error. Model and endpoint
  are now part of the selection's identity, so changing the model on the *active*
  provider is a change you can Apply (it used to silently disable the button), and
  it persists through the existing apply-and-restart flow. `ProviderSelection::Cloud`
  gained `base_url`; an older settings file without it still loads.
- **The stored key never round-trips through the webview.** "Test" on a configured
  provider reads the key from the Credential Manager in Rust; a key is only sent
  from the UI when the user is checking one they have not saved yet.
- **Honest-UX note in the panel** (the spec's point 5): *any online model works* is
  true of chatting and emphatically not of **agent** use — this app hands the model
  two dozen tools and needs well-formed tool calls back, and models differ far more
  at that than at chat. The panel names known-good agentic models and warns that
  small models (≤8B) will chat happily and fail at tasks.

**Two robustness items, both from this session's own scars:**

- **A UTF-8 BOM no longer resets your settings.** `serde_json` rejects a BOM at
  column 1, so a BOM'd `settings.json` read as *corrupt* and every setting silently
  reverted to defaults — which is exactly what happened here when PowerShell 5.1's
  `Set-Content -Encoding utf8` wrote one. The loader now strips it. Pinned by
  `a_settings_file_with_a_byte_order_mark_still_loads`.
- **A TODO for an abort endpoint** in `ic_llama::proxy`, referencing the cancel
  finding: the proxy holds the upstream request, so a small loopback control route
  that drops it would make llama.cpp abort generation — Stop would finally stop the
  GPU. Not built because the widget cannot yet tell *which* proxy request belongs to
  the run being cancelled (the gateway sends no correlation id); that mapping is the
  actual work.

Next: **8b — connectors**, whose first job is its own ⚠️ VERIFY: drive a registry
connector to a *real tool call* before building any UI.
