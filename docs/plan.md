# Development plan

This is the public roadmap of the tool. It describes the general problem and the
design, not any particular mailbox.

## 1. Problem

After a mailbox migration done by a third-party service, the target Exchange mailbox
can contain items that are present but damaged. Typical defects:

| Defect | Likely cause |
|---|---|
| Received/sent date equals the migration date | Delivery and submit times (`PR_MESSAGE_DELIVERY_TIME`, `PR_CLIENT_SUBMIT_TIME`) not set on creation |
| Sender, recipients or headers missing | Item created without MIME headers (`PR_TRANSPORT_MESSAGE_HEADERS`) or sender properties |
| HTML body displayed as raw text | HTML stored in the plain-text body, or wrong MIME `Content-Type` |
| Items from every source folder dumped into the mailbox root (Top of Information Store), which most clients do not show as a mail folder | Folder mapping failed, items created in `msgfolderroot` |
| Folder invisible in the web client | Wrong or missing folder class (`IPF.Note`), hidden attribute, folder outside the IPM subtree, or localized default folders created twice (`Inbox` / `Boîte de réception`) |
| Items marked as drafts / unsent | `MSGFLAG_UNSENT` left on, because `PR_MESSAGE_FLAGS` can only be set at creation |
| Duplicates | Migration run several times, or retries |
| Calendar, contacts or tasks missing or flattened | Non-mail items copied as messages, or not copied at all |

Items received after the migration are healthy and must never be touched.

## 2. Goals and non-goals

**Goals**

- Read trusted **sources**: a Unicode PST file, an Outlook.com mailbox through
  Microsoft Graph, and later an Exchange mailbox over EWS.
- Read and repair a **target** Exchange mailbox over EWS.
- Detect and report every defect above, per item and per folder.
- Repair by moving, fixing properties, or re-uploading from the source with full
  fidelity (dates, sender, flags, categories, attachments, read state).
- Handle mail, calendar (including recurring series and exceptions), contacts,
  distribution lists, tasks and notes.
- Be safe to interrupt and run again (idempotent, journaled).

**Non-goals (for now)**

- Writing to PST files or to the source mailbox.
- IMAP, POP or Microsoft Graph as a target.
- Migrating signatures, delegates, permissions, or the safe and blocked sender lists
  (no API exposes them; inbox rules are copied, `plan-rules`).
- A graphical interface.

## 3. Safety rules

1. A source is never written to. A PST file is opened with a read-only file handle;
   its SHA-256 is computed at the start and checked at the end of every run.
2. `apply` only runs actions from a plan file produced by `plan` and reviewed by the user.
3. No hard delete. Removed items go to a quarantine folder (`/ecm-quarantine/<run-id>`);
   `plan-discard` later moves a run folder into Deleted Items, which the user empties.
4. Every action is written to a journal before and after it runs, so a run can resume
   and be audited.
5. A `backup` command exports the target with EWS `ExportItems` before changes; the
   backup can be restored with `UploadItems`.
6. Items that exist only on the target and were received after a cutoff date are
   protected by default.
7. Credentials are never stored in files or passed on the command line: the Windows
   Credential Manager is used.

## 4. Architecture

Cargo workspace:

| Crate | Role |
|---|---|
| `ecm-model` | Common model: item records, folder summaries, property comparison, fingerprints, folder paths |
| `ecm-pst` | PST source, built on Microsoft's `outlook-pst`; full message content and its rebuilding as MIME |
| `ecm-ews` | EWS client for the target: SOAP requests, paging, throttling and retries; every change in one module (`write`) |
| `ecm-graph` | Microsoft Graph (Outlook.com, Microsoft 365): device code sign-in, mail, calendar, contacts, To Do, inbox rules as a source; as a target, every change in one module (`write`): folders, messages from JSON or MIME, files (upload sessions), moves |
| `ecm-http` | HTTP transport with NTLM/Negotiate. First implementation: WinHTTP through Microsoft's `windows` crate. Possible portable implementation later (`reqwest` + an SSPI/NTLM crate) |
| `ecm-store` | Local state as files: inventories (JSON Lines), CSV reports |
| `ecm-audit` | Matching, defect detection, plans (JSON Lines actions), verification |
| `ecm` (binary) | Command-line interface (`clap`) |

### Dependencies

Prefer official Microsoft crates when they exist:

- `outlook-pst` (microsoft/outlook-pst-rs): PST reading.
- `windows` (microsoft/windows-rs): WinHTTP, SSPI, Credential Manager.
- `outlook-mapi` (microsoft/mapi-rs) is **not** used: it needs a local Outlook/MAPI
  profile, which is exactly what may be unavailable.

