# shellcheck shell=bash
# =============================================================================
# Windows CRT-linkage guard, shared by ci-local.sh and release-local.sh.
# =============================================================================
# oam ships as a bare .exe: install/install.ps1 downloads one file, verifies its
# checksum and puts it on PATH. Nothing installs a Visual C++ redistributable,
# and nothing checks for one. So the binary must not need one.
#
# Until the crt-static rustflags in .cargo/config.toml (see the long comment
# there), it did. Rust's *-pc-windows-msvc targets default to the DYNAMIC CRT,
# so oam.exe imported VCRUNTIME140.dll and MSVCP140.dll -- both redist-only, and
# neither guaranteed present on a clean Windows install. node.exe, by contrast,
# imports 11 DLLs and not one of them is a CRT. The same mismatch is what
# produced the LNK4098 "defaultlib 'libcmt.lib' conflicts" warnings, since the
# prebuilt V8 and ada objects are both compiled /MT.
#
# That regression is silent on any dev box (they all have the redist, installed
# with Visual Studio) and shows up only on an end user's machine, as a
# missing-DLL dialog at startup. Running the binary cannot catch it; only
# inspecting it can -- hence a gate rather than an extra smoke assertion.
#
# Method is a string scan of the whole file, not a parsed PE import table: a
# dependency's name is stored as a plain ASCII string in the import directory,
# so a hit is reliable, and the release box has no PE parser on PATH to do
# better with. The trade is that an unrelated occurrence of one of these names
# anywhere in the image (a V8 snapshot payload, an embedded fixture) would read
# as a false positive. Verified not to happen for the names below -- and a false
# positive fails the gate loudly rather than passing a bad binary, which is the
# right way round for a release check.

# Every CRT DLL an MSVC binary can pull in that lives in the redistributable
# rather than in Windows itself:
#   VCRUNTIME140*  vcruntime, incl. VCRUNTIME140_1.dll (the EH half on x64)
#   MSVCP140*      C++ runtime, incl. the _1/_2/_ATOMIC_WAIT/_CODECVT_IDS splits
#   MSVCR<n>       the pre-UCRT monolithic runtimes
#   concrt140      Concurrency Runtime; vcomp140  OpenMP
#                  Neither is reachable from today's dependency graph -- Rust
#                  and V8 use neither -- but both are redist-only, so a new C
#                  dependency could introduce one and it would fail on a clean
#                  machine exactly like the others. Cheaper to match than to
#                  re-derive the list later.
#   ucrtbase / api-ms-win-crt-*  the UCRT, which ships with Windows 10+ but
#                  whose presence still proves the DYNAMIC CRT was linked.
OAM_CRT_DLL_PATTERN='VCRUNTIME140[A-Za-z0-9_]*\.dll|MSVCP140[A-Za-z0-9_]*\.dll|MSVCR[0-9]+\.dll|concrt140[A-Za-z0-9_]*\.dll|vcomp140[A-Za-z0-9_]*\.dll|ucrtbase\.dll|api-ms-win-crt-[A-Za-z0-9-]*\.dll'

