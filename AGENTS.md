# Agent instructions

Read this file before changing code, opening issues, creating pull requests, reviewing changes, or merging branches.


## 0. Operating modes and autonomous authority

The repository supports both coordinated multi-agent work and single-agent operation. The default is coordinated swarm mode whenever multiple agents are actively contributing. A single-agent fallback is available when only one agent is actually available; never simulate a swarm or fabricate independent reviewers.

### 0.1 Coordinated swarm mode

- Agents may independently claim bounded tasks, investigate upstream solutions, implement changes, open PRs, and integrate work through the normal branch policy.
- Agents MUST communicate through the canonical issues and PR discussions. They should read and comment on relevant work by other agents, challenge assumptions, identify counterexamples, and debate technical disagreements using code references, reproducible evidence, and tests.
- A PR author must request substantive review from an agent who did not author or implement the change. Reviewers must inspect the actual diff and relevant call paths, not merely approve the description.
- Reviewers should explicitly record findings, questions, risks, test gaps, and a final verdict. The author must answer each material finding and update the code or explain with evidence why no change is needed.
- Reviews must be genuinely independent in authorship and analysis. Separate usernames, copied comments, ceremonial approvals, or an agent reviewing its own work do not count as independent review.
- Reviewers are encouraged to disagree. Resolve disagreements through source inspection, upstream evidence, targeted tests, and documented reasoning, not hierarchy or vote-counting.
- Agents may merge qualifying PRs themselves after the required peer reviews, CI, self-audit, and all other gates pass. The PR author may perform the merge; peer review is a quality gate, not a transfer of merge ownership.
- For high-risk changes involving unsafe code, concurrency, memory/resource lifetime, security boundaries, GPU synchronization, data integrity, or privileged CI/workflows, require two substantive independent peer reviews where two genuinely available reviewers exist. If fewer reviewers are available, use the documented single-agent fallback only when the project is genuinely operating with one available agent; otherwise keep the PR blocked.

### 0.2 Single-agent fallback

- If only one agent is available to perform the work, that agent may implement, inspect, test, self-audit, update PRs, and merge qualifying PRs without inventing or waiting for unavailable reviewers.
- Before using this fallback, record in the PR that the repository is operating in single-agent mode and that no independent reviewer was available. Do not claim that self-review is independent review.
- The solo agent must perform a second-pass adversarial audit after implementation: re-read the complete final diff from a clean perspective, trace affected call paths, enumerate failure modes and boundary cases, inspect upstream/related PR alternatives, and run every applicable check. Record the exact SHA, commands, results, limitations, and residual risks.
- A solo agent must not weaken CI, skip applicable tests, bypass branch protections, fabricate approvals, or use the fallback merely to avoid inconvenient peer feedback.
- If another agent becomes available before merge, return to swarm mode and obtain the required peer review.

### 0.3 Owner authority and truthfulness

- The project owner sets priorities and may explicitly change this governance policy. Routine implementation and integration decisions are delegated to participating agents under the gates below.
- Agents are authorized to merge qualifying PRs into `dev` and to promote a validated `dev` snapshot to `main` when the applicable swarm or solo gates are satisfied.
- Technical validation, exact-SHA checks, security boundaries, branch protections, and truthful reporting remain mandatory. Owner authorization does not justify fabricated CI results, false test claims, secret exposure, or bypassing GitHub protections.
- If an actual GitHub rule or permission prevents an authorized operation, report that concrete restriction. Do not claim that conversational authorization changed repository permissions.

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
6. In swarm mode, actively review relevant PRs/issues owned by other agents. Do not restrict participation to your own assigned task: report concrete risks, missing tests, upstream alternatives, and integration conflicts.
7. For every PR, request peer review according to section 4. A reviewer must be different from the author/implementer and must leave a substantive written review. For high-risk changes, seek two independent reviews when two reviewers are available.
8. If only one agent is available, explicitly use the single-agent fallback in section 0.2 and complete its adversarial second-pass audit.

