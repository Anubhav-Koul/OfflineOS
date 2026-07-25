# Desktop fork — build-phase history

History of what actually shipped, phase by phase, newest first, moved verbatim
out of `CLAUDE.md` during the 2026-07-24 doc restructure (see that file's git
history for the pre-restructure single-file version). Every session-relevant
takeaway from these notes is compressed into `CLAUDE.md`'s "Current status"
block; this file is the record of *why*, kept in full because it has caught
real bugs before (see the "Phase 4 lesson" referenced repeatedly below).

The dated notes are followed, at the very bottom, by the original Phase 0–6
build-phase plan text — the oldest content here, written before any of it was
built. Phases 7 and 8 had long enough plans to get their own files:
`docs/desktop/phases/phase7.md` and `phase8.md`.

Standing instruction: future dated progress notes are appended to this file,
newest entry at the top (right after this header) — not to `CLAUDE.md`.

---

## Phase 8e addendum — what a real third-party skill taught the review card (recorded 2026-07-25)

`crates/ic_widget` (`skill_import.rs`, `git_import.rs`, `main.rs`) + `ui/`
(`dashboard.tsx`, `api.ts`, `styles.css`). **No core patch, no new dependency.**
Contracts verified against the pinned upstream commit **`a492857`**.

The 8e importer was exercised against a second real repo —
[`pskoett/self-improving-agent`](https://github.com/pskoett/self-improving-agent)
— and it imported cleanly. What it *couldn't say* is what this addendum is about.

### The finding: half of what installed could never run, and nothing said so

**`ironclaw_skills` has no hook concept.** Not "a different hook API" — the crate
does not contain the word (verified: zero occurrences across
`crates/ironclaw_skills/`). The only thing the runtime does with a bundle beyond
`SKILL.md` is put the bundle's path in the prompt (`format_skills` in
`orchestrator/default.py` appends `Installed bundle path on disk:`); there is no
dispatcher, no event, no lane. The fixture skill ships
`hooks/openclaw/{handler.js,handler.ts,HOOK.md}` — an event handler its own host
runs — and that half installs here, sits on disk, and never fires.

Nothing in the review told the user that. **That is the failure mode: learning a
skill is inert by wondering why nothing happens.** So the card now says it, at
review time, before the yes:

> This skill's automatic parts (hooks) will not run in this app; only the
> instructional parts will.

Structured as a table (`INERT_LANES`, one `LaneRule` row: marker directory +
what to call it), because the next lane will be found exactly the way this one
was — by importing a real skill written for a host that has one and noticing we
don't. Adding a row is the whole change; matching, wording, and both cards are
shared. A nested `docs/hooks/` is deliberately *not* a lane: only a top-level
`hooks/` directory or a root `hooks.*` file counts.

### The second thing the text can't tell you: what it costs to keep it

An installed skill is the **trusted tier** — `format_skills` puts its whole body
between `<skill>` tags and nothing trims it. That is a recurring cost on the
user's context, paid on every activation, and the moment to see it is the moment
of consent, not afterwards when replies have quietly got worse. The card now
carries a line like:

> Trusted tier: the full 20.1 KB body is injected into the model's context on
> activation — about 5,142 tokens, 85% of the 6,000 the runtime allows all
> active skills in one turn.

Every number in it is the runtime's, not ours:

- **0.25 tokens/byte** is `ironclaw_skills::selector::skill_token_cost` (and its
  Python mirror `_skill_token_cost`) — the arithmetic that actually decides
  whether a skill fits a turn.
- **6,000 tokens** is `LOCAL_DEV_MAX_SKILL_CONTEXT_TOKENS` in
  `ironclaw_reborn_composition/src/runtime.rs` — our profile's whole-turn skill
  budget, shared by up to three skills. A skill that doesn't fit what's left is
  **dropped, not truncated** (`TrySelectOutcome::BudgetFull`).
- The cost is charged on the **body**, not the file: the runtime keeps
  `prompt_content` (everything after the frontmatter) and prices that.

**The warning threshold is 8 KiB of body, and the reasoning is the point.** At
the runtime's own rate that is 2,048 tokens — the 2,000 of
`default_max_context_tokens()`, which is the cost the selector *assumes* a skill
has when it declares none. Past that line three things become true at once: the
skill costs more than it says, it eats a third of the turn budget it shares with
two others, and on the 16k window `ic_llama` gives a small local model
(`MIN_AGENT_CTX`) it is a visible slice of the context before the user's own
message is added. Below it, the cost is worth showing and not worth a warning.
Both facts render through one shared `SkillReviewFacts` component, so a skill
reviewed through the folder door and the git door is told the same thing.

### The name that said itself twice

`pskoett/self-improving-agent` ships one skill, `self-improving-agent` — and a
single-skill repo is usually named after its skill. Blindly prefixing gave the
user `pskoett-self-improving-agent-self-improving-agent` in their skills list,
forever. `namespaced_name` now collapses it: when the slug already ends in the
skill's name, the slug *is* the namespaced name — it still carries the owner, so
two repos stay distinguishable. Three assertions pin it: the collapse, the
exact-equal case (`thing`/`thing`), and that a slug merely *containing* the skill
name (`owner-agent-tools` + `agent`) still gets prefixed.

### ClawHub's install methods don't reach us; the git URL does

