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
- `ecm-store`, `ecm-audit`: empty for now.
- `ecm-cli`: the `ecm` binary.

## Rules

- Never write to a source. Never call `outlook_pst::open_store`: it opens the file
  for writing. Use `ecm_pst::PstSource`, which opens the file read-only and shares
  read access only.
- On a target: no hard delete (quarantine instead), and changes only through
  `plan`/`apply`.
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
- Commit only when asked. Messages say what and why, ending with the
  `Co-Authored-By` line.

## Gotchas

- `quick-xml` 0.42: text comes from `xml10_content()`, and entities arrive as
  separate `GeneralRef` events.
- `windows` 0.62: `WINHTTP_AUTH_SCHEME_*` are newtypes: use `.0`.
- OVH EWS: Exchange 2019 (15.2), NTLM/Negotiate only. Send
  `RequestServerVersion` `Exchange2013_SP1`.
- Read-only probing from an OWA session: POST to `/owa/service.svc?action=X&EP=1`
  with the `X-OWA-CANARY` header (from the cookie) and the JSON body in
  `X-OWA-UrlPostData`. Opening a mail in OWA marks it read: avoid that.
