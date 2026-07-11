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

---

## CP-4 — Reborn cannot reach an on-device MCP server (PATCHED)

- **Files:**
  - `crates/ironclaw_extensions/src/hosted_mcp_discovery.rs` — `valid_hosted_mcp_url`
    (+ new `is_loopback_url`, re-exported from `lib.rs`)
  - `crates/ironclaw_reborn_composition/src/mcp.rs` — `HostedMcpEndpoint::parse`,
    `HostedMcpEndpoint::allows_target`, `hosted_mcp_network_policy`
- **Why:** Phase 4 (browser automation) needs the agent to call tools that drive a
  real browser over CDP — native host access that cannot live in the WASM sandbox.
  `CLAUDE.md`'s Phase 4 plan assumed *"standalone MCP server (stdio), register
  through IronClaw's MCP config"*. **That assumption is false for Reborn**, and it
  fails in the worst possible way:
  - **stdio is hard-rejected at dispatch.** `ironclaw_mcp/src/lib.rs` →
    `if transport == "stdio" { return Err(ExternalStdioTransportUnsupported) }`
    (*"unsupported until process-level egress controls land"*). The crate spawns no
    processes at all. A stdio manifest still **parses, installs, and activates**
    cleanly — then every `tools/call` dies. It looks wired, then fails at runtime.
    The stdio docs in `docs/capabilities/mcp.md` describe the **legacy v1 binary**,
    not Reborn.
  - **Loopback HTTP was blocked too**, so the obvious fallback was also shut: the
    hosted-MCP lane forced `scheme == "https"` (unreachable for a sidecar, which
    cannot hold a publicly-trusted cert) *and* planned egress with
    `deny_private_ip_ranges: true` (which denies `127.0.0.1` outright).
- **Why a patch and not a route-around:** every alternative was measured first.
  - A **WASM shim tool** hits the *same* wall one seam over —
    `runtime/local_dev/extension_surface.rs` hardcodes `deny_private_ip_ranges: true`
    for **every** extension capability. Same size of patch, but a far wider blast
    radius (all extensions, not just this provider), plus a wasm32 toolchain and a
    pointless proxy hop.
  - A **native first-party capability** would mean editing `factory.rs` *and*
    `ironclaw_first_party_extensions`, and dragging `chromiumoxide` into the core
    dependency graph.
  - Doing it **outside the agent loop** (e.g. executing browser tools inside the
    `ic_llama` SchemaProxy, CP-3 style) would bypass the safety layer and the
    approval flow entirely — a security regression, and forbidden by `CLAUDE.md`
    ("Do not weaken any of it").

  There is no config knob, env var, or profile — including `local-dev-yolo` — that
  relaxes any of this. Some core edit is unavoidable; this is the smallest one, and
  it lands on the lane upstream already supports.
- **Fix:** allow a hosted MCP provider to be an **on-device sidecar**: accept `http`
  for, and only for, a **literal loopback IP**, and waive private-range denial for
  exactly that endpoint. Everything remote stays `https` + `deny_private_ip_ranges`.
- **Blast radius — what the patch deliberately still refuses** (each pinned by a
  `cp4_*` test):
  - `http` to **any** non-loopback host — including `169.254.169.254` (cloud
    metadata, the classic SSRF target) and private LAN ranges.
  - **`localhost`** — a DNS *name*, not an IP literal, so it could be rebound.
    `is_loopback_url` requires the host to parse as a loopback IP.
  - Lookalike hosts such as `127.0.0.1.evil.com`.
  - The waiver is scoped to a single endpoint: the plan's allowlist holds exactly
    one target, and scheme is part of the endpoint's identity, so an `http`
    loopback plan cannot authorize an `https` request to the same host:port.
  - Remote providers (e.g. Notion) are untouched — `planner_denies_http_scheme_for_notion_provider`
    and `cp4_a_remote_https_provider_still_denies_private_ranges` both still pass.
  - `ManifestSource::HostBundled` is still required, so a user-installed extension
    cannot mint a loopback endpoint for itself.
- **Upstream candidate:** yes — filed as
  **[nearai/ironclaw#5998](https://github.com/nearai/ironclaw/issues/5998)**
  ("Reborn has no transport for a local (on-device) MCP server"). The error message
  upstream already ships (*"unsupported until process-level egress controls land"*)
  says they intend to solve it eventually.
- **DELETE CP-4 WHEN #5998 LANDS.** Whichever way upstream fixes it:
  - If they ship **stdio**, the sidecar drops its HTTP server entirely and speaks
    stdio; `manifest.rs` loses the port and the whole timing dance around it (the
    manifest no longer has to be written before the gateway boots, because there is
    no port to bake in).
  - If they take the **loopback-HTTP** option (proposed in the issue, and what CP-4
    implements), the patch is simply upstreamed and deleted here.

    Either way the `cp4_*` tests in both crates are the tripwire: they fail loudly
    if the patch is lost, and they are the first thing to remove.

### Related: the runtime approval flow is a no-op (not a patch — a route-around)

CP-4 puts browser tools on the agent, and every discovered MCP tool is stamped
`default_permission: Ask`. **That `Ask` is never enforced** — nothing in the
workspace reads `default_permission`, no production authorizer returns
`Decision::RequireApproval` (only `GrantAuthorizer` is wired into Reborn, and it
returns only `Allow`/`Deny`), and every active capability gets a standing grant. So
sensitive fills would run unprompted.

We do **not** patch this — the fix is not a one-liner and the disagreement is ours to
own at our boundary, not upstream's schema. Instead the browser sidecar enforces
consent itself (`ic_browser_mcp::consent` + `::classify`), which the model cannot
route around because the sidecar decides. Reported upstream for a disclosure channel:
[#6000](https://github.com/nearai/ironclaw/issues/6000) (no `SECURITY.md`, GitHub
private reporting disabled, so the issue asks *how* to report without disclosing the
finding). If upstream later wires `RequireApproval` for `Ask` capabilities, the
sidecar gate becomes defence-in-depth. Full write-up: `CLAUDE.md` Phase 4 notes,
"SECURITY" section.
- **Replay after merge:** grep `core-patch (desktop fork)` in both files. If an
  upstream sync restores the bare `scheme() != "https"` check or flips
  `deny_private_ip_ranges` back to an unconditional `true`, re-apply. The `cp4_*`
  tests in both crates fail loudly if the patch is lost — that is the signal.
- **Ordering consequences this creates** (they bite in `ic_widget`, not here):
  - The extension catalog is scanned **once, at `serve` boot**, so the manifest must
    be on disk *before* the gateway starts.
  - `discover_hosted_mcp_package` runs at **activation**, not at boot — so the
    sidecar must be **listening when the extension is activated**, and activation
    must be driven against the **running** `serve` process. `restore_extension_lifecycle_state`
    republishes the *bundled manifest* on restart, which carries only the capability
    *template* — the six real tools come from the live `tools/list`. A transient
    discovery failure **silently** falls back to that template, so the widget must
    verify the discovered capability count rather than assume success.