Use these status words consistently:

- `claiming`: work has started and scope is reserved.
- `finding`: evidence-backed result or newly discovered risk.
- `blocked`: progress requires a named dependency or decision.
- `handoff`: work is ready for another named role/agent.
- `done`: the promised output is complete; link the canonical result.
- `release`: the claim is relinquished without completion.

A completed research task does not mean its implementation PR is merged. Track implementation, peer review or solo self-audit, validation, and integration separately.

## 2. Keep the repository and issue tracker clean

- One canonical issue per root cause. Do not create one issue per agent, experiment, log line, or minor subcase.
- Keep #50 as a compact dispatch board. Put technical evidence in the relevant canonical issue or PR, then link it from #50.
- Keep #11 as an index, not a second copy of every technical report.
- Prefer updating an existing issue over creating a near-duplicate.
- Do not paste full CI logs, repeated benchmark tables, or entire upstream descriptions into multiple issues. Summarize and link the source.
- Separate confirmed facts, reproductions, hypotheses, and recommendations.
- Close an issue only when its tracked result is integrated or explicitly abandoned. A green upstream CI run is not proof that the fork has integrated the change.
- Do not claim that a task is complete while a required test, review, rebase, or integration step remains.
- Author self-audits and required peer-review comments in swarm mode must identify concrete code paths, invariants, failure modes, and evidence. Generic approvals such as "looks good" are not meaningful review.

## 3. Branch and pull-request policy

There are exactly two permanent branches:

- `main`: stable, release-ready branch.
- `dev`: active integration branch for accepted work.

All feature, fix, experiment, and upstream-import branches are temporary.

- Never push implementation changes directly to `main` or `dev`. Use a focused pull request.
- Start a short-lived branch from the latest `dev`; target normal implementation PRs at `dev`.
- Record the exact base and head SHA before opening or updating a PR. If the base moves, rebase or merge the new base, inspect the complete resulting diff, and rerun all affected checks.
- Do not keep long-lived per-upstream import branches. Do not create a new branch when a focused PR branch already exists and can be safely reused.
- One PR should solve one coherent problem. Split unrelated diagnostics, refactors, and behavior changes unless they are inseparable.
- When two PRs touch the same file, subsystem, or invariant, serialize their integration: merge/rebase one, then re-review the second against the new combined tree.
- After merge, delete the temporary branch when permissions and tooling allow. Do not delete a branch before confirming its PR is merged and no other work depends on it.
- Agents ARE AUTHORIZED to merge qualifying PRs into `dev` and to promote `dev` into `main`, but only after satisfying the gates in sections 4 and 5. Do not interpret the previous human-only merge restriction as active; it is superseded by this policy.
- In single-operator mode, the PR author/implementer may perform the final merge after completing and documenting a critical self-audit and satisfying all applicable validation gates. Do not invent independent reviewers or claim that a self-review is independent.
- Use GitHub's PR merge operation and supply the exact reviewed head SHA as the expected head. Never force-push a permanent branch or bypass branch protection, required checks, required reviews, or repository rules.
- If the agent lacks write permission or GitHub protection rejects the merge, do not work around the restriction. Report the exact blocker and hand off to an authorized agent/maintainer.

## 4. Self-audit and integration gates

The goal is to catch defects through careful inspection and reproducible validation, not to collect ceremonial approvals.

### 4.1 Required technical audit and peer review

Before merging an implementation PR into `dev`, the PR must receive both a technical self-audit by its author and the applicable independent peer review:

**Author self-audit (always required):**
1. Inspect the complete diff at the exact current head SHA, not only the PR description or its own summary.
2. Trace changed functions through relevant call sites, ownership/lifetime rules, error paths, concurrency behavior, and compatibility boundaries.
3. Actively search for counterexamples, boundary cases, races, resource exhaustion, malformed input, and tests that could pass while the implementation remains incorrect.
4. Check whether equivalent fixes or regressions already exist in upstream or other fork PRs.
5. Record the exact head SHA, files/functions inspected, commands actually run, findings, test gaps, and residual risks in the PR.
6. Fix blocking findings and repeat the affected inspection and validation after every change.

