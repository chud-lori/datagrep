# dbx

A lightweight, multi-paradigm database client (SQL + NoSQL) built to a published
resource budget — *the client you don't have to close to get your laptop back.*

Full design: [`../dbx-design.md`](../dbx-design.md). The performance budget lives
in [`budget.toml`](budget.toml) (machine-readable transcription of design §5, for CI).

## Workspace layout

```
dbx/
  budget.toml        performance budget (design §5) as TOML, for future CI gates
  crates/
    dbx-api/         THE STABLE SEAM (design §3.1): Driver / Connection / Cursor /
                     Catalog / Value / Shape / Capabilities. No tokio, no Arrow,
                     no reqwest — ~5 small deps by rule.
    ...              dbx-core, drivers, frontends slot in here (built separately;
                     the workspace globs crates/*)
```

Crate rules that keep it honest (design §3): drivers never see Arrow or the UI;
`dbx-core` never names a concrete driver; no `if driver_id == …` above `dbx-api` —
any such branch is a missing capability flag.

## Build & test

```
cargo build
cargo test
cargo clippy -- -D warnings
```
