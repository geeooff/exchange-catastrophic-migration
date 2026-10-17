# exchange-catastrophic-migration

Audit and repair an Exchange mailbox after a botched migration.

Migration services sometimes leave a mailbox in a bad state: items dated on the day of
the migration, senders and headers lost, HTML bodies shown as plain text, folders the
web client cannot see, duplicates everywhere. This tool compares the mailbox with a
trusted source, tells you exactly what is wrong, and repairs it without touching what
arrived after the migration.

> **Status: early development.** What works so far, all of it read-only:
>
> - `ecm source check --pst <file>`: a PST's folder tree and counts, with proof the
>   file was not modified.
> - `ecm target check --ews <url>`: an Exchange mailbox's folder tree, counts and sizes
>   over EWS (NTLM/Negotiate, Windows only).
> - `ecm backup --ews <url> --out <dir>`: a resumable full-fidelity export
>   (`ExportItems`) of the mailbox.
> - `ecm inventory` and `ecm audit`: match a PST with a mailbox item by item and report
>   what is ok, misplaced, damaged, missing, duplicated, new or unmatched (CSV).
>
> See [docs/plan.md](docs/plan.md).

## What it will do

- **Read a trusted source**, never writing to it:
  - a Unicode PST file (e.g. an export from Outlook.com or Microsoft 365), or
  - another Exchange mailbox over EWS.
- **Read and repair a target** Exchange mailbox over EWS (Exchange 2013 and later,
  including hosted offers that only allow NTLM/Negotiate).
- Cover **mail, calendar, contacts, tasks and notes**, plus the folder tree.
- Work in safe stages: `inventory` → `audit` (report) → `plan` (reviewable actions) →
  `apply` (journaled, resumable) → `verify`.

## Safety principles

- Sources are opened read-only. A PST is opened with a read-only file handle and its
  checksum is verified before and after a run.
- Nothing changes on the target without an explicit plan file you have reviewed.
- Items are never hard-deleted: they are moved to a quarantine folder first.
- A full backup of the target can be taken before any change.

## Built on

- [`outlook-pst`](https://github.com/microsoft/outlook-pst-rs) (Microsoft) to read PST files.
- [`windows`](https://github.com/microsoft/windows-rs) (Microsoft) for WinHTTP, SSPI
  (NTLM/Negotiate) and the Windows Credential Manager.
- Exchange Web Services (EWS) for both reading and writing mailboxes.

## License

[MIT](LICENSE)
