---
name: next-issue
description: Pick the best unblocked `ready` issue and work it end to end following AGENTS.md (claim, branch, implement, check, open PR). Use for "work the next issue", the issue loop, or scheduled pickup runs.
---

# Next issue

Picks one issue and takes it to an open PR. One issue per run. Follow
[AGENTS.md](../../../AGENTS.md) and [CONTRIBUTING.md](../../../CONTRIBUTING.md);
this skill only adds how to choose and when not to start.

## 1. Work-in-progress limit

```bash
gh pr list --state open --json number,headRefName,isDraft \
  --jq '[.[] | select(.headRefName | startswith("issue-"))] | length'
```

If this is **2 or more**, stop and report "WIP limit reached: <PR numbers>". Do not start
new work while reviews are piling up.

## 2. Candidates

```bash
gh issue list --state open --label ready --limit 100 --json number,title,labels,body
```

Drop every issue that also has `in-progress`, `needs-decision` or `blocked`.

## 3. Dependencies

An issue depends on every `#N` named in its body as a dependency ("Depends on #9",
"Depends on #9 and #12", "after #N", "requires #N", a "Dependencies" list). Plain
references for context ("see #20", "deferred there") are not dependencies; read the
sentence. Check each dependency:

```bash
gh issue view <N> --json state --jq .state
```

Keep the candidate only if **every** dependency is `CLOSED`. A dependency with an open
PR does not count as done; do not stack branches on unmerged work.

## 4. Order

Rank the remaining candidates and take the first:

1. Most other open `ready` issues it unblocks (count issues whose dependencies name it).
2. `bug` before `type:task` before `type:feature`.
3. Lowest issue number.

State the ranking in one short list before starting, with the reason for the winner.

## 5. Work it

Run the AGENTS.md protocol on the winner:

1. Claim: `gh issue edit <nr> --add-label in-progress` and comment "Taking this."
   Re-check the labels right before claiming; if someone else claimed it meanwhile, go
   back to step 2.
2. Branch from up-to-date `origin/main`: `issue-<nr>-<slug>`.
3. Implement within scope. Out-of-scope findings become new issues from the template.
4. Run `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`
   and `cargo test --workspace`. All must pass before pushing.
5. Conventional commits. No "Generated with Claude Code" footer and no `Co-Authored-By`
   trailer in commits or the PR.
6. Push and open a PR whose body contains `Closes #<nr>` and says how each "Done when"
   item is met. Move the board card to Review.

If a decision blocks you, do not guess: add `needs-decision`, remove `in-progress`,
comment with the question, options and your recommendation, then stop.

## 6. Nothing to do

If no candidate survives, report in one line why (WIP limit, all blocked, none ready)
and stop. Do not pick an issue that is not `ready`.
