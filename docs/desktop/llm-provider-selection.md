# LLM provider selection (and why failover is not a config toggle)

How `ironclaw-reborn serve` picks an LLM provider, what the Phase 2b dashboard
can therefore offer, and the one thing the v1 definition of done promises that
nothing in the tree implements.

Verified against the pinned upstream commit (`a492857`) on 2026-07-10.

## How the gateway resolves a provider

Reborn does **not** read a per-provider set of environment variables we invent.
`ironclaw_llm::resolution::resolve_provider_config_from_env`
(`crates/ironclaw_llm/src/resolution.rs:111`) does this:

1. Read `LLM_BACKEND`.
2. Match it — by id **or alias** — against `providers.json` at the repo root,
   which `ironclaw_llm::registry` embeds with `include_str!`.
3. Read that provider's **own** declared key variable, `api_key_env`.

So the key for Anthropic is `ANTHROPIC_API_KEY`, not a generic `LLM_API_KEY`.
`LLM_API_KEY` is the key variable of exactly one catalog entry:
`openai_compatible` — the one the `ic_llama` SchemaProxy uses.

If `LLM_BACKEND` is unset, resolution falls back to scanning the catalog for the
first provider whose key variable happens to be present in the environment.

A representative slice of the 26-entry catalog:

| `id` | `api_key_env` | `default_model` |
|---|---|---|
| `anthropic` | `ANTHROPIC_API_KEY` | `claude-sonnet-4-20250514` |
| `openai` | `OPENAI_API_KEY` | `gpt-5-mini` |
| `openai_compatible` | `LLM_API_KEY` | `default` |
| `ollama` | *(none)* | `llama3` |
| `gemini_oauth` | *(none — OAuth)* | `gemini-2.5-flash` |
| `openai_codex` | *(none — subscription)* | `gpt-5.5` |

Consequences for the dashboard:

- **The provider list is data, not code.** `crates/ic_widget/src/providers.rs`
  embeds the same `providers.json`, so a provider added upstream shows up with
  no code change here, and the two copies cannot disagree about what
  `anthropic` means. Only the fields the dashboard needs are decoded; unknown
  fields are ignored so an upstream schema addition does not break the build.
- **Providers without an `api_key_env` are not offered a key field.** Pasting a
  secret for `gemini_oauth` or `ollama` would store a value nothing reads.
- **The stored key never crosses back into the webview.** `SecretStore` exposes
  `has_provider_key()` to the UI and `provider_key()` only to the code that
  builds the child process environment.

## The single-backend constraint

`LLM_BACKEND` holds exactly one value. Pointing it at a cloud provider means
*not* pointing it at the `ic_llama` SchemaProxy, and vice versa. Local and cloud
inference are therefore mutually exclusive **at the environment level**.

Because `ironclaw-reborn` reads its environment once at startup and never
re-reads it (the same fact that pins the sidecar port — see the Phase 1 notes in
`CLAUDE.md`), changing the active provider requires restarting the gateway. The
Phase 2b dashboard saves the key immediately and offers an explicit
**Apply & restart gateway** action rather than restarting silently, so a save
cannot kill an in-flight run.

## What is missing: cross-provider failover

The v1 definition of done in `CLAUDE.md` says:

> …and answer with a local GGUF model — **with cloud failover when a key is
> configured**.

Nothing in the tree does this. `crates/ironclaw_llm/CLAUDE.md` is explicit:

> **Current wiring:** The failover is set up between primary model and
> `NEARAI_FALLBACK_MODEL` (a different model name on the same NEAR AI backend),
> not across different LLM provider types. Cross-provider failover (e.g., NEAR
> AI → Anthropic) requires manual construction.

`FailoverProvider` (`crates/ironclaw_llm/src/failover.rs:108`) is perfectly
capable of holding two providers of different types — it stores
`Vec<Arc<dyn LlmProvider>>` and `new` / `with_cooldown` accept exactly that.
What does not exist is any code path that *constructs* it that way.

There is exactly one production construction site in the whole workspace,
`crates/ironclaw_llm/src/lib.rs:958`. It is reached only when
`config.nearai.fallback_model` is set (`lib.rs:931`), and it builds the second
provider by **cloning the primary's config and swapping the model string**
(`lib.rs:937`). Both entries are therefore always the same backend. Every other
call to `FailoverProvider::new` in the tree is a unit test.

So local → cloud failover is **unbuilt work in a core crate**, not a setting.
Phase 2b does not fake it. The provider panel selects **one active provider**
and says so.

### The three ways to close it, when we get there

1. **Core patch (`core-patch:` + an entry in `core-patches.md`).** Teach
   `build_provider_chain()` to read a second provider selection and construct a
   cross-type `FailoverProvider`. Smallest change, upstreamable, but it edits a
   core crate — golden rule #1 wants that avoided.
2. **Route around it in `ic_llama`, as CP-3 did.** The SchemaProxy already sits
   between the gateway and the sidecar with `LLM_BACKEND=openai_compatible`
   pointing at it. It could own the failover: on a sidecar error, forward the
   same OpenAI-shaped request to a cloud provider using the key from the
   credential store. The gateway keeps seeing one `openai_compatible` endpoint
   and needs no change at all. This preserves the additive-fork policy and
   matches the precedent set by CP-3.
3. **Do neither and cut the promise** from the v1 definition of done.

Option 2 is the one that fits the fork's existing shape: the proxy is already a
request-rewriting man-in-the-middle for tool schemas, and failover is the same
kind of interception. It also keeps the cloud key out of the gateway's
environment entirely, which is strictly better for the secrets rule. It costs
the ability to use a cloud provider's *native* API surface, since everything
must round-trip through the OpenAI-compatible shape — acceptable for Anthropic
and OpenAI, which both have compatible endpoints.

Decide before Phase 6; the packaging story ("cloud failover when a key is
configured") is a first-run-wizard promise.

## References

- `crates/ironclaw_llm/src/resolution.rs` — `resolve_provider_config_from_env`
- `crates/ironclaw_llm/src/registry.rs` — the embedded catalog
- `crates/ironclaw_llm/src/failover.rs` — `FailoverProvider`
- `crates/ironclaw_llm/CLAUDE.md` — "Failover Chain", "Provider Selection"
- `crates/ic_widget/src/providers.rs` — the dashboard's view of the catalog
- `docs/desktop/llama-cpp-tool-grammar.md` — CP-3, the route-around precedent
