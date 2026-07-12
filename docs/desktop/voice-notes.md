# Phase 5 — Voice pipeline (`crates/ic_voice`)

Recorded 2026-07-12. The architecture, the traps hit, and the decisions that shaped
the crate. Authoritative over the Phase 5 plan in `CLAUDE.md` where they conflict.

## What was built

A voice loop that links into `ic_widget` as a library (not a child process): it
needs the widget's `AppHandle` for lip-sync/state events and its `GatewayClient` to
send transcripts down the same chat path. The one subprocess is Piper.

```
mic ─cpal→ downmix+resample(16k mono) → SampleRing ─┬→ rustpotter (wake word)
                                                    └→ Silero VAD → endpointer
                                                                       │ utterance
   character mouth ◀ RMS envelope ◀ cpal playback ◀ Piper ◀ gateway ◀ whisper STT
```

- **Pure, testable core** (no hardware, no models): `format`, `resample`, `ring`,
  `envelope`, `endpoint` (VAD→utterance hysteresis), `session` (the state machine),
  and `pipeline` (the async driver) — all exercised by fakes behind the `stages`
  traits. 64 unit/integration tests, no device required.
- **Model-backed stages** behind traits, each with a real impl and an `#[ignore]`d
  real-asset test: `wake` (rustpotter), `vad` (Silero), `stt` (whisper), `tts`
  (Piper subprocess), `playback` (cpal out), `capture` (cpal in).
- **`device`**: a hand-written WASAPI `IMMNotificationClient` (cpal has no
  device-change API) that reopens capture when the default mic changes.
- **`assets`**: pinned, digest-verified download of the models via
  `ic_llama::Downloader`.
- **Widget wiring** (`ic_widget::voice` + `main.rs`): provisioning, the reply path,
  Job-Object enlistment of Piper, tray mute, summon-hotkey push-to-talk, and the
  `voice://amplitude` / `voice://state` events. Voice is an explicit opt-in
  (`settings.voice_enabled`, default off) because first enable downloads ~210 MB.

## Decisions and traps (the expensive ones)

### Wake word: rustpotter is vendored, because every published version is unusable

The plan chose rustpotter. On crates.io **only 3.0.2 is not yanked, and it does not
compile** — it pulls candle 0.2, whose `Uniform<half::bf16>` no longer satisfies the
current `rand`/`rand_distr` (feature-unifying `half` does not fix it; pinning `rand`
would cascade). Every 2.x release is yanked.

**Resolution (user decision): vendor rustpotter 2.0.1** into
`crates-src/rustpotter/` (Apache-2.0, kept with its LICENSE), referenced by path and
`exclude`d from the workspace. 2.x is the *classic correlation-based reference-model*
spotter (pure DSP: ciborium, hound, rubato, rustfft) — no candle — which is exactly
the "ship our own reference models from recordings" path the plan wanted. Its clippy
lints are silenced in its own `Cargo.toml` (we don't edit upstream source to satisfy
our lints).

Wake models are **not bundled yet** (they need recordings of the phrase). Until they
are, `voice::start` loads any `.rpw` under the bundled `voice-wakewords/` resource
dir, and with none present falls back to `NullWakeWord` — voice is then triggered by
the **summon hotkey (push-to-talk)**, `Ctrl+Alt+Space`, which injects a manual wake.

### rubato: pin `0.16`, not 3/4

The scaffold's `resample.rs` uses `FftFixedIn` + `process(&[chunk], None)`. rubato
**3.0/4.0 rewrote resampling around `audioadapters`** (`Fft` + `FixedSync`), a
heavier surface. `0.16` still has the chunked `FftFixedIn` API the code targets.

### cpal `0.18`: `SampleRate` is a `u32` alias and `build_*_stream` takes config by value

The scaffold was written against an older cpal. In 0.18 `config.sample_rate()`
returns a `u32` (not a `SampleRate` newtype — no `.0`), and `build_input_stream`
takes `StreamConfig` **by value**.

### STT build prerequisites: **CMake + LLVM/libclang**

`whisper-rs` 0.16 (CPU, default features — Vulkan silently no-ops on Windows static
builds) builds `whisper.cpp` via **CMake** and generates FFI bindings via **bindgen
(libclang)**. Neither ships with the Rust toolchain.

