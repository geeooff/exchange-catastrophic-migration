---
name: repair
description: Plan and apply the repair of the target mailbox (`ecm plan` then `ecm apply`), in small reviewed steps. Writes to the target only through a plan the user approved.
---

1. Refresh the inventories first (`/audit`). The plan uses the target's item ids,
   so a stale `target.jsonl` makes moves fail.
2. `ecm plan --ews <url> --work reports --source outlook --cutoff <date> --out reports/plan.jsonl`
   only reads. Show the user the counts and where the moves come from and go to.
   Every move must come from the top of the mailbox.
3. Get the user's explicit go-ahead before any `apply`, and again before raising
   `--limit`. Start with `--dry-run`, then a small `--limit` (the folders and a few
   re-creations), and check the result in OWA or with `ecm target check`.
4. `ecm apply --ews <url> --plan reports/plan.jsonl --outlook-com --client-id <id> --limit N`
   journals each step in `plan.jsonl.journal.jsonl`. Running it again resumes, and
   retries failed and skipped steps. The user signs in to Outlook.com themselves.
5. Nothing is deleted: duplicates and damaged copies go to
   `ecm-quarantine/<run>`, and unknown items go to `ecm-unmatched`. Purging the
   quarantine is a separate, explicit decision.