# assert_static_crt <pe-binary>
#   0 when the binary imports no C-runtime DLL, 1 + a diagnostic on stderr when
#   it does OR when the file could not be scanned.
assert_static_crt() {
  local bin="$1" raw status hits
  [ -f "$bin" ] || { echo "assert_static_crt: no such file: $bin" >&2; return 1; }

  # grep's three outcomes are three DIFFERENT answers and must not be collapsed:
  #   0  matched      -> the binary imports the dynamic CRT. FAIL.
  #   1  no match     -> the PASS condition.
  #   2+ grep failed  -> unreadable, locked, vanished mid-scan. NOT a pass.
  # An earlier revision piped straight into a `hits=$(...)` assignment, which
  # made "no match" (exit 1) collide with `set -o pipefail` + `set -e` and abort
  # the whole gate ON THE SUCCESS PATH; the obvious `|| true` patch fixed that
  # but bought a worse bug, silently PASSING any binary grep could not read --
  # the one failure direction a release gate must never have. Hence the explicit
  # three-way split. `2>&1` keeps grep's own error text for the diagnostic.
  raw="$(grep -aoiE "$OAM_CRT_DLL_PATTERN" "$bin" 2>&1)" && status=0 || status=$?

  if [ "$status" -ge 2 ]; then
    echo "CRT-linkage gate FAILED for $bin" >&2
    echo "  could not scan the file (grep exit $status): ${raw:-no output}" >&2
    echo "  treated as a failure, not a pass: an unscannable binary is not a" >&2
    echo "  binary known to be clean." >&2
    return 1
  fi
  [ "$status" -eq 1 ] && return 0

  hits="$(printf '%s\n' "$raw" | sort -fu | tr '\n' ' ')"
  echo "CRT-linkage gate FAILED for $bin" >&2
  echo "  imports the dynamic CRT: $hits" >&2
  echo "  a binary that imports these needs the VC++ redistributable, which" >&2
  echo "  install.ps1 neither ships nor checks for -- it will fail to start on" >&2
  echo "  a clean Windows machine." >&2
  echo "  Fix: restore the crt-static rustflags for this target in" >&2
  echo "  .cargo/config.toml, then rebuild (a target-feature change rebuilds" >&2
  echo "  the whole graph, so do not trust an incremental result here)." >&2
  return 1
}

# assert_static_crt_canary
#   Proves the checker still REJECTS a known-bad input before its verdict on a
#   real binary is trusted. Every failure mode this checker has had -- the
#   pipefail abort, the `|| true` that swallowed read errors -- degenerated it
#   toward always-passing, which is indistinguishable from a clean binary in the
#   gate output. A synthetic fixture that MUST fail is the cheapest way to tell
#   "the binary is clean" apart from "the checker stopped working", and it needs
#   no test harness (this repo has none for shell).
assert_static_crt_canary() {
  local d f
  d="$(mktemp -d)" || { echo "assert_static_crt_canary: mktemp failed" >&2; return 1; }
  f="$d/canary-not-a-real-binary.exe"
  printf 'MZ\0\0this fixture imports VCRUNTIME140.dll on purpose\0' > "$f"
  if assert_static_crt "$f" >/dev/null 2>&1; then
    rm -f "$f"; rmdir "$d" 2>/dev/null || true
    echo "CRT-linkage SELF-CHECK FAILED: the checker passed a fixture that" >&2
    echo "  deliberately contains VCRUNTIME140.dll. Its verdict on the real" >&2
    echo "  binaries cannot be trusted -- fix scripts/lib/crt-linkage.sh before" >&2
    echo "  reading anything else in this gate as green." >&2
    return 1
  fi
  rm -f "$f"; rmdir "$d" 2>/dev/null || true
  return 0
}

# assert_static_crt_build_script <profile-dir> <crate-name>
#   Same assertion, for the build script cargo actually runs.
#
# Worth its own check because the build script is where this bug FIRST showed
# up: oam_engine's links V8 to generate the startup snapshot, and it was the
# source of the `libcmt.lib` LNK4098. It is also the half that regresses
# invisibly -- passing `--target` to a Windows cargo invocation splits cargo's
# unit graph and withholds [target.<triple>].rustflags from host units, so the
# shipped asset stays correct and this gate stays green while the build script
# quietly goes back to linking two CRTs. Nothing else notices.
#
# Newest-by-mtime, deliberately: target/<profile>/build/ accumulates one dir per
# historical fingerprint, and the stale ones legitimately hold dynamic-CRT
# binaries from before this fix. The one cargo just linked is the newest, and a
# dir cargo did not touch cannot become newer than it.
assert_static_crt_build_script() {
  local profile_dir="$1" crate="$2" newest
  # find + %T@, not `ls -t`: same newest-first ordering without shellcheck
  # SC2012, and it matches how gc-target.sh already walks these trees.
  newest="$(find "$profile_dir/build" -maxdepth 2 -path "*/$crate-*" -name build-script-build.exe -printf '%T@ %p\n' 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2-)"
  if [ -z "$newest" ]; then
    echo "assert_static_crt_build_script: no build script for $crate under $profile_dir" >&2
    return 1
  fi
  assert_static_crt "$newest"
}
