# Installing oam

The canonical installer and update channel serve from **https://oamjs.org**
(see the repo README). These scripts are the source of truth oamjs.org serves.

## Linux / macOS

```sh
curl -fsSL https://oamjs.org/install.sh | sh
```

## Windows (PowerShell)

```powershell
irm https://oamjs.org/install.ps1 | iex
```

Both scripts: detect your OS/arch, download the matching release binary from
GitHub Releases, **verify it against the published `SHA256SUMS`**, install it to
a per-user dir (`~/.oam/bin` or `%LOCALAPPDATA%\oam\bin`), and put it on PATH.
No admin/sudo. Re-running upgrades in place.

### Overrides (env vars)

| Var | Effect | Default |
|-----|--------|---------|
| `OAM_VERSION` | install a specific tag, e.g. `v0.7.0` | latest |
| `OAM_INSTALL_DIR` | install location | `~/.oam/bin` / `%LOCALAPPDATA%\oam\bin` |
| `OAM_INSTALL_BASE` | asset base URL (oamjs.org sets this to proxy via CDN) | GitHub Releases |

## Release assets (the naming contract)

`scripts/release-local.sh` cuts a GitHub Release for every pushed `v*` tag
(GitHub-Actions-free: it builds locally and on the remote build hosts — see
its header) with one binary per target, plus a `SHA256SUMS` manifest. Asset
names are exactly:

```
oam-x86_64-pc-windows-msvc.exe
oam-aarch64-pc-windows-msvc.exe
oam-aarch64-apple-darwin
oam-x86_64-apple-darwin
oam-x86_64-unknown-linux-gnu
oam-aarch64-unknown-linux-gnu        (not yet shipped -- needs an ARM Linux build host)
SHA256SUMS
```

The installers and `oam self-update` all consume these exact
names. If you change a target triple, change it in `scripts/release-local.sh`
(+ `scripts/build-remote.sh`) and both install scripts together.

## Signing

Binaries are shipped **unsigned + checksummed** (the @yawlabs distribution
model: scoop/curl/brew fetches bypass Gatekeeper/SmartScreen quarantine, and
the SHA256SUMS manifest is the integrity check). To add Apple notarization /
Windows Authenticode later, insert a signing step in `scripts/release-local.sh`
where each binary lands in the release dir (there's a documented seam) and
supply the cert material locally; the installers don't change.

## Updating

```sh
oam self-update              # update in place to the latest release
oam self-update --version v0.7.0   # pin a specific tag
oam self-update --dry-run    # verify the release, print what would change
```

`oam self-update` is native (crates/oam_cli/src/self_update.rs). It resolves
the tag once, verifies the release's `RELEASE-MANIFEST.sig` against the keys in
`release-keys/` (compiled into the binary), checks the binary's sha256 against
the signed manifest, and renames it over the running oam (on Windows, the
running exe moves aside to `oam.exe.old` first, as `install.ps1` does). A
release before v0.18.0 has no signature; its `SHA256SUMS` must match the digest
pinned in `release-keys/presigning-sums`. Without `--version` it never installs
an older release than the running one. Any failure leaves the installed binary
as it was. It updates oam where it currently lives, or `$OAM_INSTALL_DIR/oam`
when that is set. `OAM_SELF_UPDATE_URL` (with `--version`) fetches the
release's files from another base; they must still verify.

## Not yet wired

- npm package `@yawlabs/oam` (a thin postinstall wrapper that fetches the
  matching binary, esbuild-style).
