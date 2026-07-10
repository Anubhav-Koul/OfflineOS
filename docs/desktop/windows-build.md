# Windows build — friction log (Phase 0)

Environment: Windows 11 Pro (10.0.26200), MSVC-first per CLAUDE.md rule #3.
Upstream pinned to branch `reborn-integration` @ `a492857` (Reborn is not in any release tag).

## Toolchain state at start

| Tool | Status |
|------|--------|
| git | 2.54.0.windows.1 ✅ |
| rustc / cargo | 1.96.1 ✅ (meets 1.96+) |
| rustup default | ⚠️ was `stable-x86_64-pc-windows-gnu` (active); MSVC target also installed |
| node | v24.16.0 (webui-v2-beta wants Node 22 — we skip that feature) |
| pnpm | ❌ not installed (only needed for `webui-v2-beta` — skipped) |

## Friction points

### F1 — Reborn runtime is not in any release tag (blocking design decision)
CLAUDE.md says "pin to the latest release tag." Verified this is **incompatible with our architecture**:
- `v0.21.0` (latest tag) contains essentially only `ironclaw_safety` — no Reborn.
- The full Reborn stack (`ironclaw_reborn`, `ironclaw_reborn_cli` → `ironclaw-reborn` binary, `_composition`, `_config`, `_event_store`, `_traces`, `_webui_ingress`) plus libSQL storage lives **only on `reborn-integration`**.
- **Resolution:** pin to `reborn-integration` @ `a492857`. Documented in root `CLAUDE.md`. Unshallow before first upstream sync (clone is `--depth 1 --filter=blob:none`).

### F2 — MSVC BuildTools present but broken/unregistered
- VS 18 BuildTools existed on disk (`...\18\BuildTools`, MSVC 14.51.36231) but was **not registered** with the VS installer: `vswhere` returned nothing.
- Missing: the **Windows 10/11 SDK** (`Windows Kits\10\Lib` absent → no `kernel32.lib`/UCRT → MSVC linking fails) and native **Hostx64** tools (only `Hostx86/x64` cross-tools present).
- Impact: Rust's `x86_64-pc-windows-msvc` cannot link, and its `cc`/`vswhere`-based auto-detection can't locate the toolchain.
- Repair attempts:
  1. `setup.exe modify --installPath <BuildTools> --add ...VCTools` → **exit 1**: "An installed product matching the parameters cannot be found" (instance unregistered).
  2. `winget install Microsoft.VisualStudio.BuildTools --override "...VCTools..."` → **exit 0x8A150042** (failed; SDK still absent).
  3. Fresh bootstrapper install (`vs_buildtools.exe ...`) → **also failed**, root cause found: `aka.ms/vs/18/release/vs_buildtools.exe` returned **HTML, not the .exe** (bad URL for VS 18), so attempts that "self-elevated a downloaded exe" were launching a 183 KB HTML file — nothing installed.
- **Resolution (worked):** launched the **already-installed** VS Installer GUI (`C:\Program Files (x86)\Microsoft Visual Studio\Installer\setup.exe --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended`, no `--quiet`); user completed **"Desktop development with C++"** interactively. Result: registered **VS 2026 Community** at `C:\Program Files\Microsoft Visual Studio\18\Community`, MSVC **14.51.36231** (native Hostx64 `cl`/`link`), Windows SDK **10.0.26100.0** (`kernel32.lib` + `ucrt.lib`).
- **Lesson:** don't script blind elevated installs here — the existing on-disk BuildTools was unregistered and the bootstrapper URL was broken. The reliable path was the interactive GUI. `libclang` is NOT installed, but the Reborn CLI build path has no `silk`/`bindgen` deps, so it isn't needed.

### F3 — CLAUDE.md filename clash on clone
Upstream tracks its own `CLAUDE.md`; `git reset --hard` onto `reborn-integration` overwrote our fork guide. Preserved upstream's as `CLAUDE.upstream.md`; restored the desktop-fork guide as root `CLAUDE.md`.

### F4 — The serve HTTP API is gated behind `webui-v2-beta`, but that feature does NOT need Node/pnpm
CLAUDE.md plans to skip `webui-v2-beta` "to avoid Node/pnpm" while building our own UI against the serve API. But the serve API surface (`ironclaw_webui_v2`) is **only compiled under `webui-v2-beta`** — skipping it means no gateway at all. Investigated the build cost:
- The SPA (`crates/ironclaw_webui_v2_static/static/js/`) is **pre-built vanilla ES-module JS** (`main.js`, `app.js`, …) — no TypeScript, no bundler.
- Assets are embedded at compile time via Rust codegen (`assets.rs` "Embedded asset bytes"); the crate's `build.rs` does **not** invoke node/npm/pnpm/vite/webpack (grep across all `build.rs` = zero JS-toolchain calls).
- **Conclusion:** enabling `--features webui-v2-beta` compiles fine with **no Node dependency**. CLAUDE.md's "requires Node 22 + pnpm at build time" is outdated for this branch. We build *with* the feature and simply ignore the bundled SPA (we serve our own Tauri UI against the same API).

