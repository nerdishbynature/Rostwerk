# Working on Rostwerk issues

This is the pickup protocol for people and coding agents. Code rules live in
[CONTRIBUTING.md](CONTRIBUTING.md); read it before changing code.

Work is tracked in [GitHub issues](https://github.com/nerdishbynature/Rostwerk/issues)
and on the [Rostwerk project board](https://github.com/orgs/nerdishbynature/projects).

## Labels

| Label | Meaning |
| --- | --- |
| `ready` | Specified well enough to start. |
| `in-progress` | Someone is on it. Do not take it. |
| `needs-decision` | Waiting for a maintainer decision. Do not start. |
| `blocked` | Waiting on another issue or something external. |
| `area:*` | Part of the system it touches. |

## Protocol

1. **Pick** an open issue labeled `ready` and not `in-progress`, `needs-decision` or `blocked`.
   Prefer one with no open dependencies listed in its body.
2. **Claim** it: add `in-progress` and comment that you took it.
   ```bash
   gh issue edit <nr> --add-label in-progress
   gh issue comment <nr> --body "Taking this."
   ```
3. **Branch** from up-to-date `main`: `issue-<nr>-<slug>`, e.g. `issue-12-nats-sink`.
4. **Work** within the issue's scope. Follow the ground rules in CONTRIBUTING.md and run
   `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and
   `cargo test --workspace` before pushing. Use conventional commits.
5. **Open a PR** whose body contains `Closes #<nr>` and says how each "Done when" item is met.
6. **Stuck on a decision?** Do not guess. Add `needs-decision`, remove `in-progress`,
   comment with the question, the options you see and your recommendation, then stop.
   ```bash
   gh issue edit <nr> --add-label needs-decision --remove-label in-progress
   ```
7. **Out-of-scope findings** go into a new issue (use the template), not into the PR.

Write new issues from the template in `.github/ISSUE_TEMPLATE/` so that someone with no
prior context can work on them.