- Install once: `winget install Kitware.CMake` and `winget install LLVM.LLVM`.
- `whisper-rs-sys`'s committed `WHISPER_DONT_GENERATE_BINDINGS` path (which would
  drop the libclang requirement) is a **dead end here** — the pregenerated bindings
  fail with const-eval layout-assertion overflows against the vendored whisper.cpp.
  Bindgen must run, so libclang is required. Build with
  `LIBCLANG_PATH="C:\Program Files\LLVM\bin"` and CMake on `PATH`.

This joins Node 22 + pnpm (webui) as a documented build prerequisite; Phase 6
packaging ships the *built* artifacts, so end users need neither.

### VAD pulls `ort` back in

`voice_activity_detector` (Silero v5, MIT) runs on ONNX Runtime, so `ort` returns
even though rustpotter dropped it. `ort` downloads a prebuilt ONNX Runtime at build
time; bundle its DLL beside the exe in Phase 6 (`copy-dylibs`) to dodge a System32
clash. Silero at 16 kHz requires a **512-sample** window, no other size.

### TTS: Piper one-shot per utterance, raw PCM, no disk

The archived MIT `rhasspy/piper` `2023.11.14-2` `piper_windows_amd64.zip` (piper.exe
+ espeak/onnx DLLs). We spawn it **per utterance** with `--output-raw` and read raw
16-bit LE mono PCM from stdout to EOF — one-shot means EOF *is* the end of the audio,
so there is no framing to invent and nothing is written to disk. Each spawn is
enlisted in the widget's Job Object (`ProcessJob::assign_std`, added for the
synchronous child Piper is). Piper emits no timing → lip sync is the **RMS envelope
of the output PCM**, computed on a companion thread off the audio callback (the
amplitude sink emits a Tauri event, which must never touch the real-time thread).

Voice: **`en_US-amy-medium`** (CC — **verify the MODEL_CARD licence before public
release**, Phase 6). Piper renders at 22.05 kHz; playback resamples up to the device
rate.

### Device change: a hand-written `IMMNotificationClient`

cpal exposes no device-change API. `device.rs` registers a COM
`IMMNotificationClient` (windows-rs 0.62; the `#[implement]` macro needs a direct
`windows-core` dep because it names `windows_core` at the crate root). MTA COM, so
the watcher is marked `Send + Sync` (only constructed/dropped on one thread). On
`OnDefaultDeviceChanged` for `eCapture` it fires a `RestartTrigger` that makes the
driver drop and reopen capture.

### The `ping` sleeper trap in `job_object`'s tests

`job_object::tests::dropping_the_job_kills_its_members` used
`ping 127.0.0.1 -n 60` as a "60-second sleeper" child. Once `ic_widget` links
`ic_voice` (and thus ONNX Runtime), the loaded `onnxruntime.dll` perturbs the
process enough that a spawned `ping` **fails immediately (exit 1)** instead of
sleeping, so the test's own "child is still alive" guard fired. Switched the sleeper
to PowerShell `Start-Sleep` (network-free, no console/stdin needed — unlike
`timeout`, which errors on redirected stdin). Kill-on-close is still verified.

## The reply path

A spoken transcript is a chat message. `ic_widget::voice::drive_turn` does the same
dance the typed UI does — because **the assistant's text never rides the event
stream**: `send_message` → follow the run's status on the projection stream until
`RunPhase::is_terminal()` → read `Timeline::latest_assistant_reply()`. Voice keeps
its **own lazily-created thread** so a spoken conversation has continuity without
entangling the typed chat.

## Character integration (the Phase 3 seam, closed)

`voice://state` drives the character's `listening`/`speaking` signals through the
existing `update_character` seam (the state machine re-derives). `voice://amplitude`
feeds `Live2DRenderer.setMouthOpen` → `ParamMouthOpenY`, replacing the Phase 3
test-tone stub. The stub is **kept as a fallback**: when no fresh amplitude has
arrived (a *typed* reply with no TTS, or a stalled stream), the mouth falls back to
the syllable test tone, so the character still moves its mouth while "speaking".

## The hardening pass (2026-07-13)

