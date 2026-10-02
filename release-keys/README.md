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

`scripts/lib/signing.sh` implements signing and verification. It checks both
files' format before it trusts them. `oam self-update` compiles in copies of
these files and checks every release it installs against them.

## Current keys

| Key | Fingerprint | Range | Where |
|---|---|---|---|
| `k1` | `SHA256:zB7Aq4Ky/U90VJ4sAEp0e2A65KfQpiyQXJI4FuT2oss` | `v0.18.0` on | release box |
| `k2` | `SHA256:Uy7nugF5mDzfM/8/fcUti9K+sQMbn/LdRAk+sIbWYs4` | none yet (staged) | offline |

Key lines are committed, so **every release must sign.** No knob skips the
manifest step, because anyone in control of the release box could set it.
(Before any key line existed, `scripts/release-local.sh` skipped the manifest
step with a loud warning; releases up to `v0.17.1` are unsigned.)

## Generating the keys (once)

Generate two keys at the same time. `k1` is the current key and stays on the
release box. `k2` is the next key and goes offline right away. Give each one
its own long passphrase and store both passphrases in a password manager.

```sh
mkdir -p ~/.oam-release && chmod 700 ~/.oam-release
ssh-keygen -t ed25519 -C oam-release-k1 -f ~/.oam-release/oam-release-k1
ssh-keygen -t ed25519 -C oam-release-k2 -f ~/.oam-release/oam-release-k2
```

Use dedicated keys. Never use a git, GitHub or host-login key, because those
already sit in agents and on other machines.

### Commit the public halves

```sh
for id in k1 k2; do
  printf 'oam-release-%s namespaces="oam-release" %s\n' "$id" \
    "$(cut -d' ' -f1,2 ~/.oam-release/oam-release-$id.pub)" >> release-keys/allowed_signers
done
printf 'k1 v0.18.0 -\n' >> release-keys/ranges   # the first tag k1 will sign
```

Replace `v0.18.0` with the next tag you will release. `k2` gets no `ranges`
line yet. It is a staged key: it can verify nothing until a range opens for it
at rotation. Committing its public half now lets that half travel ahead of the
rotation. How older clients' embedded copies learn about `k2`'s range is
decided together with the verifier (signing plan, step 3).

Commit `allowed_signers` and `ranges` through a normal PR. Then publish the
fingerprints (`ssh-keygen -lf ~/.oam-release/oam-release-k1.pub`) somewhere
the repo does not control as well, such as SECURITY.md and the release notes.
A key published only in the repo it protects can be swapped along with it.

### Where the private halves live

- **`k1`** stays on the release box, outside any repository and any synced
  folder. Point the release scripts at it:

  ```sh
  export OAM_RELEASE_SIGNING_KEY="$HOME/.oam-release/oam-release-k1"
  ```

  The `.pub` file must sit beside it. Signing names the key by its public half
  and gets the private half from the agent.
- **`k2`** goes offline. Copy `oam-release-k2` (the private half) to two
  encrypted removable media and keep them in different places. Test-restore
  one copy, then delete the private half from the release box. Keep
  `oam-release-k2.pub`.

## How the release uses the key

`scripts/release-local.sh` starts a private `ssh-agent` in preflight, before
anything is bumped or tagged. The agent gets its own `0700` socket directory
and is never the Windows OpenSSH agent service. The script adds `k1` with a
6-hour lifetime, so you enter its passphrase once. It then makes a real
throwaway signature outside the repo and checks it against the committed
`allowed_signers` and `ranges`.

Right after `SHA256SUMS` is written, the script writes the manifest, signs it,
verifies it from disk, and stops the agent. The script's EXIT trap also stops
the agent on any failure. If the release never went live (a dry run, a build
the sidecar matrix rejected, a failure), the trap also deletes the staged
`RELEASE-MANIFEST.sig`, so no valid signature for an unpublished build is left
behind. Preflight refuses to run if the Windows agent service holds the key,
whether the service is running or not: that service keeps keys in the
registry across reboots, and preflight looks there too.

`scripts/release-upload-local-arm64.sh` patches a binary into an existing
release. Run main's copy, whatever the tag: it builds the tag in a throwaway
git worktree. Do not check out an old tag and run the copy in that tree. Every
pinned release's copy predates all of the checks below. The script reads
`allowed_signers` and `ranges` from `origin/main`, not from the tag it builds,
so a range closed or a key rotated after that tag applies.
It verifies the published manifest before it changes anything, then re-signs
the patched one and uploads the binary, `SHA256SUMS` and both manifest files in
one call. A release with no manifest is patched unsigned only when its tag is
older than every range in `ranges`. A newer one is refused, because a missing
manifest there means someone removed it. A tag listed in `presigning-sums` is
refused too: installers verify those releases by the hash of their published
`SHA256SUMS`, which a patch would change.

### Verifying a release by hand

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

## Rotation

Rotate when the plan calls for it, or when `k1`'s custody is in doubt:

1. Close `k1`'s range at the last tag it signed: `k1 v0.18.0 v0.21.3`.
2. Open `k2`'s range at the next tag: `k2 v0.21.4 -`.
3. Restore `k2` from offline media onto the release box, and point
   `OAM_RELEASE_SIGNING_KEY` at it.
4. Generate `k3` as the new staged next key. Commit its public half with no
   range, and take its private half offline as described for `k2` above.
5. Never delete `k1`'s `allowed_signers` line. Old tags it signed must keep
   verifying, and the closed range already stops it from signing anything new.

## Compromise of a release key

1. Close the compromised key's range at its last **known-good** tag.
2. Publish a `REVOKED` statement signed by the offline key. Ship it with every
   release after the incident. (The format is defined with the verifier in
   step 3.)
3. Rotate the GitHub token on the release box. A stolen signing key is only
   useful together with upload access.
4. Post an out-of-band notice, and update the published fingerprints.

One limit applies. An attacker who controls the release can withhold the
`REVOKED` asset, so revocation reaches only clients that see the real latest
release. Write the full incident runbook (leaked release key, Azure signer
misuse, Developer ID misuse: who revokes what, and how fast) before the
installers start enforcing signatures. After that point the verifier logic is
frozen into every binary.
