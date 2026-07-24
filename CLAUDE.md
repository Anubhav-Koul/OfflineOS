# CLAUDE.md — IronClaw Fork: Desktop Widget Agent

Project instructions for Claude (Code) working in this repository. Read fully before making changes. The full build-phase history — the original Phase 0–6 plan text plus every dated progress note — lives in `docs/desktop/PROGRESS.md` (newest first) and is authoritative over the phase descriptions when they conflict; it records what the running system actually does. The full Phase 7 and Phase 8 specs live in `docs/desktop/phases/`.

## What this project is

A fork of [nearai/ironclaw](https://github.com/nearai/ironclaw) (Rust agent OS, MIT OR Apache-2.0) extended into a **Windows-first desktop app**: an animated character companion widget + dashboard (Tauri 2), **llama.cpp local inference** alongside cloud LLMs, plus browser automation, voice (wake word/STT/TTS), and a canvas window. Target capability: on par with OpenClaw for agent core + browser + voice/canvas (messaging channels are out of scope for v1).

## Golden rules

1. **Additive fork policy — do NOT edit IronClaw core crates unless unavoidable.** All new functionality lives in new crates (`crates/ic_widget`, `crates/ic_llama`, `crates/ic_voice`, `crates/ic_browser_mcp`). Integrate through existing extension points: the gateway HTTP/SSE/WS API, `LLM_BACKEND=openai_compatible`, MCP servers, and WASM tools. This keeps upstream merges cheap.
2. **Never commit secrets.** API keys via env vars / OS keychain only. IronClaw already rejects inline secrets in config — preserve that behavior.
3. **Windows is the primary target.** Every phase must build and run on Windows (MSVC toolchain). Don't introduce Unix-only code paths without a Windows equivalent.
4. **After every phase: `cargo fmt`, `cargo clippy --all-targets`, `cargo test`, and a manual smoke run.** Do not mark a phase done with failing checks.
5. When IronClaw internals are unclear, read the upstream docs in-repo first: `README.md`, `FEATURE_PARITY.md`, `AGENTS.md`, `docs/`, and `CLAUDE.upstream.md` — do not guess API shapes.

## Precedence & inherited invariants

This file governs the fork crates (`ic_*`, `ui/`). `CLAUDE.upstream.md` governs any work inside IronClaw core code — its full clippy gate, dual-backend persistence rule ("all new persistence features must support PostgreSQL AND libSQL"), and code style apply there in full. Our scoped clippy/CI is a deliberate fork-gate decision (see Phase 0 notes in `docs/desktop/PROGRESS.md`), not a license to lint core code loosely.

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

- Pinned to `reborn-integration` (see Phase 0 notes in `docs/desktop/PROGRESS.md` for why, and the exact commit).
- Monthly sync: `git fetch upstream && git merge` on a branch; the integration test suite (Phase 0) is the merge gate.
- Keep `LICENSE-MIT`, `LICENSE-APACHE`, and attribution intact.

## Key IronClaw facts (verified July 2026 — re-verify on upgrade)

- **Toolchain:** Rust 1.96+. Windows MSI installer exists upstream, so Windows builds are supported.
- **Two runtimes:** legacy `ironclaw` binary, and **Reborn** (`ironclaw_reborn_cli` → `ironclaw-reborn` binary, `reborn-integration` branch). Reborn profiles: `local-dev`, `local-dev-yolo` (host access, needs `--confirm-host-access`), `hosted-single-tenant-volume` (**libSQL embedded storage** — no Postgres!), `production` (PostgreSQL 15+ + pgvector).
- **Storage decision for us:** desktop users must not need Postgres. Primary path: Reborn **libSQL substrate**. Fallback: bundle [postgresql_embedded](https://crates.io/crates/postgresql-embedded). Verify libSQL profile supports everything we need (memory hybrid search); if pgvector-only features block us, evaluate before writing code.
- **LLM providers built in:** NEAR AI (default), Anthropic, OpenAI, Gemini, Ollama, `openai_compatible` (`LLM_BASE_URL`/`LLM_API_KEY`/`LLM_MODEL`). **llama.cpp needs no core changes**: run `llama-server` and set `LLM_BACKEND=openai_compatible`, `LLM_BASE_URL` pointing at the `ic_llama` SchemaProxy (see Phase 1 notes in `docs/desktop/PROGRESS.md`).
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

## Edge cases checklist (apply throughout)

- **Upstream merge conflicts** → additive-crate policy; if a core patch is truly unavoidable, isolate it in one commit prefixed `core-patch:` and list it in `docs/desktop/core-patches.md` for replay after merges.
- **Reborn is beta** → pinned commit; `ic_widget::gateway_client` is the only place we speak to the gateway, so protocol drift is fixed in one place; integration tests catch breakage.
- **NEAR AI default auth** → our onboarding always writes an explicit `[llm.default]`; never depend on NEAR onboarding flow.
- **Local models fumble tool-call JSON** → keep agentic routing on cloud or ≥14B local models by default; expose per-task routing in settings. (See also CP-3/SchemaProxy in Phase 1 notes in `docs/desktop/PROGRESS.md` — llama.cpp rejects oversized schema bounds.)
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

## Current status

One line per subsystem — full detail and *why* in `docs/desktop/PROGRESS.md`.

- **Phase 0 — fork bootstrap:** ✅ done.
- **Phase 1 — llama.cpp (`ic_llama`):** ✅ done.
- **Phase 2 — widget + dashboard:** ✅ done (2a shell/chat, 2b panels + LLM wiring; cloud failover, tokens/sec, GGUF picker, and wake word closed out in "Closing the open items").
- **Phase 3 — character companion:** ✅ done.
- **Phase 4 — browser automation (`ic_browser_mcp`):** ✅ done (core patches CP-4/CP-5).
- **Phase 5 — voice (`ic_voice`) + canvas (`ic_canvas_mcp`):** ✅ done.
- **Phase 6 — packaging & hardening:** config + hardening ✅; real MSI build blocked on external inputs (code-signing cert, updater keypair + endpoint, clean-VM build).
- **Phase 7 — ambient companion (7a–7d):** ✅ complete — proactive surfacing, self-learning skills, skill import, ambient watchers.
- **Phase 8 — surfacing the runtime:** 8a, 8a.5, 8b, 8b.1, 8c (runtime surfaces), 8c (voice picker), 8d (verified-negative), 8e — ✅ done. **Remaining: 8f (Telegram channel) and 8g (memory seeding + subagent visibility).**

**Open upstream tickets** (nearai/ironclaw):
- [#5998](https://github.com/nearai/ironclaw/issues/5998) — no transport for a local MCP server — CP-4/CP-5 get deleted when this lands.
- [#5999](https://github.com/nearai/ironclaw/issues/5999) — `local-dev-yolo` can't start on Windows.
- [#6076](https://github.com/nearai/ironclaw/issues/6076) — cancel doesn't abort in-flight generation; automations carry no thread/run correlation.
- [#6000](https://github.com/nearai/ironclaw/issues/6000) — how to report a security finding (no `SECURITY.md`) — unanswered.
- [PR #6098](https://github.com/nearai/ironclaw/pull/6098) — CP-1, Windows directory fsync.
- [#6099](https://github.com/nearai/ironclaw/issues/6099) — `/llm/test-connection` reports `ok` for a dead endpoint with a junk key.

## Doc index

- `docs/desktop/phases/phase7.md`, `phase8.md` — full sub-phase specs (moved verbatim from this file 2026-07-24).
- `docs/desktop/PROGRESS.md` — every dated progress note, newest first (moved verbatim from this file 2026-07-24).
- `docs/desktop/windows-build.md` — Windows build friction log.
- `docs/desktop/gateway-api-notes.md` — serve API contract + corrections C1–C8.
- `docs/desktop/llama-cpp-pin.md`, `llama-cpp-tool-grammar.md` — llama.cpp version-pin checklist, the CP-3 SchemaProxy.
- `docs/desktop/chat-rendering.md` — why there's no token streaming.
- `docs/desktop/dashboard-gaps.md` — the routeless surfaces (memory, audit, run history — skills got its own panel in 8c) and what would unblock them.
- `docs/desktop/character-pipeline.md` — Live2D pipeline architecture, Phase 6 licensing gates.
- `docs/desktop/core-patches.md` — the `core-patch:` protocol and the patch log (CP-1 through CP-5).
- `docs/desktop/voice-notes.md`, `voice-cloning.md` — voice pipeline traps, the voice-cloning design note.
- `docs/desktop/packaging.md` — MSI bundling, uninstall cleanup, failure drills.
- `docs/desktop/llm-provider-selection.md` — the cloud-failover design (now built).
- `docs/desktop/approval-gates.md` — why there's no universal tool-approval consent UI.

---

Current phase: 8 — read `docs/desktop/phases/phase8.md` before building. History: `docs/desktop/PROGRESS.md`.
