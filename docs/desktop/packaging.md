# Phase 6 — Packaging & hardening

Recorded 2026-07-12. How IronClaw Desktop becomes a single installable MSI, what
ships inside it, and the failure modes it must survive. Some steps need inputs only
the maintainer has (a code-signing certificate, an updater keypair, an update
endpoint) — those are documented with templates rather than wired live, so the repo
never carries a private key or a broken half-config.

## Build prerequisites

A release build needs, beyond the Rust toolchain (1.96+, MSVC):

| Tool | For | Install |
|---|---|---|
| Node 22 + npm | the `ui/` frontend (embedded at compile time) | nodejs.org |
| CMake | building `whisper.cpp` (voice STT) | `winget install Kitware.CMake` |
| LLVM / libclang | `whisper-rs-sys` bindgen | `winget install LLVM.LLVM` → set `LIBCLANG_PATH=C:\Program Files\LLVM\bin` |
| WiX Toolset v3 | the MSI (Tauri invokes `candle`/`light`) | `cargo tauri` fetches it, or `winget install WiXToolset.WiXToolset` |
| `tauri-cli` | `cargo tauri build` | `cargo install tauri-cli` |

The whisper build env (CMake + libclang) is the Phase 5 addition; see
`voice-notes.md`. Node is the Phase 0/2 webui requirement; see `windows-build.md`.

## Producing the MSI

```bash
# 1. Build the gateway with the webui feature and stage it as the sidecar.
cargo build --release -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta
mkdir -p crates/ic_widget/binaries
cp target/release/ironclaw-reborn.exe \
   crates/ic_widget/binaries/ironclaw-reborn-x86_64-pc-windows-msvc.exe

# 2. Build the installer (runs `npm run build` for the frontend first). The release
#    overlay adds the `externalBin` — kept out of the base config so a plain
#    `cargo build` / `cargo run` (which validates externalBin existence) is not
#    broken by a sidecar that only exists at bundle time.
LIBCLANG_PATH="C:\\Program Files\\LLVM\\bin" \
  cargo tauri build --config crates/ic_widget/tauri.release.conf.json
# → target/release/bundle/msi/IronClaw Desktop_<version>_x64_en-US.msi
```

`bundle.active` is `true`, but that only affects `cargo tauri build`; a plain
`cargo build` / `cargo run -p ic_widget --features app` never bundles, so the dev
loop is unchanged.

## What ships inside, and the offline strategy

The DoD is "offline on a clean Windows machine with one MSI install." Three tiers:

- **Always bundled** (in the MSI):
  - `ic-widget.exe` (the app) + the embedded frontend (`ui/dist`, which already
    carries the **character assets** — Live2D models, sprites — so they need no
    separate resource bundling).
  - **`ironclaw-reborn.exe`** as an `externalBin` sidecar — the gateway is located
    beside the widget binary and is *not* downloadable, so it must ship.
  - WebView2 via `webviewInstallMode: embedBootstrapper` (works offline on a clean
    machine).
  - The Silero VAD (embedded in the `voice_activity_detector` crate) and the
    rustpotter spotter (vendored) — both compiled in.
- **First-run download by default, bundle-for-offline optional**:
  - `llama-server` + backend DLLs (`ic_llama::runtime` fetches the pinned build per
    GPU backend), a chosen **GGUF model** (`ModelStore`), and the **voice models**
    (`ic_voice::VoiceAssets`: whisper `base.en`, Piper exe + `en_US-amy-medium`).
  - Each of these runtimes already **prefers a present file and only downloads what
    is missing** (`LlamaRuntime::install` checks its marker; `ModelStore`/`VoiceAssets`
    check the file). So a *fully offline* MSI is a matter of staging these files into
    the install and pointing the roots at them — no code change — at the cost of a
    ~1–2 GB installer. The default lean MSI downloads them on first run instead.
- **Not yet bundled**: rustpotter wakeword models (none recorded — voice is
  push-to-talk until they exist; see `voice-notes.md`).

**Decision for v1:** ship the lean MSI (downloads models on first run) and gate the
first launch behind the wizard, which makes the download explicit. Revisit a
fully-offline "airgap" MSI variant if a customer needs it.

## First-run experience

First launch has no models and no provider key. Storage is **not** a step — the
gateway initialises its libSQL store on boot (no Postgres, ever). The first-run
wizard (`settings.setup_complete`) walks: **GPU probe → recommended local model (or a
cloud provider key) → optional voice enable → done.** It reuses the existing commands
(`recommended_models`, `download_model`, `provider_settings`/`set_provider_key`,
`set_voice_enabled`), so it is orchestration, not new capability.

## Uninstall cleanup

An MSI removes its Program Files payload but not the two per-user artifacts:

- Credential Manager entries under service **"IronClaw Desktop"** (`gateway-token`,
  `provider-key/<id>`), and
