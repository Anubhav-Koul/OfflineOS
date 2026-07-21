# Approval gates — what fires, what doesn't (Phase 8d)

Phase 8d asked whether the runtime's **generic tool-approval gate** (the `gate`
SSE event + `POST …/gates/{gate_ref}/resolve`) could become the universal consent
surface for the desktop app — one red card in the bubble for every
consent-sensitive tool. The ⚠️ VERIFY answer is **no, it never fires under our
profile**, so there is nothing to build UI over yet. This records exactly why, what
runs unprompted, why the capability-policy "backstop" cannot help, and the tripwire
that will tell us when the answer changes.

## The `gate` event is real, and dormant

`gate` **is** the tool-approval channel — distinct from the credential/auth gate,
which is a separate `auth_required` event (`ironclaw_webui_v2/src/schema.rs`;
`ic_widget::gateway_client::events` carries them as separate variants). A `gate`
requires `TurnStatus::BlockedApproval`, which requires the capability authorizer to
return `Decision::RequireApproval`. Under `ironclaw-reborn serve` / `local-dev`:

- The **only** authorizer wired into composition is `GrantAuthorizer`
  (`ironclaw_reborn_composition/src/factory.rs:648`), and its decision function
  returns **only** `Allow` or `Deny` — `RequireApproval` is never constructed
  anywhere in `ironclaw_authorization` (`src/lib.rs:1088-1135`). The
  `RequireApproval` handling in `ironclaw_capabilities` is a dead branch.
- **No hook dispatcher / hook-gate factory is installed.** The machinery exists
  (`ironclaw_reborn::loop_driver_host`), but composition never calls
  `with_hook_dispatcher_factory` / `with_hook_gate_ref_factory` — a search across
  `ironclaw_reborn_composition/src` returns none, and `build_default_planned_runtime`
  (the path `serve` uses) builds the host factory without them.
- **Budget approval fails, not gates.** `BudgetApprovalRequired` maps to a model
  error / `Failed` run, not to `BlockedApproval`.

So the only blocked-prompt that fires under local-dev is `BlockedAuth`, emitted as
the **separate `auth_required` event** — which is exactly the connector-credential
flow Phase 8b already handles. **No tool-approval `gate` event can fire.** This
confirms and extends the Phase 4 finding (`default_permission` is never read;
`RequireApproval` has zero production producers).

## What that means: consent-sensitive tools run unprompted

Every builtin is authorized by a standing grant → `Decision::Allow` and runs with
no prompt. Verified live (not just from source):

| Capability | Consent-sensitivity | Runs unprompted? | Pinned by |
|---|---|---|---|
| `builtin.skill_install` | **high** — installs code the model runs at full trust | yes | `skill_install.rs` (turn completes, no gate) |
| `builtin.apply_patch` | medium — edits existing workspace files | yes | `approval_gate_dormant.rs` (this phase) |
| `builtin.write_file` / `shell` / `extension_install` | medium–high | yes | (granted; `shell` broad by design) |
| `builtin.trigger_create` | medium — arms a recurring schedule | yes | `ambient_surfacing.rs` |

`apply_patch` and `write_file` are **workspace-mounted**, so they can only touch
the agent's `/projects` workspace, not arbitrary host paths — that mount is the
real bound on them, not a prompt.

## The capability-policy "backstop" cannot provide approval

`local_dev_capability_policy.toml` (compile-time-embedded, read at runtime by
`serve`) is an **allow-list of grants**: each entry maps a capability to an allowed
effect set + mount + network profile. There is **no deny or approval tier** — the
only knobs are *narrow the effects* or *omit the entry* (deny-by-absence →
`Decision::Deny { MissingGrant }`). You cannot express "list it but require
approval".

So the backstop can only *remove* a capability, and:

- It is a **core file** — changing it is a `core-patch:` (golden rule #1), replayed
  on every upstream merge.
- The consent-sensitive caps we actually **use** — `skill_install`, `skill_remove`,
  `shell`, `extension_install`, `apply_patch` — cannot be removed without breaking
  Phase 4/7/8 features.
- There is no cap we clearly want gone. So we take **no core patch here**; the
  backstop buys nothing worth its cost.

## Decision

1. **Do not build a universal gate card.** The mechanism never triggers; UI over it
   would be dead code (the spec's explicit instruction).
2. **The standing consent mechanism stays the widget-side pattern** — the Phase 7b
   two-step (review in the dashboard / a red consent card in the bubble → a
   *deterministic* action with no LLM between the yes and the effect) and the Phase 4
   browser-sidecar consent gate (classify → ask → then type). These guard the two
   flows that genuinely need consent: **installing model-authored code** (skills)
   and **typing secrets into a page** (browser fill). They gate *in surfaces we
   own*, which is the only place a gate can actually be enforced given the runtime
   emits none.
3. **No double-prompt to reconcile.** Since the runtime never emits a tool-approval
   gate, the widget-side gates are the sole owner of every decision — the "one owner
   per decision" concern in the spec cannot arise.

## The tripwire

`crates/ic_integration_tests/tests/approval_gate_dormant.rs` drives `apply_patch`
through a real gateway and asserts the run **completes** with **no `gate` event and
no `blocked_approval`**. The day upstream wires `RequireApproval` (or installs a
hook gate), that run will park at `blocked_approval` and a `gate` event will appear
— both assertions flip. That failure is the signal that the universal tool-approval
gate is finally live and worth rendering as the red consent card. Until then, this
document is why the card does not exist.
