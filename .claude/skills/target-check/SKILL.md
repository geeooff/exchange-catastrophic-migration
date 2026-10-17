---
name: target-check
description: Read the target Exchange mailbox over EWS with `ecm target check` (folder tree, counts, sizes). Read-only.
---

1. The EWS URL and the credential name come from the argument, or from
   `CLAUDE.local.md`.
2. Check that the credential exists with `cmdkey /list:<name>`, which never shows the
   password. If it is missing, ask the user to run
   `cmdkey /generic:<name> /user:<address> /pass`. Never ask for the password, and
   never type it.
3. Build (`cargo build --release -p ecm-cli`), then run
   `target\release\ecm.exe target check --ews <url>` (`--all-folders` for system
   folders).
4. Compare with the measurements in `.claude/plan.local.md`: root item count,
   Inbox, folders. Report what changed since then.
5. HTTP 401: the credential is wrong, so ask the user to fix it with `cmdkey`. Do not
   retry in a loop.
