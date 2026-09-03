# Changelog

All notable changes to RuForecast are documented here.

## [0.2.0] - 2026-09-03

Fixes from a 9-agent code review of the fal.ai-hosted training path (review
of ruvnet/wifi-densepose PR #1766), addressed in PR #2:

- **Governance enforcement (ADR-350)**: `TrainSpec::new_fal_synthetic` now
  requires a `VerifiedFalDataset` minted only by `FalGovernanceVerifier`,
  closing a gap where the ADR-349 governance system existed but was never
  actually called by any real submission path.
- **Replay protection**: `FalGovernanceVerifier` now tracks used
  `receipt_id`s (single-use), closing a gap where a captured valid signed
  receipt could be replayed indefinitely.
- **Spend approval**: new `SpendApproval`/`SignedSpendApproval` type, bound
  to the exact request digest and maximum spend, enforced by
  `ReservedSyntheticSubmission::reserve`; adds a `fal plan` CLI subcommand
  so an operator can compute and sign the approval digest offline.
- **Server authentication**: the Direct Server's `/train` and
  `/train/cancel` endpoints now require a bearer secret, closing a gap
  where any reachable caller could trigger real GPU training.
- **Metrics correctness**: `weighted_quantile_loss`, `mae`, `pinball_loss`,
  `interval_coverage`, and `TimeSeries::new` now only validate finiteness
  at *observed* indices, so real partially-observed windows (WiFi CSI
  dropout, the normal case) no longer fail evaluation outright.
- **Artifact budget**: local and hosted cumulative artifact-byte budgets
  now share one constant, so a legitimately-sized completed hosted run is
  no longer rejected after real GPU spend.
- **Server reliability**: wedged training jobs now hit a real wall-clock
  timeout and release their execution slot instead of hanging the worker
  indefinitely; a cancelled job's partial artifact state is now
  recoverable as a fresh start instead of being stuck permanently.
- **CI repaired**: `ruforecast-ci.yml` had been silently non-triggering on
  every push since this repo's extraction (stale path filters); it now
  actually runs, across both pinned toolchains (1.89, 1.92).

Full technical record: [ADR-350](docs/adr/ADR-350-fal-governance-and-spend-enforcement.md).

Deferred: splitting six files that exceed the 500-line convention
(`fal.rs`, `runner.rs`, `config.rs`, `split.rs`, `artifact.rs`,
`ruvector_adapter.rs`) — flagged as a follow-up, not bundled with these
fixes to keep the security-relevant diff reviewable.

## [0.1.0] - 2026-09-02

Initial extraction from the wifi-densepose monorepo (formerly
`v2/crates/ruview-forecast-{core,model,train}`, PR ruvnet/wifi-densepose#1766).
See docs/benchmarks/ruforecast.md for the current, honest accuracy-research
status — no configuration has yet been shown to reliably beat trivial
forecasting baselines out-of-sample at the current synthetic-fixture scale.
This is active, ongoing research, not a finished result. A follow-up
zero-shot evaluation using pretrained public checkpoints (Chronos, TimesFM)
did beat naive baselines decisively — see
[ruvnet/RuView#1771](https://github.com/ruvnet/RuView/pull/1771) and
`docs/benchmarks/ruforecast.md`'s "Zero-shot pretrained-checkpoint
comparison" section for the real, measured result.
