# OfflineOS

A desktop AI companion for Windows that runs on your own machine: an animated
character on your desktop, a local GGUF model doing the thinking, and a real
agent behind it — browser automation, voice, a sandboxed canvas, persistent
memory, and skills it can learn.

Built as an additive fork of [nearai/ironclaw](https://github.com/nearai/ironclaw)
(MIT OR Apache-2.0). Upstream provides the agent runtime; this fork adds the
desktop app, local inference, and the host-side capabilities around it.

> **Status: not yet installable.** Every subsystem below is built and tested, but
> no installer has been produced — that is blocked on external inputs (a
> code-signing certificate, an updater keypair and endpoint, and a clean-VM
> build), not on code. If you want to try it today you will be building from
> source on Windows. See `docs/desktop/packaging.md`.

## What it does

- **Local inference.** `llama.cpp` runs as a supervised sidecar, with a VRAM
  planner computed from GGUF tensor offsets and the OS's live video-memory
  budget. Cloud providers are available with automatic failover — and the cloud
  key never enters the agent runtime's environment.
- **A character, not a chat box.** Live2D companion with per-pixel click-through,
  so clicks pass through empty space and land on the character. Its state
  reflects what the agent is actually doing.
- **Voice.** Locally-trained wake word, `whisper-rs` for speech, Piper for
  speech back, with lip sync driven by the real TTS signal.
- **Browser automation** over CDP, running on the host rather than in the WASM
  sandbox, with its own consent gate for sensitive form fills.
- **A sandboxed canvas** the agent can draw charts and documents onto.
- **Memory and skills** that survive restarts, including skills it drafts itself
  from completed work — none of which install without an explicit yes.

## Security posture

This is an agent with a local model, a browser, and file access, so the
boundaries are worth stating plainly rather than burying:

- **Terminal access is off by default.** `builtin.shell` is the one capability
  the runtime mounts against the whole host filesystem, and no approval prompt
  fires for it. It is withheld unless you turn it on in Settings, and withholding
  is enforced by the runtime declining to declare the capability at all.
- **Windows-shaped command denylist.** The upstream denylist is written for Unix
  and does not bind on Windows; this fork replaces it with one enumerated from
  Windows primitives. It is defence in depth, not a boundary — a denylist over a
  shell string is bypassable by writing a script.
- **Consent where we own the surface.** The browser sidecar decides whether a
  sensitive field may be filled, so the model cannot route around it, and every
  non-yes path fails closed.
- **Children die with the parent.** Windows Job Objects with
  `KILL_ON_JOB_CLOSE`, assigned before a child can spawn anything.

Known gaps are documented rather than implied: `docs/desktop/approval-gates.md`,
`docs/desktop/dashboard-gaps.md`, and `docs/desktop/core-patches.md`.

## Building

Windows, MSVC toolchain, Rust 1.96+, Node 22.

```bash
cd ui && npm install && npm run build && cd ..   # must precede the app build
cargo build -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta
cargo run -p ic_widget --features app --bin ic-widget
```

## Documentation

`docs/desktop/` holds the fork's own record — architecture, the build-phase
history in `PROGRESS.md`, and a set of notes on things that turned out not to
work as documented (`chat-rendering.md`, `channels.md`, `gateway-api-notes.md`).
`CLAUDE.md` is the working brief for the fork; `README.upstream.md` and
`CLAUDE.upstream.md` are upstream's, kept intact.

## License

MIT OR Apache-2.0, inherited from upstream. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
