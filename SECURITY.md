# Security policy

datagrep is a database client. It holds live credentials, it opens connections
to production systems, and it renders whatever those systems send back. That
makes the cost of a defect here higher than for most tools of its size, so this
document says plainly what is defended, what is not defended yet, and how to
tell us when we got it wrong.

## Reporting a vulnerability

**Use GitHub private vulnerability reporting:**
<https://github.com/chud-lori/datagrep/security/advisories/new>

That is the only supported channel. It gives you a private thread with the
maintainer, a place to attach a proof of concept, and a CVE request if one is
warranted. Please do not open a public issue, and please do not post details in
a pull request or a discussion thread — including a PR that fixes it, since the
diff is the disclosure.

If private reporting is unavailable to you for some reason, open a public issue
containing **only** the sentence "I have a security report and cannot use
private reporting" and no technical detail; the maintainer (@chud-lori) will
open a private channel from there.

### What to expect

datagrep is maintained by one person. These are the commitments that can
actually be kept, rather than the ones that read best:

| Stage | Target |
| --- | --- |
| Acknowledgement that a human has read your report | 3 business days |
| Initial assessment — is it reproducible, how bad, is it in scope | 10 calendar days |
| Fix released, or a public advisory with a workaround | 90 days from report |

If 7 days pass with no acknowledgement, assume the notification was missed
rather than ignored, and escalate using the public-issue sentence above.

Reports are handled under coordinated disclosure: we will agree a date with you
before anything is published, and we would rather publish an advisory with a
workaround than let a deadline slip silently. You will be credited in the
advisory by whatever name or handle you choose, unless you ask not to be.

### Supported versions

datagrep is pre-1.0. Only the most recent release receives security fixes;
there are no maintained backport branches. Report against `main` if you can.

## Threat model

The point of this section is to make a real review possible: to state which
inputs are assumed hostile, so that a finding can be argued against a written
boundary instead of against someone's intuition.

Three inputs are treated as fully attacker-controlled. Everything else — your
own filesystem, your keychain, the code you built — is treated as trusted, and
the "out of scope" section at the end says why.

### 1. A hostile database server

**The attacker controls:** every byte that arrives after the TCP connect. The
protocol handshake, the authentication challenge, whether TLS is offered at
all, result-set metadata (column count, declared types, declared sizes), row
payloads, error strings, and how much of any of it there is.

Note the asymmetry that makes this real: you do not have to be tricked into
connecting to an attacker's server. A legitimate server that has *already* been
compromised is the same input, and it is the one you have credentials for.

**Reaches:** the per-driver protocol decoders (`datagrep-drv-*`), the result
pipeline in `datagrep-core`, and then the FFI cell and row accessors that carry
those values across the C ABI into the Swift UI.

**What is in place:**

- Every FFI entry point wraps its body in `catch_unwind`
  (`crates/datagrep-ffi/src/ffi_util.rs`). A Rust panic unwinding across the C
  ABI is undefined behaviour, so this is load-bearing, not defensive style.
- Exactly one TLS stack. Every driver is rustls + ring, and `deny.toml` bans
  `native-tls`/`openssl`/`openssl-sys` so a transitive default feature cannot
  quietly add a second one with different defaults.
- Read-only mode reports whether it is *server-enforced* or *client-side*
  (`ReadOnlyEnforcement`) rather than claiming a guarantee the server is not
  actually making.