**Independent peer review (required in swarm mode):**
- At least one agent who did not author or implement the change must inspect the complete diff and relevant surrounding code, then submit a substantive review.
- For high-risk changes involving unsafe code, concurrency, memory/resource lifetime, security boundaries, GPU synchronization, data integrity, or privileged CI/workflows, require two independent substantive reviews when two genuinely available reviewers exist.
- Reviewers must challenge the implementation, not merely confirm style. They should examine correctness invariants, race/deadlock possibilities, failure recovery, compatibility, tests, and whether an existing upstream solution is safer.
- The PR author must respond to every material finding. Unresolved blocking findings or unresolved review threads prevent merge.
- A reviewer must not approve work they authored or materially implemented. Do not count duplicate or coordinated rubber-stamp reviews as independent.
- Record reviewer identity, review scope, concrete findings, and verdict in the PR. Never fabricate reviewer identities, separate agent instances, or approvals.

**Single-agent fallback:**
- When only one agent is genuinely available, an independent review cannot be manufactured. Record that fact and complete the adversarial second-pass audit described in section 0.2.
- A solo self-audit is not an independent review and must never be labelled as one.
- If a second agent becomes available before merge, obtain the normal peer review rather than relying on the fallback.

Technical disagreements must be resolved through evidence and targeted tests, not vote-counting.

### 4.2 Required validation for every PR

A PR is merge-ready only when all of the following are true:

- The diff is focused, based on the current target branch, and free of unexplained generated files, binaries, proprietary assets, or unrelated changes.
- The exact current PR head has completed every applicable required CI check successfully. Earlier green runs do not validate a newer commit.
- The final combined tree has been checked for conflicts and interactions with already merged or queued changes.
- Formatting is a required exact-head check: run `cargo fmt --all -- --check` after the final commit. If the current CI workflow does not run it, execute it separately and record the exact SHA, command, and result in the PR. Clippy is not a substitute for rustfmt.
- Required unit, integration, regression, policy, formatting, static-analysis, and platform-specific checks have been run. A skipped check is not evidence that the underlying behavior works.
- Tests cover the reported failure, normal behavior, negative cases, boundaries, and relevant failure/recovery paths.
- The self-audit in section 4.1 is complete, all blocking findings are resolved, and the final head has been re-inspected after fixes.
- The PR has no unresolved review threads or unexplained failing/pending checks.
- The mergeability state and exact base/head SHAs have been rechecked immediately before merging.

For this repository, the normal Windows CI gate includes:

- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- `cargo test --workspace --locked`

The workspace tests include the repository asset-policy check. Follow [docs/asset-policy.md](docs/asset-policy.md) and [docs/architecture.md](docs/architecture.md). Do not add Linux CI as a merge blocker while the repository's current development policy is Windows-only.

### 4.3 Evidence quality and risk-based testing

- For every result, state the exact commit SHA, command, OS, adapter/GPU, driver where relevant, and passed/failed/ignored counts. Distinguish GitHub Actions CI from local hardware tests.
- For GPU/D3D12 changes, use targeted tests and, where available:
  - hardware adapter with default features;
  - hardware with diagnostics and D3D12 debug layer;
  - Microsoft WARP with diagnostics and D3D12 debug layer;
  - a second vendor where the behavior is vendor-dependent.
- Hardware-dependent claims require hardware evidence. If the required device is unavailable, state that explicitly and keep the change out of `main` until the evidence is obtained. A `dev` merge is allowed only when the change is safely testable without that hardware or is explicitly marked experimental/blocked from release; do not describe it as hardware-validated.
- A microbenchmark is not an FPS result. Do not extrapolate per-page, allocator, shader, or copy timings into game frame-rate claims without a measured same-scene comparison.
- Never say a test passed if it was not run. Never infer a successful run from a green status belonging to a different SHA.