### F5 — Corrections to CLAUDE.md's Reborn facts (from serve-API source read)
- The libSQL desktop path is the **`local-dev` profile + `libsql` composition feature**, **not** a `hosted-single-tenant-volume` profile — that profile does not exist as a `RebornProfile` variant (only `local-dev` / `local-dev-yolo` / `production` / `migration-dry-run`). DB lands at `~/.<home>/local-dev/reborn-local-dev.db`.
- Serve auth env vars confirmed: `IRONCLAW_REBORN_WEBUI_TOKEN` (bearer) + `IRONCLAW_REBORN_WEBUI_USER_ID` (both required). Routes live under `/api/webchat/v2`; SSE also accepts `?token=` (EventSource can't set headers).
- **`--port 0` does NOT report the OS-assigned port** (`bound_addr_tx: None`) → our supervisor must pick a concrete free port itself and pass it via `--port`, never rely on `0`.
- No memory / skills / audit-log HTTP routes exist in the v2 table yet → affects Phase 2 dashboard panels (they may need v1 gateway or aren't ported).
- Full contract: `docs/desktop/gateway-api-notes.md`.

### F6 — Native `gemini` provider is broken with agent tools; route via `openai_compatible`
- Selecting the native `gemini` provider and sending a message that carries the agent's built-in tools fails: Gemini returns **400 `INVALID_ARGUMENT`** — `tools[0].function_declarations[*].parameters.properties[*].value.type` = `""`.
- **Cause:** `crates/ironclaw_llm/src/gemini_oauth.rs::to_gemini_request` passes each tool's JSON-Schema `parameters` to Gemini **verbatim** (no normalization). The built-in tool schemas use constructs Gemini rejects (nullable `["T","null"]` unions, typeless array `items`), which serialize to empty `type`. The `RigAdapter` path (OpenAI/Anthropic/Ollama/`openai_compatible`) applies OpenAI strict-mode schema normalization; the native Gemini provider skips it. Auth is fine (400 schema error, not 401/403).
- **Route-around (no core patch, additive-fork-compliant):** use the `openai_compatible` provider against Gemini's OpenAI-compatible endpoint. Verified working — produced a real finalized reply.
  ```
  ironclaw-reborn models set-provider openai_compatible
  # env at serve time (secrets via env only):
  LLM_BASE_URL=https://generativelanguage.googleapis.com/v1beta/openai
  LLM_MODEL=gemini-2.5-flash
  LLM_API_KEY=<gemini key>          # never in config.toml/providers.json
  ```
- Deferred core fix tracked as **CP-2** in `core-patches.md` (not applied — routed around).

## Build recipe (target — to verify once MSVC is fixed)

```bash
# Reborn CLI + serve gateway API. webui-v2-beta is REQUIRED for the HTTP API and needs no Node/pnpm (F4).
cargo build --release -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta
```

*(Confirm the `libsql` composition feature is pulled in by the `local-dev` profile at runtime; if not, add the corresponding feature flag.)*

## Verification checklist (fill in as completed)

- [x] MSVC: `vswhere` sees a registered instance (VS 2026 Community); `Windows Kits\10\Lib\10.0.26100.0\um\x64\kernel32.lib` exists; Hostx64 `link.exe` present (MSVC 14.51.36231).
- [x] `rustup default stable-x86_64-pc-windows-msvc` set; trivial `cargo run` compiles+links+runs ("Hello, world!").
- [x] `ironclaw-reborn` binary builds (`--release --features webui-v2-beta`) — 86 MB, cold build 5m47s.
- [x] `ironclaw-reborn doctor` runs (profile `local-dev`, driver registry initialized).
- [x] `config init` writes `config.toml` + `providers.json`; `models set-provider ollama` persists.
- [x] **`serve` boots on Windows** (after core-patch CP-1 + clearing stale skill lock): WebChat v2 listener bound, `turn_runner: true`.
- [x] **Gateway round-trip validated**: unauth 401; create thread; send message → `run_id`/`Queued`; SSE `projection_snapshot`/`projection_update` stream with cursor `id:`. See `gateway-api-notes.md` → "Verified end-to-end on Windows".
- [x] **Live LLM reply verified** — full turn produced a finalized assistant message ("IronClaw is a secure autonomous assistant.") via **Gemini** through the `openai_compatible` provider (see F6). End-to-end round-trip (client → gateway → turn runner → cloud LLM → reply) works on Windows.

## Gotchas for the `ic_widget` supervisor (Phase 2)

1. **CP-1 core-patch is mandatory** on Windows or `serve`/`run` cannot boot (see `core-patches.md`).
2. Active LLM provider must be resolvable **at boot** — either configure a key, or default to a keyless provider until one is set.
3. `IRONCLAW_REBORN_WEBUI_USER_ID` **must equal** `[identity].default_owner` in `config.toml` (default `reborn-cli`) — generate the token/user pair to match, or rewrite `default_owner`.
4. A crashed `serve` can leave a stale `system/skills/.ironclaw-reborn-bundled.lock`; the next boot times out on it. Supervisor should clear stale bundled-skill locks on restart (only when no `ironclaw-reborn` process is alive).
5. `--port 0` won't report the bound port — pick a concrete free port and pass `--port`.
