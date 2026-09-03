# ADR-350: fal.ai governance and spend approval are now actually enforced

- **Status**: Accepted
- **Date**: 2026-09-02
- **Deciders**: ruv
- **Owners**: RuView forecast training, security, and release maintainers
- **Tags**: forecasting, training, fal, rust, receipts, governance, cost-control
- **Parent**: ADR-349
- **Amends**: ADR-349
- **Supersedes**: None

## Decision

ADR-349 already specified two governance controls for the fal.ai hosted
path: an operator-verified dataset/schema/policy/recipe authorization, and
"an explicit local spend-approval record bound to the request digest and
maximum units." Neither was actually wired into the code path that builds a
submittable, fal-destined `TrainSpec`. A 9-agent code review of the merged
training stack (RuView PR #1766) confirmed this: `FalGovernanceVerifier`
existed but had zero callers outside its own unit tests, and no
spend-approval type existed in the codebase at all. This meant any caller
who could construct a bare `SourceState::synthetic(reference)` — a
self-asserted claim requiring no external sign-off — could reach a
real, hosted fal.ai submission with no governance verification and no
approved spend ceiling.

This ADR records that both controls are now load-bearing, not just
documented intent:

1. **Governance verification is now a construction precondition, not an
   optional check.** `TrainSpec::new_fal_synthetic` (`ruforecast-core`) now
   requires a `&VerifiedFalDataset` argument — the non-deserializable,
   non-constructible proof type that only `FalGovernanceVerifier::verify`
   can mint. Its digest is bound into the resulting `SyntheticFalContract`
   and the whole `TrainSpec`, so a fal-destined spec cannot be built, even
   accidentally, without a real operator-signed `SignedFalGovernanceReceipt`
   that was independently verified against the exact dataset digest, schema
   digest, recipe digest, seed, and policy in play.
2. **A captured governance receipt can no longer be replayed.**
   `FalGovernanceVerifier` now tracks every `receipt_id` it has successfully
   verified in an in-memory single-use guard and rejects a repeat with
   `ForecastError::ReceiptReplayed`. A request that fails verification for an
   unrelated reason (wrong schema, wrong principal, ...) does not burn the
   receipt, so a legitimate retry after a caller-side mistake still works.
3. **`synthetic_train_spec` no longer hardcodes `None` consent/DPA/export
   receipts for the fal-destined branch.** It now takes
   `fal_verification: Option<(&VerifiedFalDataset, &DataPolicy)>`; the
   caller-supplied `DataPolicy` must carry real receipts, because
   `FalGovernanceVerifier::verify` already requires `dpa_receipt` and
   `export_receipt` to be present for that exact policy before it will sign
   off. The local-only branch is unaffected and still needs no export
   receipts, since nothing leaves the operator's machine.
4. **A `SpendApproval` type now exists and is enforced.**
   `ReservedSyntheticSubmission::reserve` (`ruforecast-train::fal`) now
   requires a `SignedSpendApproval`, verified by a separate
   `SpendApprovalVerifier` against an independently configured signer
   allowlist (`RUVIEW_FAL_SPEND_SIGNERS`, distinct from the governance
   allowlist `RUVIEW_FAL_GOVERNANCE_SIGNERS` — a dataset/schema/recipe
   reviewer is not necessarily authorized to approve real spend). The
   approval is bound to the exact `request_digest` the reservation produces
   and must cover `budget.max_micro_usd`. Binding to `request_digest`
   required making the hosted job's `job_digest` a deterministic function of
   the approval's `approval_id` (previously a fresh random UUID chosen
   inside `reserve` itself, which no offline approval step could ever have
   known in advance); `preview_request_digest` lets an operator or approval
   tool reconstruct that exact digest ahead of the real submission,
   surfaced through a new `ruforecast fal plan` CLI subcommand.
5. **The Direct Server webhook now authenticates its caller.** `/train` and
   `/train/cancel` previously accepted any request with a well-formed
   `x-fal-request-id` header and a schema-valid body — nothing proved the
   caller was actually fal's routing layer. Both routes now require
   `Authorization: Bearer <RUVIEW_FAL_WEBHOOK_SECRET>`, checked in constant
   time, mirroring this workspace's existing `wifi-densepose-sensing-server`
   bearer-auth precedent.