The fixture's README offers three ways in, and all three are for a host we are
not: `clawdhub install <slug>` (a registry CLI we don't ship), a manual
`cp -r` of the skill subfolder into `~/.openclaw/skills/`, and a separate
`cp -r hooks/openclaw ~/.openclaw/hooks/…` plus a gateway restart for the hook
half. None of those paths exist here. What *does* transfer is the thing
underneath all of them — the plain **git URL** — which is exactly what 8e's
importer consumes, subfolder skill and all. Worth recording because it
generalizes: a skills repo's advertised install route is usually host-specific
packaging over a git URL, and the URL is the portable part.

### The fixture is kept, and the skill is not

The repo URL stays as the importer's **canonical fixture** in an `#[ignore]`d
networked test (`clones_the_canonical_fixture_and_reports_its_inert_half`),
because it is four awkward things at once: the skill lives in a **subfolder**,
its description is a **multi-line quoted scalar**, its frontmatter carries a
**valueless metadata key** (`metadata:` with no value), and it ships a **foreign
hook bundle**. The test asserts the shape — the name doesn't stutter, the
description reads, the hook lane is reported inert — loosely, because the repo is
upstream's to change.

**The skill itself was uninstalled after testing, deliberately.** Its automatic
half is inert here (no hook lane), and its remaining half — logging learnings and
errors to `.learnings/*.md` — duplicates what Phase 7b's reflection loop already
does natively. 21 KB of always-injected prompt for functionality we already have
is a bad trade on a small local model, and pretending otherwise in our own skills
list would have been the dishonest version of a successful import. Verified: the
widget-owned skills root (`%LOCALAPPDATA%\IronClaw Desktop\reborn\local-dev\
skills`) holds nothing.

### Gate

fmt ✅, clippy `-D warnings` ✅ (`ic_widget --lib --tests`, `ic_integration_tests
--all-targets --features webui-v2-beta`), `cargo test -p ic_widget --lib` **237
pass** (up from 232), the parser-agreement gate green, frontend `tsc --noEmit` +
build clean. The new networked fixture test was run live against the real repo
and passes; the cost line quoted above is its actual output.

## Phase 8e notes — skills from git repos, and "study this repo" (recorded 2026-07-22)

`crates/ic_widget` (new `git_import.rs`; `skill_import.rs`, `skills.rs`,
`ambient/reflection.rs`, `main.rs`, `lib.rs`, `Cargo.toml`) + `ui/`
(`dashboard.tsx`, `api.ts`, `styles.css`) + a new gate
(`ic_integration_tests/tests/skill_parser_agreement.rs`) + two new vetted
dependencies (`gix`, `serde_yml`). **No core patch.** Contracts verified against
the pinned upstream commit **`a492857`**.

### The finding that reshaped the sub-phase: our importer refused real skills

The plan's ⚠️ constraints were all on the *clone* (no `git.exe`, depth-1, caps,
symlinks). Those were the easy part. The bug was one layer in, and it took
driving a **real** repo to find it: cloning
[`anthropics/skills`](https://github.com/anthropics/skills) — eighteen skills,
about as canonical a corpus as exists — **failed the entire import**.

Two separate causes, both ours:

1. **The importer was parsing YAML with a line scanner.** 7c's `preview` reused
   `reflection::parse_skill_md`, which was written for Phase 7b to read a
   *model's reply* — a hand-rolled scanner whose narrowness is the fail-closed
   posture that path wants. A third-party file is the opposite problem: it is
   real YAML written by someone else. `claude-api` writes `description: |-` as a
   block scalar; the scanner read it as absent and refused a skill
   `builtin__skill_install` takes without comment. The scanner also imposed a
   1,000-character description cap and kebab-only names — **neither of which the
   runtime has** (its grammar is `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}`, and
   `description` is `#[serde(default)]`, so a skill with none is legal).
2. **One bad folder refused the whole repo.** Even after the parse was fixed,
   `claude-api`'s SKILL.md is 72 KiB — over the runtime's own
   `MAX_PROMPT_FILE_SIZE` (64 KiB), so it genuinely cannot install. Aborting the
   scan on it threw away the seventeen skills the user actually wanted.

Both are fixed, and the shape of the fix is the point: the importer now applies
**the runtime's rules**, via the same crate the runtime parses with
(`serde_yml`), and a rejection is **per folder** — reported with its reason, never
swallowed. The real repo now yields 17 skills plus one honest line: *"skills/
claude-api: SKILL.md is larger than the 64 KiB limit the runtime puts on a skill."*

### Two parsers, one contract — and the gate that keeps them honest

Applying someone else's rules from a second implementation is exactly the shape
this repo has been burned by three times (Phase 4 discovery, Phase 8b's
`/extensions` parser): a belief held in two places that agree with each other and
never meet the real thing. So `skill_parser_agreement.rs` is the meeting. It feeds
one corpus — block scalars, folded scalars, quoted colons, BOM, CRLF, unknown
keys, unclosed frontmatter, malformed YAML, a name outside kebab-case — to **both**
`ic_widget::skill_import::parse_skill_md` and the real
`ironclaw_skills::parse_skill_md`, and fails on any disagreement about accept/
reject, name, or description. `ic_integration_tests` now dev-depends on
`ironclaw_skills` for exactly this; it needs no gateway, because the parser is a
library and it is the same code `serve` runs.

A second test states the **two deliberate divergences** rather than leaving them
to be discovered: the importer refuses a >64 KiB SKILL.md that the runtime's
*parser* accepts (the runtime rejects it later, at `skill_install` — better to
tell the user before they consent than after), and it refuses a Windows reserved
device name that the runtime's grammar allows (it has to become a directory here).
Both assertions name the runtime's own constant, so neither can drift into a
number of our own.

`reflection::parse_skill_md` is **unchanged** and still reads model replies; its
doc comment now says so, and says why the import path does not use it.

### The clone, and its guards

- **`gix`, never `git.exe`** — pure Rust with the rustls transport (matching
  `reqwest`), so a user without git installed is not a broken feature. Depth-1
  via `Shallow::DepthAtRemote(1)`, no submodules (a plain `gix` clone fetches
  neither), into a caller-owned temp dir.
- **The timeout is an interrupt flag, not a cancellation.** `gix`'s transport is
  blocking, so the clone runs on `spawn_blocking` — which cannot be cancelled.
  `gix` polls an `&AtomicBool` during fetch and checkout; a timer trips it, and a
  tripped flag is reported as a timeout rather than as a fetch error.
- **Symlinks abort the whole import**, at the outermost of three guards (the
  scan, then `skill_import`, then the runtime). A link inside a clone can point
  anywhere on the machine.
- **Namespacing rewrites the frontmatter `name:`**, because that — not the
  directory — is what the runtime keys a skill's identity on. `<repo-slug>-<name>`,
  normalized and bounded to 64 characters. Only the top-level `name:` is touched;
  a nested `activation.name` and the body survive verbatim, pinned by a test.
- **The clone is held in app state** until an install copies from it, and is
  dropped (deleting the temp dir) when the next clone replaces it.
- **Untrusted text, per the plan:** the review renders the full SKILL.md as plain
  text in a `<pre>`, never markdown; zero-width and bidi characters are flagged by
  name; a re-import shows an LCS line diff against the installed version and the
  install is an explicit *update* (the same-named skill is removed first, because
  `skill_import::install` refuses to overwrite by design).

### "Study this repo" — what it is, and what it deliberately is not

`clone_and_study` + `study_prompt` are the same guarded clone for a repo that has
no `SKILL.md`. The reading list is **bounded and ranked**: README first, then root
manifests, then `AGENTS.md`/`CLAUDE.md`/`CONTRIBUTING.md`, then `docs/**.md` —
12 files, 8 KiB each, 40 KiB total, depth 3, skipping `.`-dirs, `node_modules`,
and `target`. Source is not study material; a small local model cannot read a
repository, and reading its code would spend the whole budget learning less. The
gathering is a **pure function over a directory**, so every cap is a unit test
rather than a network round-trip, and the panel reports which files were actually
read and how many were skipped — a study that read three files is one to distrust,
and hiding that would be the dishonest part.

The turn runs on **its own fresh thread**: a study is *solicited*, so (like a 7c
import) it must work with ambient off and must never spend a guardrail slot. Its
draft lands on the bubble as the same red consent card, and installs — on yes
only — through the 7b deterministic file write. `PendingImport.folder` became an
`Option` for this: a studied draft is text the model wrote, not a directory
someone shipped, so there is no bundle to copy.

**Item 3(b) is scoped honestly.** The panel *observes* a repo's tool surface from
its manifests ("an MCP server written for Python", "a command-line tool
installable from npm") and points at the Connectors panel. It does not register
anything: 8b's registry lane works, but wiring an arbitrary repo up is not
something the widget can do on the user's behalf, and a button that pretended
otherwise would be the worst outcome. The 7b model-quality hint is on the panel
whenever a local model is active.

### Gate

fmt ✅, clippy `-D warnings` ✅ (`ic_widget --features app`, `ic_integration_tests
--features webui-v2-beta`), `cargo test -p ic_widget` 232 pass (up from 211), the
new agreement gate green, frontend type-checks and builds. Two `#[ignore]`d
networked tests drive the **real** thing: a repo with no skills (the clone, walk,
and guards run; the scan reports none) and `anthropics/skills` (17 namespaced
skills + 1 reported rejection). All 79 crates the two new dependencies pull in
carry `deny.toml`-allowed licences (MIT / Apache-2.0 / Unlicense / Zlib);
`cargo-deny` itself is not installed on this machine, so the audit was scripted
over `cargo metadata`.

**No new *serve* route is consumed**, so there is no new HTTP contract to pin —
8e's contract is the on-disk SKILL.md format, and the agreement gate is what
catches upstream changing it. The install path is already gated end to end by
`skill_import_gate` (a reviewed folder is visible to the running agent by name)
and `skill_install` (root → listed → activatable → body injected); a git import
uses the same `skill_import::install`. The study's LLM hop is not separately
gated — `drive_turn` is exercised by the 7a/7b gates — and the manual smoke run
is the human step: clone a real skills repo, review, install one, re-import to see
the diff, and study a repo that ships none.

Next: the remaining Phase 8 topics — **channels** (Telegram behind its flag, with
pairing enforced) and **memory seeding + subagent visibility** (re-scoped per the
8c correction: `memory_import`/`memory_seed` are not agent tools; a real seed goes
through `builtin.memory_write`).

## Phase 8d notes — universal approval gates (recorded 2026-07-22)

`ic_integration_tests/tests/approval_gate_dormant.rs` (new canary) +
`docs/desktop/approval-gates.md` (new, the full write-up). **No core patch, no
new UI, by design.** This is the spec's "expected case": the ⚠️ VERIFY confirmed
the runtime's tool-approval gate never fires under our profile, and the
instruction is explicit — *do not build UI over a mechanism that doesn't trigger*.

### The VERIFY, answered from source and then driven live

- **The `gate` SSE event IS the tool-approval channel** (distinct from
  `auth_required`, the credential gate Phase 8b handles) — but it has **no
  producer under `local-dev`.** A `gate` needs `TurnStatus::BlockedApproval` needs
  `Decision::RequireApproval`; the only wired authorizer, `GrantAuthorizer`,
  returns solely `Allow`/`Deny` (`RequireApproval` is never constructed), no hook
  dispatcher is installed in composition, and budget-approval *fails* a run rather
  than gating it. This confirms and extends the Phase 4 finding.
- **`apply_patch` is active and runs unprompted** (`builtin.apply_patch`,
  workspace-mounted, handler-registered). The one consent-sensitive cap not
  otherwise driven by a gate test — so 8d drives it **live** (Phase 4 lesson: a
  source trace is not a running gateway).
- **`skill_install` runs unprompted** — re-confirmed (already pinned by 7b).

### The capability-policy "backstop" cannot approve — only remove

`local_dev_capability_policy.toml` is a compile-time **allow-list of grants**
(capability → allowed effects + mount + network). It has **no deny/approval
tier**: the only moves are narrow-the-effects or omit-the-entry (deny-by-absence
→ `Decision::Deny{MissingGrant}`). So it can *remove* a capability but never make
one "ask" — and it's a **core file** (a `core-patch:`). The consent-sensitive caps
we actually use (`skill_install`, `skill_remove`, `shell`, `extension_install`,
`apply_patch`) can't be removed without breaking Phase 4/7/8, and there's no cap
we clearly want gone. **So: no core patch — the backstop buys nothing worth its
cost.**

### Decision (unchanged posture, now documented and pinned)

The standing consent mechanism stays the **widget-side** patterns — the 7b
two-step (review → red bubble card → a *deterministic* action, no LLM between the
yes and the effect) and the Phase 4 browser-sidecar gate — which guard the two
flows that genuinely need consent (installing model-authored code; typing secrets
into a page) *in surfaces we own*, the only place a gate can be enforced when the
runtime emits none. No double-prompt to reconcile: the runtime never emits a
tool-approval gate, so the widget gates are the sole owner of every decision.

### The tripwire

`approval_gate_dormant.rs` drives `apply_patch` through a real gateway and asserts
the run **completes** with **no `gate` event and no `blocked_approval`** on the
projection stream. The day upstream wires `RequireApproval` (or a hook gate), the
run parks at `blocked_approval` and a `gate` frame appears — both assertions flip,
and *that* is the signal to build the universal consent card. Until then,
`docs/desktop/approval-gates.md` is why it doesn't exist. Same shape as the
`the_event_stream_never_carries_the_assistant_text` canary.

### Gate

fmt ✅, clippy `-D warnings` ✅ (`ic_integration_tests`), the canary passes against
a real `serve` (`apply_patch` dispatched, run completed, no gate). No unit/UI
changes — this sub-phase is a verified negative result, not a feature.

Next: the remaining Phase 8 topics — **skills from git repos**, **channels**,
**memory seeding + subagent visibility**.

## Phase 8c (voice picker) notes — the switchable TTS voice (recorded 2026-07-22)

`crates/ic_voice` (`assets.rs`, `lib.rs`) + `crates/ic_widget`
(`voice.rs`, `settings.rs`, `main.rs`) + `ui/` (`dashboard.tsx`, `api.ts`,
`styles.css`) + `docs/desktop/voice-cloning.md` (new) + `voice-notes.md`.
**No core patch.** This is the *plan's* 8c ("voice picker"), done after the
dated-notes 8c ("runtime surfaces"); the letters collided when the notes
re-sequenced, so both are recorded under 8c by topic.

### The three ⚠️ traps were answered by the VERIFY, and two needed no code

- **(b) sample rate — already fully dynamic.** `PiperTts::new` reads each voice's
  rate from its `.onnx.json`; `Speech` carries it; `playback::resample_to` builds
  the resampler from `speech.sample_rate`; the lip-sync tap runs at the device
  rate. Only prose comments hardcoded "22.05 kHz". Lip-sync **gain** is a fixed
  `EnvelopeFollower::GAIN` with an RMS clamp — robust across voices (Piper voices
  share normalization), so no per-voice calibration.
- **(c) live-switch teardown — already handled** by the existing `restart_voice`,
  whose doc comment *already named* "TTS voice" as a boot-resolved input. Its
  `shutdown()` drives the driver's exit path → `playback.stop()` (sets the atomic
  the audio callback watches), releasing the device before the rebuild. No
  deadlock switching mid-sentence.
- **(a) espeak-ng phoneme data — sidestepped.** The catalog is **English only**;
  every voice shares the English espeak-ng data the bundled Piper proves it ships
  (amy works). Non-English is gated on verifying its phoneme data ships — a
  documented follow-up, not a listed-then-broken voice.

So the picker was mostly *catalog + wiring*, not new audio code — the architecture
was already voice-agnostic below the synthesizer.

### Pinning without downloading 315 MB

The 5 curated voices' `.onnx` digests come from **HuggingFace's git-LFS metadata**
(`tree` API → `lfs.oid` = content SHA-256, cross-checked against amy's existing
pin), so a voice is pinned authoritatively without fetching its 63 MB model; the
tiny non-LFS `.onnx.json` configs were hashed from the fetched files. **All 10
pins were re-verified against live HF data at commit time** (a scripted check, not
a committed networked test — adding `reqwest`+`sha2` dev-deps for one ignored test
was disproportionate).

### What shipped

- `ic_voice::assets`: `PiperVoice`/`VOICES`/`DEFAULT_VOICE_ID` (amy, so existing
  installs sound unchanged) + `find_voice`/`voice_or_default` (unknown/dropped id
  → default, never disables voice). `VoiceAssets` is voice-parameterized; shared
  whisper/`piper.exe` are skipped on a switch, so only the new model transfers.
- `settings.voice_id`; `voice::start` resolves it at pipeline start.
- Widget: `voice_catalog` + `set_voice` (persist always; if running, download with
  progress on `voice://voice-download` then `restart_voice`; if off, save and
  apply on next enable — no download forced on a browsing user). UI: a
  `VoicePickerPanel` in the Voice panel.
- `docs/desktop/voice-cloning.md`: cloning is out of scope; the note records the
  engine candidates (XTTS non-commercial, F5-TTS, GPT-SoVITS), VRAM/latency/consent
  constraints, the clean `Synthesizer`-subprocess seam a future engine would use,
  and the offline "train a real Piper voice" alternative.

### Gate

fmt ✅, clippy `-D warnings` ✅ (`ic_voice`; `ic_widget --features app`),
`ic_voice` 88 + `ic_widget` 211 unit tests ✅ (catalog well-formedness,
unknown-id fallback, the dynamic sample-rate path), frontend builds, all 10 pins
verified against HF. **Manual smoke (the human step, matching the `#[ignore]`d
real-asset tests):** enable voice, pick each voice, confirm it downloads and
speaks, and that switching mid-utterance cuts the old voice cleanly.

Next: the remaining Phase 8 topics — **universal approval gates**, **skills from
git repos**, **channels**, **memory seeding + subagent visibility**.

## Phase 8c notes — the runtime's own surfaces (recorded 2026-07-21)

`crates/ic_widget` (new `skills.rs`; `main.rs`, `lib.rs`) + `ui/`
(`dashboard.tsx`, `api.ts`, `styles.css`) + a new gate
(`ic_integration_tests/tests/skills_panel_gate.rs`) + `docs/desktop/dashboard-gaps.md`.
**No core patch.** The ⚠️ VERIFY drove the running gateway and the on-disk state
for all four surfaces before any panel was designed, and it split them cleanly in
two — with a fork-policy decision (**hold golden rule #1**) settling the rest.

### The VERIFY: on-disk files the widget owns vs. the gateway's private libSQL

The four surfaces are **not** one gap; they are two, and the difference is *who
owns the bytes*:

- **Skills — a stale "unavailable" claim, actually buildable.** User skills are
  plain files at `<reborn-home>/local-dev/skills/<name>/SKILL.md` — the exact
  directory the widget already *writes* (7b reflection installs, 7c folder
  imports), proven read-back by `skill_install.rs`. Reading a directory the
  widget co-owns is not DB coupling; it needs no route and no LLM turn. The old
  "skills are an in-agent tool, not an HTTP route" reason was wrong: there is no
  route, and there does not need to be.
- **Memory and audit — the gateway's *private* libSQL store.** Both live in
  `reborn-local-dev.db`: memory in `root_filesystem_entries WHERE path LIKE
  '/memory/%'` (plaintext markdown), audit in `root_filesystem_events WHERE path
  LIKE '/events/audit/%'` (bare, **unversioned** `ironclaw_host_api::AuditEnvelope`
  JSON). Each has a Rust read path but **no HTTP route and no composition handle**.
  Surfacing either means reading the gateway's private DB directly — the coupling
  `dashboard-gaps.md` and golden rule #1 refuse. (Confirmed unstable: the
  `ironclaw_memory` CLAUDE.md documents `reborn_memory_*` tables the local-dev
  wiring does **not** use — the internal layout already diverges from its own docs.)
- **Run history** has no cross-thread enumeration anywhere; a conversation's own
  history is already the 8a Chats timeline.

**Decision (the user's call, taken deliberately): hold the line.** Ship only the
Skills panel; leave memory/audit/run-history unavailable, with the reasons
sharpened from vague "no route" to the precise "in the private libSQL store —
reading it would couple us to internals; the honest fix is an upstream route"
(the backlog). Golden rule #1 stays intact. The alternative (a read-only DB
browser for memory/audit) was offered and declined.

### What shipped: the Skills panel (list + remove)

- **`ic_widget::skills`** — pure, `Path`-in, testable like `skill_import`:
  `list(root)` reads each subdir with a `SKILL.md`, reusing `reflection::parse_skill_md`
  for the description (a malformed `SKILL.md` is **still listed**, flagged
  `valid: false` — it is on disk and the user may want to prune it, and a blank
  description that looked like a bug is worse). `remove(root, name)` deletes one
  directory, gated by `plain_component` — a single `Component::Normal`, re-checked
  after the join so a separator that slipped the component check still cannot
  delete outside the root (pinned by `remove_refuses_a_traversal`, which asserts a
  sibling dir survives `..`, `../outside`, `/etc`, `a/b`).
- **Only user skills appear.** The runtime's *bundled* skills are written to a
  separate `local-dev/system/skills` tree (a runtime-managed dir with an install
  lock and stale-removal) — this reconciles the CP-1 "serve writes bundled skills"
  note with 7b's "embedded, not on disk": they *are* on disk, just in the
  gateway's own managed dir, which the widget leaves alone. The panel header says
  built-ins are managed separately, so the list is honest about its scope.
- **Symmetric ownership.** Install (7b/7c) writes a directory here; remove deletes
  one. A skill is user-authored procedure, not LLM data, so a user-initiated
  removal (inline two-click confirm in the panel, not a modal) does not touch the
  never-delete invariant — the same reasoning as the permitted dashboard wipe.
- **Dashboard**: an `InstalledSkillsPanel` above the existing 7c import panel in
  the Skills section; `list_installed_skills` / `remove_installed_skill` commands;
  the `UNAVAILABLE_PANELS` list drops "Skills list" and sharpens the other three.

### The gate

`skills_panel_gate.rs` has the agent install a skill through
`builtin__skill_install` against a **real** `serve` (the `skill_install.rs`
pattern), then calls the **shipping** `ic_widget::skills::list`/`::remove` over the
directory the gateway actually wrote — asserting the panel sees it with the
on-disk description and that remove deletes exactly it. 8c consumes **no new serve
route** (it reads the filesystem), so the contract it pins is the on-disk skills
*layout*; the gate is what catches upstream ever moving user skills or reshaping
`SKILL.md`. Verified against the pinned upstream commit **`a492857`**. Unit tests
(8, in `skills.rs`) cover the absent-root-is-empty, sorted listing, non-skill
dirs, the malformed-but-listed case, footprint counting, and every `remove`
refusal.

### Two corrections the VERIFY surfaced for a later sub-phase (not 8c)

- **`memory_import` and `memory_seed` are NOT agent tools.** They exist only as
  `PromptWriteOperation` audit-labeling enum variants — there is no capability
  manifest or dispatch arm for either. This **invalidates the premise of the
  memory-seeding sub-phase** ("onboarding via `memory_import`/`memory_seed`"): the
  four real memory tools are `builtin.memory_{search,read,tree,write}`, and a real
  seed goes through `memory_write`. Re-scope that sub-phase against those before
  building UI.
- **A memory browser is not blocked by "no route" but by policy.** Memory is
  plaintext in `root_filesystem_entries WHERE path LIKE '/memory/%'`; if the
  golden-rule-#1 line is ever relaxed for a read-only surface, a browser is a
  direct SQLite read away. Recorded so the option is not re-derived from scratch.

### Gate status

fmt ✅, clippy `-D warnings` ✅ (`ic_widget --features app` + `ic_integration_tests
--features webui-v2-beta`), `cargo test -p ic_widget` 211 pass, the new integration
gate green against a real gateway. Manual click-through smoke (open the Skills
panel, list, remove-with-confirm) is the human step, as in 8a — the automated gate
already drives the shipping list/remove against the real runtime.

Next: the remaining Phase 8 topics — the **voice picker**, **universal approval
gates**, **skills from git repos**, **channels**, and **memory seeding + subagent
visibility** (re-scoped per the `memory_import`/`memory_seed` correction above).

## Phase 8b.1 notes — the Gmail OAuth lane, wired (recorded 2026-07-15)

`crates/ic_widget` (new `oauth_callback.rs`; `secrets.rs`, `settings.rs`,
`error.rs`, `gateway_client/mod.rs`, `main.rs`) + `ui/` (`dashboard.tsx`, `api.ts`,
`styles.css`) + a new gate (`ic_integration_tests/tests/connector_oauth_wired.rs`).
**No core patch.** The 8b notes stopped Gmail at a documented 503; 8b.1 closes the
gap the additive way the whole fork is built.

### The ⚠️ VERIFY answered from source, then driven against a running `serve`

`serve` reads its Google OAuth **client** from the environment once at boot
(`resolve_google_oauth_config`, `ironclaw_reborn_cli/src/runtime/mod.rs:436`):
`IRONCLAW_REBORN_GOOGLE_CLIENT_ID`, `..._CLIENT_SECRET` (optional → public-client
PKCE), `..._OAUTH_REDIRECT_URI`. Two facts decided the design:

- **The redirect URI is *ours* to choose.** `serve` embeds `config.redirect_uri()`
  verbatim into the Google authorization URL and, later, into the token exchange —
  it never checks the request's own host/port against it. So the redirect can point
  anywhere, including a widget-owned listener.
- **But `serve` owns the token exchange.** Its Google callback handler
  (`product_auth_serve/oauth.rs::google_oauth_callback_handler`) holds the PKCE
  verifier it minted during `oauth/start`, in a process-local cache, and does the
  code→token swap itself. Only `serve` can complete the flow.

So the widget **cannot** simply own the callback; it owns the *stable address* and
**proxies** the redirect into `serve`. Pinned by `connector_oauth_wired.rs`, which
boots `serve` with a well-formed fake client whose redirect URI is our fixed-port
loopback, and asserts the same start route that answered **503** in
`connector_oauth.rs` now answers **200** with a consent URL carrying our client id,
**our redirect URI percent-encoded verbatim**, and a CSRF `state`. That is the whole
widget-side contract; the two tests are each other's foil (no-client → 503,
client → 200).

### The fixed-port decision, and why

Google matches a registered redirect URI byte-for-byte, and the widget takes a
*fresh OS-assigned* port for `serve` every launch (two instances must coexist). A
redirect URI built from `serve`'s port would force the user to re-register with
Google on every launch — unusable. So OAuth gets the one **stable** address in the
system: a widget-owned loopback listener on a **fixed, configurable** port
(`settings.google_oauth.callback_port`, default **51789** — uncommon, rarely
clashes). The user registers `http://127.0.0.1:51789/api/reborn/product-auth/oauth/google/callback`
with Google once; it survives relaunches. The port is configurable because 51789
may be taken, and changing it restarts the gateway (the redirect URI is boot-time
env) and requires re-registering with Google — the panel shows the current URI with
a copy button so there is no guessing.

