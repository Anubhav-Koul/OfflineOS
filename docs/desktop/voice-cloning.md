# Voice cloning — design note (not built)

Phase 8c added a **voice picker** (choose from a curated list of pinned Piper
voices). Voice *cloning* — "upload a 30-second sample, sound like that" — is
deliberately **out of scope**, and this note records why, and what building it
later would actually take, so the decision does not have to be re-derived.

The one-line reason: Piper cannot clone. Piper is a per-speaker VITS model; a new
voice means a *trained* `.onnx`, not a runtime sample. Cloning needs a
fundamentally different, zero-shot engine, and every credible candidate is
GPU-heavy and/or license-encumbered in ways that fight this app's constraints.

## Where a cloning engine would plug in

The seam is already clean. `ic_voice::tts::PiperTts` drives Piper as a
**subprocess** behind the `Synthesizer` trait (`synthesize(text) -> Speech`,
where `Speech` carries its own sample rate). A second engine is "another
`Synthesizer` implementation that shells out to a different binary/server" — the
resampler and lip-sync envelope downstream already take the rate from `Speech`
dynamically (Phase 8c VERIFY), so nothing below the synthesizer changes. That is
the *only* easy part.

## Engine candidates, and why each is hard here

| Engine | License | First-audio latency | VRAM (rough) | Notes |
|---|---|---|---|---|
| **XTTS v2** (Coqui) | **CPML — non-commercial** | seconds | ~4 GB | Best-known zero-shot quality; the license alone rules it out of a shippable product. |
| **F5-TTS** | MIT (weights vary) | seconds (diffusion-style) | ~4–8 GB | Permissive code, but weight licenses vary by release — each must be checked; heavier than Piper by an order of magnitude. |
| **GPT-SoVITS** | MIT | seconds; needs a short reference + transcript | ~4 GB | Good few-shot quality; multi-stage pipeline (SoVITS + GPT), more moving parts to supervise than one `piper.exe`. |

Common problems regardless of engine:

- **VRAM contention.** The Phase 3 perf rule: on an iGPU-only or single-GPU
  machine the cloning model competes with `llama-server` for VRAM. Piper is CPU
  and costs nothing here; a cloning engine could evict the chat model or be
  evicted by it. Any implementation must treat GPU as a shared, contended budget
  (the same lens `ic_llama::placement` already applies to the chat model).
- **Latency.** Piper is effectively instant per utterance. Zero-shot neural TTS
  is seconds-to-first-audio, which changes the character's "speaking" cadence and
  the barge-in feel.
- **Consent / likeness.** "Upload a sample" is a cloning-of-real-people surface.
  Shipping it needs a consent gate ("you affirm you may use this voice") and a
  clear stance that samples never leave the machine — a policy design, not just an
  engineering one.
- **Bundle size.** These models + their runtimes are hundreds of MB to GB, versus
  Piper's ~60 MB per voice.

## If it is ever built

Add a `Synthesizer` impl behind the existing subprocess pattern (a supervised
child in the Job Object, like `piper.exe` and `llama-server`), gate it behind a
settings flag and a consent step, budget its VRAM against the chat model, and
prefer an engine whose **weights** are unambiguously commercial-use (F5-TTS or
GPT-SoVITS over XTTS). Store user samples on-disk under the app data dir, never
uploaded, deletable by the user.

## The offline alternative: train a real Piper voice

Distinct from cloning, and lower-risk: Piper voices can be *trained* offline
(~30+ minutes of clean recorded speech + a GPU training run, hours to days). That
produces a normal `.onnx` that drops straight into the Phase 8c catalog with a
pinned digest — no new runtime engine, no VRAM contention at inference, no
license question (the recordings are the operator's own). It is an **offline
pipeline / content task**, not an in-app feature, and belongs alongside the
wake-word recording work as an external input, not a code path in the widget.
