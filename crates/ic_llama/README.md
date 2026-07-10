# ic_llama

Phase 1 of the desktop fork: run a local GGUF model, and let IronClaw talk to it.

## The integration is four environment variables

IronClaw already ships an `openai_compatible` LLM provider. `llama-server`
already speaks the OpenAI Chat Completions API. So there is nothing to add to
any IronClaw core crate — point the provider at the sidecar and the agent loop
is running on local weights:

```rust
let llm = LocalLlm::launch(root, &model_id, LocalLlmOptions::default()).await?;

let mut command = std::process::Command::new("ironclaw-reborn");
llm.env().apply(&mut command);   // LLM_BACKEND / LLM_BASE_URL / LLM_API_KEY / LLM_MODEL
command.arg("serve").spawn()?;
```

Setting `LLM_BACKEND` also stops IronClaw consulting any *other* provider's
environment, so a run that is meant to be offline is offline.

## What `LocalLlm::launch` does

1. Probes the GPU (DXGI) and system RAM.
2. Installs the [pinned llama.cpp build](../../docs/desktop/llama-cpp-pin.md) —
   downloads, verifies the SHA-256, extracts, and finds `llama-server.exe`.
3. Reads the model's GGUF header for its architecture, block count, and the
   on-disk size of every block.
4. Computes `-ngl` from those sizes, the free VRAM, and the KV cache the chosen
   context needs. Refuses, with a reason, if the model fits nowhere.
5. Starts `llama-server` on a stable loopback port and supervises it.
6. Hands back the four environment variables.

## Design notes worth knowing

**The port is reserved once and reused across restarts.** IronClaw reads
`LLM_BASE_URL` at startup and never re-reads it, so a restart that landed on a
different port would silently break inference.

**Per-layer sizes come from tensor offsets, not a quantization table.** Computing
a tensor's size from its dimensions needs a table of ~40 `ggml` block sizes that
drift upstream. GGUF stores each tensor's offset into the data section and writes
tensors back to back, so a tensor's size is the distance to the next offset.
No table, no drift. (`gguf.rs`)

**`503` is not a failure.** `llama-server` binds its port before the weights are
resident and answers `503 Loading model` until they are — minutes, for a large
model on a cold cache. A supervisor that read `503` as a crash would restart the
server into a loop it could never escape. (`server.rs`)

**A model that crashes the server twice is marked suspect, not restarted
forever.** An out-of-memory `-ngl`, a corrupt GGUF, or an unsupported
quantization fails identically on the third attempt. The marker is written beside
the weights and survives restarts, so the user gets an explanation instead of
watching the same loop tomorrow. (`models.rs`)

**A nonzero `DedicatedVideoMemory` does not mean a discrete GPU.** AMD APUs
report a BIOS carve-out of system RAM here — 485 MiB on the machine this was
written on. `is_discrete()` uses a 1 GiB floor. (`hardware.rs`)

**Downloads resume, and a digest mismatch deletes the partial file.** A corrupt
prefix would otherwise be resumed from forever. (`download.rs`)

**IronClaw is pointed at a proxy, not at `llama-server` directly.** Tool calling
requires `llama-server --jinja`, which compiles the request's tool schemas into a
GBNF grammar — and llama.cpp's grammar parser rejects any `maxLength` of 2000 or
more, while IronClaw's `spawn_subagent` declares 65536. Without repair, *every*
agent turn fails. Patching that one schema would not help: any MCP server the
user installs can declare a bound of its own. So a loopback proxy strips
oversized bounds in flight. (`proxy.rs`, and
[the full analysis](../../docs/desktop/llama-cpp-tool-grammar.md))

## Modules

| Module | Responsibility |
|---|---|
| `release` | The exact llama.cpp build we ship, with digests |
| `download` | Resumable, checksum-verified transfers |
| `runtime` | Installing those binaries |
| `models` | The GGUF store, HuggingFace downloads, suspect markers |
| `gguf` | Reading a model's shape out of its header |
| `hardware` | How much VRAM and RAM this machine actually has free |
| `placement` | Turning all of the above into one `-ngl` number |
| `server` | Keeping `llama-server` alive, and knowing when to stop trying |
| `proxy` | Repairing tool schemas llama.cpp's grammar compiler rejects |
| `wiring` | The four environment variables |
| `local_llm` | The facade that runs the whole sequence |

## Trying it

```bash
# What does this machine look like?
cargo run -p ic_llama --example probe -- --root ./scratch

# Install the pinned llama.cpp build (31 MiB for Vulkan).
cargo run -p ic_llama --example probe -- --root ./scratch --install

# Pull a model and start it.
cargo run -p ic_llama --example probe -- --root ./scratch \
    --pull Qwen/Qwen3-4B-GGUF Qwen3-4B-Q4_K_M.gguf
cargo run -p ic_llama --example probe -- --root ./scratch --model Qwen3-4B-Q4_K_M
```

## Testing

```bash
cargo test -p ic_llama
```

Everything is hermetic: no network, no GPU required, no real weights. The GGUF
tests build synthetic model files; the download tests drive a mock origin through
`206`/`200`/`416` and digest mismatches; the supervisor tests spawn
`testsupport/fake_llama_server.rs`, a real subprocess that reproduces a slow
load, a crash on startup, a crash after being healthy, and a server that never
finishes loading.

The one test that needs real weights is the offline agent round-trip in
`crates/ic_integration_tests/tests/local_model_roundtrip.rs`, which is
`#[ignore]`d. See its module docs.
