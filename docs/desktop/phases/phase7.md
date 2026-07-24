# Phase 7 spec — Ambient companion

Moved verbatim from CLAUDE.md's "Build phases — execute in order" section during
the 2026-07-24 doc restructure (see `docs/desktop/PROGRESS.md` for what actually
shipped, sub-phase by sub-phase). This file is the original plan text, unedited.

### Phase 7 — Ambient companion (proactive suggestions + self-learning & imported skills) — 7a ✅ (see notes below)

Goal: the character *initiates* and *learns*. The dashboard stays an occasional
settings/inspection surface — everything in this phase surfaces through the
character and bubble. All work is additive (modules in `ic_widget`, or a new
`crates/ic_ambient` if it grows); no core patch is expected — if one becomes
unavoidable, follow the `core-patch:` protocol in `docs/desktop/core-patches.md`.
Build strictly in order 7a → 7b → 7c → 7d; each sub-phase ends with the full
gate (fmt, clippy, test, integration suite, manual smoke run) and a dated
progress note appended to this file in the established style.

**7a — Proactive surfacing plumbing (shared by everything below)**

1. A dedicated **ambient thread**, separate from the chat thread, created via
   `gateway_client`. Replies follow the existing contract: watch `run_status`
   to terminal, then read `GET /threads/{id}/timeline` (no token streaming
   exists — see `docs/desktop/chat-rendering.md`).
2. A new character state **`suggesting`** (interruptible, mapped to an
   expression/motion in `character.json`) + a bubble popup rendering the
   suggestion with **Accept / Not now**. "Not now" responses are recorded with
   timestamps (never deleted — LLM-data invariant) and feed the rate limiter.
3. A **guardrail module**: hard rate cap (default ≤ 2 unsolicited surfacings
   per hour), quiet hours, master toggle (`settings.ambient_enabled`, default
   **OFF**) in tray + dashboard. Everything runs locally.
4. First consumer: **scheduled proactivity through the existing
   triggers/automations lane** (`GET /automations` is already surfaced; min
   fire cadence is 60 s). A completed automation run surfaces via the popup.
   ⚠️ VERIFY FIRST, against the running gateway, how a trigger-fired run lands
   (thread routing, projection SSE visibility) versus a webchat run — read
   `ironclaw_triggers` (`trusted_submit.rs`, `worker.rs`) and the reborn
   composition before writing widget code. Do not assume it matches webchat.

**7b — Self-learning skills (Hermes-style reflection, fail-closed)**

1. After a *user-initiated* run reaches terminal, fire **one reflection turn**
   on the ambient thread (behind `settings.reflection_enabled`): "Did this
   task teach a reusable procedure? If yes, output a draft SKILL.md
   (frontmatter: name / description / activation keywords, matching
   `skills/*/SKILL.md` in-repo). If not, say no. Do NOT install anything."
2. **A draft is never auto-installed.** The runtime has no tool-approval
   prompt (Phase 4 finding: `default_permission` is never read;
   `builtin__skill_install` would execute unprompted). Installation happens
   only after the user approves in the bubble — red prompt, full skill text
   shown, consent-gate pattern from Phase 4, default answer No. On approval
   the widget sends an explicit install turn (or dispatches the install
   capability directly if a cleaner seam exists).
   ⚠️ VERIFY FIRST: is `builtin.skill_install` actually composed and
   activatable in `ironclaw-reborn serve` under our local profile? Skills have
   no HTTP route (`docs/desktop/dashboard-gaps.md`), so confirm the install
   path end-to-end against the running gateway before building UI. The Phase 4
   lesson applies: a green suite can coexist with a capability the agent
   cannot actually reach — drive the real runtime.
3. **Dedupe and cap.** Before proposing, list installed skills (find the
   listing seam — `skill_listing.rs` in the composition); a rejected draft is
   remembered so the same skill is not re-proposed every run; cap total
   self-learned skills (default 50).
4. **Model-quality constraint, documented not solved:** `LLM_BACKEND` is
   single-valued, so reflection runs on the same model as chat. Small local
   models draft poor skills. Surface a "skills learn better with a stronger
   model" hint when the active model is small; do NOT build multi-model
   routing now (`docs/desktop/llm-provider-selection.md` tracks that).

**7c — Skill import (third-party SKILL.md)**

1. Import from a local folder (URL import later): parse + validate
   frontmatter, normalize to the `ironclaw_skills` format, enforce the
   install-bundle limits (`MAX_INSTALL_BUNDLE_*`), show the full skill text
   for review, and install through the same approved path as 7b. Never
   install silently.
2. Treat skill text as **untrusted input** (prompt-injection surface).
   ⚠️ VERIFY what `ironclaw_skills/src/gating.rs` and the safety layer
   already do with skill content on install/activation, and state in the
   progress note what is and is not scanned.
3. Entry point: dashboard (settings surface is the right home for imports) +
   an "install this skill?" bubble confirmation.

**7d — Ambient watchers (event-driven proactivity)**

1. Opt-in signals, each individually toggled, all default OFF: foreground
   app/window title (extend the existing interaction/fullscreen poller from
   Phase 3), watched folders (`notify` crate), time-of-day patterns. No
   screen content capture, no audio persistence, nothing leaves the machine.
2. **v1 gate is rule-based**, not LLM-based: user-defined "when X happens,
   ask the agent to Y" rules that materialize a prompt on the ambient thread.
   An LLM "is this worth suggesting?" gate is v2 — on iGPU machines constant
   gate inference competes with the chat model (the Phase 3 perf rule
   applies).
3. Wake-word models (the outstanding Phase 6 item) later make voice a third
   sense; not a blocker — push-to-talk remains until they are recorded.

**Definition of done (Phase 7):** with ambient enabled, a scheduled automation
surfaces as a character suggestion; completing a multi-step chat task yields a
"want me to remember this?" prompt whose approval installs a skill that is
active in the next session; a third-party SKILL.md folder imports after
review; every surfacing respects the rate cap and quiet hours; a "Not now"
suppresses re-surfacing; all gates green and a manual smoke run on Windows.