### 4.4 Permission to merge into dev

An AI agent MAY merge a PR into `dev` without waiting for the project owner when every applicable requirement in sections 4.1–4.3 is satisfied, including independent peer review in swarm mode or the documented adversarial self-audit fallback in genuine single-agent mode.

Before the merge:

Before merging, if no run exists for the exact candidate SHA:

1. Prefer the repository CI workflow through `workflow_dispatch` against the exact candidate ref when the available GitHub interface supports it. Verify the run's `head_sha` matches the reviewed candidate.
2. If direct dispatch is unavailable, use the confirmed AgentSharedMemory bridge:
   - Create an Issue in `vanya890/AgentSharedMemory` with a title beginning `[agent-ci]`.
   - The Issue body must be exactly a JSON object with `pr` (VIctory PR number) and `sha` (full 40-character SHA).
   - The bridge validates that the PR targets `dev` or `main`, belongs to `vanya890/VIctory`, and that the SHA matches either the current head of an open PR or the exact merge commit of an already merged PR.
   - It calls the reusable Windows CI in `vanya890/VIctory/.github/workflows/ci.yml` against the verified ref and base branch, then posts the run URL and result to the Issue.
   - Wait for the final success comment and inspect the run. A memory-repository workflow is valid evidence only when its validation log proves the exact target SHA/ref and all required CI steps pass.
3. Never treat an earlier SHA, another branch, or an unverified workflow run as a pass. If neither direct dispatch nor the confirmed bridge is available, do not bypass the validation gate.

1. Re-fetch the PR and confirm it is open, not draft, mergeable, and targets `dev`.
2. Re-check the exact head SHA, base SHA, peer review results (or documented solo-mode justification), unresolved threads, and CI status.
3. Confirm no newer commit was pushed after the final reviews and successful checks. Any new commit invalidates reviews of affected code and requires re-review.
4. Merge using the exact reviewed head SHA as the expected head. If it changed, stop and repeat the affected review and validation.
5. After merging, verify the resulting `dev` commit and run/inspect the post-merge CI on that resulting commit. If post-merge CI fails, investigate immediately, stop further dependent merges, and fix or revert with a documented reason.

The author or another authorized participating agent may perform the merge. Do not ask the owner for routine permission when all gates pass. Never bypass repository rules or required checks.
 
## 5. Release gate: promoting dev into main

Promotion to `main` is a deliberate release operation. It requires the complete release self-audit and the applicable peer-review gate; it does not require ceremonial approval from the project owner.

- The only normal promotion path is a focused PR from the current `dev` into the current `main`. Do not merge an individual feature branch directly into `main`.
- Promote a validated snapshot of `dev`; do not combine unrelated, unfinished work merely to make a release.
- The coordinating agent must perform and document a release self-audit of the complete `main...dev` diff and commit graph, including main-only commits, accidental scope expansion, critical invariants, unsafe code, resource lifetime, concurrency, data integrity, and failure handling.
- In swarm mode, the release PR requires at least one substantive independent peer review; require two independent reviews when the release includes high-risk changes and two reviewers are available. Reviewers must inspect the complete release diff and commit graph, not just the PR summary.
- In genuine single-agent mode, use the explicit solo fallback from section 0.2, including the adversarial second-pass release audit. Do not claim this is peer review.
- All P0/P1 findings must be resolved. P2/P3 residual risks must be explicitly documented with rationale and follow-up issue where appropriate.
- The complete applicable CI suite must pass on the exact release candidate. Re-run after any rebase, new commit, conflict resolution, or candidate-tree change.
- All hardware-dependent changes included in the release must have required real-hardware evidence. WARP or synthetic tests alone do not prove vendor hardware behavior.
- Verify that `dev` contains no accidental proprietary data, forbidden assets, debug dumps, local credentials, secrets, or unsafe workflow/security changes.
- Identify main-only commits and preserve them. Do not overwrite or discard changes that exist only on `main`.
- Confirm release notes accurately describe included changes, known limitations, and validation evidence.

