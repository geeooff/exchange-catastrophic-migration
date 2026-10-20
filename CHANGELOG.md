# Changelog

What changes from one release to the next, for the person running `ecm`. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/). A release commit dates its section.

## [Unreleased]

### Added

- [docs/supported-migrations.md](docs/supported-migrations.md): what can be
  migrated, for each item type, from each source to each target, with its
  command and its limits.
- `plan-pst --keys FILE`: consider only the PST messages listed, to split a large
  restore into plans of a reasonable size.
- `plan-pst --rename FROM=TO`: put the messages of a source folder, and of the
  folders under it, into a target folder renamed since.
- `check-content`: compare the target copies of PST messages with the PST, body
  and file attachment by attachment (missing, line ends changed, otherwise
  different), and write the damaged ones as CSV. Line ends count as changed when
  every LF became CRLF, or only the LFs not after a CR, with maybe a line end
  added at the end, as found on a real photo. A body counts as lost only when the
  copy's own body is empty: Exchange's preview is empty too for an HTML body
  with only photos, or one not previewed yet, and only when the PST body shows
  something (text, or an image). A file renamed on the way is found by its
  content, and a copy deleted or moved since the inventory is counted, not an
  error.
- `plan-pst --damaged FILE`: rebuild those copies from the PST in the folder they
  are in, and quarantine them, when an earlier plan created or moved them.
- `source show`: a PST message's properties and attachments, like `target show`.
- `target show --attachments` says which files are inline, with their Content-ID.
- PST bodies kept only as compressed RTF are recovered: the HTML or plain text the
  RTF encapsulates ([MS-OXRTFEX]), else the text of the RTF, decoded in its code
  pages. `source show` says when a body came from RTF.
- `check-content --outlook-com`: compare the copies in an Outlook.com mailbox
  through Microsoft Graph, files read whole and bodies judged by what they show.
- `plan-pst --outlook-com`: a plan for an Outlook.com mailbox (Microsoft Graph),
  each message a `create-message` step (JSON with the properties only creation
  sets) followed by one `attach` step per file; the mailbox's folders are read
  once and kept in the work directory. `apply-graph` runs it, journaled and
  resumable like `apply`, renewing its token during long runs; several
  `--plan` run in order with one sign-in. A file refused because the mailbox is
  still storing the one before (a change key conflict) is sent again after a
  pause, by Graph as by EWS.
- `fidelity-graph`: re-create a few PST messages in the `ecm-test` folder of an
  Outlook.com mailbox through Graph, from MIME and from JSON with the properties
  only creation sets, and compare each copy with a known good one and the PST's
  files. It asks for `Mail.ReadWrite`.
- `fidelity-graph-pim`: copy a few calendar items, contacts (with their photo),
  tasks, inbox rules and categories of an Exchange mailbox into an Outlook.com
  mailbox through Graph, read each copy back and compare it with its source,
  property by property. The items are picked so that together they show every
  property the source has. It only creates: in an `ecm-test` calendar, contact
  folder and To Do list, and rules (disabled, after the mailbox's own) and
  categories named `ecm-test …`. Events go without their attendees, whom Graph
  would invite, and items whose reminder would ring later are left out. It asks
  for `Calendars.ReadWrite`, `Contacts.ReadWrite`, `Tasks.ReadWrite` and
  `MailboxSettings.ReadWrite`; `--dry-run` only prints what it would create.
- `plan-graph-pim`: a plan that copies the calendar, contacts and their photos,
  tasks, inbox rules and categories of an Exchange mailbox into an Outlook.com
  mailbox, leaving out what it already holds (events by start and subject,
  contacts by address or name, tasks by title and due date, rules and categories
  by name). Rules keep their folders by path, created when missing, and go after
  the mailbox's own rules. `--test` writes into the `ecm-test` containers instead,
  rules disabled and names prefixed; `--only TYPE` keeps one type. Each item is a
  `create-graph` step, each photo a `set-photo` step that `apply-graph` runs after
  its contact; `apply-graph` then asks for the other item types' `ReadWrite`
  permissions.
- `check-graph-pim`: compare each of those source items with its copy, property
  by property, and list the copies that differ and the items that have none.
  Each source item is paired with one copy, the closest first. `--plan FILE`
  also reads, by the ids in its journal, the copies the server moved out of
  their container: Outlook.com can move a contact when its photo is set.
- A PST message over 25 MB is created without its files, then given them one
  request each (plan step `attach`): an Exchange server refuses too large a
  request (HTTP 413).

### Fixed

- A debug build no longer overflows its stack while reading its arguments: `ecm`
  runs its command on a thread with a larger stack.
- Events and contacts read from Outlook.com are read again by id, like
  messages: Graph pages them by offset, and one changed during the read slips
  between two pages.
- An Outlook.com inventory no longer misses messages when a folder changes
  while it is read (Graph pages by offset, and a removed message makes another
  slip between two pages): each folder's ids are read again, and the messages
  missed are read one by one.
- A file with a Content-ID the HTML body never shows is sent as an attachment,
  not inline: targets hid such files (the PST keeps Content-IDs on many photos).
- A PST message with no body at all (only attachments, or a `Null` HTML body) is
  rebuilt; it was left out as "RTF only".
- A PST message with an attachment the PST library cannot open (an embedded
  message without its data, no attach method) is rebuilt without it instead of
  not at all; `plan-pst` lists the messages that lost attachments that way.
- `plan-pst` no longer creates folders nothing goes into: the quarantine folder
  when it replaces nothing, or, with `--replacements-only`, the folders of the
  messages it leaves out.
- `plan --after` no longer re-creates a message next to the copy an earlier plan
  created. The audit gave that copy to a twin of the message (the same message
  kept in Recoverable Items, or filed in another folder), so the message looked
  missing. Copies now go first to the messages plans rebuild, then, between
  equal shares, to the message in their own folder.
- `plan --after` also replaces the copies earlier plans re-created from
  Outlook.com when they are degraded (re-created, then quarantined, unless
  another copy would stay next to the new one), and quarantines the extra copies
  they made of a message. Healthy copies stay, copies rebuilt from a PST are left
  to `plan-pst`, and copies `ecm` did not put there are never touched.

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
