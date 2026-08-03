# Security policy

## Reporting a vulnerability

Report privately through **GitHub's private vulnerability reporting** on this
repository: the **Security** tab, then **Report a vulnerability**. That channel
is private to the maintainers and is the one to use — do not open a public
issue for anything exploitable.

<!-- TODO(maintainer): add security@oamjs.org here once oamjs.org mail is
     actually provisioned. It is deliberately absent rather than aspirational:
     a security policy that publishes an address nobody receives is worse than
     one that publishes none. -->


Include what you need to make it reproducible: the oam version
(`oam --version`), the platform, and a minimal script. If you have a working
proof of concept, send it — it shortens triage more than a description does.

You will get an acknowledgement within 3 working days. We aim to ship a fix
or give you a dated plan within 30 days, and we will tell you before the fix
goes public so the disclosure is not a surprise. If you want credit in the
release notes, say so; if you want to stay anonymous, that is the default.

## What counts as a vulnerability

oam runs untrusted-ish JavaScript on a V8 isolate with a Node-compatible
surface. In scope:

- Escaping the permission model (see below) — reading, writing, or dialling
  out when the corresponding permission is denied.
- Memory-safety failures reachable from JavaScript: a crash in the Rust or
  V8 layer that a script can trigger, and anything that looks like a
  read/write past a buffer.
- Module-resolution attacks: a specifier that loads a file outside the
  intended tree, lockfile handling that fetches something other than what
  was pinned.
- Anything in the release pipeline that would let a third party ship a
  binary users would accept as ours.

Not in scope:

- A script doing something destructive when run **without** `--permission`.
  That is the documented default: oam without the permission flag has the
  same authority as `node`, i.e. full access to the user's machine.
- Denial of service by a script that is simply allowed to run — infinite
  loops, deliberate memory exhaustion, `process.exit`.
- Node-compatibility divergences with no security consequence. Those are
  ordinary bugs; file them publicly.

## The permission model

`oam --permission` denies filesystem reads, filesystem writes, network
access, and environment access unless explicitly granted. The checks are
enforced in the native ops (`crates/oam_engine/src/permissions.rs`), not in
JavaScript, so monkey-patching `fs` does not route around them. A denial
throws Node's `ERR_ACCESS_DENIED`, carrying `permission` and `resource`.

Two things to be clear about, because the difference matters if you are
relying on this:

- **The default is no restriction.** Without `--permission`, every check
  returns granted. This matches Node, and it means the flag is opt-in
  hardening, not a sandbox that is on by default.
- **It is a permission model, not an isolate escape boundary.** It gates the
  host operations oam exposes. It is not a defence against a V8 renderer
  exploit, and it does not make it safe to run genuinely hostile code. If
  you need that, run oam inside an OS-level sandbox as well.

## V8 and upstream security updates

oam embeds V8 via the `v8` crate (currently `150.0.0`; see `Cargo.lock` for
the exact pin of any given build). V8 security fixes reach oam when we bump
that crate and cut a release — we do not carry our own V8 patches.

If you are tracking a specific CVE, `oam --version` plus `Cargo.lock` from
the matching tag tells you exactly which V8 you have.

## Release integrity

Release binaries are currently **unsigned**, published with a `SHA256SUMS`
file alongside the assets. Verify your download against it:

```
sha256sum -c SHA256SUMS --ignore-missing
```

A checksum file served from the same place as the binaries protects against
corruption and mirror tampering, not against someone who has compromised the
release channel itself. Code signing is a planned addition; until it lands,
treat the checksums as integrity, not authenticity.

## Supported versions

Fixes land on the latest release. There is no long-term-support branch —
upgrade to pick up security fixes.
