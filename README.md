# exchange-catastrophic-migration

Audit and repair an Exchange mailbox after a botched migration.

Migration services sometimes leave a mailbox in a bad state: items dated on the day of
the migration, senders and headers lost, HTML bodies shown as plain text, folders the
web client cannot see, duplicates everywhere. This tool compares the mailbox with a
trusted source, tells you exactly what is wrong, and repairs it without touching what
arrived after the migration.

> **Status: 0.1, used on a real mailbox.** It repaired a mailbox of
> about 21,000 messages that a migration had copied four times, with emptied bodies,
> corrupted attachments, lost senders and mangled dates. See [docs/plan.md](docs/plan.md).
> An Outlook.com mailbox as a target, through Microsoft Graph, repaired a real
> mailbox's messages; the other item types are being validated
> (`ecm fidelity-graph-pim`). What can be migrated,
> from which source to which target: [docs/supported-migrations.md](docs/supported-migrations.md).

## How it works

Every step but `apply` only reads. `apply` runs a plan file you have reviewed,
journals each step and resumes where it stopped.

1. **Look**: `ecm source check --pst <file>` and `ecm target check --ews <url>` show
   both folder trees and counts. `ecm backup --ews <url> --out <dir>` exports the whole
   target first (`ExportItems`, resumable).
2. **Inventory and audit**: `ecm inventory` reads a PST (`--pst`), the target (`--ews`)
   and/or Outlook.com through Microsoft Graph (`--outlook-com --client-id <id>`, browser
   sign-in). `ecm audit` matches them item by item and reports what is ok, misplaced,
   damaged, missing, duplicated, new or unmatched (CSV).
3. **Plan**: each command writes a JSON Lines plan.
   - `ecm plan`: create folders, re-create damaged or missing messages from
     Outlook.com, move misplaced copies, move duplicates and damaged copies to a
     quarantine folder. `--after <plan>` replaces copies an earlier plan moved and
     degraded copies it re-created, and quarantines extra copies it re-created.
   - `ecm plan-pst`: re-create from a PST, rebuilt as MIME with the original headers,
     bodies, attachments and S/MIME signed entities, the messages the target lacks or
     earlier plans made badly. With `--outlook-com`, the target is an Outlook.com
     mailbox through Microsoft Graph, and `ecm apply-graph` runs the plan.
   - `ecm plan-pim`, `ecm plan-contact-photos`, `ecm plan-notes`, `ecm plan-rules`:
     calendar (recurrences, time zones), contacts and their photos, To Do tasks,
     sticky notes and inbox rules.
   - `ecm plan-graph-pim`: the other way, from an Exchange mailbox into Outlook.com
     through Graph: calendar, contacts and their photos, tasks, inbox rules and
     categories, run with `ecm apply-graph` and checked with `ecm check-graph-pim`.
   - `ecm plan-park`: move the reviewed `ecm-unmatched` folder into quarantine.
   - `ecm plan-discard [--run <name>]`: once you are sure, move quarantine run folders
     into Deleted Items, which you empty yourself. `ecm` never deletes anything.
4. **Apply**: `ecm apply --plan <file> [--dry-run] [--limit N]`.
5. **Verify**: `ecm verify` inventories the target again and checks every source
   message has a healthy copy in its folder. `ecm check-content` compares each copy
   with its PST message, body and attachment by attachment, and lists the damaged
   ones for `ecm plan-pst --damaged`. `ecm target show --id <id> --attachments`,
   `ecm source show --key <key> --attachments` and `ecm target pim` inspect items,
   the calendar and the rules.

When the source mailbox stays in use for other addresses, `ecm report-cleanup`
sorts its messages into those to keep (they involve an address it keeps), those
safe to delete (they involve the moved domain and the target has them) and those to
look at, without changing anything: the deletion is yours, in that mailbox.

`ecm fidelity-test` re-creates a few items in an `ecm-test` folder and compares them
property by property with their reference. `ecm fidelity-graph` and
`ecm fidelity-graph-pim` do the same in an Outlook.com mailbox: messages from a
PST, and calendar items, contacts, tasks, inbox rules and categories from an
Exchange mailbox, into `ecm-test` containers.

## What it does

- **Read trusted sources**, never writing to them:
  - a Unicode PST file (e.g. an export from Outlook.com or Microsoft 365),
  - an Outlook.com mailbox through Microsoft Graph,
  - another Exchange mailbox over EWS (planned).
- **Read and repair a target** Exchange mailbox over EWS (Exchange 2013 and later,
  including hosted offers that only allow NTLM/Negotiate).
- Cover **mail, calendar, contacts, tasks and notes**, plus the folder tree.
- Work in safe stages: `inventory` → `audit` (report) → `plan` (reviewable actions) →
  `apply` (journaled, resumable) → `verify`.

## Install

Windows x64 only for now (WinHTTP, SSPI and the Credential Manager). Download
`ecm-vX.Y.Z-windows-x64.zip` from the
[releases](https://github.com/geeooff/exchange-catastrophic-migration/releases), check
it against its `.sha256`, and unzip `ecm.exe` anywhere. To build it yourself:
`cargo build --release` (see [CONTRIBUTING.md](CONTRIBUTING.md)).

- **Exchange (EWS)**: store the mailbox password in the Windows Credential Manager,
  once: `cmdkey /generic:ecm:<host> /user:<address> /pass` (the host of the EWS URL).
  `ecm` reads it from there and never asks for it.
- **Outlook.com (Microsoft Graph)**: register an application in Microsoft Entra
  (accounts in any identity provider or personal Microsoft accounts, public client
  flows allowed, delegated `Mail.Read`, `Contacts.Read`, `Calendars.Read`,
  `Tasks.Read`, `User.Read`, and `MailboxSettings.Read` for inbox rules; add
  `Mail.ReadWrite` to write messages into Outlook.com, and `Calendars.ReadWrite`,
  `Contacts.ReadWrite`, `Tasks.ReadWrite` and `MailboxSettings.ReadWrite` for the
  other item types), then pass its application id with
  `--client-id`. For Microsoft 365, the registration must accept work or school
  accounts. You sign in in your browser with a code `ecm`
  shows; tokens stay in memory.
- Work files (inventories, plans, journals, reports) contain mailbox data: keep them
  private.

## Safety principles

- Sources are opened read-only. A PST is opened with a read-only file handle and its
  checksum is verified before and after a run.
- Nothing changes on the target without an explicit plan file you have reviewed.
- Items are never deleted: they are moved to a quarantine folder, which leaves only
  into Deleted Items, for you to empty when you are sure.
- A full backup of the target can be taken before any change.

## Built on

- [`outlook-pst`](https://github.com/microsoft/outlook-pst-rs) (Microsoft) to read PST files.
- [`windows`](https://github.com/microsoft/windows-rs) (Microsoft) for WinHTTP, SSPI
  (NTLM/Negotiate) and the Windows Credential Manager.
- Exchange Web Services (EWS) for both reading and writing mailboxes.

## License

[MIT](LICENSE)
