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

## CP-5 — a hosted-MCP capability is never granted its own endpoint (PATCHED)

**CP-4 alone does not work.** It opens the loopback lane for the *dispatch plan*,
but the grant the capability actually runs under is built somewhere else, and there
the sidecar is still unreachable. Found on 2026-07-13, by driving a real browse
through the running app: the six tools were listed on the agent, the model called
`browser_navigate`, and every call died with
`capability ic-browser.browser_navigate obligation handling failed: Network` — ~0.4 ms
in, before a byte left the process.

- **Files:**
  - `crates/ironclaw_reborn_composition/src/mcp.rs` — new `hosted_mcp_grant_policy`
    (reuses CP-4's `HostedMcpEndpoint` + `hosted_mcp_network_policy`)
  - `crates/ironclaw_reborn_composition/src/extension_lifecycle.rs` —
    `ActiveExtensionCapability` gains `network: Option<NetworkPolicy>`, resolved in
    `active_model_visible_capabilities` where the *package* (and so the endpoint) is
    in scope
  - `crates/ironclaw_reborn_composition/src/runtime/local_dev/extension_surface.rs` —
    `extension_network_policy` honors it
- **Why:** `extension_network_policy` builds a grant's `NetworkPolicy` **solely from
  the capability's runtime-credential audiences**, and hardcodes
  `deny_private_ip_ranges: true`. An on-device sidecar needs no credentials, so it
  gets an **empty allowlist** — and `obligations.rs::validate_network_policy_metadata`
  rejects an empty allowlist outright (`network_obligation_failed()`). So the call
  fails in obligation preflight, which is *upstream of* the network policy CP-4
  patched: `authorize_static_policy` is never even reached. CP-4's notes predicted
  this seam ("`extension_surface.rs` hardcodes `deny_private_ip_ranges: true` for
  **every** extension capability") but read it as a WASM-shim problem; it is in fact
  on the hosted-MCP path too, and CP-4 cannot be used without it.
- **Blast radius:** `hosted_mcp_grant_policy` returns `None` for anything that is not
  a hosted-HTTP-MCP package, so every other extension keeps exactly the
  credential-audience policy it has today. A remote (`https`) provider still gets
  `deny_private_ip_ranges: true` — only a **loopback IP literal** waives it, which is
  the only thing `HostedMcpEndpoint::parse` accepts for `http` (CP-4). The allowlist
  holds exactly the one declared endpoint.
- **Tripwires:** `cp5_a_loopback_hosted_mcp_provider_is_granted_its_own_endpoint` and
  `cp5_a_remote_hosted_mcp_provider_still_denies_private_ranges` in `mcp.rs`.
- **DELETE CP-5 WITH CP-4** — same issue
  ([#5998](https://github.com/nearai/ironclaw/issues/5998)). Any upstream fix that
  makes an on-device MCP server reachable has to solve the grant too, or it has not
  actually shipped a working local MCP server.

### Related: hosted-MCP tool *discovery* cannot succeed at all (route-around, not a patch)

Discovery is a third, independent break — and unlike CP-4/CP-5 we did **not** patch
it, because it can be routed around.

`discover_hosted_mcp_package` calls the sidecar's `tools/list` through
`RuntimeHttpEgress`, which resolves its `NetworkPolicy` from the staged
`NetworkObligationPolicyStore`, keyed by `(scope, capability_id)`. That store is only
ever written during a **capability-dispatch** obligation preflight
(`obligations.rs::finish_prepare`). Discovery runs at **activation**, outside any
dispatch, so nothing has staged a policy: the lookup fails with
`network_policy_missing`, the error boundary collapses that to an opaque
`network_error`, and `extension_lifecycle.rs` logs at `debug!` and **silently falls
back to the bundled manifest while still reporting `activated: true`**. Verified: the
sidecar is never contacted.

So the "the runtime rebuilds every capability from your live `tools/list`" contract
(`hosted_mcp_discovery.rs`) **never actually runs** in `ironclaw-reborn serve`. The
bundled manifest is not a fallback — it is the only path. **Route-around:**
`ic_browser_mcp::manifest` declares all six capabilities (generated from
`protocol::Tool`, the same source `tools/list` is generated from, so they cannot
drift) instead of the single representative template the discovery contract asked
for. If upstream ever fixes discovery it will rebuild those same six from the live
list and nothing has to change.

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

---

## CP-6 — the shell denylist does not bind on Windows (PATCHED)

- **Files:**
  - `crates/ironclaw_host_runtime/src/first_party_tools/shell_core.rs` — Windows
    entries in `BLOCKED_COMMANDS`/`DANGEROUS_PATTERNS`/`FILE_READ_COMMANDS`, the
    new `detect_windows_command_abuse` family, Windows interpreters in
    `contains_shell_pipe`, and both-tokenization path checks
  - `crates/ironclaw_safety/src/sensitive_paths.rs` — Windows credential stores
- **Why:** `shell_core::validate_command` applies a denylist before executing, and
  the denylist is **entirely Unix-shaped**: `rm -rf /`, `dd if=/dev/zero`, `mkfs`,
  `> /dev/sda`, `sudo `, `| bash`, `/etc/passwd`, `~/.ssh`; `FILE_READ_COMMANDS`
  is `cat, head, tail, less, vim, nano, …`. On Windows — the fork's only supported
  target — essentially **none of it binds**. There is no `/etc/passwd`, no `sudo`,
  no `/dev/sda`, and no `rm`. `LocalHostProcessPort` runs `cmd /C <command>` there,
  so this is a live path, not a hypothetical one.

  The accurate statement is not "shell is unbounded" — it is **a control that
  looks present and does not bind on the target platform**, the same failure class
  this repo has named twice (a signal that cannot report success; a warning
  logged at ERROR). That is what makes it a patch rather than a preference.
- **Three gaps, three fixes:**
  1. **The existing detectors could not see.** `detect_command_injection`'s
     decode-and-execute family (`base64 -d`, `xxd -r`, `printf '\x..'`, `| rev`)
     all end at `contains_shell_pipe`, which knew only `sh`/`bash`/`zsh`/`dash`.
     Adding `iex`, `invoke-expression`, `powershell`, `pwsh`, `cmd.exe` switches
     the whole family on for Windows. Smallest change here, probably the largest
     return: the detectors were already written.
  2. **The sensitive-path check had nothing to bind to.** Windows readers
     (`get-content`, `type`, `findstr`, `copy`, `certutil`, …) now populate
     `FILE_READ_COMMANDS`. Two supporting fixes were needed for that to actually
     work: `shell_words` treats `\` as a POSIX escape, which dissolves every
     Windows path it is given (`C:\Users\me\.ssh\id_rsa` → `C:Usersme.sshid_rsa`,
     no longer containing `/.ssh/`), so the path checks now run over both
     tokenizations; and `sensitive_paths` had no Windows credential stores at all
     — the dot-directories carry over, but DPAPI master keys, Credential Manager
     blobs, and `%SystemRoot%\System32\config` did not exist in the list.
  3. **The primitives themselves**, enumerated from Windows rather than
     translated from Unix: recursive-forced deletion, drive format, shadow-copy
     and backup-catalog deletion, machine-hive and Run-key registry writes, LSASS
     access and hive export, Defender/firewall/event-log tampering, the
     fetch-then-execute family (`certutil -urlcache`, `bitsadmin /transfer`,
     `mshta http…`, `regsvr32 /i:http`, `msiexec http…`,
     `powershell -EncodedCommand`), task/service creation, local account changes.
     Token-aware, not substring, because Windows defeats substrings three ways:
     free flag order, PowerShell parameter prefixes (`-rec`, `-r`), and quoting
     (`powershell -Command "Remove-Item …"`, which is flattened first).
- **Blast radius — what the patch deliberately does not touch:**
  - `rm` is **not** a delete verb here. It is a `Remove-Item` alias in PowerShell
    but also *the* Unix delete command, and the recursive/force detection honours
    two-character parameter prefixes — including it would reclassify ordinary
    `rm -r -f dir` on Linux and macOS. The existing `rm -rf /` entry keeps that.
  - Short ambiguous names (`reg`, `net`, `sc`, `format`) match only when
    immediately followed by a mutating subcommand, so `Format-Table`,
    `reg query`, `net user`, and `sc query` are untouched.
  - Account roots (`%USERPROFILE%`, `C:\Users`) match **exactly**, never by
    prefix; prefix-matching would put every recursive delete on a Windows desktop
    in the never-waivable tier.
  - The two-tier split is preserved: catastrophic → always blocked; high-risk →
    blocked unless `allow_dangerous` (which the Reborn shell path never sets).
- **This is not a boundary.** A denylist over a shell string is bypassable by
  construction — write a script, then run the script. CP-6 is defence in depth
  behind CP-7, which is the control that actually decides.
- **Tripwires:** `validate_command_blocks_windows_primitives`,
  `detect_command_injection_covers_windows_interpreter_pipes`,
  `sensitive_path_detection_covers_windows_readers`,
  `validate_command_allows_ordinary_windows_and_unix_work`,
  `windows_account_roots_match_exactly_not_by_prefix` in `shell_core.rs`;
  `blocks_windows_credential_stores` in `sensitive_paths.rs`. All are run by the
  `core-patches` CI job, which exists because no fork job ran core crates before.
  End-to-end: `shell_denylist_binds.rs` (see CP-7).
- **Upstream candidate:** yes, and drafted — a portability/hardening PR in the
  shape of #6098. It deliberately says nothing about `PermissionMode::Ask` never
  being enforced; that stays private pending #6000. Draft is held for review and
  **not posted**, per the standing preference.

---

## CP-7 — `builtin.shell` cannot be withheld (PATCHED)

- **Files:**
  - `crates/ironclaw_host_runtime/src/first_party_tools/mod.rs` —
    `SHELL_TOOL_ENABLED_ENV`, applied to both the manifest and the handler
  - `crates/ironclaw_host_runtime/src/lib.rs` — re-export
- **Why — what the VERIFY found.** The question was whether the capability policy
  layer could deny `builtin.shell` or scope its mounts/network from fork-owned
  code. **It cannot, by three independent routes:**
  - `local_dev_capability_policy.toml` is `include_str!`-compiled into
    `ironclaw_reborn_composition` and parsed once into a process-wide `OnceLock`.
    There is no env var, config key, CLI flag, or file override — the only way to
    change a grant is to edit the file and rebuild, which *is* a core patch.
  - The loop framework does have a deny primitive — `CapabilityFilter::Deny` —
    but it is `pub(crate)` in `ironclaw_agent_loop`, whose `CLAUDE.md` states
    strategy traits are deliberately not exposed downstream. The default strategy
    returns `CapabilityFilter::All`.
  - The built-in package and its handler registry are composed entirely inside
    `factory.rs` from `builtin_first_party_package()` /
    `builtin_first_party_handlers_*()`, with no injection point for a caller.

  So lane (a) — a fork-owned flag driving the policy layer — is not reachable, and
  lane (b) applies: a core patch is justified on security grounds.
- **Why this and not only the denylist.** The denylist (CP-6) is bypassable by
  construction. `builtin.shell` is the only capability granted `mounts =
  "ambient"` (the host filesystem) plus `spawn_process`, `execute_code`, and
  `local_dev_wildcard` network; every other coding capability is workspace-mounted
  and bounded by that mount. No approval can fire — Phase 8d established that
  `PermissionMode::Ask` is declared and never enforced. Two untrusted inputs reach
  the model that could drive it: browser `innerText` (returned unscanned) and
  imported skill bodies (the trusted tier, scanned for nothing). The control that
  actually holds is therefore *not offering the capability*.
- **Fix:** `IRONCLAW_SHELL_TOOL_ENABLED` decides whether `shell::manifest()` joins
  the built-in package and whether the handler is registered. **Unset means
  enabled**, so upstream deployments and every core test are unaffected — the
  patch is a no-op unless something sets it.
- **Fail-closed at two layers:** no manifest means the capability never reaches
  the model's surface; no handler means a dispatch that arrives anyway resolves to
  `UndeclaredCapability` rather than to a shell.
- **The fork side, and why the default direction is inverted from ambient's.**
  `GatewayConfig::shell_enabled` is a typed field defaulting to `false`, and
  `GatewayConfig::env()` emits the variable **unconditionally, in both
  directions**. This is deliberately unlike `IRONCLAW_TRIGGER_POLLER_ENABLED`,
  which rides in `extra_env` and is simply absent when off: there, the runtime's
  default is off, so silence is safe. Here the runtime's default is *on*, so
  silence is the dangerous answer and an omission-shaped bug would fail open.
  Pinned by `the_shell_tool_is_off_by_default_and_always_stated_explicitly`.
  Surfaced as `settings.agent_shell_enabled`, off, in Settings → "What it can do".
  Toggling restarts the gateway, because the runtime decides at boot.
- **Tripwire:** `crates/ic_integration_tests/tests/shell_denylist_binds.rs`, which
  drives both halves through a *running* gateway on Windows: the capability is
  absent from the model's tool list with the switch off, and a denylisted command
  is refused with it on. It carries a positive control — the same command run
  through the same `cmd /C`, which must destroy a sacrificial directory — because
  without it "the directory survived" could equally mean the command was a no-op.
  That control earned its place twice during development (see the file's notes on
  `powershell` not being on `PATH`, and on `cmd` quoting). The whole file was run
  red before it was trusted green, per the `routeless_surfaces` precedent.
- **Upstream candidate:** plausible but not drafted. It is a policy feature rather
  than a portability fix, and bundling it with CP-6 would weaken that PR's tight
  framing. Revisit if upstream ever wires a real approval gate — at which point
  CP-7 becomes redundant and should be deleted.

---

## Upstream status (checked 2026-07-15)

| Ref | What | Status |
|---|---|---|
| [#5998](https://github.com/nearai/ironclaw/issues/5998) | No transport for a local (on-device) MCP server | **Open, no response.** CP-4 + CP-5 stay until it lands. |
| [#5999](https://github.com/nearai/ironclaw/issues/5999) | `local-dev-yolo` cannot start on Windows (host path used as `MountAlias`) | Open, no response. Not ours; explains 3 red tests in the baseline. |
| [#6000](https://github.com/nearai/ironclaw/issues/6000) | How to report security issues (no `SECURITY.md`) | Open. A collaborator replied asking a colleague how to handle it — **so there is still no disclosure channel**, and the `PermissionMode::Ask`-is-never-enforced finding remains unreported. |
| [#6076](https://github.com/nearai/ironclaw/issues/6076) | Automations carry no thread/run correlation | Open, no response. The 7a watcher keeps pairing by timing. |
| [#6099](https://github.com/nearai/ironclaw/issues/6099) | `POST /llm/test-connection` reports `ok` for a dead endpoint with a junk key | **Filed 2026-07-15.** Pinned by `chat_control.rs`; the widget's own `probe.rs` is the route-around. |
| **[PR #6098](https://github.com/nearai/ironclaw/pull/6098)** | **CP-1 upstreamed** — skip directory fsync on Windows | **Opened 2026-07-15** against `reborn-integration`. If merged, delete CP-1 from this file and drop the local patch. |

**CP-1 is now a PR.** The evidence that made it worth raising: upstream's *own*
test suite fails on Windows without it —
`catalog_contract::composite_routes_filesystem_operations_to_matching_backend`
dies with `WriteFile … "permission denied"`, because `sync_parent_dir` fsyncs a
read-only directory handle and Windows answers `ERROR_ACCESS_DENIED`. Every write
through `LocalFilesystem` fails, so `serve` cannot boot at all. The PR is one
`#[cfg(windows)]` guard, one file, one commit, with every other platform
byte-for-byte unchanged.

### A finding that was withdrawn before it was filed

An earlier read of the 8b connector probe concluded that **WASM registry
connectors hang forever on first tool call**. That was wrong, and it is recorded
here because the mistake is instructive. The tool call *worked* — it reached
GitHub, which answered `401` for the deliberately-bogus token, and the run then
parked in an **auth gate** rather than completing. A probe watching only for
`"status":"completed"` waits forever and reads that as a hang.

What made it look like a runtime failure rather than a gate: **`serve` reads
`IRONCLAW_REBORN_LOG`, not `RUST_LOG`** (`ironclaw_reborn_cli/src/runtime/mod.rs:34`).
With the wrong variable set the log is empty, and an empty log next to a stalled
run is very easy to misread as a wedged process. It is neither. Set the right
variable before concluding anything upstream is broken — a false bug report costs
a maintainer's afternoon.