Other candidates, to evaluate:

- `ews` (thunderbird/ews-rs, MPL-2.0) was evaluated and not used: it has no
  `FindFolder`, `ExportItems` or `UploadItems`. `ecm-ews` writes its SOAP requests by
  hand and parses responses with `quick-xml`.
- `mail-parser` / `mail-builder` were not needed: MIME comes whole from Graph, and the
  few lines that rebuild it from a PST are in `ecm-pst`.
- `clap`, `serde`/`serde_json`, `sha2`, `base64`.
- SQLite (`rusqlite`) was planned for local state. JSON Lines files turned out to be
  enough: about 100,000 records load in memory in well under a second, the files
  are readable with any tool, and no C dependency is needed.

### Commands

Every command but `apply` only reads. `ecm <command> --help` gives the options.

```
ecm source check   --pst <file>                 # open, checksum, folder tree, counts
ecm source show    --pst <file> --key <key>     # one PST message's properties, attachments
ecm target check   --ews <url>                  # authenticate, folder tree, counts, sizes
ecm target show    --ews <url> --id <id>        # one item's properties, attachments
ecm target pim     --ews <url>                  # calendar, contacts, inbox rules
ecm backup         --ews <url> --out <dir>      # ExportItems of the whole mailbox, resumable
ecm inventory      --work <dir> --pst/--ews/--outlook-com   # item records, JSON Lines
ecm audit          --work <dir> --cutoff <date> # report: summary + CSV
ecm plan           --ews <url> --work <dir> ... # repair plan from Outlook.com
ecm plan-pst       --ews <url> --pst <file> ... # re-create from a PST, rebuilt as MIME
ecm plan-pim | plan-contact-photos | plan-notes | plan-rules   # other item types
ecm plan-park | plan-discard                    # set aside, then into Deleted Items
ecm apply          --plan <file> [--dry-run] [--limit N]   # run it, journaled, resumable
ecm apply-graph    --client-id <id> --plan <file> ...   # the same in an Outlook.com mailbox
ecm verify         --ews <url> --work <dir> ... # re-inventory and check again
ecm check-content  --ews <url> | --outlook-com --client-id <id> --pst <file> ...   # copies vs the PST
ecm report-cleanup --work <dir> --domain ...    # sort the old mailbox once a domain moved
ecm fidelity-test  --ews <url> ...              # property-by-property spike in ecm-test
ecm fidelity-graph --client-id <id> --pst <file> --keys <file> ...   # the same through Graph
```

## 5. Matching and classification

Each item gets a strong key and a fallback fingerprint.

| Item type | Strong key | Fallback fingerprint |
|---|---|---|
| Message | `PR_INTERNET_MESSAGE_ID` | normalized subject + sender address + submit time (± 2 min) + attachment count |
| Appointment | `PidLidGlobalObjectId` (iCalendar UID) | subject + start + end + organizer |
| Contact | — | display name + normalized e-mail addresses + phone numbers |
| Task / Note | — | subject + body hash + due/created date |

Fingerprints are computed from normalized values, so they survive the damage the
migration may have done (lost sender, wrong date). The audit records which keys
matched and how confident the match is.

Each source item and each target item ends up in exactly one class:

- **OK**: matched, no defect.
- **Degraded**: matched, with a list of defects (wrong folder, wrong dates, missing
  sender or headers, body format, missing attachments, flags, categories, read state…).
- **Missing**: in the source, not in the target.
- **Duplicate**: several target items match one source item.
- **Target-only**: no source match. Split into *after cutoff* (protected) and
  *before cutoff* (to review).

Folders are classified the same way: OK, missing, hidden/wrong class, duplicate
(localized names), unexpected.

## 6. Repair strategies

The plans keep healthy copies, move misplaced ones, re-create what is damaged or
missing from a source, and quarantine duplicates and the damaged copies they
replace. Nothing is fixed in place: properties a migration got wrong are often the
ones that can only be set at creation. A first pass also showed that migration
copies which look healthy can still be damaged, so a second pass (`plan --after`)
re-creates every migration copy the first one kept.

Actions (JSON Lines, one per step, `requires` naming the step that must succeed
first): `create-folder`; `recreate` (MIME fetched from Outlook.com at apply time);
`recreate-mime` (MIME carried by the plan, rebuilt from a PST); `create-message`
(the same for an Outlook.com mailbox through Graph: JSON with the properties only
creation sets, run by `apply-graph`); `attach` (a file of a message created
without its files: over 25 MB for `recreate-mime`, every file for
`create-message`; each `attach` waits for the message or the file before it); `create` (a typed calendar
item, contact or task, as EWS XML); `contact-photo`; `create-rule`; `move`
(misplaced, duplicate, damaged, replaced, unmatched); `discard-folder`.