A dedicated bug-hunt (regression suites + two independent code reviews + running
every stage against the **real** models and devices) found and fixed a cluster of
real defects. The ones that reshape how the pipeline works:

- **The default input device cannot be trusted.** On this machine the default is a
  Bluetooth soundbar's "Headset" (HFP) endpoint that opens cleanly, reports
  healthy, and delivers **zero samples forever** — voice was silently deaf.
  `CpalCapture::start` now probes each device for actual audio flow (~500 ms
  window) and falls back through the input list; verified live (it skipped the
  soundbar and found the real mic in the smoke run). The driver also retries a
  failed mic open every 3 s and replaces a stream that dies without a
  device-change notification.
- **The driver no longer blocks on the turn.** Whisper, the gateway round-trip,
  and Piper each run in a spawned stage task reporting back through a channel;
  mute/shutdown/device-change/push-to-talk are now instant in every state
  (previously wedged for up to 120 s mid-turn, and `shutdown` could hang the
  settings toggle). Stage results carry a **turn generation**, so a slow stale
  synthesis can never speak into a later turn.
- **Barge-in is the wake phrase or the summon hotkey — never raw VAD.** The mic
  hears the character's own TTS through the speakers; a VAD-driven interrupt
  self-triggers on that echo and turns speaker playback into a self-driving
  conversation loop. The ring is also drained when playback starts, so speech from
  the transcribe/await window cannot replay into the detector.
- **Listening now times out** (no speech onset within `listen_timeout`, default
  12 s → back to Idle) and the utterance buffer is hard-capped — a false wake used
  to listen forever and grow ~230 MB/hour.
- **Piper is fed exactly one line.** Multi-line replies deadlocked the pipes
  (Piper synthesizes line 1 into a full stdout while we were still writing its
  stdin). All whitespace runs collapse to single spaces; `speechify` in
  `ic_widget::voice` additionally strips markdown (code fences → "code omitted",
  emphasis/links dissolve) so Piper doesn't read asterisks aloud.
- **`drive_turn` matches the `SubmitOutcome` variant.** `DeferredBusy` carries the
  *previous* run's id — tracking it spoke the previous question's answer as if it
  were the new one. Busy → stay quiet. A failed send drops the cached voice thread
  and retries once on a fresh one.
- **`start_voice` re-checks settings after provisioning** (minutes can pass):
  disabling voice mid-download no longer leaves a hot mic behind a settings
  toggle that says off, duplicate starts are discarded, and the mute state is
  re-read at install time. All settings writes now go through one serialized
  `update_settings` (concurrent commands were losing updates).
- **Character signals split:** `CharacterInputs` gained `voice_listening/
  voice_thinking/voice_speaking`, OR-ed with the typed pair — a stale typed
  reading-time timer used to freeze the mouth mid-TTS by clearing the shared flag.
  Voice transitions apply through one ordered consumer task (per-transition spawns
  raced). A session-level guard makes a redundant mute/unmute a strict no-op (it
  used to cancel an in-flight turn and orphan a live playback).
- **UI:** Solid `onCleanup` after an `await` never registers (no owner) — both
  `onMount`s now register cleanup synchronously and append teardown steps as they
  create things. Playback completion is now gated on the audio callback's
  progress, not the wall clock, so high-latency devices don't clip the reply tail.

Real-asset verification (run with `--ignored`, assets under the session
scratchpad): Piper synthesis (verifying `--output-raw` against the real binary),
the **TTS → `Resampler` → whisper round trip** (`tests/real_voice_loop.rs` —
transcribes back the exact sentence), live speaker playback with the amplitude
tap, live mic capture through the fallback, and the WASAPI watcher registration.

## Follow-ups

- **Record + train wake models** and bundle them under `voice-wakewords/`; until
  then voice is push-to-talk only.
- **Phase 6**: bundle piper.exe + DLLs, whisper + voice models, and the ONNX Runtime
  DLL; verify the amy voice licence; ship CMake/libclang-built artifacts so users
  need no toolchain; add the mic device picker + a first-run "enable voice" prompt.
- A dashboard voice panel (enable/disable, mute, device, model status) — the
  `voice_status` / `set_voice_enabled` / `set_voice_muted` commands exist for it.
