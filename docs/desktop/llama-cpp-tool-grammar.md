# llama.cpp cannot compile IronClaw's tool schemas

Discovered while bringing up the Phase 1 offline agent round-trip. This is the
one place where IronClaw and llama.cpp genuinely disagree, and the reason
`crates/ic_llama/src/proxy.rs` exists.

## Symptom

Every agent turn against a local model fails. `llama-server` logs:

```text
parse: error parsing grammar: number of repetitions exceeds sane defaults, please reduce the number of repetitions
E failed to parse grammar
E srv send_error: task id = 0, error: Failed to initialize samplers: failed to parse grammar
```

and answers the request with `400`. The model loads fine and generates fine —
only requests carrying `tools` fail.

## Root cause

Three facts combine:

1. **Tool calling requires `--jinja`.** Without it, `llama-server` rejects the
   request outright: `500 {"error":{"message":"tools param requires --jinja flag"}}`.
   So the flag is not optional for an agent.

2. **With `--jinja`, llama.cpp compiles the tool schemas into a GBNF grammar**
   that constrains the model's output to a well-formed tool call. A JSON Schema
   `"maxLength": N` becomes a GBNF repetition `char{0,N}`.

3. **llama.cpp's GBNF parser rejects repetition counts of 2000 or more.**
   Measured against `b9948` by binary search (`probe_cap.py`, reproduced below):
   `char{0,1999}` compiles; `char{0,2000}` does not.

IronClaw's built-in `builtin__spawn_subagent` declares `maxLength: 65536` on both
its `task` and `handoff` properties. That is 32× over the limit, so the grammar
never compiles and **every** turn fails.

### Reproducing the measurement

Start the pinned server on any model, then bisect a one-tool payload:

```python
# POST /v1/chat/completions with a single tool whose only property is
#   {"type": "string", "maxLength": N}
# and binary-search N. 200 means the grammar compiled.
```

The full script is in this repo's history; the answer on `b9948` is **1999**.
Re-run it when the pin moves (see [`llama-cpp-pin.md`](./llama-cpp-pin.md)) — if
upstream raises or removes the cap, `MAX_GRAMMAR_REPETITIONS` should follow, and
if the cap disappears entirely the proxy can be deleted.

## Why we did not patch IronClaw

Lowering `spawn_subagent`'s `maxLength` is a one-line change to an IronClaw core
crate. We didn't, for two reasons.

**It fixes one schema, not the problem.** The incompatibility is between
llama.cpp and the OpenAI tool-schema dialect in general. Every WASM tool, and
every MCP server the *user* installs, contributes its own schema to the same
request. A third-party tool declaring `maxLength: 4096` — an utterly ordinary
thing to write — would break local inference exactly as completely, and no patch
to IronClaw's core could prevent it.

**The boundary is ours.** Per the additive-fork policy (`CLAUDE.md` golden rule
1), a problem that can be solved at the seam between IronClaw and a component we
own should be. `ic_llama` already owns the `LLM_BASE_URL` that IronClaw is
handed; pointing it at something of ours is free.

## What we do instead

`ic_llama::proxy::SchemaProxy` is a loopback reverse proxy in front of
`llama-server`. `LocalLlm::env()` sets `LLM_BASE_URL` to the proxy.

For `POST /v1/chat/completions` it parses the body and, within `tools` and
`response_format` only, removes `maxLength`, `minLength`, `maxItems`, and
`minItems` whose value exceeds `MAX_GRAMMAR_REPETITIONS`. Everything else —
other paths, other methods, headers, the `Authorization` bearer that satisfies
`--api-key`, the response body — is forwarded untouched, and the response is
streamed rather than buffered.

Notes on the choices:

- **Bounds within the limit are kept.** They compile, and they usefully constrain
  the model.
- **Removing a bound only widens what the model may emit.** It never makes an
  invalid tool call valid; the schema is still enforced by IronClaw when it
  deserializes the call.
- **`messages` is never walked.** A user message that happens to contain the text
  `{"maxLength": 65536}` is content, not schema, and is passed through byte for
  byte.
- **A body that is not JSON, or that needs no repair, is forwarded verbatim.** The
  proxy is not a validator; `llama-server` remains the authority on what is a
  valid request.

## Verification

- `cargo test -p ic_llama --lib proxy` — the sanitizer as a pure function,
  including the exact `spawn_subagent` shape and the 1999/2000 boundary.
- `cargo test -p ic_llama --test proxy_sanitizing` — over real HTTP against a
  recording upstream: the rewritten body, its `Content-Length`, header
  pass-through, and `502` on a dead server.
- `crates/ic_integration_tests/tests/local_model_roundtrip.rs` — the whole thing,
  end to end, on real weights.

Before the proxy, that last test failed with `failed to parse grammar`. After it,
Qwen3-4B answers through IronClaw's agent loop in about 7 seconds.

## If this ever gets fixed upstream

Watch for llama.cpp raising `MAX_NUMBER_OF_REPETITIONS`, or for its
json-schema-to-grammar converter learning to emit `char*` for bounds it cannot
express. Either would let us drop `proxy.rs` and point `LLM_BASE_URL` straight at
the sidecar again — `LlmEnv::for_sidecar` already does exactly that and is still
used by the supervisor tests.