### Re-upload with full fidelity

Two methods, chosen per item type after a spike (milestone M3):

1. **MIME + extended properties** (messages): the MIME message from Outlook.com, or
   rebuilt from a PST (transport headers, text and HTML bodies, attachments, S/MIME
   signed entities), sent with `CreateItem` as `MimeContent`, with in the same
   request `PR_MESSAGE_FLAGS` (read, and without `MSGFLAG_UNSENT`), delivery and
   submit times.
2. **Typed items** (appointments, contacts, tasks, notes): EWS `CalendarItem`,
   `Contact` and `Task` elements built from Graph (recurrence, local times with their
   time zone, attendees of the user's own meetings), notes from the PST, contact
   photos with `CreateAttachment`. Exceptions to a recurring series are not copied
   yet.

Exchange → Exchange copies can use `ExportItems` + `UploadItems`, which keeps every
property.

## 7. Milestones

| # | Deliverable | Done when |
|---|---|---|
| M0 ✅ | Workspace, CI (fmt, clippy, tests), contribution guide | CI is green on an empty skeleton |
| M1 ✅ | `source check` on PST | Folder tree and item counts match what Outlook shows for a test PST; checksum unchanged after the run |
| M2 ✅ | `target check` and `backup` over EWS with NTLM | Authenticates to an Exchange 2019 server that only offers Negotiate/NTLM; a backup can be restored to a test mailbox |
| M3 ✅ | Spike: re-upload fidelity, in a dedicated `ecm-test` folder (`ecm fidelity-test`). Messages: backup → `UploadItems` and Outlook.com MIME → `CreateItem` match their reference on every compared property; PST messages are rebuilt as MIME (`ecm plan-pst`). Calendar items, contacts and tasks are created as typed EWS items from Graph (`ecm plan-pim`); notes from the PST (`ecm plan-notes`) | For each item type, an item uploaded from a PST is identical to the original on every property we care about (property-by-property diff) |
| M4 ✅ | `inventory` + `audit` | CSV report with classes and defects; tested on synthetic damaged mailboxes |
| M5 ✅ | `plan` + `apply` + journal + quarantine | Interrupting and running again gives the same result; `--dry-run` changes nothing |
| M6 ✅ | `verify` | After `apply`, a second audit finds no Degraded, Missing or Duplicate items |
| M7 | Exchange (EWS) as a source | Same results as with a PST of the same mailbox |
| M7b ✅ | Outlook.com / Microsoft 365 (Microsoft Graph) as a source, read-only, device code sign-in | Inventory and audit against Graph give the same matches as against a complete PST |
| M7c ✅ | Calendar, contacts and photos, tasks, notes, inbox rules (`plan-pim`, `plan-contact-photos`, `plan-notes`, `plan-rules`) | Each type is copied without duplicates and checked on the target |
| M8 ✅ | Release 0.1 | Documentation, binaries for Windows x64 |
| M9 ✅ | Microsoft Graph as a target for messages (Outlook.com): `fidelity-graph`, `check-content --outlook-com`, `plan-pst --outlook-com` and `apply-graph` | A real Outlook.com mailbox repaired from a PST, checked with `check-content` |
| M9b | Microsoft Graph as a target for every other item type: calendar, contacts and photos, To Do tasks, inbox rules, categories, notes. Source: an Exchange mailbox read over EWS (the part of M7 these types need). See [Graph as a target](#graph-as-a-target-for-every-item-type) | Each type, copied into test containers of a real Outlook.com mailbox, matches its source on every compared property; what Graph cannot do is documented |
| M10 | PST as a source for calendar items (recurrences and their exceptions, [MS-OXOCAL]), contacts and their photos, and tasks | Each type, copied from a PST to EWS and to Graph, matches the PST on every compared property |

### Graph as a target for every item type

Milestone M9b, after messages (M9). Same method: a fidelity test first, then plans and
`apply-graph`, then a check.

1. **Permissions**: the app registration also gets the delegated `Calendars.ReadWrite`,
   `Contacts.ReadWrite`, `Tasks.ReadWrite` and `MailboxSettings.ReadWrite` (rules,
   categories); the user consents at sign-in.
2. **Source**: an Exchange mailbox read over EWS, whole: calendar items with their
   recurrence, exceptions, attendees and time zones; contacts and their photos; tasks;
   notes; inbox rules; categories. This is the part of M7 these types need.
3. **Test containers** in the target, never its real ones: a calendar, a contact folder
   and a To Do list named `ecm-test`, rules created disabled and categories, both with
   an `ecm-test` prefix. Nothing else in the mailbox is touched; the user deletes them
   at the end.
4. **Fidelity test** per type: a few items created, read back and compared property by
   property with the source. Open questions:
   - events with attendees: whether Graph sends invitations when it creates them (it
     must not), time zones, recurrences and their exceptions, reminders;
   - contacts: every field, the photo (`PUT /contacts/{id}/photo/$value`);
   - To Do tasks: due date, reminder, recurrence, completion, notes;
   - inbox rules (`messageRules`): conditions, actions, folders by id;
   - notes (`IPM.StickyNote`): no known Graph API; try a message of that class in
     the Notes folder, else document the limit;
   - contact groups (distribution lists) and safe and blocked senders: no Graph API
     known, to document as limits (the latter stay a setting the user makes).
5. **Plans and `apply-graph`** for each type, journaled and resumable, and a check
   comparing each type between the target (Graph) and the source (EWS).
6. **A full run** of every type into the test containers, then the check.

## 8. Testing

- Unit tests on the model, fingerprints and defect detection.
- Small test PST files generated for the tests (never real mailboxes in the repository).
- Integration tests against a dedicated, disposable test mailbox, run manually and
  never in public CI.
- Golden reports: given a source and a damaged target fixture, the audit output is stable.

## 9. Risks and open questions

- `outlook_pst::open_store` also opens the file **for writing** when it can. `ecm-pst`
  never calls it: it opens the file itself, read-only and sharing read access only, and
  passes the handle to `UnicodePstFile::read_from`, which has no writer.
- `outlook-pst` 1.2 refuses messages without a sub-node tree ("Missing Sub-Node Tree
  on message"), which MS-PST allows and which is common for contacts, appointments and
  tasks. The inventory falls back to the folder's contents table row for them; a few
  still fail and are reported. Worth fixing upstream.
- Bodies can be silently degraded: on a real mailbox, about 68,000 copies had their
  HTML body stored as a plain-text body (`PR_NATIVE_BODY_INFO` = 1), so clients show
  raw HTML. PSTs rarely store `PR_NATIVE_BODY_INFO`; the inventory derives it from the
  bodies present (HTML, else RTF, else text), and the audit flags the mismatch.
- Migrations can also empty bodies and duplicate recipients: in a real test, damaged
  copies had a zero-length body or every recipient listed twice, while the copy
  re-created from the source was faithful.
- Folder names can contain `/` ("Commandes / Factures"): paths escape it as `\/`
  (`ecm_model::path`), so a name is never split into two folders.
- Only items at the top of the target mailbox, where the migration dumped its
  copies, are ever moved. Items in real folders were delivered or filed by the user
  and are left alone, together with the source items they serve.
- A PST export can be incomplete. On a real mailbox, about 3,000 sent items existed
  only as damaged copies on the target. Items with no source match are therefore
  never quarantined by default: they are reported as `unmatched` for review.
- `outlook-pst` gives properties, recipients and attachments, enough to rebuild a
  message as MIME. A compressed RTF body (`compressed-rtf`, Microsoft's) is
  de-encapsulated into the HTML or text it carries ([MS-OXRTFEX]), else read as
  text; only an RTF body nothing comes out of keeps a message from being rebuilt.
  Attachments `outlook-pst` cannot open ("Missing PidTagAttachMethod", an embedded
  message without data) are left out and listed. PST subjects can start with the MAPI prefix-length marker (SOH
  and a length), which must be stripped before matching.
- Migration copies that look healthy can still be damaged: emptied bodies (found with
  the EWS `item:Preview` and Graph `bodyPreview`), the LF bytes of attachments turned
  into CRLF (all of them, or those not after a CR, with a line end added at the
  end), sent dates with day and month swapped, Message-IDs rewritten by the
  migration's servers. They are never trusted: each is replaced from a source.
- Outlook.com MIME of some sent items lacks the Message-ID, or names the sender by its
  internal Exchange address (`IMCEAEX-…`). The PST copy, with a given sender, fixes
  them.
- EWS limits on hosted Exchange: throttling, maximum request size. OVH refused a
  97 MB `CreateItem` (HTTP 413) and took 30 MB ones: larger messages go as the
  message, then one `CreateAttachment` per file.
- Some properties can only be set at creation, so some fixes require a re-upload.
- Recurring appointments with exceptions are the hardest item type to re-create
  faithfully.
- EWS is being retired from Exchange Online; this tool targets on-premises and hosted
  Exchange, where EWS remains available.
