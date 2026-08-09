# Contributing

Thanks for looking. datagrep is a small project with one maintainer, so the most
useful thing you can do is make your change easy to verify.

## The gate

```
▶ ./ci/gates.sh
```

That is the whole contract. It runs `cargo fmt --check`, `cargo clippy` with
warnings denied, the workspace tests, `cargo audit` + `cargo deny`, the
anti-pattern greps, and the size/crate-count budgets — and it is the **same
script CI runs**, so "green locally" and "green in CI" mean the same thing.

It must end with the literal line:

```
gates.sh: ALL GATES PASSED
```

Read that line. Do not infer success from a command that happens to exit 0.

For the macOS app:

```
▶ cd ui/macos && ./build-app.sh
```

A ~24 MB bundle means the real engine linked. A ~4 MB one means you got the stub
backend, which opens fine and cannot connect to anything.

> [!NOTE]
> Local `clippy` is often older than CI's, so CI can fail on lints your machine
> never reports. If that happens it is not a flake — read the CI log and fix the
> lint. Note that clippy stops at the first failing crates, so its output is
> never a complete list of violations.

## Tests that need a server

`cargo test --workspace` runs everything that needs no live database. The
per-engine suites sit behind `#[ignore]` and `DATAGREP_TEST_*` environment
variables — see [`notes/testing.md`](notes/testing.md).

Two rules that exist because breaking them cost real time:

- **Tests must never touch the real OS keychain.** Use
  `SecretResolver::in_memory()` (the `test-support` feature) or
  `crate::context::test_ctx()`. A test that writes to the login keychain fails
  outright on Linux CI and quietly litters a developer's Mac on every run.
- **A test asserting a negative needs a positive control.** "No secret reached
  disk" is only meaningful if you have watched the test fail when a secret does.

## Architecture rules

These are enforced by greps in the gate, not just by convention:

- Drivers never see Arrow or the UI.
- `datagrep-core` never names a concrete driver.
- `if driver_id == …` above `datagrep-api` is banned. Such a branch means a
  capability flag is missing — add the flag.
- `datagrep-api` keeps a tiny dependency list on purpose. Adding one is a
  decision, not a detail.

## Commits and pull requests

- Explain **why** in the body when the change is surprising, and skip the body
  entirely when it is not. Length should track how much a future reader will
  need, not be uniform.
- One logical change per commit. A refactor and a behaviour change in the same
  commit are hard to review and harder to revert.
- If you change something a user can see, say how you checked it. "Builds" is
  not the same as "I looked at it".

## Reporting bugs

Include what you ran, what you expected, and what happened — and for UI bugs, a
screenshot. The app prints `MEASURE cold start …` on stderr and can render its
own window headlessly:

```
▶ datagrep.app/Contents/MacOS/datagrep --screenshot /tmp/shot.png 6 --quit-after-shot
```

**Security issues do not go in the issue tracker** — see
[SECURITY.md](SECURITY.md) for private reporting.

## Scope

Reasonable to propose: a new driver, a missing capability flag, correctness
fixes, performance work with a measurement attached.

Please open an issue first for: anything that changes the `datagrep-api` seam,
anything that adds a dependency to `datagrep-api`, and anything that trades
memory for speed — the streaming design is the point of the project, and a
million-row result must never become a million resident rows.
