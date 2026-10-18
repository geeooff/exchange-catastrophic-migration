# AGENTS.md

`ecm`: Rust CLI that audits and repairs an Exchange mailbox after a botched
migration. Source (read-only): PST file or Exchange over EWS. Target: Exchange over
EWS. Public repo: no personal data. Roadmap and design: [docs/plan.md](docs/plan.md)
(milestones M0–M8, mark ✅ when done).

## Layout

- `ecm-model`: `FolderSummary`, shared by sources and targets.
- `ecm-pst`: read-only PST on Microsoft's `outlook-pst`.
- `ecm-http`: WinHTTP + NTLM/Negotiate, Credential Manager. Only crate with `unsafe`.
- `ecm-ews`: hand-written SOAP + `quick-xml` tree: GetFolder, FindFolder, FindItem,
  ExportItems, backup.
- `ecm-graph`: read-only Microsoft Graph source (Outlook.com), device code sign-in.
- `ecm-store`: JSON Lines inventories, CSV. `ecm-audit`: matching and defects.
- `ecm-cli`: the `ecm` binary.

## Rules

- Never write to a source. Never call `outlook_pst::open_store`: it opens the file
  for writing. Use `ecm_pst::PstSource`, which opens the file read-only and shares
  read access only.
- On a target: no hard delete (quarantine instead), and changes only through
  `plan`/`apply`, with the user's go-ahead for each `apply` run. Quarantine run
  folders leave only through `plan-discard`, into Deleted Items: the user empties it. The single exception
  is `ecm fidelity-test`, which only creates items, only in the `ecm-test` folder.
- Plans only move items from the top of the target mailbox (migration debris).
- Never track mailbox data or private notes. Exceptions: fixtures under
  `crates/*/tests/fixtures/`, each named in that folder's `README.md`. Private notes and
  config go in `*.local.*` files, which are gitignored.
- Credentials: generic Credential Manager entry `ecm:<host>`. Never in code, files,
  command-line arguments or logs.
- `unsafe` only in `ecm-http`, each block with `// SAFETY:`.
- Prefer official Microsoft crates (`outlook-pst`, `windows`).
- Tests use synthetic data only. Test EWS with `FakeTransport` replaying canned SOAP.
- Docs and code in English. Talk to the user in French.

## Workflow

- Before a commit: `.\scripts\build.ps1` (fmt, clippy and rustdoc with warnings as
  errors, tests, doc links, no data tracked). `build` adds a release build. CI runs
  the same script on `windows-latest`.
- One branch and one PR per milestone. `main` is protected (required check `build`).
  Dependabot merges compatible crate updates on its own.
- Releases: bump the workspace version and date its `CHANGELOG.md` section in the
  release commit; `.\scripts\build.ps1 release` then zips into `dist\`. A tag
  `vX.Y.Z` makes `release.yml` rebuild, attest (`actions/attest`) and publish the zip
  with its checksum: tag only with the user's go-ahead.
- Agents may push branches, open PRs, fix CI failures and enable auto-merge
  (`gh pr merge --auto --merge`). Never bypass a required check or the ruleset.
- Commit only when asked. Messages say what and why, ending with the
  `Co-Authored-By` line.

## Gotchas

- `quick-xml` 0.42: text comes from `xml10_content()`, and entities arrive as
  separate `GeneralRef` events.
- `windows` 0.62: `WINHTTP_AUTH_SCHEME_*` are newtypes: use `.0`.
- OVH EWS: Exchange 2019 (15.2), NTLM/Negotiate only. Send
  `RequestServerVersion` `Exchange2013_SP1`.
- Graph with personal accounts: `/me` needs the `User.Read` scope, or Graph answers a
  bare `401 UnknownError`. Folder names come back in the account's language
  (Outlook.com: English).
- Read-only probing from an OWA session: POST to `/owa/service.svc?action=X&EP=1`
  with the `X-OWA-CANARY` header (from the cookie) and the JSON body in
  `X-OWA-UrlPostData`. Opening a mail in OWA marks it read: avoid that.
- EWS `CreateItem` from MIME answers `ErrorObjectTypeChanged` when Exchange turns the
  MIME into another type (meeting request); `t:Item` does not help. Graph access
  tokens last about an hour: long runs ask for `offline_access` and renew in memory.
- Migration copies can look healthy and still be damaged: emptied bodies (detected
  with the EWS `item:Preview` and Graph `bodyPreview`), duplicated recipients, and
  attachments where every LF byte became CRLF. Never trust them: replace them.
- EWS `CreateItem` for calendar items: `SendMeetingInvitations="SendToNone"`, elements
  in schema order, local times with `StartTimeZone`/`EndTimeZone` (Windows ids). Read
  each Graph event again with `Prefer: outlook.timezone` in its own zone.
- `GetInboxRules`/`UpdateInboxRules` answer without `ResponseMessages`. Graph returns
  client-only Outlook rules (alerts) with `actions` and `conditions` null.
- The migration also mangled sent dates (day and month swapped, seconds lost, hours
  shifted) and rewrote Message-IDs (`…@gra-omtapp10`). Outlook.com MIME of sent items
  can name the sender `IMCEAEX-…@…prod.outlook.com`. PST messages with only an RTF
  body cannot be rebuilt as MIME.