### `oauth_callback.rs` — what the listener owns (and what it doesn't)

- **Loopback bind only** (`127.0.0.1`), never a routable interface.
- **Proxy, not handler.** It forwards the browser's callback — path and query
  verbatim, `Accept: text/html` — into `{serve_base}/api/reborn/product-auth/oauth/google/callback`,
  and streams `serve`'s own "you can close this window" page back. The markup and
  the token exchange are `serve`'s; the listener is a stable doorway.
- **A CSRF binding the widget can actually enforce.** `serve` generates and
  cryptographically validates the opaque `state`; the widget cannot re-derive it,
  but it *can* extract the `state` from the authorization URL `serve` returned and
  require the callback to carry that exact value before forwarding. A mismatched or
  missing `state` is a `400` and is **never** forwarded, and deliberately does *not*
  consume the one-shot latch — a forged hit cannot burn the genuine callback.
- **One-shot** (a replay is `409`) and **closed when idle**: the port is bound only
  for the duration of one flow (`arm()` binds, `ArmedListener::wait()` tears down),
  and binding happens **before** the browser opens so a port clash is an error the
  user sees, not a browser onto a dead redirect.

### The one env difference from every other secret

The Google client id **and secret** enter `serve`'s environment on purpose
(`google_oauth_env` in `main.rs`). This is the exact opposite of the cloud-failover
key, which the `ic_llama` proxy deliberately keeps *out* of `serve` because the
proxy owns that retry. Here `serve` owns the OAuth token exchange, so it genuinely
needs the secret. The client lives in the OS credential store
(`SecretStore::{set,has,clear}_google_oauth`), never `settings.json`, and the client
id/secret never round-trip back to the webview — the panel asks only
`has_google_oauth`.

### The panel

For any installed OAuth connector the dashboard now renders a shared **Google
sign-in** block (one client covers Gmail/Drive/Calendar): it links the exact console
page, shows the redirect URI to register with a copy button, and takes the client id
+ secret. Once configured, each OAuth connector row shows an **Authorize** button
that runs `authorize_google_connector` end to end — read the OAuth secret's
invocation/scopes, `oauth/start`, arm the listener, open the **system** browser
(Google blocks embedded webviews), await the proxied callback, confirm the
credential landed by polling the setup projection (the honest check — a 2xx callback
means `serve` accepted it, not that the account was stored), then activate. The
character goes `concerned` while the user is away consenting.

### Where the gate stops, and the manual smoke item

`connector_oauth_wired.rs` drives every automatable link — env wiring, install,
setup, and the start route producing a consent URL with our redirect. It stops
exactly where GitHub's real-token read stopped in `connector_verify.rs`: the
callback is a real token exchange against Google's servers, needing a real OAuth
client, a real user consenting, and a real Gmail account. **Manual smoke gate:**
register a real client, `Authorize` Gmail through the panel, then ask "summarize my
latest email" and confirm the agent reads it — the one hop that needs a human and a
mailbox, not a fixture.

Next: **8c — the runtime's own surfaces** (memory, skills, audit, run history), and
Gmail's OAuth callback is no longer among its open items.

## Phase 8b notes — connectors (recorded 2026-07-15)

`crates/ic_widget` (`gateway_client`, `main.rs`, `character.rs`) + `crates/ic_llama`
(`proxy.rs`) + `ui/` (`dashboard.tsx`, `widget.tsx`, `chat.ts`, `api.ts`) + three
gates (`connector_verify.rs`, `connector_oauth.rs`, `tool_flood.rs`) + C5–C8 in
`gateway-api-notes.md`. **No core patch.** The registry lane works as upstream built
it; everything below is additive.

### The ⚠️ VERIFY answered: the registry lane works, all the way to GitHub's API

Install → credential → activate → **a real tool call that leaves the machine**. The
proof is a `401`: with a deliberately bogus token, the WASM guest reaches
`api.github.com` and GitHub refuses it (`github_api_error_status_401`). Registration,
WASM execution, host egress, and credential injection are therefore all working —
only the key was wrong. 34 GitHub tools reach the model.

### Three things that were true, and none of them what I expected

- **A bad credential does not fail a run. It *parks* it.** The runtime's answer to a
  `401` is to raise an **auth gate** (`blocked_auth`) and wait, indefinitely, for a
  better credential. A UI that waits for `completed` therefore spins forever and
  looks like a hang — which is exactly what an earlier probe of mine concluded, and
  it nearly became an upstream bug report. Compounding it: `serve` reads
  **`IRONCLAW_REBORN_LOG`**, not `RUST_LOG`, so I had also been reading an empty log
  and calling the runtime silent (C5). The gate payload names the run and the gate
  but **not the connector** (C6), so the widget infers it from the last
  `capability_activity` seen before the gate.
- **The gate cannot be answered the documented way.** `/manual-token/submit` exists
  precisely to answer a live gate — it demands `run_id` + `gate_ref` — and a
  well-formed call against a real one returns a bare `400 invalid_request`, naming no
  field, logging nothing. `/secret-submit` yields a `credential_ref` only on the
  *first* call for a provider, so there is nothing to `resolve_gate` with either.
  Rather than ship a button over a route that will not answer, the fix-it path is
  built from primitives that are **proven**: store the new credential, cancel the
  parked run, re-send the question. The user types their token once and gets their
  answer; the turn re-runs rather than resumes. Pinned by
  `a_parked_run_can_be_answered_and_resumed`. If upstream fixes submit, this collapses
  back into a true resume.