- **`%LOCALAPPDATA%\IronClaw Desktop\`** (settings, the libSQL store, downloaded
  models, the browser profile).

So the installer's uninstall step runs `ic-widget.exe --uninstall-cleanup`
(`run_uninstall_cleanup` in `main.rs` → `SecretStore::clear_all` + `remove_dir_all`
of the data root). It is best-effort and idempotent. The WiX plumbing is
`wix/uninstall-cleanup.wxs`; **two caveats are documented in that file** and must be
reconciled at first real build:

1. the `FileKey` referencing Tauri's generated main-exe `File` Id, and
2. per-user data cleaned only for the *uninstalling* user under a per-machine
   install (a perUser MSI sidesteps it).

This is a *user-initiated* wipe (they chose to uninstall), which the fork's data
policy permits — unlike an automatic or agent-callable deletion.

## Auto-updater (needs a keypair + an endpoint — templated, not wired)

Tauri's updater verifies each update against a public key baked into the app; the
matching private key signs releases and must never be committed.

```bash
cargo tauri signer generate -w ~/.tauri/ironclaw.key   # once; keep the .key SECRET
```

Then add to `tauri.conf.json` and register the plugin (kept out of the committed
config until the maintainer holds the key, so the build doesn't fail on a placeholder):

```jsonc
// tauri.conf.json
"plugins": {
  "updater": {
    "endpoints": ["https://<your-release-host>/updates/{{target}}/{{current_version}}"],
    "pubkey": "<contents of ironclaw.key.pub>"
  }
}
```

```rust
// main.rs, in the Builder chain
.plugin(tauri_plugin_updater::Builder::new().build())
```

`TAURI_SIGNING_PRIVATE_KEY` (+ password) in the release CI signs the bundle; the
endpoint serves a `latest.json` per target. Until a host + key exist, updates are
manual (download the new MSI).

## Code-signing (needs a certificate)

Unsigned **+ microphone capture + several spawned child processes** is the trifecta
that trips SmartScreen and aggressive AV. An Authenticode cert (EV for instant
SmartScreen reputation) signs `ic-widget.exe`, the bundled `ironclaw-reborn.exe`, and
the MSI:

```jsonc
// tauri.conf.json → bundle.windows
"certificateThumbprint": "<cert thumbprint in the machine store>",
"digestAlgorithm": "sha256",
"timestampUrl": "http://timestamp.digicert.com"
```

Sign the downloaded-at-runtime binaries too where feasible (llama-server and Piper
are third-party and already signed by their publishers; our own sidecar and the MSI
are what we sign). Without a cert, first launch shows a SmartScreen prompt — expected
for a dev build.

## Failure drills

What each failure does today, and whether it is covered by design (✅), needs a
manual drill before ship (🔎), or is a known gap (⚠️).

| Drill | Behaviour | Status |
|---|---|---|
| Kill `llama-server` mid-generation | `Sidecar` supervisor restarts with exponential backoff; 2 quick crashes → `Suspect` marker (survives restarts), not a restart loop | ✅ by design (`ic_llama`); 🔎 end-to-end |
| Kill `ironclaw-reborn` mid-job | `GatewaySupervisor` restarts; the widget mirrors `Unhealthy` on the character; a `401` is fatal (no retry loop) | ✅ by design; 🔎 end-to-end |
| Hard-kill the widget (Task Manager) | Job Object `KILL_ON_JOB_CLOSE` takes gateway + llama-server + browser + Piper down too (verified by the `job_object` test) | ✅ verified |
| Occupied ports | Every sidecar reserves its port by binding `127.0.0.1:0` and reusing the assignment; no fixed ports | ✅ by design; 🔎 verify with a port hog |
| Sleep / resume | Supervisors reconnect; the SSE `EventStream` reconnects; window position keyed by monitor-arrangement hash | ✅ by design; 🔎 drill |
| Monitor unplug / rearrange | A saved widget point no longer on any monitor is discarded on read (never stranded offscreen); the tray "Reset position" is the escape hatch | ✅ by design; 🔎 drill |
| Default mic change (voice) | WASAPI `IMMNotificationClient` reopens capture on the new default (`ic_voice::device`) | ✅ by design; 🔎 drill with a headset |
| Disk-full during a model download | `Downloader` streams to a `.part` and renames only after the digest verifies; a write failure returns `Err` and leaves the `.part` to resume — never a corrupt model presented as complete | ✅ by design; 🔎 drill |
| Two instances | `tauri-plugin-single-instance` focuses the running widget instead of starting a second gateway on the same DB | ✅ by design |

The 🔎 rows are the pre-ship manual pass. The ⚠️ column is empty today — no known
uncovered failure — but re-audit after the first signed build on a clean VM.

## Open items before public release

- Stage `ironclaw-reborn` into `binaries/` and produce a first real MSI on a clean
  VM; reconcile the two `wix/uninstall-cleanup.wxs` caveats against the generated WiX.
- Obtain a code-signing certificate; wire signing.
- Stand up an update endpoint + generate the updater keypair; wire the plugin.
- Verify the `en_US-amy-medium` voice licence (MODEL_CARD) for redistribution.
- Record + bundle rustpotter wakeword models (then wake word replaces push-to-talk).
- Run the 🔎 failure drills on the signed build.
