# ic_integration_tests

Phase 0 **upstream-merge gate** for the desktop fork.

Spawns `ironclaw-reborn serve` (libSQL `local-dev` profile) wired to a hermetic
mock LLM and drives the minimal WebChat v2 chat contract that `ic_widget` builds
against:

1. auth is enforced (`GET /threads` without a bearer → `401`),
2. a thread is created (`POST /threads`),
3. a message starts a turn (`POST /threads/{id}/messages` → `run_id`),
4. the turn streams to `completed` over SSE (`GET /threads/{id}/events`), and
5. the assistant reply round-trips through the mock provider into the timeline
   (`GET /threads/{id}/timeline`).

If an upstream sync changes the serve API shape, the storage substrate, or the
agent loop, this test fails — that is the point.

## Why a mock LLM

`LLM_BACKEND=openai_compatible` routes through `RigAdapter`, which uses the
**non-streaming** OpenAI Chat Completions API. So the mock only has to answer
`POST /v1/chat/completions` with one canned assistant message (no tool calls, so
the loop terminates). The test is fully offline and deterministic — no keys, no
network, no real model.

## Running

The test crate launches the `ironclaw-reborn` binary but cannot build it (a
separate crate can't force a sibling binary build). Build it first, then run the
gate:

```bash
cargo build -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta
cargo test  -p ic_integration_tests                       --features webui-v2-beta
```

Without the `webui-v2-beta` feature the gate test is compiled out, so a plain
`cargo test --workspace` does not fail for a missing `serve` binary.

Override binary discovery with `IRONCLAW_REBORN_BIN=<path>` if it lives outside
the default `target/<profile>/` location.

CI runs this on Windows in `.github/workflows/desktop-ci.yml` (the `gate` job).
