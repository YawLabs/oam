# Mac signing

The macOS release leg (`scripts/build-remote.sh mac-release`, run on the build
Mac by `scripts/build-platforms-tailnet.sh`) codesigns both binaries,
`oam-aarch64-apple-darwin` and `oam-x86_64-apple-darwin`. It signs each one
right after it is copied into `dist/` and before anything runs it:

1. **Sign** with the hardened runtime, the identifier `org.oamjs.oam` and the
   three entitlements in `oam.entitlements.plist` (JIT, unsigned executable
   memory, and disabled library validation for opt-in native addons).
2. **Verify** with `codesign --verify --strict`, then check the identifier, the
   runtime flag, the exact entitlement set and the signer against the pin
   (the designated requirement, and the SHA-1 of the leaf certificate that
   actually signed).
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
certificate plus notarization. The switch is a new identity and its SHA-1 in
the pin file; the signing code already picks a secure
timestamp and the Developer ID verify rules from the certificate it finds.
Notarization is a separate step that has not been added yet.

## Before the first release that signs

The mac gates (signing, the verify gate and the JIT smoke) run on the build
Mac inside the mac leg, and that leg runs after `release-local.sh` has pushed
the tag. The preflight checks the signing decision and, with a pin, that
the build Mac can sign with it. It cannot check that a signed, hardened-runtime binary can still
JIT, because that needs a built oam.

So, before the first release made after this signing code lands, run the mac
probe kept outside the repo (`mac-probe.sh`, with its README) against the build
Mac. It signs the last published mac binaries exactly the way the release
will, and JIT-smokes them. If a gate fails during a real release anyway, the
tag has been pushed but nothing is published. Fix the cause and re-run
`release-local.sh` with the same tag, which re-points an unpublished tag. To
ship that one release unsigned instead, set `OAM_SKIP_MAC_SIGN=1`.

## `OAM_SIGN_REQUIRED`

`OAM_SIGN_REQUIRED` is one knob for every signing bootstrap, shared with
`scripts/lib/signing.sh`. With `OAM_SIGN_REQUIRED=1`, a missing piece of
signing setup fails the release in preflight instead of warning. The release
key and the mac pin are both committed now, so neither of those cases can
fire: the manifest must be signed and the mac binaries must be signed with
the pinned identity whatever the knob says. What the knob still changes is
Windows: with it set, an unset `OAM_WIN_SIGN_METADATA` is fatal, where
without it both `.exe` assets would ship unsigned behind one warning line.
`scripts/release-local.sh` sets it to 1 unless you export
`OAM_SIGN_REQUIRED=0`, so an unsigned release is always a deliberate choice.
`OAM_SKIP_MAC_SIGN=1` is still honored under it.