6. **The Direct Server now force-fails a wedged job instead of holding its
   execution slot forever.** `train` wraps its `spawn_blocking` training
   call in a wall-clock `tokio::time::timeout` (the job's own
   `max_wall_time_seconds` plus a fixed grace period); on expiry it signals
   cooperative cancellation, transitions the job to `Failed`, and releases
   the single execution-slot permit this server enforces. The job-table
   `retain` pass was also fixed to prune expired `Running` entries, not only
   `Complete`/`Failed` ones, as defense in depth.
7. **Local `TrainingBudget` and hosted `HostedBudget` now share one
   cumulative artifact-byte cap** (`MAX_CUMULATIVE_ARTIFACT_BYTES`, four
   times `ruforecast_model::MAX_ARTIFACT_BYTES`, one per fixed artifact
   kind) instead of the hosted path silently capping at a quarter of what
   local accepted — previously a legitimately-sized completed hosted run
   could be rejected after real GPU spend already happened.
8. **Cooperative cancellation no longer permanently wedges a `job_id`.**
   `BurnTrainer::recover_existing` used to accept only "zero artifacts" or
   "all four artifacts" and fail closed on anything else — but cancellation
   deliberately commits exactly one artifact (the model-only checkpoint) and
   nothing else. That specific shape is now recognized as a safe "fresh
   start": the stale checkpoint is cleared via the existing
   `ArtifactStore::remove_job_outputs` and a retry trains from scratch. Any
   other partial combination — a shape this trainer's own cancellation path
   never produces — still fails closed as likely corruption.

Items 5–8 are not literally ADR-349 governance/spend text, but sit on the
same trust boundary FT-004 and FT-005 describe (non-escalating resource
caps, and cooperative/idempotent cancellation); they are recorded here
because they were found and fixed in the same review pass, for the same
reason: what ADR-349 specified was not fully what the code did.

## Requirements and acceptance

