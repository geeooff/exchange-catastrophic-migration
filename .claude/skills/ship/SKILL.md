---
name: ship
description: Finish a milestone or change - checklist, docs, commit, push, PR - once the user asked to commit.
---

1. Work on a branch, never on `main`. Stack it on an unmerged milestone branch when
   it needs that milestone.
2. Run `/check` and fix everything it reports.
3. Update the docs: `docs/plan.md` (mark the milestone ✅, add any new risk or
   lesson), `README.md` status, `AGENTS.md` if a rule or gotcha changed, the
   `CHANGELOG.md` section of the next release, and section 0 of
   `.claude/plan.local.md`.
4. Make sure nothing private is staged: `git status --short`, no `*.local.*`, no real
   paths or addresses in tracked files.
5. Commit with a message that says what and why, ending with the `Co-Authored-By`
   line. Push with `-u origin <branch>`.
6. Open the PR with `gh pr create`, its body ending with the Claude Code line. Then
   bind it with the app's PR tools, and offer Auto-fix.
7. A release: in its commit, bump the workspace version and date its `CHANGELOG.md`
   section. After the merge, and only with the user's go-ahead, push the tag
   `vX.Y.Z`; `release.yml` builds, attests and publishes the zip.