**What is not in place yet** (tracked in
[issue #5](https://github.com/chud-lori/datagrep/issues/5)):

- The panic surface reachable from a malformed server response has not been
  triaged. The Tier-1 gate counts `unwrap`/`expect`/`panic!` but does not yet
  distinguish "impossible state" from "the server said the column count was
  4 billion". A panic inside an FFI entry point is contained by `catch_unwind`;
  a panic in a driver task can still take a connection down mid-query.
- The decoders are not fuzzed.
- There is no per-driver test proving a plaintext downgrade cannot happen
  silently.

### 2. A hostile connection URL

**The attacker controls:** scheme, host, port, path, userinfo, and every query
parameter. This input arrives by paste far more often than by attack — from a
chat message, a wiki page, a colleague, a shared profile bundle — which is
exactly why it is in the model. A connection URL is executable configuration
that most people skim.

**Reaches:** driver selection (`driver_for_url`), per-driver config parsing,
and then all of section 1, because the URL is what decides which server you are
talking to in the first place.

**The concerns, specifically:**

- **Credential handover.** The URL names the host. Connect to the attacker's
  host with your real credentials and you have sent them your credentials —
  no memory-safety bug required. This is the highest-likelihood item in this
  entire document.
- **Silent TLS downgrade.** `sslmode=disable`, `tls=false` and their per-driver
  equivalents are ordinary query parameters, and a long URL hides one easily.
- **Filesystem reach.** Some driver parameters name local paths (client
  certificates; for SQLite the URL *is* a path, and opening it may create a
  file).

**What is in place:** profiles reject secret-shaped configuration keys
(`validate_no_secrets`), so credentials pulled out of a URL cannot be silently
persisted into the profile store as an ordinary field — secrets are reachable
only through a `secret_ref`.

**What is not in place yet:** nothing warns you that a URL you pasted turns TLS
off. Until that exists, read connection URLs before using them, and look at the
query string specifically.

### 3. A hostile profile import

`datagrep profiles export` produces plain TOML that is deliberately safe to
commit to git — `Folder`, `Profile` and `Tunnel` have no field capable of
holding a secret, so exports are secret-free structurally rather than by a
filter someone has to remember to run. That property is about *export*. Import
is the dangerous direction, and it is the sharpest edge in this document.

**The attacker controls:** the entire TOML document — every profile, every
tunnel definition, and every `secret_ref` string.

> **A profile bundle is executable. Treat one exactly as you would treat a
> shell script from the same source.**
>
> A profile's secret can be declared as `exec:<command line>`, which the
> resolver runs through `/bin/sh -c` and reads trimmed stdout from
> (`crates/datagrep-secrets/src/resolver.rs`). This is a real and useful
> feature — it is how `op read …` and `aws rds generate-db-auth-token …` work
> — but it means an imported profile carrying an `exec:` reference runs the
> importer's chosen command, as you, with your environment, the first time
> that profile is connected. Nothing at import time currently flags or
> confirms this.
>
> Before importing a bundle you did not write: read it, and grep it for
> `exec:` and `env:`.

Two further properties of import worth knowing:

- `ImportStrategy::Replace` deletes profiles that are not in the incoming file.
  A bundle can therefore remove your existing entries, not only add its own.
- A bundle can define an SSH tunnel pointing at a jump host of its choosing.
  Host-key trust is TOFU (`~/.datagrep/known_hosts`), so the first connection
  to that host is the trust decision. The store is built so that a dropped
  prompt counts as *reject* rather than silent acceptance, and a store with no
  listener fails closed — but a user who clicks through the prompt has pinned
  the attacker's key.

**Not in place yet:** an import-time confirmation for `exec:` references, and a
warning on `Replace`. Both are open work; until then the boxed rule above is
the mitigation.

### Supply chain

Dependencies are the fourth untrusted input, and the one that changes without
anyone touching the repo. `deny.toml` is the written policy —
`cargo audit` and `cargo deny` run in the Tier-1 gate on every pull request and
weekly against an unchanged tree, advisories fail the build whether or not a
fix exists, only crates.io is an allowed source, and every accepted exception
carries its reasoning and a review-by date in that file. Read `deny.toml`
before changing anything in `ci/gates.sh`.

### Out of scope

Not because these do not matter, but because datagrep cannot defend against
them and saying otherwise would be dishonest:

- **An attacker who already runs code as your user.** They can read your
  keychain, your profile store and your shell history through the OS, with or
  without datagrep. Local secret hygiene defends against accidental disclosure
  — a password in a git-committed export, a credential in a log — not against
  a local attacker.
- **The security of the servers you connect to.** SQL injection in *your*
  queries against *your* database is your database's problem; datagrep sends
  what you type.
- **Physical access to an unlocked machine.**
- **Gatekeeper warnings on locally built binaries.** Development builds are
  ad-hoc signed; release signing and notarization are tracked in issue #5 and
  are a distribution gap, not a vulnerability report.
