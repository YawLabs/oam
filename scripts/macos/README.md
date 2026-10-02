# Mac signing

The macOS release leg (`scripts/build-remote.sh mac-release`, run on the build
Mac by `scripts/build-platforms-tailnet.sh`) codesigns both binaries,
`oam-aarch64-apple-darwin` and `oam-x86_64-apple-darwin`. It signs each one
right after it is copied into `dist/` and before anything runs it:

1. **Sign** with the hardened runtime, the identifier `org.oamjs.oam` and the
   three entitlements in `oam.entitlements.plist` (JIT, unsigned executable
   memory, and disabled library validation for opt-in native addons).
2. **Verify** with `codesign --verify --strict`, then check the identifier, the
   runtime flag, the exact entitlement set and the signer against the pin.
3. **JIT smoke**: run `scripts/fixtures/jit-smoke.js` against the signed binary
   (x86_64 under Rosetta). A missing entitlement does not stop `--version`; it
   kills the process at its first JIT.

The leg then writes `dist/mac-sha256.txt`. The release box compares the binaries
it pulled against that file before they reach `SHA256SUMS`.

`scripts/lib/mac-signing.sh` holds the logic, and its header explains why it
works this way.

## Which signature

| `scripts/mac-signing-identity.sha1` | Result |
|---|---|
| No SHA-1 (bootstrap) | Ad-hoc signature, still with the hardened runtime and entitlements. The release warns. `OAM_SIGN_REQUIRED=1` turns this into a failure. |
| A SHA-1 | Signed with that certificate. If the build Mac cannot sign with it, the release fails, before anything is tagged. |

`OAM_SKIP_MAC_SIGN=1` skips signing for one release, with a loud warning. It is
honored even when `OAM_SIGN_REQUIRED=1` is set.

Today's identity is a **self-signed** certificate, "oam Code Signing
(self-signed)". It is cosmetic: `codesign -dv` names a signer instead of
"adhoc". Gatekeeper treats it like an ad-hoc signature, so a quarantined browser
download is still blocked. That changes with a Developer ID Application
certificate plus notarization. The switch is a new identity in the build
keychain and its SHA-1 in the pin file; the signing code already picks a secure
timestamp and the Developer ID verify rules from the certificate it finds.
Notarization is a separate step that has not been added yet.

## Provisioning the identity (once, on the build Mac)

The identity lives in its own keychain, `~/.oam-signing/oam-codesign.keychain-db`.
A release unlocks it over ssh with the password stored next to it. Never use
another product's signing identity or keychain for oam.

1. On the build Mac, from a checkout of this repo:

   ```sh
   bash scripts/provision-mac-signing.sh --generate
   ```

   It creates the keychain and certificate, proves it can make a signature
   from the current session, and prints `sha1=<fingerprint>`. It refuses to
   run if an identity already exists.

2. Back up `~/.oam-signing/oam-codesign.p12` and
   `~/.oam-signing/oam-codesign.p12-password` **off the build Mac**. You need
   both files to restore the identity.

3. Put the printed fingerprint on its own line in
   `scripts/mac-signing-identity.sha1` and commit it. From that commit on,
   every release must be signed with that certificate.

Other modes:

- `bash scripts/provision-mac-signing.sh --check` proves the identity can
  sign from the current session. Once a SHA-1 is pinned, the release preflight
  runs this over ssh before it tags.
- `bash scripts/provision-mac-signing.sh --import <oam-codesign.p12> <oam-codesign.p12-password>`
  restores a backup onto a replacement Mac.

To rotate the identity, move `~/.oam-signing` aside by hand, generate or import
the new identity, and update the pin in a commit that says why.
