# Contributing

Thanks for your interest. Read [docs/plan.md](docs/plan.md) first: it describes the
design and the milestones. [CHANGELOG.md](CHANGELOG.md) says what each release
brought.

## Build and test

You need [rustup](https://rustup.rs): `rust-toolchain.toml` selects the stable toolchain
with rustfmt and clippy. Before committing, run the same checklist as CI:

```
.\scripts\build.ps1            # fmt, clippy, rustdoc, tests, doc links, no mailbox data tracked
.\scripts\build.ps1 build      # the same, then a release build
.\scripts\build.ps1 release    # from a clean tree: the same, then the zip in dist\
```

Warnings are errors, in clippy and in rustdoc.

## Layout

| Path | Content |
|---|---|
| `crates/ecm-model` | Common model: item records, folders, property comparison, folder paths |
| `crates/ecm-pst` | Read-only PST source, and rebuilding its messages as MIME |
| `crates/ecm-http` | HTTP transport with NTLM/Negotiate, Credential Manager |
| `crates/ecm-ews` | EWS client for the target mailbox |
| `crates/ecm-graph` | Read-only Microsoft Graph source (Outlook.com) |
| `crates/ecm-store` | Local state: JSON Lines inventories, CSV reports |
| `crates/ecm-audit` | Matching, defects, plans, verification |
| `crates/ecm-cli` | The `ecm` command-line tool |

## Rules

- **Never commit mailbox data**: no real PST, MSG or EML files, no reports, no logs from
  a real mailbox. Test data must be synthetic. `.gitignore` blocks the usual file types,
  but check your diff anyway.
- **Never write to a source.** Code that reads a PST or a source mailbox must not be able
  to modify it.
- No hard delete on a target: items go to a quarantine folder, which leaves only
  through `plan-discard`, into Deleted Items, for the user to empty.
- Credentials never appear in code, files, logs or command-line arguments.
- `unsafe` code is forbidden everywhere except `ecm-http`, where it is limited to calls
  into the Windows API, each with a `// SAFETY:` comment.
- Prefer official Microsoft crates (`outlook-pst`, `windows`) when they cover the need.

## Commits and pull requests

- One logical change per commit, with a message that says what and why.
- Pull requests must pass CI. Add tests for new behavior.
