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

Next: obtain a code-signing cert + updater keypair + update endpoint, produce the
first real MSI on a clean VM, reconcile the WiX caveats, run the manual drills, and
record + bundle rustpotter wakeword models (then wake word replaces push-to-talk).

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
