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
10. Messages only the PST still has: `ecm plan-pst --ews <url> --work reports --pst <file> --cutoff <date> --after <applied plans…> --out reports/plan-pst.jsonl` rebuilds them as MIME (original headers, bodies, attachments, S/MIME signed entities) and replaces copies earlier plans made without Message-ID. Refresh `target.after.jsonl` with `ecm verify` first.
    For an older PST, a large restore goes in batches (`--keys <file>`, one PST key per line, about
    1,000 messages or 150 MB each), and `--rename FROM=TO` sends a folder renamed since to its new name.
    A message over 25 MB is created first, then its files one by one (`attach` steps).
    For an Outlook.com target: `ecm plan-pst --outlook-com --client-id <id> --work <dir> …` (the work
    directory holds `outlook.jsonl`; the folders are read once into `outlook-folders.json`), then
    `ecm apply-graph --client-id <id> --plan <file> [--dry-run]`; check with `ecm check-content --outlook-com`.
    Copies that look healthy can still be damaged when the source they came from already was:
    `ecm check-content --ews <url> --work <dir> --pst <file> --out <dir>/damaged.csv` compares every copy
    with its PST message (emptied body, missing or changed attachment, LF turned into CRLF), then
    `ecm plan-pst … --after <applied plans…> --replacements-only --damaged <dir>/damaged.csv` rebuilds them
    where they are and quarantines the damaged copies.
11. Inbox rules: `ecm plan-rules --ews <url> --client-id <id> --out reports/plan-rules.jsonl`
    (asks for `MailboxSettings.Read`) maps each rule's folder to the target's,
    creating missing folders, and leaves out rules the target has by name. Rules
    only classic Outlook ran come without actions or conditions: recreate by hand.
12. Sticky notes: `ecm plan-notes --ews <url> --work reports --pst <file> --out reports/plan-notes.jsonl`
    copies the PST's notes (HTML body when they have no text) into the Notes folder.
12. After review, `ecm plan-park --ews <url> --work reports --after reports/plan.jsonl --out reports/plan-park.jsonl`
    moves what `ecm-unmatched` holds into a quarantine run folder.
    `ecm plan-pst … --sender "<Name> <address>" --replacements-only` replaces copies
    with a missing or internal (IMCEAEX) sender without re-creating messages the
    user may have deleted since; a message whose RTF body cannot be converted is kept as it is.
13. Discarding the quarantine is the user's decision, weeks after the repair:
    `ecm plan-discard --ews <url> [--run <name>] --out reports/plan-discard.jsonl`,
    then `apply`, moves run folders into Deleted Items; the user empties it.
