# Release keys

Every oam release from the first signed one on carries two extra assets next
to the binaries and `SHA256SUMS`:

- `RELEASE-MANIFEST`: the line `oam-release-manifest v1`, the line
  `tag <tag>`, then the release's `SHA256SUMS` bytes, verbatim.
- `RELEASE-MANIFEST.sig`: an SSH signature over it (`ssh-keygen -Y sign`,
  namespace `oam-release`), made with an ed25519 release key.

`SHA256SUMS` alone proves a download matches its release. The signature proves
the release came from the holder of a release key, and the `tag` line stops an
old, correctly signed release from being served as a newer one.

This directory holds the public half of each key and the tags each key may
sign. Nothing secret is ever committed here.

| File | Contents |
|---|---|
| `allowed_signers` | One line per key: `oam-release-<id> namespaces="oam-release" ssh-ed25519 <base64>` |
| `ranges` | One line per key that may sign: `<id> <from-tag> <to-tag or ->`, both ends inclusive |
| `presigning-sums` | One line per release cut before signing existed (tags before `v0.18.0`): `<tag> <sha256 of its published SHA256SUMS>`. Bounded and immutable; a pre-`v0.18.0` tag not listed is refused |

`scripts/lib/signing.sh` implements signing and verification. It checks
`allowed_signers` and `ranges` for format before it trusts them. The three
files are the trust root of every verifier: `oam self-update` compiles in
copies of them, `install/install.sh` and `install/install.ps1` embed
byte-identical copies (`scripts/test-scripts.sh` fails on drift, so a change
here means pasting the file into both installers), and all three enforce them
on every release they install, from `v0.18.0` by signature and before it by
the pinned digest.

## Current keys

| Key | Fingerprint | Range |
|---|---|---|
| `k1` | `SHA256:zB7Aq4Ky/U90VJ4sAEp0e2A65KfQpiyQXJI4FuT2oss` | `v0.18.0` on |
| `k2` | `SHA256:Uy7nugF5mDzfM/8/fcUti9K+sQMbn/LdRAk+sIbWYs4` | none yet (staged) |

Key lines are committed, so **every release must sign.** No knob skips the
manifest step. (Before any key line existed, `scripts/release-local.sh`
skipped the manifest step with a loud warning; releases up to `v0.17.1` are
unsigned.)

The same fingerprints are published in SECURITY.md, "Release integrity".

## What a range means to a verifier

A verifier accepts a manifest only when the key that signed it has a line in
`ranges` covering the tag inside the signed manifest.

- A key with no `ranges` line is **staged**: its public half is committed
  ahead of time, and it verifies nothing until a range opens for it.
- A key is **retired** by closing its range at the last tag it signed, for
  example `k1 v0.18.0 v0.21.3`. Its `allowed_signers` line is never deleted,
  so the tags it signed keep verifying, and nothing it signs for a later tag
  is accepted.
- A key is **revoked** the same way: its range is closed at its last
  known-good tag, and every verifier that carries the updated `ranges`
  refuses anything it signed for a later tag.

The installers and `oam self-update` carry embedded copies of these files, so
a copy learns a changed range only when the installer script or the oam binary
that carries it is replaced.

## Verifying a release by hand

```sh
gh release download vX.Y.Z --repo YawLabs/oam \
  --pattern SHA256SUMS --pattern RELEASE-MANIFEST --pattern RELEASE-MANIFEST.sig
ssh-keygen -Y verify -f release-keys/allowed_signers -I oam-release-k1 \
  -n oam-release -s RELEASE-MANIFEST.sig < RELEASE-MANIFEST
head -2 RELEASE-MANIFEST                      # oam-release-manifest v1 / tag vX.Y.Z
tail -n +3 RELEASE-MANIFEST | cmp - SHA256SUMS
```

You also need to check that the signing key's line in `ranges` covers the tag.
`release_verify_manifest` in `scripts/lib/signing.sh` does all of these checks
in one call.

## Windows Authenticode

The release key above signs the release. The two Windows binaries are also
signed one by one, with Authenticode, through Azure Artifact Signing. Each
certificate is issued to the validated publisher name and is valid for three
days, so every signature carries an RFC 3161 timestamp from
`http://timestamp.acs.microsoft.com`.

A released Windows binary has:

- an embedded signature with status `Valid`;
- a timestamp;
- the publisher as the signer's CN and O;
- "Microsoft ID Verified Code Signing PCA 2021" in the certificate chain.

The release checks each of these from disk, with `signtool verify /pa` and
with `scripts/lib/verify-authenticode.ps1`, before `SHA256SUMS` is written.

To check a downloaded binary by hand, in PowerShell:

```powershell
Get-AuthenticodeSignature .\oam-x86_64-pc-windows-msvc.exe | Format-List Status, SignerCertificate, TimeStamperCertificate
```
