# Coordination triage workflow

The `Coordination triage` workflow produces a read-only snapshot of open upstream PRs against the exact upstream `main` and fork `dev` commits.

## What it checks

- exact PR head SHA and base SHA observed from GitHub metadata;
- whether the PR head is already an ancestor of fork `dev`;
- whether a non-mutating `git merge-tree` succeeds against upstream `main` and fork `dev`;
- stable patch-id plus identical merge-tree evidence for exact duplicate candidates;
- common changed files combined with shared closing issue or similar title as low-confidence overlap hints.

## What it never does

- does not checkout or execute upstream PR code;
- does not create branches or worktrees;
- does not push, merge, approve, label, hold, or close PRs/issues;
- does not resolve conflicts or reuse `rerere` automatically;
- does not treat an overlap hint as proof of duplication or a merge recommendation.

The Markdown summary appears in the Actions run. Full JSON and Markdown are retained as a 14-day artifact. Reports are snapshots: always recheck the exact head/base SHA before acting.

## Triggers

- every push to fork `dev`;
- manual `workflow_dispatch` when available;
- scheduled every 12 hours once the workflow is present on the repository default branch (GitHub schedules run from the default branch).

Until the workflow is promoted to the default branch, the reliable trigger is a push to `dev`. No additional permanent branch is created.

## Local run

Requires Git 2.38+ and GitHub CLI authenticated for public upstream metadata.

```bash
git remote add upstream https://github.com/Marvisak/VIctory.git
gh pr list --repo Marvisak/VIctory --state open --limit 3000 \
  --json number,title,url,isDraft,headRefOid,baseRefName,baseRefOid,closingIssuesReferences,author > prs.json
git fetch --no-tags upstream +refs/heads/main:refs/remotes/upstream/main
# Fetch refs/pull/<number>/head to refs/remotes/upstream/pr/<number> for each non-draft PR targeting main.
python -B -I -m unittest discover -s scripts -p 'test_coordination_triage.py' -v
python -B -I scripts/coordination_triage.py --repo . --prs prs.json \
  --upstream-main refs/remotes/upstream/main --fork-dev dev --out out
```

The report is evidence for agents and maintainers, not permission to merge. Use [Agent Coordination Desk #50](https://github.com/vanya890/VIctory/issues/50) for claims and [Upstream Coordination Index #11](https://github.com/vanya890/VIctory/issues/11) for canonical integration decisions.
