---
name: check
description: Run the repository checklist (fmt, clippy, rustdoc, tests, doc links, no mailbox data tracked) before a commit or a PR.
---

1. Stage new files first (`git add -A`): the "no mailbox data tracked" step reads the
   index.
2. Run `.\scripts\build.ps1` with PowerShell (add `build` for a release build too,
   or `release`, from a clean tree, for the zip a release publishes).
3. On failure, fix the cause and run it again. Never skip a step or weaken a check.
4. Report the numbered steps briefly, and any test that failed with its message.
