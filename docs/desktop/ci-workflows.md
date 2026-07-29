# CI on the fork: which workflows run, and why the rest are switched off

The fork inherited **21 workflows** from `nearai/ironclaw`. On the public repo
(`Anubhav-Koul/OfflineOS`) twelve of them are **disabled at the GitHub API
level**, not in this tree. This file is the only record of that, so read it
before wondering why `coverage.yml` exists and never runs.

**Why disabled through the API rather than by editing the files:** golden rule #1
— the fork is additive and does not edit upstream files without cause, so that
merges stay cheap. Adding an `if: github.repository == 'nearai/ironclaw'` guard
to every job of twelve workflows would be a large, permanently-conflicting diff
against files we otherwise never touch. Disabling leaves them byte-identical to
upstream and is reversible in one call. The cost is that the decision is
invisible in the repo — which this file pays.

Re-enable any of them with:

```bash
gh api -X PUT repos/Anubhav-Koul/OfflineOS/actions/workflows/<id>/enable
gh api repos/Anubhav-Koul/OfflineOS/actions/workflows --jq '.workflows[] | [.id,.state,.name] | @tsv'
```

## What still runs

| Workflow | Why it stays |
|---|---|
| **`desktop-ci`** | **The fork's gate.** The only one that speaks for this project: fork crates + the serve path, on Windows, plus the core-patch and supply-chain jobs. |
| `Replay Snapshot Gate`, `E2E Tests`, `Reborn E2E`, `Reborn Integration…` | `workflow_call` only — inert unless something calls them. |
| `Regression Test Check`, `PR: Classify`, `PR: Scope Labels`, `Claude Code Review` | Pull-request only. They fail on Dependabot PRs for want of upstream secrets and app installs; noisy but harmless, and left on pending a decision. |
| `Dependabot Updates` | Useful here. |

## What is disabled, and the actual reason

Recorded 2026-07-29, from the failures on the first two pushes to the public
repo (runs `30446674347`, `30446674516`, `30446674311`).

### Red on `main` — three independent causes, none of them a defect in fork code

`Code Coverage`, `Run Tests`, `Code Style`.

1. **`alsa-sys` fails to build on the Ubuntu runners.** `ic_voice` depends on
   `cpal`, which on Linux needs `libasound2-dev`; upstream's Ubuntu jobs never
   installed it because upstream has no audio crate. This is the fork's
   *dependency*, but not a fork bug — needing ALSA headers to build an audio
   crate on Linux is ordinary. It is only a failure because upstream's workflows
   build `--workspace`, which now includes ours.
2. **`package ironclaw_reborn does not have feature libsql`.** Every
   `libsql-only` job, on Linux *and* Windows. Upstream's own flag/feature
   mismatch: the root `ironclaw` package has a `libsql` feature and
   `crates/ironclaw_reborn` does not, so `--workspace --features libsql` errors.
   Reproduces locally at HEAD, and `crates/ironclaw_reborn/Cargo.toml` is
   untouched by the fork — pre-existing upstream breakage.
3. **`tokio_postgres::config::Host::Unix` E0599** in
   `ironclaw_reborn_event_store`, Windows clippy. Upstream portability bug, of a
   piece with the other pre-existing Windows failures noted in `core-patches.md`.

And a fourth reason that makes `Code Coverage` unfixable here regardless of the
three above: it uploads to **`codecov.io/gh/nearai/ironclaw`** with
`use_oidc: true` and `fail_ci_if_error: true`. A fork has no OIDC trust with
upstream's Codecov project, so the upload step fails even on a fully green
build — which is exactly what happened to the `E2E Coverage` job, whose only
failing step was `Upload to Codecov`.

This is not "hiding failures". These workflows measure a different project:
`CLAUDE.md` and `desktop-ci.yml` have both said from Phase 0 that the fork gate
is deliberately narrow *"so pre-existing upstream lints do not fail the fork
gate"*. Publishing the repo simply made that policy visible, and it had never
been carried into the workflow files because there was no public repo to carry
it into.

### Release and publish automation

`Release-plz`, `Release`, `Rebuild Release Image`, `Docker Image`.

These point at upstream's crates.io and container registries. **`Release-plz`
triggers on `push: branches: [main]`** — it fired on both of our pushes and only
`skipped` rather than acting. Release automation belonging to another project,
firing on our default branch, is the kind of thing to switch off before it finds
a code path where it does not skip, not after.

### Scheduled crons against upstream infrastructure

`Live Canary` (every 6 hours), `Nightly Deep CI` (daily 04:00 UTC), `Nightly E2E`
(daily 03:00 UTC), `nearai-bench`, `nearai-bench dispatcher tests`.

These would have run forever, needing upstream's secrets and infrastructure,
burning Actions minutes and posting a failure every few hours on a repo whose
own gate is elsewhere.

## If the fork ever wants these signals back

Nothing here says full-workspace testing is worthless — only that upstream's
workflows cannot deliver it on this fork. A fork-owned equivalent would need to,
in rough order of effort: install `libasound2-dev` on Linux jobs; drop or fix the
`--features libsql` matrix leg; point coverage at a Codecov project this repo
owns, or drop the upload; and accept that the Windows legs stay red until the
upstream `Host::Unix` bug is fixed. The merge rehearsal
(`PROGRESS.md`, still open) is the natural moment to decide, because it is the
first time the fork will care what upstream's suite says.