| ID | Requirement | Acceptance evidence | Current state |
|---|---|---|---|
| GS-001 | A fal-destined `TrainSpec` cannot be constructed without a `VerifiedFalDataset` minted by `FalGovernanceVerifier::verify` for the exact dataset/schema/recipe/seed/policy in play. | `ruforecast-core::split::tests::hosted_spec_accepts_only_explicit_synthetic_recipe`, `ruforecast-core::privacy::tests::verified_handle_requires_signature_and_exact_current_context` | **MEASURED** (unit-tested) |
| GS-002 | A governance receipt's `receipt_id` cannot be verified twice; a receipt that fails verification for an unrelated reason is not consumed. | `ruforecast-core::privacy::tests::verified_receipt_cannot_be_replayed`, `ruforecast-core::privacy::tests::rejected_verification_does_not_burn_the_receipt` | **MEASURED** |
| GS-003 | `synthetic_train_spec`'s fal-destined branch cannot build a `DataPolicy` with absent consent/DPA/export receipts. | `ruforecast-train::fal::tests::reserved` (exercises the real `Some((verified, policy))` path end to end via `hosted_reservation_cannot_exceed_source_retention` and friends) | **MEASURED** |
| GS-004 | `ReservedSyntheticSubmission::reserve` cannot build a hosted request without a `SignedSpendApproval` bound to the exact `request_digest` and covering `budget.max_micro_usd`, verified against an allowlist independent of the governance-receipt allowlist. | `ruforecast-train::fal::tests::reserved`, `preview_request_digest` round-trip in the same test | **MEASURED** |
| GS-005 | The Direct Server rejects `/train` and `/train/cancel` requests that do not carry the configured webhook bearer secret, checked in constant time. | `ruforecast-train::server::tests::train_requires_matching_webhook_secret`, `cancel_requires_matching_webhook_secret`, `constant_time_eq_matches_exact_bytes_only` | **MEASURED** |
| GS-006 | A training job that exceeds its wall-clock deadline is force-failed and its execution slot is reclaimed; the job table prunes expired `Running` entries. | `ruforecast-train::server::tests::expired_running_job_is_pruned_alongside_terminal_states` (timeout branch itself is exercised only by the 90-second `burn-cpu-rust-192` CI job's real training runs, not a fast unit test) | **PARTIALLY MEASURED** |
| GS-007 | Local and hosted artifact-byte budgets share one cumulative cap. | `ruforecast-train::config::tests::v1_checkpoint_contract_rejects_unimplemented_resume_claims`, `ruforecast-train::fal::tests::fal_result_enforces_cumulative_artifact_budget_boundary` | **MEASURED** |
| GS-008 | A cooperatively-cancelled job (checkpoint-only artifact state) recovers as a fresh start on retry instead of failing closed forever. | `ruforecast-train::runner::tests::cancelled_only_checkpoint_recovers_as_a_fresh_start` | **MEASURED** |

GS-001 through GS-004 close FT-001/FT-004's code-level gap; GS-005 through
GS-008 close analogous code-level gaps under FT-002/FT-004/FT-005. None of
these change ADR-349's own **OPEN / UNMEASURED** verdict on FT-004/FT-005 as
a whole: real fal.ai execution, provider cancellation, and cost
reconciliation still need the acceptance evidence ADR-349 already
describes, which this ADR does not supply.

## Context

RuView PR #1766 ("feat(forecast): add independent Rust multivariate
training stack") added this crate family directly under
`v2/crates/ruview-forecast-{core,model,train}` and merged
2026-09-02T03:35:43Z. A follow-up commit on the same branch, before merge,
extracted it into this standalone repository
(`github.com/ruvnet/RuForecast`), vendored back into RuView as a git
submodule at `v2/crates/ruforecast` and renamed to
`ruforecast-{core,model,train}`. A 9-agent review swarm evaluated the
pre-extraction code and reported 14 findings against the old paths/names;
this repository is where those findings were actually triaged and fixed,
since the code moved here before the review's findings could be acted on.

Four of the 14 findings shared this one root cause — governance specified
but not enforced — and are grouped into this single ADR rather than four
separate ones, per this ADR's own precedent of grouping related decisions.

## Consequences

A fal-destined `TrainSpec` cannot be constructed, and a hosted reservation
cannot be built, without both an operator-verified governance receipt and
an operator-verified spend approval that are cryptographically bound to the
exact request in question. This is strictly more restrictive than the code
this ADR replaces: any caller or test that was previously building a
fal-destined spec from a bare `SourceState::synthetic(...)` with no
verification must now go through `FalGovernanceVerifier::verify` (and, to
actually reserve a submission, `SpendApprovalVerifier::verify`) first. The
CLI's `fal submit` reflects this: it now requires `--governance-policy`,
`--governance-receipt`, and `--spend-approval` file arguments in addition
to its existing budget flags, and a new `fal plan` subcommand exists solely
to let an operator compute the exact digest a spend approval must be signed
for, ahead of a real submission. This repository does not yet include a
tool that mints a `SignedFalGovernanceReceipt` or `SignedSpendApproval` in
the first place — verification-only was the existing design for governance
receipts (`FalGovernanceVerifier` was already verify-only) and this ADR
keeps spend approvals symmetric with that; minting is deliberately a
separate, out-of-band operator ceremony, not something this crate family
performs.

FT-004 and FT-005 in ADR-349's requirements table remain **OPEN /
UNMEASURED** exactly as before: this ADR closes the code-level gap between
what ADR-349 specified and what the code actually enforced, but it does not
supply the real fal.ai run, cancellation drill, or cost-reconciliation
evidence those requirements still call for.

## References

- [ADR-349](./ADR-349-governed-local-and-fal-forecast-training.md)
- [`ruforecast-core` privacy/governance](../../crates/ruforecast-core/src/privacy.rs)
- [`ruforecast-core` split/TrainSpec](../../crates/ruforecast-core/src/split.rs)
- [`ruforecast-train` fal client and SpendApproval](../../crates/ruforecast-train/src/fal.rs)
- [`ruforecast-train` Direct Server](../../crates/ruforecast-train/src/server.rs)
- [`ruforecast-train` local runner](../../crates/ruforecast-train/src/runner.rs)
