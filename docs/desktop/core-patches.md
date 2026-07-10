# Core patches (replay after upstream merges)

Per `CLAUDE.md` golden rule #1, the desktop fork is **additive** — new
functionality lives in new crates and we do not edit IronClaw core crates
unless it is unavoidable. This file lists the few unavoidable edits to core
crates so they can be re-applied (or upstreamed) after each upstream sync.

Each patch is a single, isolated change with a `core-patch (desktop fork):`
comment at the edit site so it is greppable:

```
grep -rn "core-patch (desktop fork)" crates/
```

---

## CP-1 — Windows: skip directory fsync in `LocalFilesystem`

- **File:** `crates/ironclaw_filesystem/src/local.rs`, fn `sync_parent_dir`
- **Why:** `atomic_write_file` ends by fsync-ing the parent directory to durably
  persist a create/rename. It did `tokio::fs::File::open(parent).sync_all()`.
  On **Windows** you cannot flush a directory handle: `sync_all` →
  `FlushFileBuffers` on a handle opened without write access returns
  `ERROR_ACCESS_DENIED` (`io::ErrorKind::PermissionDenied`). This made **every
  write through `LocalFilesystem` fail on Windows**, which broke
  `ironclaw-reborn serve`/`run` at boot: the `local-dev` bundled-skill install
  writes `/projects/system/skills/.ironclaw-reborn-bundled.lock` and aborted with
  `filesystem backend error during write_file ... permission denied`.
- **Fix:** make the directory-fsync a **no-op on Windows** (`#[cfg(windows)]`),
  unchanged on all other platforms. NTFS persists directory-entry changes for
  create/rename without an explicit directory flush, so POSIX durability
  semantics are preserved everywhere they exist.
- **Blast radius:** behavior-preserving on non-Windows (identical code path).
  Windows previously could not write at all, so there is no regression risk.
- **Upstream candidate:** yes — this is a portability fix, not a fork-specific
  behavior. Consider filing upstream; directory fsync being a no-op on Windows
  is the standard approach (matches the `tempfile`/DB-durability ecosystem).
- **Replay after merge:** if an upstream sync reverts `sync_parent_dir` to the
  unconditional `File::open(parent).sync_all()`, re-apply the `#[cfg(windows)]`
  guard. Grep `core-patch (desktop fork)` to confirm the marker survived.

---

## CP-2 — Native Gemini provider drops tool-parameter types (DEFERRED — routed around, not patched)

- **File:** `crates/ironclaw_llm/src/gemini_oauth.rs`, fn `to_gemini_request`
  (tool `parameters` passed to Gemini verbatim around the `functionDeclarations`
  construction).
- **Symptom:** with the native `gemini`/`gemini_oauth` provider, a turn carrying
  the agent's built-in tools fails with Gemini **400 `INVALID_ARGUMENT`**:
  `function_declarations[*].parameters.properties[*].value.type` = `""`.
- **Cause:** no schema normalization for Gemini. Built-in tool schemas contain
  nullable unions (`["string","null"]`) and typeless array `items` that Gemini's
  strict function-declaration schema rejects. The `RigAdapter` providers apply
  OpenAI strict-mode normalization (`tool_schema.rs`); the native Gemini provider
  does not.
- **Status: NOT patched.** Per the additive-fork policy we route around it: use
  the `openai_compatible` provider against Gemini's OpenAI-compatible endpoint
  (`https://generativelanguage.googleapis.com/v1beta/openai`), which goes through
  `RigAdapter` and normalizes schemas. Verified to produce real replies. See
  `windows-build.md` F6.
- **If we ever need the native provider:** normalize `t.parameters` before
  emitting `functionDeclarations` — recurse the schema, ensure every node has a
  valid Gemini `type` (map `["T","null"]` → `T` + `nullable`, give typeless
  `items`/`properties` a concrete type), and drop unsupported keywords
  (`$schema`, `additionalProperties`, `oneOf`/`anyOf`/`allOf`). Prefer reusing a
  policy from `tool_schema.rs` over hand-rolling. This would be an upstreamable
  correctness fix. Until then, leave the provider unpatched and rely on the
  route-around.

---

## CP-3 — llama.cpp rejects IronClaw's tool schemas (ROUTED AROUND — not patched)

- **Would-be file:** wherever `builtin__spawn_subagent`'s parameter schema is
  declared (`task` and `handoff` carry `maxLength: 65536`).
- **Symptom:** with `LLM_BACKEND=openai_compatible` pointed at `llama-server`,
  **every** agent turn fails. The server answers `400` and logs
  `parse: error parsing grammar: number of repetitions exceeds sane defaults`.
- **Cause:** tool calling requires `llama-server --jinja`, which compiles the
  request's tool schemas into a GBNF grammar. `"maxLength": N` becomes
  `char{0,N}`, and llama.cpp's GBNF parser rejects repetition counts >= 2000
  (measured on `b9948`: 1999 compiles, 2000 does not).
- **Status: NOT patched.** Lowering that one `maxLength` would edit a core crate
  to fix one schema, while the same failure is reachable from any WASM tool or
  user-installed MCP server that declares a bound of its own. The disagreement is
  between llama.cpp and the OpenAI tool-schema dialect at large, so it is fixed
  at the boundary we own: `ic_llama::proxy::SchemaProxy` sits in front of
  `llama-server`, `LLM_BASE_URL` points at it, and it strips oversized repetition
  bounds from `tools`/`response_format` in flight. Everything else is forwarded
  verbatim.
- **Full analysis:** `docs/desktop/llama-cpp-tool-grammar.md`.
- **Revisit when:** upstream raises `MAX_NUMBER_OF_REPETITIONS` or emits `char*`
  for bounds it cannot express. Then `proxy.rs` can be deleted and `LLM_BASE_URL`
  can point straight at the sidecar (`LlmEnv::for_sidecar` already does this).
