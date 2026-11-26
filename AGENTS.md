# Agent instructions

Read this file before changing code, opening issues, or creating pull requests.

## 1. Coordination is mandatory

The coordination system has two canonical entry points:

- [Agent Coordination Desk (#50)](https://github.com/vanya890/VIctory/issues/50): task queue, claims, blockers, handoffs, and ownership.
- [Upstream Coordination Index (#11)](https://github.com/vanya890/VIctory/issues/11): upstream findings, dependencies, import decisions, and integration status.

Before starting work:

1. Search the open issues and the two coordination issues for an existing task covering the same root cause.
2. Read recent comments and check whether another agent has claimed overlapping files or behavior.
3. Claim exactly one existing `TASK-ID` in #50. Include your agent identifier, precise scope, files/functions, and expected output.
4. If no canonical issue exists, create one focused issue only after checking for duplicates; then add its `TASK-ID` to #50.
5. Do not begin overlapping implementation while another active claim owns that scope. Coordinate a handoff first.

Use these status words consistently:

- `claiming`: work has started and scope is reserved.
- `finding`: evidence-backed result or newly discovered risk.
- `blocked`: progress requires a named dependency or decision.
- `handoff`: work is ready for another named role/agent.
- `done`: the promised output is complete; link the canonical result.
- `release`: the claim is relinquished without completion.

A completed research task does not mean its implementation PR is merged. Track implementation separately.

## 2. Keep the repository and issue tracker clean

- One canonical issue per root cause. Do not create one issue per agent, experiment, log line, or minor subcase.
- Keep #50 as a compact dispatch board. Put technical evidence in the relevant canonical issue or PR, then link it from #50.
- Keep #11 as an index, not a second copy of every technical report.
- Prefer updating an existing issue over creating a near-duplicate.
- Do not paste full CI logs, repeated benchmark tables, or entire upstream descriptions into multiple issues. Summarize and link the source.
- Separate confirmed facts, reproductions, hypotheses, and recommendations.
- Close an issue only when its tracked result is integrated or explicitly abandoned. A green upstream CI run is not proof that the fork has integrated the change.
- Do not claim that a task is complete while a required test, rebase, or integration step remains.

## 3. Branch and pull-request policy

There are two permanent branches:

- `main`: upstream-aligned baseline.
- `dev`: integration branch for accepted work.

All feature/import branches are temporary.

- Never push implementation changes directly to `main`.
- Start a focused short-lived branch from the latest `dev`; target PRs at `dev`.
- Before opening or updating a PR, record the exact base and head SHA. If `dev` moves, re-check the diff and update/rebase the branch.
- Do not keep long-lived per-upstream import branches. Do not create a new branch when a focused PR branch already exists and can be safely reused.
- One PR should solve one coherent problem. Split unrelated diagnostics, refactors, and behavior changes unless they are inseparable.
- When two PRs touch the same file or invariant, serialize their integration: merge/rebase one, then re-review the second against the new combined tree.
- After merge, delete the temporary branch when permissions and tooling allow. Do not delete a branch before confirming its PR is merged and no other work depends on it.
- Do not merge on behalf of the project owner unless explicitly assigned that authority. Provide a merge recommendation with evidence instead.

## 4. Upstream import procedure

For every upstream issue or PR:

1. Verify the current upstream state, exact head SHA, base SHA, changed files, and whether it is merged, closed, superseded, or still active.
2. Read the PR description, review comments, requested changes, and later follow-up commits. A stale green CI result does not validate a newer head.
3. Compare the candidate with the fork's current `dev`, not only with fork `main` or upstream `main`.
4. Identify dependencies, overlapping changes, API/ABI effects, and any behavior intentionally omitted by the upstream author.
5. Check whether an equivalent fix is already present in the fork.
6. Port only the smallest coherent change. Do not stack alternative implementations or reserve the same resource range twice.
7. Preserve upstream attribution and link the source commit/PR in the fork PR.
8. Add or retain regression tests for the actual failure mode, including negative/boundary cases.
9. Re-run checks on the final fork PR head after every rebase or merge that changes the combined tree.

Do not cherry-pick a large integration PR merely because it makes a title reach a scene. First isolate its dependencies and independently review each subsystem.


### Workflow security

- Never execute upstream PR/build/test code in a job that has repository write permissions, secrets, or a token capable of mutating refs/issues/PRs.
- Keep scheduled triage read-only: fetch metadata and Git objects, compute merge trees/patch IDs, and publish reports only. Do not execute candidate code for overlap detection.
- If a separate publisher is ever introduced, isolate it from candidate execution and independently verify pinned candidate heads, artifacts, and expected refs. Do not trust reports or bundles produced in a candidate-controlled workspace without validation.
- Do not automate merge, approval, hold, label, issue closure, or PR closure. Human review remains mandatory.

## 5. Validation and evidence

The repository CI workflow runs on Windows and checks:

- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- `cargo test --workspace --locked`

The workspace tests include the repository asset-policy check. Follow [docs/asset-policy.md](docs/asset-policy.md) and [docs/architecture.md](docs/architecture.md).

For GPU/D3D12 changes, use targeted tests and, where available:

- hardware adapter with default features;
- hardware with diagnostics and D3D12 debug layer;
- Microsoft WARP with diagnostics and D3D12 debug layer;
- a second vendor where the behavior is vendor-dependent.

For every result, state the exact commit SHA, command, OS, adapter/GPU, driver where relevant, and passed/failed/ignored counts. Distinguish GitHub Actions CI from local hardware tests. Never say a test passed if it was not run.

A microbenchmark is not an FPS result. Do not extrapolate per-page, allocator, shader, or copy timings into game frame-rate claims without a measured same-scene comparison.

## 6. Correctness rules for low-level changes

Pay particular attention to:

- unsafe pointer validity, lifetime, alignment, aliasing, and readable/writable range contracts;
- integer overflow and bounds before pointer arithmetic or D3D12 calls;
- COM/resource lifetime through command recording and execution;
- descriptor heap bounds, null slots, dynamic ranges, and page-table reservations;
- lock ordering, re-entrancy, concurrent Release/eviction, and snapshot-before-unprotect invariants;
- subresource, mip, array/depth, plane-count, block-compression, and copy-footprint semantics;
- shader cache keys and all translation options that affect generated DXIL.

An upstream author's safety comment is a claim to verify against every fork call site, not proof by itself.

## 7. Asset and data policy

Nothing from GTA VI or another proprietary title may be committed. Do not add executables, game files, extracted assets, shader dumps, screenshots, decompiled/disassembled code, or data derived from them. Use synthetic fixtures and user-supplied local paths. The only existing exception is documented in [docs/asset-policy.md](docs/asset-policy.md); agents must not create new exceptions.

## 8. Required handoff format

Every completed task must leave a concise comment in its canonical issue containing:

- upstream issue/PR and exact commit SHA;
- fork `main`/`dev` status and relevant file/function comparison;
- changed files and behavior reviewed;
- tests with exact command, platform/GPU/WARP, and counts;
- unresolved review objections, risks, and dependencies;
- verdict: `ready to port`, `needs changes`, `blocked`, or `already present`;
- link to the fork PR or the next owner/action.

Then add one short `done`, `blocked`, or `handoff` entry to #50. Do not duplicate the full technical report there.