- **Installing GitHub made the local model stop answering — and it was our bug.**
  With the connector active, *every* turn 400'd and no run ever terminated. Not
  tool-flooding: llama.cpp **could not compile the grammar**. GitHub's `owner`/`repo`
  properties carry `pattern: "[^\\s/?#]+"`; llama.cpp transcribes a JSON-Schema
  `pattern` into GBNF verbatim; GBNF has no `\s`. And because *all* of a turn's tools
  compile into **one** grammar, a single unsupported pattern in a single property of
  a single tool takes down every tool call the model could have made. Fixed in the
  CP-3 lane — `ic_llama`'s SchemaProxy now drops a `pattern` it cannot prove GBNF can
  express, exactly as it already drops an oversized repetition bound (fail closed:
  any `\` or `(?` and it goes). Same question, before: 240 s, no answer. After: 9.2 s,
  correct.

### The parser bug the whole suite agreed with

`GET /extensions` returns a **flat** `RebornExtensionInfo` (`package_ref`, `active`,
`tools`). The widget's type had been written from the *internal*
`LifecycleInstalledExtensionSummary`, which nests all of that under `summary` — so
`serde` rejected every response, and the Connectors panel would have listed nothing
while blaming the gateway. The `summary` shape is real, but it belongs to a
**different route** (`/extensions/{id}/setup`), which is how the confusion started.

What let it through is the part worth keeping: the probe that "verified" the shape
**printed** its parse instead of asserting it, and hand-walked the JSON — so a wrong
belief was held in two places that agreed with each other and never met the wire. The
gate now decodes a live response **through the type the widget ships**
(`the_panels_parser_decodes_the_live_extensions_route`). *The Phase 4 lesson, a third
time: a green suite can agree with you and still be wrong about the runtime. Drive the
shipping code against the real thing, or all you have tested is your own beliefs.*

### The tool-flood warning now says what was measured

The constant carried a comment claiming a measurement that had never been made, and it
claimed the wrong effect. `tool_flood.rs` (a real Qwen3-4B, GPU-offloaded) makes it
honest: at 62 tools (28 built-in + GitHub's 34) the model still answered **4/4
correctly** — it does not get confused. What it gets is **slow**: ~3.6 s → ~10.7 s per
question, because every schema is re-sent on every turn, including the turns that need
no tool at all. The panel says that now, and nothing more.

### What the panel does

Registry list → **Install** → credential → **Save & activate** → Enable/Disable, with
the vendor's own onboarding copy rendered rather than ours. An **OAuth** connector
shows what it needs and *stops* (below). A parked auth gate surfaces in **both**
surfaces: a red card in the bubble carrying a token field and a Continue that actually
recovers, plus the character in `concerned`. Never a spinner.

### Where Gmail stops, exactly (item 5)

It installs, its setup projection is complete (6 capabilities, `setup.kind: "oauth"`,
3 scopes, a fresh `invocation_id`) — and `POST /extensions/gmail/setup/oauth/start`
answers **503 `backend_unavailable`**, because `serve` builds its Google OAuth config
from `IRONCLAW_REBORN_GOOGLE_CLIENT_ID` + `..._REDIRECT_URI` and there is none.

That is not a bug to route around. A Google OAuth **client** can only be created by a
human in the Cloud console; a client id baked into a public MSI would name *us* on a
consent screen for someone else's mailbox, and restricted Gmail scopes need a review
process, not a config value. And the registered **redirect URI** must match exactly,
while the widget takes a *fresh port* for `serve` at every launch — so this needs a
stable loopback callback before those variables are worth setting. Both are decisions
with consequences; neither belongs in a commit that claims Gmail "works". Pinned, with
the reasoning, by `connector_oauth.rs`. **8c work.**

### The one link not proven end to end (item 4)

The definition of done says GitHub answers "what are my newest repos" with a **real**
token. Every link in that chain is verified except the last, and the last one is
GitHub's: a valid PAT returning repositories instead of a `401`. That needs the user's
own credential — theirs to paste into the panel, not mine to obtain, and not something
worth faking in a test. The path it would travel is the one `connector_verify.rs`
drives today.

### Upstream tracking

| Ref | What | State |
|---|---|---|
| [PR #6098](https://github.com/nearai/ironclaw/pull/6098) | CP-1: Windows directory fsync (upstream's own catalog test fails on Windows without it) | open |
| [#6099](https://github.com/nearai/ironclaw/issues/6099) | `/llm/test-connection` reports `ok` for a dead endpoint with a junk key | open |
| [#5998](https://github.com/nearai/ironclaw/issues/5998) | No transport for a local MCP server — **CP-4/CP-5 get deleted when this lands** | open |
| [#5999](https://github.com/nearai/ironclaw/issues/5999) | `local-dev-yolo` cannot start on Windows | open |
| [#6076](https://github.com/nearai/ironclaw/issues/6076) | Cancel does not abort the model's in-flight generation | open |
| [#6000](https://github.com/nearai/ironclaw/issues/6000) | How to report a security finding (no `SECURITY.md`, private reporting disabled) | **unanswered** — so the Phase 4 `default_permission`-is-never-read finding stays undisclosed |

Two findings from this phase are worth filing once #6000 is answered: the
`/manual-token/submit` `400`, and the auth-gate payload that names no connector.

Next: **8b.1 — finish the Gmail OAuth lane** (the stable loopback callback the 8b
notes left as future work), then **8c — the runtime's own surfaces** (memory, skills,
audit, run history), whose first job is its own ⚠️ VERIFY: all four have no HTTP route
today (`docs/desktop/dashboard-gaps.md`), so establish what `serve` will and will not
answer *before* designing a panel for any of them.

## Phase 8a notes — dashboard shell + chat management (recorded 2026-07-14)

`crates/ic_widget` (`main.rs`, `gateway_client/mod.rs`, `error.rs`, `settings.rs`) +
`ui/` (`dashboard.tsx`, `chat.ts`, `widget.tsx`, `api.ts`, `styles.css`) + a new gate
(`ic_integration_tests/tests/chat_control.rs`). **No core patch.** Every contract
below was verified against the pinned upstream commit **`a492857`**
(`reborn-integration`) by driving a real `serve` — on the next upstream merge those
assertions are the alarm.

### The three ⚠️ VERIFY items, answered before any feature code

**1. Nothing deletes a thread.** All five plausible spellings 404
(`DELETE /threads/{id}`, `POST .../delete`, `.../archive`, `.../hide`;
`DELETE .../messages` is a 405), and the thread survives every attempt. So the
button says **Hide**, and means it: the conversation stays in the gateway exactly
as the agent left it, and the widget keeps a local `settings.hidden_threads` list
of what not to show. No libSQL was touched — that would couple us to internals and
break the never-delete-LLM-data invariant.

**2. Stop does not stop the model.** This is the finding that reshapes the feature.
`cancel_run` answers `status: "CancelRequested"` — a *request*, not a completion —
and a slow-provider mock proves the gateway **leaves its in-flight HTTP request to
the LLM open**: three seconds after the cancel, the provider request was neither
aborted nor answered. The run does go terminal on the projection stream, so the UI
recovers correctly; but with a local model, `llama-server` keeps generating to
completion and the GPU burns tokens nobody will read. **Stop means "stop showing me
this", not "stop computing this."** Pinned by
`cancelling_an_in_flight_run_reports_terminal_and_what_it_does_to_the_provider`,
whose verdict line prints which way it went — if upstream ever wires cancellation
through to the provider request, that test says so.

**3. 🚨 `POST /llm/test-connection` cannot test a connection — 8a.5 is NOT built.**
The spec asked for a Test-connection button "instead of restarting the gateway to
find out a key works". The route exists and is inert: `LlmProvider::list_models()`
has a **default impl returning an empty list** (`ironclaw_llm/src/provider.rs:494`),
and **`RigAdapter` — which serves OpenAI, Anthropic, Ollama and `openai_compatible`,
i.e. every provider our picker can configure — never overrides it**.
`test_connection` (`llm_config_service.rs:536`) asks the adapter for a model list,
gets an empty one, and reports **`ok: true`** without opening a socket. Proven by
probing a **port with nothing listening on it, using a junk key**: it reports
"connection ok". A button over this would show a green tick for a bogus key against
a dead endpoint — worse than no button — and a model dropdown fed by
`/llm/list-models` would always be empty. **Reported rather than adapted, per the
rules of engagement; awaiting a decision.** Pinned by
`the_provider_probe_reports_ok_for_an_endpoint_that_does_not_exist`, which starts
failing the day upstream implements `list_models` for `RigAdapter` — that is the
signal the button can finally exist.

### What shipped

- **The sidebar landed as its own logic-free commit** (`19d9d66`), as required.
  Each existing panel is *wrapped* in a nav `Show` rather than extracted into a
  routed component, so there is no seam through which a panel's behaviour could
  have shifted — the diff is a section registry, a signal, and moved JSX.
  **Connectors is deliberately absent** from the sidebar: it arrives with 8b, and a
  nav entry that opens an empty panel is worse than no entry.
- **Chats panel**: `GET /threads` with cursor paging (the widget's client gained
  `list_threads_page`), read any conversation's history through the timeline,
  **Continue** to point both windows at it, **Hide/Unhide**. Switching conversations
  while a run is in flight is **blocked** with "finish or stop that answer first" —
  otherwise the reply lands in a thread nobody is watching. A message kind this
  build has never met (a summary, a tool result with `content: null`) renders as a
  neutral event row; an unknown kind must never be able to crash the panel.
- **Stop is now in the bubble**, which is where the user actually is — it only ever
  existed in the dashboard, which is closed most of the time. It says "Stopping…"
  from the moment it is clicked rather than waiting for the gateway to admit the
  cancel (which takes a poll or two).
- **The two Stop races no longer show a dialog.** An already-finished run
  (`already_terminal`, the common case — the reply lands as the click flies) and an
  unknown run id (a clean `404`) both now mean "collect the reply and move on".
  Before this, a stale run id produced a red "Could not stop" the user could do
  nothing about.
- **One chat per window.** The Dashboard now owns the `createChat()` and hands it to
  both the chat pane and the Chats panel: the panel has to know whether a run is in
  flight, and a second `createChat()` would open a second event pump against the
  same thread — the gateway caps a caller at three concurrent streams.

### Two contract quirks worth remembering

- The cancel response's `status` is **PascalCase** (`CancelRequested`, `Completed`),
  unlike the projection stream's snake_case `run_status`. Two spellings of the same
  vocabulary on one API.
- **`next_cursor` is omitted, not null**, when there is no next page. A client that
  reads it as a required field breaks on the common case; both spellings normalize
  to "no more" in `gateway_client`.

### The regression checklist (8a.1)

The layout commit changes no panel logic *by construction* (the diff is wrapping),
the frontend type-checks and builds, and the app boots with the dashboard webview
mounting every panel **with no console error**. What could not be automated: a
human clicking each interactive flow (model download + cancel, model switch,
provider apply-and-restart, ambient toggles, skill review card). Those call Tauri
commands this phase did not touch, but a visual pass is still worth one minute of
someone's time.

Next: **8b — connectors**, whose first job is its own ⚠️ VERIFY: drive a registry
connector to a *real tool call* before building any UI.

### 8a.5 resolution — the provider directory, and a probe that tells the truth (2026-07-15)

The blocked item is closed **widget-side, additively, with no core patch** — and the
⚠️ VERIFY turned up a second inert route, which settles the design rather than merely
constraining it.

**⚠️ VERIFY answered: `POST /llm/providers` accepts *no* protocol under our profile.**
All eleven come back **`503 service_unavailable`**; only a bogus adapter name gets a
`400`, which proves the route parses and the *service* is simply not composed under
`local-dev`. So the gateway's provider-config lane is unusable, on top of its probe
lane being inert (the 8a finding above). Everything — directory, keys, probe — is
ours. Pinned by `provider_protocols.rs`, which turns green→red the day upstream
composes the service.

- **Provider directory** from the runtime's own `providers.json` (26 entries): each
  row shows the vendor's own name, a **"get a key" link** (20 of 26 carry one), the
  endpoint it will be reached at, and whether it can be tested at all. The panel
  features **OpenRouter** explicitly — one key there reaches most models online, and
  you can change model without changing provider. `openai_compatible` remains the
  escape hatch; **`cloudflare` joins it** as bring-your-own-endpoint (its URL embeds
  an account id), which a test caught rather than a user.
- **The probe is ours** (`ic_widget::probe`), by **protocol family, not brand**:
  OpenAI-shaped (16 of 26, plus OpenRouter) → authenticated `GET /models`;
  Anthropic → `x-api-key` + version header; Gemini → key as a query parameter.
  **The trap, and why the fallback exists:** plenty of OpenAI-compatible servers
  serve `/models` to *anyone*, so a `200` there proves the endpoint exists, not that
  the key is good — exactly the lie the gateway's own probe tells. So the probe asks
  again with a deliberately invalid key; if that also passes, it falls through to a
  **one-token completion**, the only probe that truly validates. Pinned by
  `an_endpoint_that_does_not_check_the_key_falls_back_to_a_completion`.
- **Failures are told apart**: unreachable (typo'd URL, no network) vs key rejected
  vs rate-limited vs "authenticates out of band, so a pasted key cannot be tested"
  (Bedrock, Codex, NEAR AI, Copilot's token exchange). A green tick that means
  nothing is worse than no button.
- **Model dropdown** is fed by the same probe, with **free text when the provider
  lists nothing** — which is most of them, and is not an error. Model and endpoint
  are now part of the selection's identity, so changing the model on the *active*
  provider is a change you can Apply (it used to silently disable the button), and
  it persists through the existing apply-and-restart flow. `ProviderSelection::Cloud`
  gained `base_url`; an older settings file without it still loads.
- **The stored key never round-trips through the webview.** "Test" on a configured
  provider reads the key from the Credential Manager in Rust; a key is only sent
  from the UI when the user is checking one they have not saved yet.
- **Honest-UX note in the panel** (the spec's point 5): *any online model works* is
  true of chatting and emphatically not of **agent** use — this app hands the model
  two dozen tools and needs well-formed tool calls back, and models differ far more
  at that than at chat. The panel names known-good agentic models and warns that
  small models (≤8B) will chat happily and fail at tasks.

**Two robustness items, both from this session's own scars:**

- **A UTF-8 BOM no longer resets your settings.** `serde_json` rejects a BOM at
  column 1, so a BOM'd `settings.json` read as *corrupt* and every setting silently
  reverted to defaults — which is exactly what happened here when PowerShell 5.1's
  `Set-Content -Encoding utf8` wrote one. The loader now strips it. Pinned by
  `a_settings_file_with_a_byte_order_mark_still_loads`.
- **A TODO for an abort endpoint** in `ic_llama::proxy`, referencing the cancel
  finding: the proxy holds the upstream request, so a small loopback control route
  that drops it would make llama.cpp abort generation — Stop would finally stop the
  GPU. Not built because the widget cannot yet tell *which* proxy request belongs to
  the run being cancelled (the gateway sends no correlation id); that mapping is the
  actual work.

Next: **8b — connectors**, whose first job is its own ⚠️ VERIFY: drive a registry
connector to a *real tool call* before building any UI.

## Closing the open items (recorded 2026-07-14)

Everything that was outstanding except the **installer itself**. Two of the four
turned out to be *mostly built already* — the notes above were stale, which is
its own lesson: check the code before believing a TODO.

`crates/ic_llama` (`proxy.rs`, `local_llm.rs`, `lib.rs`) + `crates/ic_widget`
(`providers.rs`, `settings.rs`, `main.rs`) + `ui/` + `docs/desktop/llm-provider-selection.md`.
**No core patch.**

### 1. Cloud failover — decided (option 2) and built

The v1 promise ("answer with a local GGUF model — with cloud failover when a key
is configured") is now kept, without touching a core crate. **The proxy owns the
retry**, exactly as `llm-provider-selection.md` recommended: `CloudFallback` names
an OpenAI-shaped endpoint + key + model, and `relay()` retries a **chat completion**
there when the sidecar refuses the connection or answers `5xx`. The full write-up
is in that doc; the load-bearing findings:

- **The gateway never learns it happened.** One `openai_compatible` endpoint, no
  second `LLM_BACKEND`, no restart, no core patch.
- **The cloud key never enters the gateway's environment** — it is read from the
  credential store into the widget's own process. The predicted security win, real.
- **The sidecar's throwaway bearer is stripped** before the cloud call: forwarding
  it would leak a local secret to a third party and authenticate nothing.
- **Only a chat completion fails over.** A failing `/v1/models` probe means the
  local server is down; answering it from the cloud would advertise models the
  sidecar does not have.
- **Not every provider can serve.** The proxy forwards the body the gateway already
  built, so a fallback must speak OpenAI Chat Completions. `providers.json` says
  `anthropic` speaks `anthropic` and `deepseek` speaks `deep_seek` — so the picker
  offers only `open_ai_completions` providers **plus Anthropic via its documented
  OpenAI-compatible layer**. `Provider::can_fail_over()` is the one place that
  decides, and `set_cloud_fallback` refuses a keyless or incompatible choice at the
  command boundary rather than storing a setting that silently never fires.
- **With no fallback, a dead local model is still an honest `502`.**

Pinned by three end-to-end proxy tests (dead sidecar → cloud answers; no fallback
→ 502; healthy sidecar → the cloud never sees the request *or* the key).

### 2. tokens/sec — built, in the only place that can see it

The gateway's event stream carries **no token usage at all**, so nothing downstream
of it can count. The proxy can: it reads `llama-server`'s `timings.predicted_per_second`
off each non-streamed completion (falling back to tokens-over-wall-clock, which reads
a little low because it includes the prompt pass). Surfaced in the model panel as
Speed / Last turn, polled every 3 s **only while a model is loaded**. The panel also
flags when the last answer came from the cloud — a silent failover would otherwise
look like a suspiciously fast GPU.

### 3. GGUF download UI — was already built; the missing half added

The panel (catalog, progress, cancel, resume, digest verify, custom repo/file,
remove) shipped in `de702a9`; the notes above never got updated. What was actually
missing: **nothing let the user choose which installed model runs.** `launch_local_model`
took "the first one that isn't suspect". Added `settings.default_model` + a **Use this**
button per installed model, which pins it and restarts the sidecar onto it (a model is
chosen at launch, so a new choice needs a new sidecar — and the gateway behind it,
which holds the proxy URL). A pin that no longer resolves (removed, or gone suspect)
falls back to the old rule rather than refusing to start.

### 4. Wake word — was already built; the last hop closed

Record → train → `.rpw` → `RustpotterWake` all existed, and `voice.rs` already swaps
`NullWakeWord` for the real spotter automatically when a model is present. The gap:
**the pipeline reads its models once, at start**, so a word trained under a running
pipeline did nothing until the next launch — the feature looked broken at exactly the
moment the user tried it. `train_wake_word` now restarts voice itself (new
`restart_voice` helper) and clears the banked takes, so a retrain is not trained on
both voices. Still outstanding and **genuinely external**: the user must speak the
recordings, and the `#[ignore]`d detection test needs a real `.rpw` + WAV.

### What remains

**Only the installer**, and what it needs from outside the repo: a code-signing
certificate, an updater keypair + endpoint, and a clean-VM MSI build with the manual
failure drills (`docs/desktop/packaging.md` has the checklist and the config
templates). No code is blocked on any of it.

Next: **Phase 8 — surfacing the runtime.** An audit against the serve route table
found whole tiers of runtime capability the product never surfaces; Phase 8 wires
the highest-value ones in, starting with 8a (dashboard shell + chat management).

## Phase 7d notes — ambient watchers (done, recorded 2026-07-14)

`crates/ic_widget` (new `ambient/watch.rs`; `ambient/mod.rs`, `automations.rs`,
`settings.rs`, `main.rs`) + `ui/` + a new gate
(`ic_integration_tests/tests/watcher_fire.rs`) + one new dependency (`notify`,
for folder events). **No core patch.**

- **The engine is a pure state machine** (`WatchEngine`), fed signals by the
  app, so every firing rule is a unit test rather than a wait. Three kinds:
  foreground window title (extends the Phase 3 Win32 machinery with a
  `GetWindowTextW` sampler — a *title*, never screen content), watched folders
  (`notify`, recursive, recreated when the rule set's paths change), and the
  local clock. Each kind is its own opt-in, all default OFF, honoured **inside
  the engine** as well as by the caller — an engine that trusts its caller is
  one settings refactor away from watching something the user switched off.
- **Edges, priming, cooldown — the anti-nag rules.** A foreground rule fires on
  the match *transition* (the first sample only primes); a time rule fires once
  a day and a mark that already passed at startup is primed as spent (the 9 am
  rule must not greet a 3 pm launch); every rule has a 30-min cooldown so one
  noisy folder mid-build cannot spend the hourly guardrail cap alone.
- **v1 is rule-based, not LLM-based, per spec** — the only inference a watcher
  runs is the prompt the user wrote. And it runs *after* the guardrail:
  `AmbientService::would_allow` (a non-recording pre-check) means a firing
  under quiet hours or a spent cap costs **no thread and no turn** — pinned by
  `a_suppressed_firing_spends_no_llm_turn`, which asserts the mock saw no
  request. `propose` still re-checks and records when the answer arrives.
- **A firing lands in a fresh thread**, like the gateway's own trigger fires —
  the ambient thread stays the app's private conversation, and Accept opens a
  transcript holding exactly this rule's question and answer. Surfaced as
  `SuggestionKind::Watcher`, a calm blue offer (the widget's card predicate now
  names the two skill kinds as the only red consent prompts).
- **Rules are config, not LLM data** — editable and deletable in the dashboard
  panel, unlike the logs. Rule edits and kind toggles are read every 3 s
  sample, no restart; only the ambient master switch restarts (the gateway's
  boot-time constraint, not this loop's). Rules are validated on save (a
  non-folder path, an empty needle, an impossible time are refused with
  reasons; an empty needle would otherwise fire on every window change).

### Phase 7 definition of done — accounting

- ✅ a scheduled automation surfaces as a character suggestion (7a, gated)
- ✅ a completed task yields a "want me to remember this?" prompt whose approval
  installs a skill active in the next session (7b, gated end to end)
- ✅ a third-party SKILL.md folder imports after review (7c, gated)
- ✅ every surfacing respects the rate cap and quiet hours (one guardrail path;
  7d additionally pre-checks before spending turns)
- ✅ a "Not now" suppresses re-surfacing (7a log; exact-key = once ever)
- ✅ all gates green + manual smoke on Windows, per sub-phase
- Outstanding from Phase 6, unchanged: wake-word models (voice as a third
  sense; push-to-talk remains), certificate/updater/MSI externals.

Next: **Phase 8 — surfacing the runtime** (dashboard shell + chat management,
connectors, voice picker, universal gates, skill repos, channels, memory
seeding). Phase 7 is complete; the 2b follow-ups and the failover call are
closed too (see "Closing the open items" below). What remains outside Phase 8
is Phase 6's external-input gates: cert, updater keypair, MSI on a clean VM,
and the wake-word recordings.

## Phase 7c notes — skill import (done, recorded 2026-07-14)

`crates/ic_widget` (new `skill_import.rs`; `ambient/mod.rs`, `ambient/reflection.rs`,
`main.rs`, `lib.rs`) + `ui/` + a new gate
(`ic_integration_tests/tests/skill_import_gate.rs`). **No core patch.**

- **The ⚠️ VERIFY item was answered in 7b and it is the whole design**: the runtime
  scans skill text for *nothing* on install (`gating.rs` = environment requirements
  only; the context-build scan is structural, not semantic), and an installed skill
  is the trusted tier with full-body injection. So the review is the only gate a
  third-party skill ever passes through — which is why the dashboard shows the
  **entire SKILL.md**, not a summary, under a warning that says exactly that.
- **Two steps, both explicit.** The dashboard is the review (path in → `preview`,
  a pure read: name, description, full text, bundle file list with sizes); the
  final consent is the **red card on the character's bubble** — the same
  `SkillDraft` machinery, a new `SuggestionKind::SkillImport`. An import is
  *solicited*, so it lives outside the ambient service on purpose: it works with
  ambient off, never consults the guardrail, and never spends a rate-cap slot.
  Its consent trail is its own append-only `skill-import-log.jsonl` — writing it
  into the ambient log would desync the running service's in-memory view and
  let solicited imports eat unsolicited-surfacing slots on the next launch.
- **What installs is what was reviewed, verbatim.** `install` takes the reviewed
  SKILL.md *text* back in and writes it — never a re-read of the folder, which
  could have changed between the review and the yes (pinned by
  `install_writes_the_reviewed_text_not_the_folder`). Bundle data files are
  copied fresh (their names and sizes were the reviewed part) with caps
  re-checked, and a failed install removes the half-written directory.
- **The runtime's own bundle limits are mirrored and enforced** —
  `MAX_INSTALL_BUNDLE_*` (256 files / 2 MiB per file / 20 MiB total), so an
  import can never admit more than `builtin__skill_install` would have. Path
  rules per component (no control chars, no `:`, no Windows reserved device
  names), and **symlinks are refused outright**: a link inside the folder can
  point anywhere on the machine, and "import this folder" must never quietly
  become "import whatever this folder points at".
- **A parser fix this surfaced (also benefits 7b):** `parse_draft` shredded a
  bare SKILL.md whose *body* contained fenced code blocks — real skills carry
  code examples — into inner candidates and never tried the whole. The whole
  reply is now always the last candidate, and imports use the stricter
  `parse_skill_md` (the file *is* the candidate; a prose file with a fenced
  skill inside is not itself a skill).
- **No new plugin for folder picking**: the panel takes a typed/pasted path. A
  native dialog means adding `tauri-plugin-dialog`; worth it later, not
  load-bearing now.

The gate (`skill_import_gate`) drives the shipping `preview` + `install` pair
against a **running** `serve` and asserts the agent's very next `skill_list`
names the import — no restart, because the skills root is read lazily. Together
with the 7b chain (root → listed → activatable → body injected) that closes the
import path end to end. Unit tests cover the refusals: no SKILL.md, invalid
frontmatter, oversized file/bundle, reserved names, symlinks, overwrite, and
the half-install cleanup.

Next: **Phase 7d — ambient watchers** (opt-in signals, each defaulting OFF;
v1 gate is rule-based, not LLM-based; wake-word models remain the outstanding
Phase 6 item).

## Phase 7b notes — self-learning skills (done, recorded 2026-07-14)

`crates/ic_widget` (new `ambient/reflection.rs`; `ambient/mod.rs`, `automations.rs`,
`settings.rs`, `main.rs`) + `ui/` + two new gates
(`ic_integration_tests/tests/skill_install.rs`, `tests/skill_reflection.rs`) and one
harness affordance (`RebornServer::start_scripted_in_home` — consecutive servers over
one caller-owned home, the only way a test can watch a restart). **No core patch.**
Related upstream filing from this phase: [nearai/ironclaw#6076](https://github.com/nearai/ironclaw/issues/6076)
(automations carry no thread/run correlation — 7a's weak spot, filed with approval).

### The ⚠️ VERIFY item, answered by a probe before any feature code

`tests/skill_install.rs` drives `builtin__skill_install` through a real `serve`
(the Phase 4 lesson applied: a listed capability is not a reachable one). Findings,
each pinned by an assertion:

- **Install is reachable — the opposite of the Phase 4 egress gap.**
  `local_dev_capability_policy.toml` grants `skill_install` exactly the effects it
  declares. The agent installed a skill from inline `content` end to end.
- **`PermissionMode::Ask` ran unprompted** — third confirmation of the Phase 4
  finding. The consent gate is ours, as the plan assumed.
- **Skills are plain files**, not libSQL rows:
  `<IRONCLAW_REBORN_HOME>/local-dev/skills/<name>/SKILL.md`, re-read lazily. They
  survive a restart iff the home does (ours is stable).
- **Install ≠ in-context.** The local-dev selector is `ExplicitOnly`; the skill
  reaches the model only when the agent calls `builtin__skill_activate`. And the
  decisive fact: an inline-content install lands as **`source: user` — the trusted
  tier — so activation injects the FULL body**, not the description-only fate of
  URL-provenance installs. Without that, a learned skill would be a name, not a
  procedure. Pinned; if upstream changes the trust assignment, the gate says so.

### What 7b is, mechanically

- **`RunWatch` (edge, not level).** The projection stream repeats `run_status`
  every poll and replays it on every snapshot, so "completed" must be detected as
  an in-flight → `Completed` **transition**. A run already terminal at first sight
  is history, not news (the automations watcher's priming rule, same shape).
  Failed/cancelled runs never fire. Hooked in `pump_events`; both toggles
  (`ambient_enabled` + the new `settings.reflection_enabled`, default **off**) are
  read at fire time, so no restart.
- **The reflection turn rides 7a's plumbing exactly**: `AmbientService::ask` on the
  ambient thread — and the transcript travels *in the prompt*, because the ambient
  thread knows nothing of the chat thread (separate conversations, by design).
  Tail-truncated, whole messages only.
- **Parsing fails closed.** A draft is a fenced (or bare) SKILL.md with top-level
  `name:` + `description:` frontmatter and a non-empty body; the name is validated
  kebab-case, bounded, and not a Windows reserved device name. "NO", prose, or
  anything malformed proposes nothing.
- **Dedupe is three layers, cheapest first**: the cap needs no LLM turn (accepted
  `reflection:*` sources ∩ disk — removing a skill frees its slot, default 50);
  a name already on disk declines; and the guardrail's exact-key memory makes
  `skill:<name>` (no per-run component, deliberately) a **once-ever** offer — that
  existing 7a mechanism *is* "a rejected draft is never re-proposed".
- **Consent is the red card** (the Phase 4 `ask-fill` pattern, not the blue offer):
  full draft text in the bubble, No is the default and the focused button. An
  Accept installs **deterministically — a validated file write** to the skills
  root, no LLM between the yes and the write. This is the "cleaner seam" the spec
  allowed: a user-placed skill is *identical* in trust and effect to an
  inline-content capability install (verified above), and it cannot mangle the
  text the user just approved. `main.rs` `respond_suggestion` routes
  `SuggestionKind::SkillDraft` accepts through it and reports via
  `ambient://install-result`.
- **The model-quality constraint is surfaced, not solved** (per spec): the ambient
  panel shows "skills learn better with a stronger model" while a local model is
  active and reflection is on. Multi-model routing stays tracked in
  `docs/desktop/llm-provider-selection.md`.

### Two honest limitations, stated rather than papered over

- **The reflection turn runs with the agent's full tool surface and no runtime
  gate** (Phase 4). A model that disobeys "do NOT install anything" could call
  `builtin__skill_install` mid-reflection and nothing would stop it. Not
  preventable from the widget; it **is detected** — `reflect` snapshots the skills
  root around the turn and logs an error naming any skill that appeared without
  consent. If upstream ever wires `Decision::RequireApproval`, this becomes
  defence-in-depth.
- **Dedupe cannot see bundled system skills** (embedded in the binary, not on
  disk), so a draft could shadow one by name; `skill_listing` keys on
  `(name, source)`, so both would list. Low stakes, worth knowing.

### A finding that belongs to 7c

Nothing scans skill text on install: `ironclaw_skills/src/gating.rs` checks only
*environment requirements* (bins/env/config), and the context-build scan
(`skill_context.rs`) is structural (control chars, host paths, handle markers,
byte budgets — fail-closed) not semantic. Combined with content installs landing
**trusted with full-body injection**, 7c's review-before-install UI is not a
nicety — it is the only gate a third-party skill passes through.

### The gates

`cargo test -p ic_integration_tests --features webui-v2-beta` now includes
`skill_install` (install → restart → list → activate → body-injected) and
`skill_reflection` (task completes → reflection drafts → consent card → install
into the real skills root → never re-proposed → cap declines before the LLM
turn). Together they are the 7b definition-of-done chain. Unit tests cover the
parser's hostile cases, `RunWatch`, the cap arithmetic, and the installer's
refusals.

Next: **Phase 7c — skill import** (local folder, frontmatter validation,
review-before-install through the same consent card). Its ⚠️ VERIFY item is
answered above: treat skill text as untrusted, because the runtime does not.

## Phase 7a notes — proactive surfacing plumbing (done, recorded 2026-07-14)

`crates/ic_widget` (new `ambient/` module: `mod.rs`, `guardrail.rs`, `log.rs`,
`automations.rs`; plus `character.rs`, `settings.rs`, `supervisor.rs`, `main.rs`) +
`ui/` + a new gate, `ic_integration_tests/tests/ambient_surfacing.rs`. **No core
patch** — nothing in 7a needed one. Verified by a manual smoke run on 2026-07-14
(ambient off: silent, no poller; ambient on: ambient thread opened and persisted,
watcher primed) and by the new gate, which drives a **real** trigger fire through a
real `serve` and runs the shipping watcher over the result.

### The ⚠️ VERIFY item, answered — and the plan was wrong in one load-bearing way

The spec said "scheduled proactivity through the existing triggers/automations lane
(`GET /automations` is already surfaced)". It is surfaced. It also **never fires**.

- **The trigger poller is off by default.** `TriggerPollerSettings::default()` has
  `enabled: false` (`runtime_input.rs`), so `ironclaw-reborn serve` composes the
  poller and never starts it. A schedule the agent creates is listed by
  `GET /automations`, shows a `next_run_at`, and sits there forever. The switch is
  `IRONCLAW_TRIGGER_POLLER_ENABLED=1|true` (env beats config; `_INTERVAL_SECS` tunes
  the poll, min 1 s — the *fire* cadence floor is still cron's 60 s). Read once at
  boot, like `LLM_BASE_URL` — **so the ambient toggle restarts the gateway**, and
  `apply_provider`'s teardown/bring-up became the shared `restart_gateway`.
- **We tie the poller to `settings.ambient_enabled` deliberately, and that is a
  security decision, not a convenience one.** `builtin__trigger_create` is declared
  `PermissionMode::Ask` and **runs with no prompt** — the Phase 4 finding
  (`default_permission` is read by nothing), now confirmed live in this lane: the mock
  agent armed a recurring schedule and the run just completed. An agent talked into
  arming a minute-by-minute trigger would otherwise own a heartbeat the user never
  granted. Ambient off ⇒ the poller is absent ⇒ **no unprompted run can exist at
  all.** Pinned by `a_schedule_never_fires_while_the_trigger_poller_is_off`.
- **A fire lands in a brand-new thread, one per fire** — not the ambient thread, not
  the chat thread. `TriggerFireIdentity` hashes `(domain, tenant, trigger, fire_slot)`
  into a `route_thread_id`, which is a *binding key*, not a `ThreadId`; the binding
  misses and mints a fresh UUID thread (`conversations/memory.rs`). It is owned by the
  same scope, so `GET /threads` lists it, its timeline reads normally, and its
  projection SSE works. Ordinary in every way except that nobody told you it exists.
- **Nothing correlates an automation to the thread its run landed in.**
  `RebornAutomationInfo` carries no `thread_id` and no `run_id` (`TriggerRecord` *has*
  an `active_run_ref`; `trigger_output` does not emit it). So the watcher pairs them
  **by timing**: between two polls, one completed automation + one new thread = the
  same event; anything else surfaces the automation's name and status with **no body**,
  because a wrong body is worse than none. This is the one honest weak spot in 7a, and
  it is a missing field upstream, not a design choice here — filed as
  [nearai/ironclaw#6076](https://github.com/nearai/ironclaw/issues/6076); the timing
  pairing gets deleted when it lands.

### What "the character speaks first" is made of

- **`ambient::guardrail::check` is pure and fails closed.** Four ways to be suppressed
  (`Disabled` → `QuietHours` → `AlreadySurfaced` → `Dismissed` → `RateCap`), one way
  through. Defaults: **off**, ≤ **2/hour**, quiet **22:00–08:00 local**. Quiet hours
  are a *local* wall-clock question and the cap is an *elapsed-time* one, so both come
  off one `chrono` instant. `start == end` is an **empty** window, not a full day — a
  quiet period that silenced everything forever is indistinguishable from the feature
  being broken.
- **The log is the rate limiter's memory, and it is append-only JSONL**
  (`%LOCALAPPDATA%\IronClaw Desktop\ambient-log.jsonl`). "Not now" is the user's answer
  to the agent, so it is retained and timestamped, never rewritten — the inherited
  LLM-data invariant. A line this build cannot read is skipped **and left on disk**;
  rewriting the file to "clean" it would be the deletion the invariant forbids. If the
  log cannot be written, `propose` **declines to surface**: no memory ⇒ no cap ⇒ stay
  quiet.
- **Two keys, not one.** `key` = `automation:<id>:<last_run_at>` (this exact run, shown
  once ever); `source` = `automation:<id>` (what a "Not now" quiets, for an hour). Share
  them and a dismissal either silences the automation forever or not at all.
- **Priming.** The watcher's first tick records and surfaces **nothing**. A run that
  finished while the app was closed is not news, and greeting the user with last
  night's digest every morning is how a companion gets switched off for good.
- **`suggesting`** joins the character states, ranked **below** work/gates and **above**
  speech: what the user asked for always beats what the character volunteered, but an
  unanswered question it asked outranks it finishing a sentence. The bubble card is the
  calmest of the three (blue, not amber/red) — an offer must never read as an alarm.
  **Accept** means "show me": it repoints both windows at the run's thread. Both answers
  are recorded.
- **The ambient thread is real plumbing, and in 7a only `ensure_thread` has an in-app
  caller.** It is created on first enable, **persisted** (`settings.ambient.thread_id`),
  reused across launches, and replaced if the store was wiped. `AmbientService::ask`
  (send → await terminal → read timeline, reusing `voice::drive_turn`) is what 7b's
  reflection turn and 7c's skill review send down; in 7a it is exercised by the gate
  test against the real gateway, not by app code. That is the "shared plumbing" the
  spec asked for, stated plainly rather than given a fake consumer.

### The gate

`cargo test -p ic_integration_tests --features webui-v2-beta --test ambient_surfacing`
(3 tests, ~115 s — the cron floor is 60 s). It spawns `serve`, has the agent create a
**real** trigger through `builtin__trigger_create` (the mock LLM now emits tool calls —
`MockReply::ToolCall`, content-conditioned because chat, ambient, and the fired run are
all in flight against one mock), waits for the gateway's **own** poller to fire it, then
runs the shipping `AutomationWatch` over the result and asserts one suggestion carrying
the agent's actual answer and pointing at the right thread. `ic_integration_tests` now
dev-depends on `ic_widget` (a dev-dep cycle, which Cargo permits) so the gate drives the
code that ships instead of a re-implementation that agrees with us. **This is the Phase 4
lesson applied up front:** the earlier probe of this lane was written *before* any feature
code, and it is what caught the disabled poller — a green suite over our own beliefs would
have shipped a feature whose automations never run.

### Incidental fix

`cargo fmt --all -- --check` (the CI quality gate) has been **failing since Phase 5** on
the *vendored* `crates-src/rustpotter`, which `--all` reaches through the path dep.
Formatted it (whitespace only; rustfmt's `ignore` key is nightly-only, so a config fix
was not available on stable). The fork crates were already clean.

Next: **Phase 7b — self-learning skills** (reflection turn on the ambient thread, draft
SKILL.md, consent-gated install). Its first job is the ⚠️ VERIFY item: `builtin__skill_install`
and `builtin__skill_list` **are** model-visible in `serve` under `local-dev` (seen in the
28-tool list during the 7a probe) — so confirm the *install path* end to end against the
running gateway before building any UI, and remember that `Ask` is not enforced, so the
consent gate is ours to build (as in Phase 4).

### Phase 6 — done to the limit of what the repo can hold (recorded 2026-07-13)

`ic_widget` (`main.rs`, `secrets.rs`, `settings.rs`, `tauri.conf.json` +
`tauri.release.conf.json` + `wix/uninstall-cleanup.wxs`) + `ui/`. **Full write-up:
`docs/desktop/packaging.md`.** Everything buildable and verifiable *without a
certificate, an updater keypair, or an update endpoint* is done; those three (plus
the real MSI build, which needs WiX + the staged sidecar on a clean VM) are
documented with config templates rather than committed as half-configs or secrets.

- **Bundle config** (`tauri.conf.json`): MSI target, WebView2 `embedBootstrapper`
  (offline), publisher/metadata, and the WiX uninstall fragment. `externalBin`
  (the `ironclaw-reborn` sidecar) lives in a **release-only overlay**
  (`tauri.release.conf.json`, applied via `cargo tauri build --config …`) because
  tauri-build validates `externalBin` existence on *every* build — committing it in
  the base config broke `cargo run`. Character assets ride in the embedded frontend,
  so they need no resource bundling.
- **Offline strategy:** ship the lean MSI (llama-server / models / Piper / whisper
  download on first run — each runtime already prefers a present file, so a fully
  offline MSI is a staging exercise, no code change) and gate the first launch behind
  the wizard. `ironclaw-reborn` is the one binary that *must* ship (not downloadable).
- **Uninstall cleanup:** `ic-widget.exe --uninstall-cleanup` (`SecretStore::clear_all`
  + remove `%LOCALAPPDATA%\IronClaw Desktop\`), invoked by the WiX custom action.
  Two real caveats (Tauri's main-exe `FileKey`; per-user data under a per-machine
  MSI) are documented in the `.wxs` and packaging doc — they need the real generated
  WiX to reconcile.
- **First-run wizard:** `settings.setup_complete` + `needs_setup`/`complete_setup`
  commands; a `SetupWizard` overlay in the dashboard (orients toward the existing
  model/provider panels rather than duplicating them, offers a voice opt-in); the
  widget opens the dashboard on first launch so it's seen. Storage is *not* a step —
  the gateway inits libSQL on boot.
- **Auto-updater + code-signing:** templated in the packaging doc (keypair gen, config
  block, plugin registration; cert thumbprint + timestamp). **Not wired live** — a
  placeholder pubkey would fail the build, and the repo must never carry a private key.
- **Failure drills:** the checklist is in the packaging doc with each mode marked
  covered-by-design / needs-manual-drill / gap. No known uncovered failure; the
  manual pass runs on the first signed build on a clean VM.

Next: **Phase 7 — ambient companion** (7a ✅ — see its notes at the bottom; 7b → 7c →
7d remain). Phase 6's remaining work is gated on external
inputs and stays open alongside it: obtain a code-signing cert + updater keypair +
update endpoint, produce the first real MSI on a clean VM, reconcile the WiX caveats,
run the manual drills, and record + bundle rustpotter wakeword models (then wake word
replaces push-to-talk).

## Phase 5 notes — canvas (done, recorded 2026-07-11)

`crates/ic_canvas_mcp` (new) + `crates/ic_widget` (`canvas.rs`, `main.rs`) + `ui/`
(`canvas.tsx`, `canvas.html`). **No new core patch** — it reuses CP-4. Verified: the
contract gate (`ic_integration_tests/tests/canvas_mcp_contract.rs`) drives the real
canvas server through the runtime's own discovery code and confirms `canvas_render`
becomes the capability `ic-canvas.canvas_render`.

### The data path is the whole design

The agent runs in `ironclaw-reborn serve`; the render must reach a Tauri window. The
decisive fact: **every gateway→widget channel is content-hostile.** The SSE
`CapabilityDisplayPreview` is `sanitize_text`'d and capped at 16 KiB
(`ironclaw_reborn_composition/src/projection/display_preview.rs`), and a tool
result in the timeline has `content: None`. So HTML routed back through the gateway
would arrive corrupted and truncated. The only way to get raw markup to the window
is to **produce it inside the widget process.**

So `canvas_render` is served by an MCP server that runs **in-process in `ic_widget`**
(not a child, unlike the browser sidecar). When the agent calls it, Reborn POSTs the
arguments to the loopback URL (CP-4), the in-proc `tools/call` handler holds the raw
HTML, and hands it to a `CanvasSink` → `app.emit("canvas://render", …)` to the canvas
window. The markup never touches the gateway. A WASM tool or a native first-party
tool would both strand the HTML in the *runtime* process and each need a new
unsanitized projection channel — the exact core surface the fork avoids.

### Rendering safety: isolation, not sanitization

Agent markup is untrusted (a prompt-injected agent could emit hostile HTML). It
renders in an iframe with an **empty `sandbox`** (no scripts, no same-origin, no
forms, no navigation) under an injected `default-src 'none'` CSP (inline styles +
`data:` images only). Static HTML and inline SVG render; scripts and every network
fetch are inert. We deliberately do **not** strip-sanitize (ammonia/DOMPurify): the
sandbox+CSP already bound what the content can do, and stripping would break
legitimate inline SVG. The shell assigns `iframe.srcdoc`, never `innerHTML`, so the
markup is always parsed as a separate isolated document. The global app CSP gained
one token, `frame-src 'self'`, to permit the srcdoc frame (no other window uses
frames, so the widening is inert for them).

### Reused from Phase 4, exactly

The `ic-canvas` manifest, install/activate, and the three timing rules (manifest on
disk before boot; discovery at activation; re-activate every launch and verify the
capability count) are the browser's, one more extension. `GatewayClient::install_extension`
/`activate_extension`/`extension_capabilities` already existed. First-open race (an
event before the shell's listener attaches) is covered by storing the last render in
app state and having the shell fetch it via the `canvas_content` command on mount.

### Voice — decisions made (2026-07-11), build in progress

The plan was pressure-tested and the user made the three genuine calls:

- **Wake word: `rustpotter`** (MIT/Apache, pure Rust, **no ort/ONNX dependency**).
  Ship our own reference models (trained from recordings of the wake phrase) so the
  license is clean. openWakeWord is out (its pretrained models are CC-BY-NC).
- **TTS: the archived `rhasspy/piper` MIT `piper.exe`** — a self-contained binary,
  bundled and invoked as a Job-Object-supervised subprocess like `llama-server`. **Not**
  `piper1-gpl` (no binary; GPL). Repo is archived/unmaintained but functional. Pin a
  specific build and a **CC-BY-4.0 voice** (verify its MODEL_CARD; some voices are
  non-commercial). Piper emits no timing → compute an **RMS envelope from the output
  PCM** for lip sync.
- **VAD: `voice_activity_detector`** (Silero v5 ONNX, MIT). This *does* pull in `ort`
  even though rustpotter dropped it — so `ort` returns for VAD alone; bundle the ORT
  DLL beside the exe (`copy-dylibs`) to dodge the System32 clash.
- **STT: `whisper-rs`** 0.16 (MIT), `base.en` (q5_1), **CPU first** — Vulkan silently
  no-ops on Windows static builds (whisper.cpp #3750).
- **Sequencing: build voice fully now** (not a thin slice, not Phase 6 first).

Settled architecture: `crates/ic_voice` is a **library crate linked into `ic_widget`**
(needs `AppHandle` for lip-sync events, tray mute, and the existing `GatewayClient`) —
voice is an alternate *input* to the same chat path, not a new gateway channel — with
**only `piper.exe` as a supervised subprocess**. Reuse `ic_llama::download::Downloader`
and `SpawnHook` directly for models + the TTS child; `Sidecar`/`release.rs`/`ModelStore`
are blueprints. Device-change needs a hand-written `IMMNotificationClient` (cpal has no
API for it). Capture is cpal + `rubato` (downmix mono, resample 48k→16k f32).

### Voice — done (recorded 2026-07-12)

`crates/ic_voice` (new, 64 tests) + vendored `crates-src/rustpotter` + `ic_widget`
(`voice.rs`, `job_object.rs`, `main.rs`, settings) + `ui/`. No IronClaw core crate
touched. **Full write-up + every trap: `docs/desktop/voice-notes.md`** — read it
before touching voice. The load-bearing surprises, in one breath:

- **The wake-word library from the plan is unusable as published.** rustpotter's only
  non-yanked crate (3.0.2) won't compile (candle 0.2 vs modern `rand`/`half`); every
  2.x is yanked. **User's call: vendor rustpotter 2.0.1** (Apache-2.0, pure DSP, the
  reference-model spotter we actually want) at `crates-src/rustpotter`, path-dep +
  workspace-`exclude`d. No wake models are recorded yet → `NullWakeWord` + the
  **summon hotkey as push-to-talk** until they are.
- **STT now needs two build prerequisites: CMake + LLVM/libclang** (whisper.cpp build
  + bindgen). `winget install Kitware.CMake LLVM.LLVM`; build with `LIBCLANG_PATH` set
  and CMake on `PATH`. The `WHISPER_DONT_GENERATE_BINDINGS` shortcut is a dead end
  (its committed bindings fail layout asserts). Users need neither — Phase 6 ships
  built artifacts. This is the third build-env doc after `windows-build.md`.
- **Pins the plan didn't foresee:** `rubato = 0.16` (3/4 rewrote the API), cpal 0.18's
  `SampleRate` is a `u32` alias with by-value stream configs, `whisper-rs = 0.16` CPU.
- **A regression the voice deps caused elsewhere:** linking ONNX Runtime made the
  `job_object` kill-on-close test's `ping` sleeper fail (exit 1). Fixed by switching
  the test sleeper to PowerShell `Start-Sleep` (network-free). Kill-on-close still
  verified.
- **Everything model/hardware-bound is behind a trait with a fake**, so the whole
  loop — wake → VAD/endpoint → STT → gateway turn → TTS → playback, plus barge-in and
  mute — is tested with no mic and no models. Real impls have `#[ignore]`d asset tests.
- **The reply path reuses the typed chat exactly** (`voice::drive_turn`: send → await
  terminal run → read timeline, since the reply text isn't on the event stream), on
  voice's own thread. **Lip sync** feeds Piper's PCM RMS into `ParamMouthOpenY`
  (`voice://amplitude` → `setMouthOpen`), with the Phase 3 test tone kept as the
  no-audio fallback. Voice is **opt-in** (`settings.voice_enabled`, default off — first
  enable downloads ~210 MB); tray mute + `voice_status`/`set_voice_*` commands exist.

Next: **Phase 6 — packaging & hardening** (bundle piper/whisper/voice/ORT binaries,
verify the amy voice licence, record + bundle wake models, first-run wizard).

## Phase 4 notes — browser automation (done, recorded 2026-07-11)

`crates/ic_browser_mcp` (new) + `crates/ic_widget` (`browser.rs`, `gateway_client`,
`main.rs`) + **one core patch, CP-4** (`docs/desktop/core-patches.md`). Verified
against real Chrome: navigate, read, find, click, screenshot, and a recoverable
missing-selector error all round-trip through the MCP server.

### The plan above was wrong: stdio MCP does not work in Reborn

`ironclaw_mcp` **hard-rejects `transport = "stdio"`** — *"unsupported until
process-level egress controls land"* — and spawns no processes at all. The stdio
docs in `docs/capabilities/mcp.md` describe the **legacy v1 binary**, not Reborn.
The trap is that a stdio manifest **parses, installs, and activates cleanly**, then
fails at *every* `tools/call`. It looks wired, then dies at runtime.

Loopback HTTP — the obvious fallback — was blocked too: the hosted-MCP lane forced
`https` (impossible for a sidecar, which cannot hold a publicly-trusted cert) and
planned egress with `deny_private_ip_ranges: true` (which denies `127.0.0.1`). No
config, env var, or profile — **including `local-dev-yolo`** — relaxes either.

So **some** core edit was unavoidable. Every alternative was measured first: a WASM
shim hits the same wall one seam over (`extension_surface.rs` hardcodes the same
flag for *every* extension capability — wider blast radius); a native first-party
tool means patching `factory.rs` *and* `ironclaw_first_party_extensions` and pulling
`chromiumoxide` into core; executing browser tools inside the `ic_llama` SchemaProxy
(CP-3 style) would bypass the safety layer and approval flow entirely. **CP-4** is
the smallest cut and lands on the lane upstream already supports: `http` is accepted
for, and only for, a **literal loopback IP** (not `localhost` — a DNS name is
rebindable), and private-range denial is waived for exactly that one endpoint.

### ⚠️ Correction (2026-07-13): CP-4 alone never worked. See CP-5 + the discovery break.

The three claims below marked "free" were read off the *code contract*, not off a
running agent. Driving a real browse end to end on 2026-07-13 found the browser tools
had **never actually worked through the gateway** — two further breaks sat behind
CP-4, both silent:

1. **Tool discovery can never succeed** (`network_policy_missing` — the egress wants a
   staged policy that only a *dispatch* stages, and discovery runs at *activation*).
   Reborn then falls back to the bundled manifest **while reporting `activated: true`**.
   So the manifest is not a template — it is the whole tool list. `ic_browser_mcp::manifest`
   now declares all six capabilities, generated from `protocol::Tool`.
2. **The capability is never granted its own endpoint** (`extension_network_policy`
   builds the allowlist only from credential audiences → empty for a credential-less
   sidecar → rejected in obligation preflight). Fixed by **CP-5**.

Both are written up in `docs/desktop/core-patches.md`. Verified after the fix: the
agent calls `browser_navigate` + `browser_get_text` against real Chrome and answers
from the page. **The lesson worth keeping: the Phase 4 gate drove the sidecar and the
runtime's discovery *code*, but never the runtime's *egress* — so a green suite
coexisted with a browser the agent could not reach. The same shape of gap as "the bug
every unit test passed" below, one layer out.**

### What we got for free by riding the supported lane

- ~~**Schemas come from the live `tools/list`.**~~ **False in practice — see the
  correction above.** Reborn *would* discard the manifest's capability declarations and
  rebuild every capability from our `inputSchema` (`hosted_mcp_discovery.rs`), but that
  discovery call never reaches us. The manifest's declarations are what the agent gets,
  so `protocol::Tool` generates them (schemas included) rather than a single template.
- **Approval gating is enforced in the sidecar, not the runtime.** The runtime's
  own approval flow is a no-op — `default_permission: Ask` is hardcoded for every
  discovered MCP tool and **nothing reads it** (see the security section below). So
  `browser_fill` routes sensitive fields through a human via the sidecar's consent
  gate. This is the one claim here that is *not* free from the hosted-MCP lane; we
  built it.
- **Annotations are load-bearing, not cosmetic.** `readOnlyHint`/`destructiveHint`
  are what promote a tool to `EffectKind::ExternalWrite`. `browser_fill` and
  `browser_click` are annotated destructive — that is what makes "submit this form"
  a write effect rather than a read.

### 🟠 SECURITY — Reborn has no tool-approval prompt; we gate in the sidecar (closed)

**The runtime never prompts before a tool runs.** Left alone, `browser_fill` would
type into password and payment fields with no user prompt, on the default profile,
with nothing switched on. That would break Phase 4 step 3 ("sensitive actions must
route through the approval flow"). **The gap is closed in the sidecar** — see "The
consent gate" below — but the underlying runtime finding stands and is worth
recording, because anything else we build on discovered MCP tools inherits it.
Verified, not inferred:

- `default_permission` is **never read** anywhere in the workspace. Every occurrence
  is a manifest literal, a struct-literal write, a field-to-field copy, or a test
  assertion. There is no `match`/`==`/`if` on it in any decision path.
- `Decision::RequireApproval` has **zero production producers**. The only authorizer
  wired into Reborn composition is `GrantAuthorizer` (`factory.rs:648`, and 4 more
  sites), and it returns only `Allow` or `Deny` (`ironclaw_authorization/src/lib.rs`,
  `authorize_from_grants_with_authority_ceiling`). `LeaseBackedAuthorizer` — the one
  that *could* require approval — exists but is **never wired in**.
- `ActiveExtensionCapability` (`extension_lifecycle.rs:54`) has **no
  `default_permission` field at all**; `from_descriptor` discards it. Then
  `extension_surface.rs:55` mints every active capability a standing grant with
  `expires_at: None`, `max_invocations: None`, and `allowed_effects` = its own
  declared effects. So `ExternalWrite` clears its own ceiling.

The approval machinery downstream (`CapabilityHost`'s `RequireApproval` arm,
`ironclaw_approvals`, `resolve_gate`) is fully built and simply **unreachable**. The
gates the widget *does* handle are **auth** gates (product-auth / credential setup),
not tool approvals — which is why this was easy to mistake for a working gate.

So there is no "prompt fatigue" switch to worry about, because **there is no prompt
to lose**, and no per-capability policy surface in which to pin one. `ExternalWrite`
only widens the effect list; it forces nothing. `ironclaw_safety` contains no
approval-forcing code. `local-dev-yolo` changes filesystem/network/secrets, not
approvals — there are none to change.

**Consequence for us:** the approval requirement cannot be met by riding the
runtime — see the consent gate below for how it is met instead. And it was reported
upstream (privately-first: [#6000](https://github.com/nearai/ironclaw/issues/6000)
asks how to disclose, since the repo has no `SECURITY.md` and private reporting is
disabled). If upstream wires `Decision::RequireApproval` for `Ask` capabilities, the
sidecar gate becomes defence-in-depth rather than the only line.

### The consent gate (how step 3 is actually met)

`ic_browser_mcp::consent` + `::classify`. Sensitive fills route through a human
**in the sidecar** — the last boundary the model cannot route around, because the
sidecar decides, not the prompt (the same move as CP-3). Enforced in
`BrowserSession::fill`: classify → ask → *then* type, so a denied fill never touches
the page.

- **Fail closed, everywhere.** The classifier has three outcomes, not two —
  `Sensitive`, `Benign`, `Unknown` — and **both `Sensitive` and `Unknown` ask**.
  Only a positively-ordinary field types without a prompt. A probe that throws, a
  selector that matches nothing, a shadow-DOM/custom element, an unrecognised input
  type, or a `type="text"` field inside a form that holds a password → all ask. An
  unnecessary prompt is annoying; a missed one is the whole gap back. The hinge is
  `Sensitivity::needs_approval` (`Unknown → true`); flipping it reopens the gap.
- **The channel is the sidecar's stdout/stdin**, which the widget already owns as
  the parent process — no new port, no auth to get wrong. `IC_BROWSER_MCP_APPROVAL`
  out, `IC_BROWSER_MCP_DECISION` in. Every non-yes is a no: no channel (standalone
  sidecar → `DenyAll`), timeout, closed pipe, malformed answer, answer for a
  different request.
- **The prompt shows what will be typed and where** (field label + URL + the value,
  and flags non-HTTPS) — a consent prompt the user can't evaluate isn't consent. The
  widget surfaces it as a red prompt distinct from a normal amber gate, defaulting to
  the safe answer, and the character goes `concerned`.
- **The value never reaches a log** (`FillApproval::redacted`).
- Verified against real Chrome (`tests/real_browser.rs`, `--ignored`) *and* end to
  end through the real sidecar binary's stdout/stdin channel: ordinary field silent,
  password field prompts, deny types nothing, approve types.
- A denial is a **recoverable** `isError`, not a crash — the agent reports it and
  moves on.

### Three timing rules, or you get an extension with no tools

Each one is silent when violated. All three are encoded in `ic_widget::browser`:

1. **The extension catalogue is scanned once, at `serve` boot.** The manifest must
   be on disk *before* the gateway starts — so the sidecar launches first and its
   live port is written into the manifest's `url`.
2. **Discovery runs at *activation*, not at boot.** The gateway calls our
   `tools/list` when the extension is activated, so the sidecar must be listening
   then, and activation is driven against the **running** gateway.
3. **A restart does not re-discover.** `restore_extension_lifecycle_state`
   republishes the *bundled manifest*, which carries only a capability **template**
   — not the six tools. So the widget re-activates on **every** launch.

And a discovery failure makes the gateway **silently** fall back to that template
*while still reporting `activated: true`*. So the widget verifies the capability
count rather than trusting the activation response.

### The bug every unit test passed

The unit tests fill the `ToolExecutor` seam with a fake, so they exercise the MCP
transport and never touch CDP. The CDP layer was dead on arrival against current
Chrome: it emits events `chromiumoxide` 0.7 cannot deserialize, the event pump
treated the first one as fatal and exited, the handler dropped — and every tool call
failed with *"send failed because receiver is gone"*, ~60 ms after a launch that
reported success. **A green suite and a browser that never worked.** The pump now
logs an unparseable event and keeps pumping. Pinned by
`tests/real_browser.rs` (`#[ignore]`d — run with `--ignored`; it needs a browser).

### Other decisions worth keeping

- **Screenshots are viewport JPEGs, not full-page PNGs.** The host caps an MCP
  result at 1 MiB and base64 inflates by a third; a full-page PNG of a real site
  blows the budget and fails with an opaque `response_error`. A q70 viewport JPEG of
  example.com is ~15 KiB.
- **A missing selector is a *recoverable* `isError` result, not a JSON-RPC error.**
  The model must see it and try another selector; a JSON-RPC error fails the whole
  capability. Only a genuinely broken browser is an error.
- **The sidecar rides in the widget's Job Object**, so a hard kill takes the
  automation browser down too — an orphaned Chrome would hold a profile lock and a
  port. `BrowserSession`'s `Drop` deliberately does *not* call
  `Browser::close()`/`kill()`: both are `async`, and the discarded future closed
  nothing while reading like a graceful shutdown.
- **The automation browser never touches the user's real profile** — a dedicated
  user-data dir, so the agent only ever has access to what the user logs into inside
  the automation window themselves.

`cargo test -p ic_integration_tests --test browser_mcp_contract` is the gate: it
drives the real sidecar over HTTP and feeds its `tools/list` through the runtime's
**own** discovery code, so it fails if IronClaw would refuse us — rather than
re-asserting our own beliefs about IronClaw back at ourselves.

**Known-unrelated failures:** three `ironclaw_reborn_composition` tests
(`local_yolo_policy_*`, `local_dev_yolo_shell_*`) fail on Windows with *"backslashes
are not allowed"*. Pre-existing upstream bug, confirmed failing with our core changes
stashed, and filed as
**[nearai/ironclaw#5999](https://github.com/nearai/ironclaw/issues/5999)**. Not caused
by CP-4. Root cause: `build_workspace_filesystems` passes a **host path** where a
`MountAlias` (a `/`-rooted POSIX string) is expected — so it is not really a
UNC/`\\?\` issue at all; a plain `C:\...` fails the same two checks. It bites only
the yolo path, because the ambient alias is built only under `--confirm-host-access`.
**`local-dev-yolo` is therefore completely unusable on Windows** (`ironclaw-reborn
serve --confirm-host-access` fails during runtime assembly) — we don't use that
profile, so it does not block us.

### Upstream issues filed from this phase

| Issue | What | Why we care |
|---|---|---|
| [#5998](https://github.com/nearai/ironclaw/issues/5998) | No transport for a local MCP server (stdio rejected, loopback denied) | **CP-4 gets deleted when this lands** — see `core-patches.md` |
| [#5999](https://github.com/nearai/ironclaw/issues/5999) | `local-dev-yolo` can't start on Windows (host path used as `MountAlias`) | Explains 3 red tests in our baseline; not ours |

Next: **Phase 5 — voice (`crates/ic_voice`)**. Canvas is done (below). The Phase 3
lip-sync test-tone stub is still the seam TTS amplitude replaces.

## Phase 3 notes — animated character companion (done, recorded 2026-07-11)

`crates/ic_widget` (`character.rs`, `hit_test.rs`, `settings.rs`, `main.rs`) +
`ui/` (+ `docs/desktop/character-pipeline.md`, which holds the full pipeline
architecture, the licensing gates for Phase 6, and the open follow-ups). No
IronClaw core crate was touched.

- **The Phase 3 compatibility check failed exactly as CLAUDE.md warned, but
  one layer deeper than expected.** The Cubism 5 web core (SDK 5-r.5, core v6)
  moved render orders from `drawables.renderOrders` to
  `model.getRenderOrders()`; pixi-live2d-display 0.4.0 bundles the Cubism 4
  framework, which reads the old field. The crash surfaced as ONE console
  error and a forever-blank canvas, because a throw inside a Pixi ticker
  callback ends the rAF chain — the render ticker died while the model's
  updates kept ticking on the shared ticker. Fixed with a core API bridge plus
  a guarded render loop (`ui/src/character.ts`); the earlier "WebView2 uploads
  blank textures" theory was probably this bug wearing a costume (retest noted
  in the pipeline doc). **Hiyori (Cubism 4) is the dev model and renders;
  Ren (5.3, offscreen blending) stays parked until a Cubism 5 framework.**
- **Per-pixel click-through is split across the IPC boundary** because a
  click-through window's webview receives no mouse events at all: the UI
  rasterizes the chat panel rect + the character's alpha silhouette (GPU
  readback at one texel per 8-px cell, dilated) into `ic_widget::hit_test::
  HitMask`; a Rust poller (`spawn_interaction_watch`) tests the global cursor
  against it and flips `set_ignore_cursor_events`. No mask yet = fully
  interactive (fail-safe). The same poll feeds eye tracking (`cursor://pos`)
  and pauses animation while a fullscreen app is foreground
  (`character://active`, with the Progman/WorkerW desktop-shell exclusion).
- **Layout**: the character stands at the window's bottom edge over the
  transparent desktop; the Phase 2 chat card became a collapsible bubble panel
  above it. Head click (model hit areas, bounding-box top-third fallback)
  toggles the panel; body drag moves the window; an incoming tool gate forces
  the panel open alongside the `concerned` face.
- **The state machine's Phase 5 hooks are live**: `set_character_signals`
  drives `listening` (composer focus) and `speaking` (reply render, ended on a
  reading-time estimate). Lip sync runs off a test-tone stub patched into
  `motionManager.update` — the exact seam Phase 5's TTS amplitude replaces.
  State-entry motions play at FORCE priority so transitions interrupt; idle is
  the exception (the motion manager already loops it).
- **Character choice is a settings toggle** (`settings.json` → `character`,
  `CharacterId` newtype validates the path segment; dashboard picker reads the
  bundled `characters/manifest.json`). `apply_provider` now load-modify-saves
  settings — a rebuilt literal would have wiped the new field.
- **Perf rules landed**: both tickers capped at 30 fps; animation pauses when
  the window is hidden (WebView2 stops rAF) and under fullscreen apps.
- `SpriteRenderer` (PNG poses + CSS cheap-puppet transforms) exists and ships
  no art; the placeholder remains the no-assets fallback.

Next: **Phase 4 — browser automation** (`crates/ic_browser_mcp`).

## Phase 2b notes — dashboard panels + LLM wiring (recorded 2026-07-10)

`crates/ic_widget` + `ui/` + `crates/ic_llama` (+
`docs/desktop/llm-provider-selection.md`). No IronClaw core crate was touched.
The interactive panels are done and **verified by a manual smoke run on
2026-07-10**: `cargo run -p ic_widget --features app` launched the widget, the
supervised gateway reached ready, and the dashboard panels rendered. The two
remaining pieces are **explicitly-deferred follow-ups — GGUF download UI and
tokens/sec** (see below); with those outstanding, 2b is functionally complete
but not feature-complete.

> ⚠️ **Superseded (2026-07-14).** Both follow-ups are done, and so is the failover
> decision this section leaves open — see **"Closing the open items"** at the
> bottom of this file. The paragraphs below are kept as the record of what was
> true in July; where they conflict with that note, the note wins.

### The seam that had to be closed first

- **The widget never started a local model.** `ic_widget` depended on `ic_llama`
  but `main.rs` used none of it: a launched app supervised the gateway with an
  empty `llm_env`, so the agent had no LLM. Phase 1's round-trip only ran through
  `ic_integration_tests`. `spawn_gateway` now brings up the model *before* the
  gateway (which reads `LLM_BASE_URL` once at boot) and keeps the `LocalLlm` in
  app state so its `Drop` stops the sidecar and proxy.
- **The sidecar now dies with the widget under a hard kill.** This needed an
  `ic_llama` addition: `SpawnHook`, an optional callback the sidecar runs against
  its `llama-server` child on *every* (re)spawn. The widget passes one that
  enlists the child in the same Windows Job Object the gateway rides in; a hook
  error fails the spawn and kills the child. Without it, `TerminateProcess` of the
  widget would orphan `llama-server` holding VRAM and a port. Pinned by
  `the_spawn_hook_runs_on_every_spawn_including_restarts` and
  `a_spawn_hook_error_fails_the_attempt_and_kills_the_child`.

### Panels

- **Sessions** (`GET /threads`) and **automations** (`GET /automations`) —
  live routes. Automations are schedule entries, *not* run history; "Run history"
  joins memory/skills/audit in the unavailable-with-reason list.
- **Local model** — read-only: model id, backend, live sidecar state, GPU-layer
  offload, estimated VRAM/RAM, placement warnings. `None` when running without
  local inference. **tokens/sec is deferred** (needs live metrics polling, not the
  static placement).
- **Provider** — the active-provider switch and cloud key manager. See below.

### Provider selection is single-valued, and failover is unbuilt

- `LLM_BACKEND` holds one value, so the local sidecar and a cloud provider are
  mutually exclusive. The dashboard drives a `ProviderSelection` (`Local` |
  `Cloud{id, model}`) persisted in `settings.json`; `apply_provider` persists it,
  tears down the running gateway + model, brings the gateway back up on the new
  selection, and reloads the webviews so they re-establish their client and
  thread. The provider list is read from the **same `providers.json` the gateway
  resolves against** (`ic_widget::providers`), so the dashboard cannot drift from
  the runtime; OAuth/subscription providers with no `api_key_env` are filtered
  out of the key UI. Keys live in the credential store under
  `provider-key/<id>`; `has_provider_key()` is the only thing the UI sees, never
  the key.
- **The v1 "cloud failover when a key is configured" promise is not implemented.**
  `FailoverProvider` exists but is only ever constructed same-backend in one
  production site. The three ways to close it (a core patch, a route-around in the
  `ic_llama` SchemaProxy à la CP-3, or cutting the promise) are written up in
  `docs/desktop/llm-provider-selection.md`. Decide before Phase 6.

Next: finish 2b's deferred follow-ups (**GGUF download UI**, **tokens/sec**) and
do the **manual smoke run**, then **Phase 3 — animated character companion**
(Live2D, `ren_en/` model), then **Phase 4 — browser automation**.
*(All three follow-ups landed — see "Closing the open items" at the bottom.)*

## Phase 2a notes — widget shell, supervision, chat (done, recorded 2026-07-10)

`crates/ic_widget` + `ui/` (+ `docs/desktop/chat-rendering.md`,
`docs/desktop/dashboard-gaps.md`, and four **corrections** appended to
`gateway-api-notes.md`). No IronClaw core crate was touched. Verified by
launching the app: it spawns `ironclaw-reborn`, and a **hard kill** of the widget
(`TerminateProcess`, no `Drop`) takes the gateway down with it.

Phase 2 was split. **2a** = shell + supervision + chat. **2b** = the remaining
dashboard panels.

### Two findings that reshaped the plan

- **The event stream carries no assistant text — at all.**
  `ProductProjectionItem::Text` has *no producer* in the workspace, and
  `final_reply` is only produced on the Telegram/Slack push path. Upstream's own
  SPA has a dead branch waiting for it. So the widget watches `run_status` until
  the run is terminal, then reads the reply from `GET /threads/{id}/timeline`.
  There is no token streaming, and the stream is a 1-second poll. See
  `docs/desktop/chat-rendering.md`; pinned by
  `the_event_stream_never_carries_the_assistant_text`.
- **Memory browser, skills list, and audit log have no HTTP route**, and
  `ironclaw_gateway` (v1) is not compiled into `ironclaw-reborn serve`. "Jobs" is
  only `GET /automations` (scheduled cron entries, no run history). The dashboard
  lists these as unavailable with the reason rather than faking them. See
  `docs/desktop/dashboard-gaps.md`.

### Design decisions worth keeping

- **`gateway_client` is the only place we speak to the gateway.** Newtyped ids
  (`ThreadId`/`RunId`/`GateRef` all appear in one URL path, so `String` would make
  transposition a 404 rather than a compile error), tolerant decoding (unknown SSE
  events and projection items become `Unknown` instead of failing), and a
  spec-correct SSE decoder.
- **`RunPhase::Other` is deliberately non-terminal.** An unknown status from a
  newer gateway is far more likely a new in-flight state than a new way of
  finishing; assuming terminal would make the widget stop listening mid-turn.
- **A `401` from the gateway is fatal, not a retry.** The process is healthy and
  simply rejects our token; restarting cannot help. The supervisor stops and says
  so. (`GatewayState::Unhealthy`, not a restart loop.)
- **Windows Job Object with `KILL_ON_JOB_CLOSE`** owns every child. This is the
  only thing that survives a hard kill of the widget, and it is what stops an
  orphaned gateway from holding the libSQL write lock and the port.
- **Widget position is keyed by a hash of the monitor arrangement**, and a saved
  point that is no longer on any monitor is discarded on read. An always-on-top
  widget stranded offscreen cannot be dragged back.
- **`EventStream` must be `Send`.** An earlier version awaited `self.open()`
  inside `reconnect(&mut self)`, holding a `&EventStream` across the await;
  `&T: Send` requires `T: Sync`, which a boxed response body never is. The stream
  would not compile inside `tokio::spawn`. Pinned by
  `a_pump_task_over_the_event_stream_is_send`.
- **Tauri sits behind an optional `app` feature**, so `cargo test -p ic_widget`
  never builds a WebView. The frontend (`ui/`, SolidJS + Vite) is built before
  `tauri-build` runs, not through `beforeBuildCommand`.
- **The gateway is stored into app state *before* `Ready` is emitted.** The UI
  falls back to reading `gateway_state` when it misses the event; emitting first
  left a window where that read still said `Starting` and the event had already
  gone, stranding the widget on "starting" forever.
- **The widget creates its thread on the first `ready` it observes**, not on
  mount. The gateway takes ~500 ms to boot (much longer on a first run), so
  creating the thread on mount failed with "still starting" and never retried.

### Corrections to `gateway-api-notes.md` (C1–C4)

Most `WebChatV2Event` variants are unreachable; the assistant's text is not on the
stream; `401` returns a bare text body rather than the documented JSON error
shape; and a replayed `client_action_id` only yields `already_submitted` while the
original message is still `Submitted` — after that the identical replay is a
**409**. Both mean "accepted exactly once"; `Error::is_duplicate_action()` exists
so the UI does not show a failure for either.

### Running it

```bash
cd ui && npm install && npm run build && cd ..     # MUST run before the app build
cargo build -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta
cargo run -p ic_widget --features app --bin ic-widget
```

**The frontend is embedded into the binary at compile time.** `generate_context!`
reads `ui/dist`, so a change to `ui/src` needs `npm run build` *and then* a Rust
rebuild. There is no dev server: `tauri.conf.json` deliberately sets **no
`devUrl`**. It used to, and because `tauri-build` sets `cfg(dev)` for a plain
`cargo run`, the webview navigated to `http://localhost:1420` — which only the
`tauri` CLI's `beforeDevCommand` would have started. The result was WebView2's
"can't reach this page" error inside the widget. Hot reload would need
`cargo install tauri-cli` and `cargo tauri dev`; it is not wired up.

`ironclaw-reborn` is found via `IRONCLAW_REBORN_BIN`, then beside the widget
binary. The bearer token is minted into the Windows Credential Manager under
**"IronClaw Desktop" / "gateway-token"**; state lives in
`%LOCALAPPDATA%\IronClaw Desktop\`.

Logs go to stderr; `RUST_LOG=ic_widget=debug` for more. A healthy first launch
prints `ironclaw-reborn is ready`, `gateway is ready`, and `the widget created a
thread and started its event pump` — that last line is the proof the webview
loaded and its IPC reached Rust.

The summon hotkey (Ctrl+Alt+Space) is registered best-effort. If another app owns
it you get a warning and the tray still works; nothing else is affected.

### Verified facts that are easy to get wrong

- **Threads survive a gateway restart.** They are persisted through the
  libSQL-backed root filesystem, *not* a `threads` table — a schema dump of
  `reborn-local-dev.db` shows no thread rows, which looks like they are in-memory.
  They are not. The Phase 2b sessions panel can rely on `GET /threads`.
- **Nothing about the widget's own behavior is observable in that database.** Use
  the stderr log, not the DB, to check whether the UI is alive.

## Phase 1 notes — llama.cpp integration (done, recorded 2026-07-10)

`crates/ic_llama` (+ `docs/desktop/llama-cpp-pin.md`,
`docs/desktop/llama-cpp-tool-grammar.md`, crate `README.md`). No IronClaw core
crate was touched. Verified on this machine: Qwen3-4B-Q4_K_M, full GPU offload
(`-ngl 37`) on an RX 7900 XTX, agent round-trip through `ironclaw-reborn serve`
in 7.5 s, fully offline.

- **Pinned llama.cpp `b9948`**, digests from the GitHub release API's `digest`
  field. Vulkan is the default (31 MiB, covers all vendors); CUDA is opt-in only
  (628 MiB with the cudart archive); CPU is the fallback. Bumping the pin has a
  checklist — read `llama-cpp-pin.md` before touching `release.rs`.
- **The archive layout is not assumed.** `runtime.rs` extracts, then *searches*
  for `llama-server.exe` (upstream has moved it between `/` and `build/bin/`).
  It launches in place because the backend DLLs are its siblings.
- **Per-layer sizes come from GGUF tensor *offsets*, not a `ggml` type table.**
  Tensors are written back to back, so a tensor's size is the distance to the
  next offset. This is why `placement.rs` can size the VRAM budget exactly
  without tracking 40 quantization block sizes that drift upstream.
- **`-ngl N` offloads the LAST N blocks**, and `N > block_count` also offloads
  the output tensors — the planner fills the budget from the last block backwards
  and accounts for each layer's KV-cache slice, which at long contexts exceeds
  the layer itself.
- **A nonzero DXGI `DedicatedVideoMemory` does not mean a discrete GPU.** AMD
  APUs report a BIOS carve-out of system RAM (485 MiB here). `is_discrete()` uses
  a 1 GiB floor; without it the planner offloads to the iGPU.
- **`llama-server` answers `503 Loading model` before it is ready.** The
  supervisor treats `503` as loading, not as a crash; a server that is up but
  never reaches `200` within the startup budget is killed and counted as a
  failure. Two consecutive failures mark the model *suspect* (a marker beside the
  weights, surviving restarts) rather than restarting forever.
- **The sidecar port is reserved once and reused across restarts**, because
  IronClaw reads `LLM_BASE_URL` at startup and never re-reads it.
- **CP-3 (route-around, not a patch): llama.cpp cannot compile IronClaw's tool
  schemas.** Tool calls require `--jinja`, which compiles tool schemas into a
  GBNF grammar; llama.cpp rejects repetition counts >= 2000, and
  `builtin__spawn_subagent` declares `maxLength: 65536`, so *every* agent turn
  failed. Fixed additively: `ic_llama::proxy::SchemaProxy` sits in front of the
  sidecar (`LLM_BASE_URL` points at it) and strips oversized bounds from
  `tools`/`response_format` in flight. Patching the one core schema was rejected
  because any user-installed MCP tool can declare a bound of its own. See
  `docs/desktop/llama-cpp-tool-grammar.md`.
- **Tests are hermetic** (`cargo test -p ic_llama`, wired into CI): synthetic
  GGUF files, a mock HTTP origin for `206`/`200`/`416` + digest mismatch, and
  `testsupport/fake_llama_server.rs` — a real subprocess reproducing a slow load,
  a crash on startup, a crash after being healthy, and a wedged server. The
  real-weights round-trip is `#[ignore]`d in
  `crates/ic_integration_tests/tests/local_model_roundtrip.rs`.

## Fork bootstrap notes (Phase 0 — recorded during setup)

- **Upstream source pinned to branch `reborn-integration`** (commit `a492857`), *not* a release tag: the Reborn runtime (`ironclaw_reborn`, `ironclaw_reborn_cli`, libSQL storage) that this entire desktop architecture depends on exists **only** on `reborn-integration`. The latest release tag (v0.21.0) contains none of it. This is a deliberate, necessary deviation from the "pin to a release tag" rule above — revisit if/when Reborn lands in a tagged release. Pin the exact commit for reproducibility; unshallow before the first upstream sync (current clone is `--depth 1 --filter=blob:none`).
- Cloned upstream directly into this repo (`git remote upstream = https://github.com/nearai/ironclaw.git`); no fork org yet. Add your fork as `origin` when ready.
- Upstream's own `CLAUDE.md` is preserved as **`CLAUDE.upstream.md`** (it holds useful IronClaw dev conventions — code style, extension/auth invariants, crate layout). This file (the desktop-fork guide) is the authoritative root `CLAUDE.md`.
- The repo also ships its own `.claude/` skills (`reborn-feature`, `add-tool`, `ship`, `trace`, `mintlify-docs`, `architecture-video`) and rules — use them.

### Phase 0 steps 5 & 6 — done (recorded 2026-07-09)

- **Step 5 — merge-gate crate `crates/ic_integration_tests`** (added to workspace members). It spawns `ironclaw-reborn serve` (libSQL `local-dev` profile) against a **hermetic mock LLM** and drives the WebChat v2 chat contract end-to-end: `401` without a bearer → create thread → send message → stream SSE to `run_status: completed` → assert the assistant reply round-tripped into the timeline. Verified passing on Windows (see the crate `README.md` for the two-step run recipe). Key facts baked in: `openai_compatible` → `RigAdapter` → **non-streaming** Chat Completions, so the mock only answers `POST /v1/chat/completions`; the assistant reply surfaces in the **timeline**, not as a `text` item in the projection SSE (which carries only `run_status`); `IRONCLAW_REBORN_WEBUI_USER_ID` must equal the runtime default owner (`reborn-cli`); a separate crate can't use `CARGO_BIN_EXE_*`, so the binary is located from the target dir (override `IRONCLAW_REBORN_BIN`).
- **Step 6 — Windows CI** at `.github/workflows/desktop-ci.yml` (three `windows-latest` jobs: `quality` = fmt + scoped clippy `-D warnings`; `gate` = build the `serve` binary then run the merge gate; `release-build` = `cargo build --release` core, no webui). Scoped to fork crates + the serve path so pre-existing upstream lints don't fail the fork gate. Pinned action SHAs match the upstream workflows.
- Phase 0 is complete.

## Build phases — execute in order

### Phase 0 — Fork bootstrap ✅ (see notes below; Windows build friction log: `docs/desktop/windows-build.md`, serve API contract: `docs/desktop/gateway-api-notes.md` + its C1–C4 corrections)

### Phase 1 — llama.cpp integration (`crates/ic_llama`) ✅ (see notes below)

### Phase 2 — Widget + dashboard (`crates/ic_widget` + `ui/`) — 2a ✅, 2b in progress
Split into **2a** (shell + supervision + chat — done, see notes) and **2b** (remaining dashboard panels).

Phase 2b scope:
1. Dashboard panels with a live route: sessions (`GET /threads` — threads survive gateway restarts via the libSQL-backed root filesystem), automations (`GET /automations`), model picker + GGUF download UI + token/s + VRAM stats from `ic_llama`, provider keys (Credential Manager).
2. Panels without a route (memory browser, skills, audit log, run history) stay visible as unavailable-with-reason per `docs/desktop/dashboard-gaps.md`. Do not fake them.
3. Tool-approval prompts surfaced from the safety layer (also mirrored in the speech bubble).
4. Keep the speech-bubble chat UI as the Phase 2 face of the app — the character lands in Phase 3. Transparency, undecorated frame, and always-on-top landed in 2a (`WebviewWindowBuilder` in `crates/ic_widget/src/main.rs`). **Per-pixel hit testing did not** — clicks do not yet pass through the transparent regions. Build that plumbing in 2b or at the start of Phase 3.

### Phase 3 — Animated character companion (in `ui/`) ✅ (see notes below; pipeline doc: `docs/desktop/character-pipeline.md`)

The widget becomes an **animated anime-style character** standing on the desktop; the Phase 2 speech bubble anchors beside it.

1. **Dev model (already in-repo): `ren_en/`** — Live2D's official "Ren Foster – PRO" sample (Cubism 5.3). Copy `ren_en/runtime/` into `assets/characters/ren/live2d/`. It ships with everything we drive: `LipSync` group → `ParamMouthOpenY`, `EyeBlink` group, Idle motion + 2 motions, 5 expressions, Head/Body hit areas. License: Live2D Free Material License — commercial use permitted for general/small-scale users, so it may ship in v1.
2. **FIRST TASK — compatibility check:** Ren Foster uses Cubism 5.3 drawing features (alpha-blend masks, offscreen rendering). Verify `pixi-live2d-display` with the **Cubism 5 web core** renders it correctly in the Tauri webview (WebView2). If it misrenders, fall back to an official Cubism 4 sample (Hiyori/Mao) for dev and revisit.
3. **`CharacterRenderer` interface, two backends:**
   - `Live2DRenderer` (primary): loads `.model3.json` from `assets/characters/<name>/live2d/`. Drive: blink cycles, cursor-following eye tracking, head sway, expressions, `ParamMouthOpenY` lip sync from TTS amplitude (stub with a test tone until Phase 5 wires Piper).
   - `SpriteRenderer` (fallback/simple): flat PNG art from `assets/characters/<name>/sprite/` with cheap-puppet transforms (idle bob, tilt, squash). User-supplied copyrighted art is dev-only — **must not ship in public releases**.
   - Character = asset folder + `character.json` (name, renderer type, scale, anchor, param/expression → state mappings). A commissioned model later drops in with zero code changes; character choice is a settings toggle.
4. **Animation state machine** driven by supervisor/gateway events: `idle` → `listening` (input focus / wake word) → `thinking` (run in flight) → `speaking` (reply rendering / TTS) → `concerned` (approval pending) → `error` (child unhealthy, e.g. `GatewayState::Unhealthy`). Map Ren's expressions + motions to these states in `character.json`. Transitions interruptible; Stop always returns to `idle`. Note: because there is no token streaming, `thinking` runs until the timeline fetch returns — `speaking` is entered on reply render, not first token.
5. Use the model's hit areas for click handling (click head = summon input, drag body = move window); clicks outside the character pass through (per-pixel hit testing).
6. Performance: Ren is heavyweight (4096px texture, offscreen rendering). Cap FPS ~30, pause animation when hidden or a fullscreen app is foreground, measure GPU alongside llama.cpp on iGPU-only machines.
7. Licensing gates before public release: verify Live2D Cubism Web SDK license tier for our distribution size; check any marketplace model's redistribution terms; drop dev-only sprite art.

### Phase 4 — Browser automation (`crates/ic_browser_mcp`) ✅ (see notes below)

⚠️ **Step 1 and step 3 below were wrong** — stdio MCP does not work against Reborn. Corrected in the Phase 4 notes at the bottom; read those, not this list.

1. ~~Standalone MCP server (stdio)~~ → **streamable-HTTP MCP server on loopback** wrapping `chromiumoxide`: `browser_navigate`, `browser_get_text`, `browser_find`, `browser_fill`, `browser_click`, `browser_screenshot`.
2. Launch a dedicated browser profile (probe registry: Chrome → Edge; Edge is guaranteed on Win10+). Never attach to the user's running profile by default.
3. ~~Register with IronClaw through its MCP config.~~ → **host-bundled extension manifest + CP-4**. Sensitive actions route through the approval flow (every discovered MCP tool is `default_permission: Ask`, hardcoded upstream).
4. CAPTCHAs/logins: pause, notify via widget (character `concerned` state), let user complete, resume. Selector failures: screenshot → vision-capable model fallback.

### Phase 5 — Voice (`crates/ic_voice`) + Canvas — **Canvas ✅, Voice ✅ (see notes below; full write-up: `docs/desktop/voice-notes.md`)**

⚠️ **The voice pipeline plan below has two dead ends and several Windows landmines — verified before any code. Read the Phase 5 notes at the bottom before building voice.**

1. Pipeline: `cpal` capture → ring buffer → wake word (~~openwakeword ONNX via `ort`~~ — **models are CC-BY-NC, non-commercial; use `rustpotter` or self-trained**) → silero VAD gate (`voice_activity_detector`, MIT ✓) → `whisper-rs` transcribe (**CPU first — Vulkan silently no-ops on Windows static builds**) → post to gateway → reply → ~~Piper TTS playback (bundled piper1-gpl binary)~~ **the piper1-gpl binary no longer exists (it's a Python package now); use the archived MIT `rhasspy/piper` exe or run the VITS ONNX in-proc**. Barge-in: stop TTS when VAD triggers.
2. Wire TTS playback amplitude into `CharacterRenderer` lip sync (`ParamMouthOpenY`), replacing the Phase 3 test-tone stub (the seam is `patchLipSync` in `ui/src/character.ts`; Piper emits no timing, so compute RMS from the PCM yourself); barge-in returns the character to `listening`.
3. WASAPI device-change handling (**cpal exposes no device-change API — hand-write an `IMMNotificationClient` via the `windows` crate**), mic-live indicator on the character/bubble, tray mute toggle, audio never written to disk.
4. ~~Canvas: dedicated Tauri window; agent emits HTML/SVG via a `canvas_render` tool (register as WASM tool or MCP); render in sandboxed iframe, sanitize output.~~ **✅ Done — see Phase 5 canvas notes. Route: in-process loopback MCP (reusing CP-4), not a WASM tool — WASM/first-party would strand the HTML in the wrong process behind a sanitizing, 16 KiB-capped channel.**

### Phase 6 — Packaging & hardening — **config + hardening ✅, real MSI build gated on external inputs (see notes below; full write-up: `docs/desktop/packaging.md`)**
1. Single MSI (Tauri bundler): our app + `ironclaw-reborn` + llama.cpp binaries + Piper + bundled character assets. First-run wizard: GPU probe → model recommendation → provider keys → storage init (libSQL — no Postgres install!).
2. Uninstaller must remove the Credential Manager entry ("IronClaw Desktop" / "gateway-token") and `%LOCALAPPDATA%\IronClaw Desktop\`.
3. Tauri auto-updater; code-sign (unsigned + mic capture + child processes = SmartScreen/AV flags).
4. Failure drills before ship: kill llama-server mid-generation; kill ironclaw-reborn mid-job; sleep/resume; monitor unplug; disk-full during GGUF download; occupied ports (ports are already dynamic — verify end to end).

