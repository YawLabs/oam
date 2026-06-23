# Installing oam

The canonical installer and update channel serve from **https://oam.sh**
(see the repo README). These scripts are the source of truth oam.sh serves.

## Linux / macOS

```sh
curl -fsSL https://oam.sh/install.sh | sh
```

## Windows (PowerShell)

```powershell
irm https://oam.sh/install.ps1 | iex
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
| `OAM_INSTALL_BASE` | asset base URL (oam.sh sets this to proxy via CDN) | GitHub Releases |

## Release assets (the naming contract)

`release.yml` cuts a GitHub Release on every `v*` tag with one binary per
target, plus a `SHA256SUMS` manifest. Asset names are exactly:

```
oam-x86_64-pc-windows-msvc.exe
oam-aarch64-pc-windows-msvc.exe      (public-repo runners only, for now)
oam-aarch64-apple-darwin
oam-x86_64-apple-darwin
oam-x86_64-unknown-linux-gnu
oam-aarch64-unknown-linux-gnu        (public-repo runners only, for now)
SHA256SUMS
```

The installers and (forthcoming) `oam self-update` all consume these exact
names. If you change a target triple, change it in `release.yml` and both
install scripts together.

## Signing

Binaries are shipped **unsigned + checksummed** (the @yawlabs distribution
model: scoop/curl/brew fetches bypass Gatekeeper/SmartScreen quarantine, and
the SHA256SUMS manifest is the integrity check). To add Apple notarization /
Windows Authenticode later, insert a signing step in each `release.yml` build
job (there's a documented seam) and supply the cert material as secrets; the
installers don't change.

## Not yet wired

- `oam self-update` (next slice): version-check against the update channel,
  download + verify + atomic self-replace.
- npm package `@yawlabs/oam` (a thin postinstall wrapper that fetches the
  matching binary, esbuild-style).