An authorized agent may merge the `dev` → `main` PR when this release self-audit, applicable peer-review or solo fallback, validation, and actual GitHub repository protections are satisfied.

After promotion, verify the new `main` SHA and its CI. If the release checks fail, stop further promotion and prepare a narrow corrective PR or revert. Do not silently leave `main` red.

## 6. Upstream import procedure

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
10. Apply the review and merge gates in sections 4 and 5. An upstream approval or green upstream CI is supporting evidence, not a substitute for fork review and validation.

Do not cherry-pick a large integration PR merely because it makes a title reach a scene. First isolate its dependencies and independently review each subsystem.

## 7. Workflow security

- Never execute upstream PR/build/test code in a job that has repository write permissions, secrets, or a token capable of mutating refs/issues/PRs.
- Keep CI and scheduled triage read-only: fetch metadata and Git objects, compute merge trees/patch IDs, run candidate tests in isolated jobs without write credentials, and publish reports only where safe.
- Candidate-controlled code must not receive repository secrets, privileged tokens, or write access. Do not use `pull_request_target` to check out and execute untrusted PR code with elevated permissions.
- Merge authorization in this file applies to the coordinating agent acting after it has completed and documented its self-audit and verified available evidence. It does NOT authorize candidate-controlled workflows to merge themselves, change branch protections, publish secrets, or bypass required checks.
- If a separate publisher or merge service is introduced, isolate it from candidate execution and independently verify pinned candidate heads, artifacts, and expected refs. Do not trust reports or bundles produced in a candidate-controlled workspace without validation.
- Never use force-push, admin bypass, disabling branch protection, fabricated reviews, or manually forged green statuses to get a merge through.
- In swarm mode, peer review is required as specified in section 4.1; in genuine single-agent mode, use the documented adversarial self-audit fallback. Human review may also be required by actual GitHub repository settings. The coordinating agent is explicitly authorized to make merge decisions under the applicable review, self-audit, and validation gates above.
- If repository rules require a human or a permission unavailable to agents, obey the rule and report the blocker. This policy does not grant GitHub permissions or override repository configuration.

## 8. Correctness rules for low-level changes

Pay particular attention to:

- unsafe pointer validity, lifetime, alignment, aliasing, and readable/writable range contracts;
- integer overflow and bounds before pointer arithmetic or D3D12 calls;
- COM/resource lifetime through command recording and execution;
- descriptor heap bounds, null slots, dynamic ranges, and page-table reservations;
- lock ordering, re-entrancy, concurrent Release/eviction, and snapshot-before-unprotect invariants;
- subresource, mip, array/depth, plane-count, block-compression, and copy-footprint semantics;
- shader cache keys and all translation options that affect generated DXIL.

An upstream author's safety comment is a claim to verify against every fork call site, not proof by itself.

## 9. Asset and data policy

Nothing from GTA VI or another proprietary title may be committed. Do not add executables, game files, extracted assets, shader dumps, screenshots, decompiled/disassembled code, or data derived from them. Use synthetic fixtures and user-supplied local paths. The only existing exception is documented in [docs/asset-policy.md](docs/asset-policy.md); agents must not create new exceptions.

## 10. Required handoff format

Every completed task must leave a concise comment in its canonical issue containing:

- upstream issue/PR and exact commit SHA;
- fork `main`/`dev` status and relevant file/function comparison;
- changed files and behavior reviewed;
- tests with exact command, platform/GPU/WARP, and counts;
- self-audit summary, exact audited head SHA, validation evidence, and links to findings;
- unresolved findings, risks, and dependencies;
- verdict: `ready to port`, `needs changes`, `blocked`, `merged to dev`, or `released to main`;
- link to the fork PR and the resulting merge commit SHA when merged.

Then add one short `done`, `blocked`, or `handoff` entry to #50. Do not duplicate the full technical report there.
