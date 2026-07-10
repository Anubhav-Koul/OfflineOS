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

### Phase 3 — Animated character companion (in `ui/`)

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

### Phase 4 — Browser automation (`crates/ic_browser_mcp`)
1. Standalone MCP server (stdio) wrapping `chromiumoxide`: `browser_navigate`, `browser_get_text`, `browser_find`, `browser_fill`, `browser_click`, `browser_screenshot`.
2. Launch a dedicated browser profile with `--remote-debugging-port` (probe registry: Chrome → Edge; Edge is guaranteed on Win10+). Never attach to the user's running profile by default.
3. Register with IronClaw through its MCP config. Sensitive actions (fill on password/payment fields) must route through the approval flow.
4. CAPTCHAs/logins: pause, notify via widget (character `concerned` state), let user complete, resume. Selector failures: screenshot → vision-capable model fallback.

### Phase 5 — Voice (`crates/ic_voice`) + Canvas
1. Pipeline: `cpal` capture → ring buffer → wake word (openwakeword ONNX via `ort`) → silero VAD gate → `whisper-rs` transcribe → post to gateway → reply → Piper TTS playback (bundled [piper1-gpl](https://github.com/OHF-Voice/piper1-gpl) binary). Barge-in: stop TTS when VAD triggers.
2. Wire TTS playback amplitude into `CharacterRenderer` lip sync (`ParamMouthOpenY`), replacing the Phase 3 test-tone stub; barge-in returns the character to `listening`.
3. WASAPI device-change handling (re-open stream), mic-live indicator on the character/bubble, tray mute toggle, audio never written to disk.
4. Canvas: dedicated Tauri window; agent emits HTML/SVG via a `canvas_render` tool (register as WASM tool or MCP); render in sandboxed iframe, sanitize output.

### Phase 6 — Packaging & hardening
1. Single MSI (Tauri bundler): our app + `ironclaw-reborn` + llama.cpp binaries + Piper + bundled character assets. First-run wizard: GPU probe → model recommendation → provider keys → storage init (libSQL — no Postgres install!).
2. Uninstaller must remove the Credential Manager entry ("IronClaw Desktop" / "gateway-token") and `%LOCALAPPDATA%\IronClaw Desktop\`.
3. Tauri auto-updater; code-sign (unsigned + mic capture + child processes = SmartScreen/AV flags).
4. Failure drills before ship: kill llama-server mid-generation; kill ironclaw-reborn mid-job; sleep/resume; monitor unplug; disk-full during GGUF download; occupied ports (ports are already dynamic — verify end to end).

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

Next: **Phase 2b — dashboard panels** (sessions, automations, model picker +
GGUF/VRAM via `ic_llama`, provider keys), then **Phase 3 — animated character
companion** (Live2D, `ren_en/` model), then **Phase 4 — browser automation**.
