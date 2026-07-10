# The llama.cpp pin

`crates/ic_llama/src/release.rs` pins one exact upstream llama.cpp build. This
document says why, and how to move it.

## What is pinned

| | |
|---|---|
| Tag | `b9948` (published 2026-07-10) |
| Verified against | `llama-server.exe --version` → `version: 9948 (074944998)` |
| Vulkan | `llama-b9948-bin-win-vulkan-x64.zip`, 32,907,039 bytes |
| CPU | `llama-b9948-bin-win-cpu-x64.zip`, 18,219,242 bytes |
| CUDA 12.4 | `llama-b9948-bin-win-cuda-12.4-x64.zip` + `cudart-llama-bin-win-cuda-12.4-x64.zip` |

Each entry carries the SHA-256 that GitHub publishes for the asset. The
downloader refuses any archive that does not match, and a unit test
(`release::tests::every_pinned_digest_is_a_valid_sha256`) fails the build if a
digest in the table is malformed.

## Why pin, and why a build number rather than a release

llama.cpp tags several times a day and does not cut semantic releases. There is
no "stable" line to track. Meanwhile the things we depend on — GGUF metadata
keys, `/health` semantics, `-ngl` behavior, the `--jinja` chat-template path that
makes tool calls parseable — all move with `master`.

A desktop user must never get a different inference engine than the one we
tested. So the pin is a build number, and moving it is a reviewed change with a
checklist, not a dependency bump.

## Why Vulkan is the default

| Backend | Download | Covers |
|---|---|---|
| Vulkan | 31 MiB | NVIDIA, AMD, Intel — any GPU with a current driver |
| CUDA 12.4 | 628 MiB | NVIDIA only, and needs the CUDA runtime redistributable |
| CPU | 17 MiB | Fallback |

CUDA is meaningfully faster on NVIDIA, but it costs a 628 MiB download and a
second archive. `Backend::recommended_for` therefore **never** selects it
automatically: it is a settings decision the user makes knowing the price.
`Backend::recommended_for` picks Vulkan when a discrete GPU is present and CPU
otherwise.

The CUDA 13.3, HIP, SYCL, and OpenVINO archives upstream publishes are not
pinned. Add them only with digests taken from the API, never by hand.

## Bumping the pin

1. **Pick the tag.** Read the release notes between the current pin and the
   candidate, watching for changes to the server API, GGUF, or CLI flags.

2. **Take the digests from GitHub, not from a mirror or a local download.**

   ```bash
   gh api repos/ggml-org/llama.cpp/releases/tags/<TAG> \
     --jq '.assets[]
           | select(.name | test("win-(vulkan|cpu|cuda-12\\.4)-x64|^cudart"))
           | "\(.name) \(.digest) \(.size)"'
   ```

   The `digest` field is `sha256:<hex>`. Copy the hex into `release.rs`, update
   `LLAMA_CPP_TAG`, and update `size_bytes`.

3. **Re-verify the assumptions this crate makes about the archive.** None of
   these are guaranteed by upstream; all have changed before.

   - [ ] `llama-server.exe` is somewhere within four directory levels of the
         archive root. (`runtime.rs` searches rather than joining a fixed path,
         precisely because upstream has moved it between `/` and `build/bin/`.)
   - [ ] The backend DLLs sit **beside** `llama-server.exe`. The sidecar launches
         it in place for this reason; do not relocate the binary.
   - [ ] `GET /health` still answers `200 {"status":"ok"}` when ready and
         `503 Loading model` while loading. `server.rs` treats `503` as *loading*,
         not as a failure — if that ever became a hard error, every slow model
         load would look like a crash loop.
   - [ ] The flags `--model --alias --host --port --n-gpu-layers --ctx-size
         --api-key --jinja` all still exist. `server.rs` passes every one of them.
   - [ ] `-ngl N` still offloads the **last** `N` blocks, and `N > block_count`
         still additionally offloads the output tensors. `placement.rs` fills the
         VRAM budget from the last block backwards on this basis.
   - [ ] The GBNF repetition cap is still 1999. `proxy.rs` strips tool-schema
         bounds above `MAX_GRAMMAR_REPETITIONS` because llama.cpp cannot compile
         them; if upstream raised the cap, raise the constant, and if the cap is
         gone, delete the proxy. See
         [`llama-cpp-tool-grammar.md`](./llama-cpp-tool-grammar.md).

4. **Run the gate.**

   ```bash
   cargo test -p ic_llama
   cargo run -p ic_llama --example probe -- --root ./scratch --install
   ```

   The probe downloads and extracts the new archives, then prints the path it
   found `llama-server` at. A digest error fails at download; a layout change
   fails with `Error::Archive`.

5. **Run the offline round-trip** (see
   `crates/ic_integration_tests/tests/local_model_roundtrip.rs`), which is the
   only test that exercises real weights through IronClaw's agent loop.

## Rolling back

The pin is the only source of truth, and installs are content-addressed by tag
and backend:

```text
<root>/runtimes/b9948-vulkan/
<root>/runtimes/b9949-vulkan/
```

Reverting `LLAMA_CPP_TAG` makes the app use the old directory again, which is
still on disk. Nothing needs to be cleaned up to roll back; `<root>/runtimes`
and `<root>/cache` can be pruned at leisure.
