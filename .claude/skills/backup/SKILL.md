---
name: backup
description: Back up the target mailbox with `ecm backup` (ExportItems, resumable) before any change to it.
---

1. Run `/target-check` first: it gives the item count and the size the backup needs.
2. Ask the user for the backup directory. It must be outside the repository and
   OneDrive, on a disk with room for the size reported. Never choose it yourself.
3. Run `target\release\ecm.exe backup --ews <url> --out <dir>` in the background (it
   can take hours), and report progress when notified.
4. If it stops (network, throttling), run the same command again: it resumes from
   `manifest.jsonl`. Items in `errors.jsonl` are retried on each run.
5. Done when a run reports 0 saved and 0 failed. Record the date, the item count
   and the directory in `.claude/plan.local.md` section 0.
