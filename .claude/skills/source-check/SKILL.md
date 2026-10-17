---
name: source-check
description: Read a PST source read-only with `ecm source check` and compare it with the recorded reference (checksum, folder counts).
---

1. Build: `cargo build --release -p ecm-cli`.
2. The PST path comes from the argument, or from `CLAUDE.local.md`. Never copy,
   open in Outlook, or write to the file.
3. Run `target\release\ecm.exe source check --pst <file>` (`--all-folders` for the
   whole store). Redirect long output to the scratchpad.
4. Compare the SHA-256 and the per-folder counts with the reference in
   `.claude/plan.local.md`, when present. A different SHA-256 means the file changed
   outside ecm: stop and tell the user.
5. Report the totals and any difference. Update `plan.local.md` only if the user
   agrees.
