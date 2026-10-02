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

Both scripts: detect your OS/arch, resolve the release tag once, **verify the
release** (see [Verification](#verification)), download the matching binary
and check it against the verified checksums, install it to a per-user dir
(`~/.oam/bin` or `%LOCALAPPDATA%\oam\bin`), and put it on PATH. No admin/sudo.
Re-running upgrades in place.

### Overrides (env vars)

| Var | Effect | Default |
|-----|--------|---------|
| `OAM_VERSION` | install a specific tag, e.g. `v0.18.0` | latest |
| `OAM_INSTALL_DIR` | install location | `~/.oam/bin` / `%LOCALAPPDATA%\oam\bin` |
| `OAM_INSTALL_BASE` | asset base URL, for a mirror or CDN. Requires `OAM_VERSION`: the signature is checked against the tag you asked for, and a mirror cannot be asked which tag it serves | GitHub Releases |
| `OAM_INSECURE_SKIP_SIGNATURE` | `1` installs a `v0.18.0`+ release on a host with no usable `ssh-keygen`, without checking its signature (loud). It skips only a missing tool: a bad signature or a missing manifest still fails | unset |

## Verification

Every release from **`v0.18.0`** on carries `RELEASE-MANIFEST` (the line
`oam-release-manifest v1`, the line `tag <tag>`, then the `SHA256SUMS` lines
verbatim) and `RELEASE-MANIFEST.sig`, an SSH signature over it (namespace
`oam-release`). For such a tag the installers:

1. verify the signature with `ssh-keygen -Y verify` against the release keys
   **embedded in the script** (a copy of `release-keys/allowed_signers`; a key
   list fetched from the release would prove nothing);
2. require the manifest's tag line to be exactly the tag being installed (no
   replaying an old release as the latest), and the signing key's range in the
   embedded `release-keys/ranges` to cover it (a retired or staged key signs
   nothing);
3. take the binary's sha256 from the signed manifest and check the download
   against it.

A signature that is present but does not verify always fails, and so does a
`v0.18.0`+ release with no manifest. There is no override for either.

Releases **before `v0.18.0`** were cut before signing existed. For those, the
SHA-256 of the release's published `SHA256SUMS` must equal the value pinned in
the embedded `release-keys/presigning-sums` (captured 2026-10-02 from the
published releases; bounded and immutable). The binary's sha256 comes from that
file. A pre-`v0.18.0` tag that is not pinned is refused. Because of the pins,
`scripts/release-upload-local-arm64.sh` refuses to patch a pinned release: a
changed `SHA256SUMS` would break every install of it. Only main's copy has that
check: the copy in a pinned tag's own tree predates it, so never run that one.

The embedded copies are byte-identical to `release-keys/*`, and
`scripts/test-scripts.sh` fails on drift. Change a key file, then paste it into
both scripts verbatim. There is no environment variable that points an
installer at a different key set, pin table or cutoff.

### ssh-keygen

`v0.18.0`+ installs need an `ssh-keygen` with `-Y` (OpenSSH 8.1 or later). The
installers probe the tool itself, not its version string. When it is missing
or too old, a `v0.18.0`+ install stops and names the fix (or
`OAM_INSECURE_SKIP_SIGNATURE=1`); a pre-`v0.18.0` install does not need it and
only notes that later ones will.

| Host | Fix |
|------|-----|
| Debian / Ubuntu | `apt-get install openssh-client` |
| Alpine | `apk add openssh-keygen` |
| Fedora / RHEL 9 / Amazon Linux | `dnf install openssh` (RHEL/Alma/Rocky 8 ship OpenSSH 8.0, which has no `-Y`) |
| macOS | ships it |
| Windows | `Add-WindowsCapability -Online -Name OpenSSH.Client~~~~0.0.1.0` (elevated) |

`install.ps1` looks for `ssh-keygen.exe` in `%windir%\Sysnative\OpenSSH` (a
32-bit PowerShell on 64-bit Windows sees `System32` redirected), then
`%windir%\System32\OpenSSH`, then `PATH`.

Measured (2026-10-02, by `scripts/test-scripts.sh` on Windows 11 ARM64): the
inbox client (`OpenSSH_for_Windows_9.5p2`) verifies for `install.ps1` from both
64-bit Windows PowerShell 5.1 (through `System32`) and 32-bit (through
`Sysnative`), and Git for Windows' OpenSSH 10.2p1 verifies for `install.sh`.

**Not measured yet:** `ssh-keygen -Y` availability in the `debian:*-slim`,
`ubuntu`, `alpine`, `amazonlinux` and RHEL 8 UBI container images, and in the
Windows 10 inbox client (8.1p1). The box this change was made on had no running
Docker daemon, so those results are not recorded here rather than guessed. The
expectation, unverified: slim and minimal images ship no `openssh-client` at
all, so a `curl | sh` inside one needs the package (or the skip variable), and
RHEL 8 UBI's OpenSSH 8.0 has no `-Y`.

Also not measured: `install.sh`'s wget branch (used when there is no curl, as
on a bare `alpine` with busybox wget). `scripts/test-scripts.sh` runs it
against a local HTTP stand-in for github.com, `/releases/latest` redirect
included, but only where wget is installed. The box this change was made on
has none, so that case was skipped there. Resolving the latest tag relies on
`wget -S --spider` printing each hop's `Location:` header. That is GNU wget's
behavior and is expected of busybox's too, but busybox has not been checked.
`OAM_VERSION=vX.Y.Z` skips that step entirely.

### Not checked yet

The binaries themselves are not code-signed: no Apple Developer ID, no
Authenticode. Each installer has an inactive, clearly marked hook for that
check (`macos_signature_check` in `install.sh`: `codesign --verify --strict`
plus a pinned TeamIdentifier; `Test-OamAuthenticode` in `install.ps1`), turned
on by setting the first signed tag when that signing goes live.

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
RELEASE-MANIFEST                     (v0.18.0 on)
RELEASE-MANIFEST.sig                 (v0.18.0 on)
```

The installers and `oam self-update` all consume these exact
names. If you change a target triple, change it in `scripts/release-local.sh`
(+ `scripts/build-remote.sh`) and both install scripts together.

## Signing

The release is signed (the manifest above, `scripts/lib/signing.sh`;
`release-keys/README.md` is the runbook). The binaries are not yet: scoop,
curl and brew fetches bypass Gatekeeper/SmartScreen quarantine, and the signed
manifest is the integrity check. Apple notarization and Windows Authenticode
are planned; when they land, their checks turn on in the installers' marked
hooks (see [Not checked yet](#not-checked-yet)).

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
