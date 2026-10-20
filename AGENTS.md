# AGENTS.md

`ecm`: Rust CLI that audits and repairs an Exchange mailbox after a botched
migration. Sources (read-only): a PST file, and an Outlook.com mailbox through
Microsoft Graph; Exchange over EWS as a source is planned (M7). Targets: Exchange over
EWS, and Outlook.com through Graph (messages; other types M9b). Public repo: no
personal data. Roadmap and design: [docs/plan.md](docs/plan.md) (milestones, mark ✅
when done). What can be migrated: [docs/supported-migrations.md](docs/supported-migrations.md).
Releases: [CHANGELOG.md](CHANGELOG.md).

## Layout

- `ecm-model`: item records, folder summaries, property comparison, folder paths.
- `ecm-pst`: read-only PST on Microsoft's `outlook-pst`; full message content and
  its rebuilding as MIME.
- `ecm-http`: WinHTTP + NTLM/Negotiate, Credential Manager. Only crate with `unsafe`.
- `ecm-ews`: hand-written SOAP + `quick-xml` tree: folders, items, export (backup),
  details, calendar items, contacts and tasks (listings and whole items), the
  category list; every change in `write`.
- `ecm-graph`: Microsoft Graph (Outlook.com): device code sign-in, mail, calendar,
  contacts, To Do, inbox rules, categories as a source; as a target (messages M9,
  the other types M9b in progress), every change in `write`.
- `ecm-store`: JSON Lines inventories, CSV. `ecm-audit`: matching, defects, plans,
  verification.
- `ecm-cli`: the `ecm` binary, one module per command family.

## Rules

- Never write to a source. Never call `outlook_pst::open_store`: it opens the file
  for writing. Use `ecm_pst::PstSource`, which opens the file read-only and shares
  read access only.
- On a target: no hard delete (quarantine instead), and changes only through
  `plan`/`apply`, with the user's go-ahead for each `apply` run. Quarantine run
  folders leave only through `plan-discard`, into Deleted Items: the user empties
  it. The exceptions are the trials `ecm fidelity-test`, `ecm fidelity-graph` and
  `ecm fidelity-graph-pim`, which only create items, only in `ecm-test` containers
  (and rules, disabled, and categories named `ecm-test …`), each run with the user's
  go-ahead.
- Plans only move items `ecm` put where they are: migration debris at the top of the
  target mailbox, and items an applied plan moved (`--after`, `plan-park`).
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
- A change that adds, extends or limits what can be migrated (an item type, a source,
  a target, a limit) updates [docs/supported-migrations.md](docs/supported-migrations.md)
  in the same pull request. A test fails when the page names a command or flag that
  does not exist.
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
- OVH EWS refuses requests near 100 MB (HTTP 413): messages over 25 MB are created
  without their files, then one `CreateAttachment` each. A `CreateAttachment` right
  after another on the same item can answer `ErrorIrresolvableConflict` (change key),
  through EWS and through Graph upload sessions alike: it passes after a pause.
- Graph as a target: a message created from MIME stays a draft, gets the current
  time as received date, and has addresses of the account's aliases rewritten to
  its primary one. Create it from JSON instead, with `singleValueExtendedProperties`
  set at creation: `Integer 0x0E07` (flags, read and not a draft), `SystemTime
  0x0E06` and `0x0039` (received, sent), `String 0x007D` (Internet headers). A file
  sent inline (with a Content-ID) is hidden from the files: inline only when the
  HTML shows it (`cid:`). Requests take about 4 MB: larger files go through upload
  sessions; Outlook.com refuses messages past about 40 MB. A device code expires
  after 15 minutes. Graph pages a folder's messages by offset: a message removed
  during the listing makes another slip between pages, so inventories read the
  ids again.
- EWS `CreateItem` for calendar items: `SendMeetingInvitations="SendToNone"`, elements
  in schema order, local times with `StartTimeZone`/`EndTimeZone` (Windows ids). Read
  each Graph event again with `Prefer: outlook.timezone` in its own zone. The other
  way, Exchange 2016+ `AllProperties` gives `StartWallClock` and `StartTimeZoneId`
  (UTC can be `tzone://Microsoft/Utc`): Graph takes them as is.
- Graph as a target for other types: an event created with attendees invites them,
  so they are left out. A contact photo is `PUT` on
  `/contactFolders/{id}/contacts/{id}/photo/$value` and comes back byte for byte.
  Test rules go disabled and after the mailbox's own (`sequence`). The EWS master
  category list is the calendar folder's `CategoryList` user configuration (XML,
  base64); Outlook's color index *n* is Graph's `preset`*n*, -1 is `none`.
- Outlook.com can move a contact into a folder Graph cannot open when its photo is
  set: list a folder, then read the copies a plan made by the ids in its journal.
  Graph pages events and contacts by offset too: read the ids again. It gives an
  all-day event's days as set, in UTC: compare such events by day.
- A debug build of `ecm` needs more than the 1 MB main thread stack Windows gives:
  `main` runs the command on a 16 MB thread.
- `GetInboxRules`/`UpdateInboxRules` answer without `ResponseMessages`. Graph returns
  client-only Outlook rules (alerts) with `actions` and `conditions` null.
- The migration also mangled sent dates (day and month swapped, seconds lost, hours
  shifted) and rewrote Message-IDs (`…@gra-omtapp10`). Outlook.com MIME of sent items
  can name the sender `IMCEAEX-…@…prod.outlook.com`. A PST message with only a
  compressed RTF body is rebuilt from the HTML or text the RTF encapsulates
  ([MS-OXRTFEX]), else from the RTF's text; a PST `PR_HTML` can be `Null`, and a
  message with no body at all (only attachments) is still rebuilt.
