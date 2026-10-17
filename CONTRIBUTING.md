# Contributing

Thanks for your interest. The project is at an early stage: read
[docs/plan.md](docs/plan.md) first, it describes the design and the milestones.

## Build and test

You need a stable Rust toolchain (minimum supported version: 1.85).

```
cargo build
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets
```

CI runs the same checks on Windows and Linux and treats warnings as errors.

## Layout

| Path | Content |
|---|---|
| `crates/ecm-model` | Common model: folders, items, property bags, fingerprints |
| `crates/ecm-pst` | Read-only PST source |
| `crates/ecm-http` | HTTP transport with NTLM/Negotiate |
| `crates/ecm-ews` | EWS client |
| `crates/ecm-store` | Local SQLite state |
| `crates/ecm-audit` | Matching, defects, reports |
| `crates/ecm-cli` | The `ecm` command-line tool |

## Rules

- **Never commit mailbox data**: no real PST, MSG or EML files, no reports, no logs from
  a real mailbox. Test data must be synthetic. `.gitignore` blocks the usual file types,
  but check your diff anyway.
- **Never write to a source.** Code that reads a PST or a source mailbox must not be able
  to modify it.
- No hard delete on a target outside the explicit `quarantine purge` command.
- Credentials never appear in code, files, logs or command-line arguments.
- `unsafe` code is forbidden everywhere except `ecm-http`, where it is limited to calls
  into the Windows API, each with a `// SAFETY:` comment.
- Prefer official Microsoft crates (`outlook-pst`, `windows`) when they cover the need.

## Commits and pull requests

- One logical change per commit, with a message that says what and why.
- Pull requests must pass CI. Add tests for new behavior.
