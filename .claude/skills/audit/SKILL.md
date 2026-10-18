---
name: audit
description: Inventory the source PST and the target mailbox, then audit them (`ecm inventory` + `ecm audit`). Read-only on both.
---

1. Paths, URL and cutoff come from the argument, or from `CLAUDE.local.md`.
2. Build with `cargo build --release -p ecm-cli`. If `ecm.exe` is locked (a backup
   is running), add `--target-dir target\alt` and run that binary instead.
3. Inventory into the gitignored `reports/`:
   `ecm inventory --work reports --pst <file> --ews <url>`. Either side can be
   re-inventoried on its own.
   Outlook.com as a second source: add `--outlook-com --client-id <id>` (id in
   `CLAUDE.local.md`). The user signs in in the browser with the code shown; never
   do it for them.
4. Audit: `ecm audit --work reports --cutoff <YYYY-MM-DD>`. It prints a summary and
   writes `reports/audit-source.csv` and `reports/audit-target.csv`. Add `--source outlook` to audit
   against Outlook.com instead of the PST.
5. Never commit anything from `reports/`. Record the totals in
   `.claude/plan.local.md`, and explain to the user what each status means for the
   repair.
