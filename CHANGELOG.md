# Changelog

What changes from one release to the next, for the person running `ecm`. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/). A release commit dates its section.

## [Unreleased]

### Added

- `plan-pst --keys FILE`: consider only the PST messages listed, to split a large
  restore into plans of a reasonable size.
- `plan-pst --rename FROM=TO`: put the messages of a source folder, and of the
  folders under it, into a target folder renamed since.

## [0.1.0] - 2026-10-19

The first release. It repaired a real mailbox of about 21,000 messages that a
migration had copied four times, with emptied bodies, corrupted attachments, lost
senders and mangled dates.

### Added

- Read-only sources: a Unicode PST file (opened read-only, checksum checked before
  and after), and an Outlook.com mailbox through Microsoft Graph with a browser
  sign-in.
- An Exchange target over EWS with NTLM/Negotiate, the password kept in the Windows
  Credential Manager: `target check`, `target show`, `target pim`, and a resumable
  full `backup`.
- `inventory`, `audit` and `verify`: item-by-item matching by Message-ID, then by
  subject and send time, with the damage a migration leaves: wrong folder, dates,
  senders, headers, HTML turned into text, emptied bodies.
- Reviewable plans, applied in journaled, resumable steps with `apply`:
  - `plan`: create folders, re-create damaged or missing messages from Outlook.com,
    move misplaced copies, quarantine duplicates and damaged copies;
  - `plan-pst`: rebuild messages from a PST as MIME, with their original headers,
    bodies, attachments and S/MIME signed entities;
  - `plan-pim`, `plan-contact-photos`, `plan-notes`, `plan-rules`: calendar,
    contacts and photos, To Do tasks, sticky notes, inbox rules;
  - `plan-park` and `plan-discard`: set aside what was reviewed, and move quarantine
    folders into Deleted Items when you are sure. `ecm` never deletes anything.
- `report-cleanup`: once a domain has moved, sort the old Outlook.com mailbox into
  what to keep and what the target already has.
- `fidelity-test`: re-create a few items in an `ecm-test` folder and compare them
  property by property with their reference.

[0.1.0]: https://github.com/geeooff/exchange-catastrophic-migration/releases/tag/v0.1.0
