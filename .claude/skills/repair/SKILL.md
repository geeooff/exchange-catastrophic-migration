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
6. After `apply`: `ecm verify --ews <url> --work reports --cutoff <date>` (only reads)
   inventories the target again into `target.after.jsonl` and checks the live
   folders. Inspect a suspicious item with `ecm target show --id <id> --attachments`.
7. A second pass replaces the migration copies the first plan moved into folders:
   `ecm plan … --after reports/plan.jsonl --out reports/plan2.jsonl`, with the
   after-inventory as `target.jsonl` in its work directory.
8. Calendar, contacts and To Do tasks have no MIME in Graph: `ecm plan-pim --ews <url> --client-id <id> --out reports/plan-pim.jsonl`
   converts them to EWS XML and leaves out what the target holds (same UID, or same
   start and subject). Apply it like any plan; no Graph sign-in is needed then.
   `ecm target pim` lists the target calendar to check times.
9. Contact photos: `ecm plan-contact-photos --ews <url> --client-id <id> --out reports/plan-photos.jsonl`, then apply. It matches contacts by e-mail, then name, and skips those that already have a photo.
