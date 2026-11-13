#!/bin/bash
# =============================================================================
# Prune stale cargo build outputs from target/.
# =============================================================================
# cargo never garbage-collects: every rebuild writes a NEW hash-named artifact
# and leaves every previous one behind forever. Measured on the dev box
# 2026-08-06, mid-v0.8.2-release:
#
#   target/debug/incremental   53.8 GB
#   target/debug/deps          48.2 GB   (e2e-<hash>.exe copies back to Jun 17)
#   target/release              6.6 GB
#   target/x64-host             4.9 GB
#
# What this does NOT do is prune by age. A flat `find -mtime +N -delete` over
# deps/ deletes rlibs that are OLD BUT STILL CURRENT -- a dependency compiled
# in June and untouched since is still the one being linked -- and for this
# workspace that means re-running the V8 build. So:
#
#   incremental/  deleted wholesale. Pure cache, regenerated on demand; the
#                 only cost is a slower next build.
#   deps/         per-FAMILY prune. Files are grouped by name-with-the-hash-
#                 stripped plus extension, sorted newest-first, and only copies
#                 beyond --keep are removed. A crate with exactly one artifact
#                 keeps it no matter how old it is. A family can hold more
#                 than one LIVE artifact, though -- see --keep below.
#                 Hashed codegen-unit objects (<name>-<hash>.*.rcgu.o) follow
#                 their unit instead. An unhashed cdylib`s (a path crate such
#                 as oam_napi_test_addon: <name>.*.rcgu.o) match no rule and
#                 are left, a few KB per build.
#   build/<pkg>/<hash>/
#                 cargo >= 1.100's unit dirs (rust-lang/cargo#17354: deps/ and
#                 .fingerprint/ are gone; each unit's outputs land in out/, its
#                 fingerprint in fingerprint/, a build-script run in run/).
#                 Pruned by REACHABILITY -- see "Units" below. Without it, a
#                 1.100 tree reclaimed nothing but incremental/.
#   deps/, .fingerprint/, build/<pkg>-<hash>/
#                 the <= 1.99 layout. Once a tree holds 1.100 units these are
#                 dead weight -- the pinned cargo never reads them again -- so
#                 they go wholesale, which is what a manual `cargo clean` per
#                 leg used to buy after a toolchain bump. A tree with no 1.100
#                 units still gets the deps/ family prune below.
#   *.inuse-*     parked copies from scripts/lib/build-locks.sh whose holder
#                 has since exited.
#
# A file some process still has mapped simply fails to unlink and is left; that
# is not an error here.
#
# Usage:
#   ./scripts/gc-target.sh                 # prune, keeping 1 copy per family
#   ./scripts/gc-target.sh --dry-run       # report only, delete nothing
#   ./scripts/gc-target.sh --keep 2        # keep the 2 newest per family
#   ./scripts/gc-target.sh --keep-incremental   # leave incremental/ alone
#
# On --keep: a deps/ family is crate name + extension, and several artifacts in
# it can be live at once -- a bin and its test harness (both `oam-<hash>`, or
# `oam-<hash>.exe` on Windows), or one crate built at several versions or
# feature sets (getrandom 0.2, 0.3 and 0.4 all link into oam). Evicting a live
# one forces a rebuild of it and everything above it. That is a time cost,
# never a correctness one -- everything under target/ is regenerable -- but it
# is why the remote `gc` dispatch in build-remote.sh passes --keep 3. For
# 1.100 units --keep is the per-leaf-slot count below.
#
# Units (cargo >= 1.100). A unit is live when some build still uses it, and
# cargo writes down what uses what: fingerprint/<kind>.json carries "deps",
# one entry [<pkg id>,"<name>",<public>,<fingerprint hash>] per unit this one
# was built from, and the named unit's fingerprint/<kind> file holds the same
# hash (the json's u64 in decimal, the file's its 8 little-endian bytes as hex;
# measured with cargo 1.100.0 on a probe built with build, --release, test,
# check, clippy and --target: 247 of 247 entries resolved that way). So the
# prune is a mark-and-sweep over every tree that shares one CARGO_TARGET_DIR
# (target/ holds target/debug, target/release and target/<triple>/<profile>;
# target/x64-host is a separate one): a unit in target/<triple>/debug/build
# names build-script compiles in the HOST tree target/debug/build (3 of the
# --target tree's 54 entries on that probe), so a tree on its own would orphan
# them. Every <root>/<triple>/<profile> holding units joins its target dir's
# sweep whether or not TREES lists it, as closure source and as pruned tree.
# The roots:
#
#   1. Uplifted units. cargo HARDLINKS each final artifact up -- target/debug/
#      oam.exe is build/oam_cli/<hash>/out/oam.exe, liba.rlib is out/liba-
#      <hash>.rlib, examples/<x> likewise -- so a unit with a file that is the
#      same inode as a file at the top of a tree or in its examples/ is the
#      LAST build of that artifact. Where cargo had to copy instead of link,
#      nothing matches and root 2 alone decides. cargo rewrites an uplift on
#      every build of that artifact, so an uplift is its CURRENT build however
#      old the file is: age never disowns one (a member untouched for weeks
#      while another is rebuilt with -p is still current, and judging it
#      against the tree's newest uplift once pruned that member's whole
#      closure on a probe). The one exception is an uplift linked by another
#      toolchain than the one most of the tree's uplifts were linked by -- a
#      toolchain bump left it behind (a bin removed before the bump): it
#      roots nothing, and its slot roots nothing either (2). The tree file
#      itself is never deleted: it is a link, and only its unit's copy is
#      reclaimed.
#   2. The --keep newest units of each LEAF SLOT: package + kind (lib-x,
#      bin-x, test-bin-x, build-script-build-script-build -- the kind names the
#      target) + profile hash + compile_kind + activated features, as the json
#      records them, REFINED by the slots of the units its "deps" names, to a
#      fixed point. So `cargo build -p a` and a workspace build that unifies
#      more features into a's dependencies, or a build and a test whose
#      dev-dependencies do, are two slots of a and each keeps its newest. A
#      version bump or a source change is in no key, so it stays one slot and
#      the superseded unit goes. Newest = newest file in the unit. This is
#      what holds the units nothing uplifts: test harnesses, check and clippy
#      units. Build-script RUN units are no root of their own (their json
#      carries profile 0 and no features, so every run of a package would
#      share one slot); they live through the compile units that name them.
#      A slot roots nothing once its newest unit trails the newest unit of
#      its GROUP -- same tree, package, kind and compile_kind, any profile
#      hash, features or dependency variant -- by more than STALE_DAYS (7):
#      a [profile] edit or a feature change opens a new slot, and the old
#      one's newest unit (an old rusty_v8 OUT_DIR through its closure) would
#      otherwise be pinned for good.
#
# Everything reachable from a root through "deps", to a fixed point, is live:
# a build's build-script compiles and runs (the OUT_DIRs -- rusty_v8's static
# lib, oam_engine's snapshot), its build-dependency libs, both live versions of
# a crate linked twice, the check-mode units a clippy unit names. A unit is
# pruned when it is not live, its newest file is over an hour old (a build
# running right now may be writing units nothing names yet), and its json
# parsed. A unit with no json or one this script cannot read is left alone --
# unless it holds no file at all (cargo leaves empty out/ dirs behind) and is
# over an hour old.
#
# What this costs, honestly. Liveness is what cargo last WROTE, not what the
# next build will ask for:
#   * a unit beyond --keep in its leaf slot that no root names goes, and the
#     next build that wants it recompiles it (never correctness: cargo
#     rebuilds what is missing). Two shapes hit that at --keep 1, both seen on
#     the cargo 1.100.0 probe:
#       - `cargo check` and `cargo clippy` of one workspace member: same slot
#         (the json is identical but for the dep hashes, and the deps are the
#         same slots; the wrapper moves only the unit hash), so only the one
#         run last survives, with what it names.
#       - switching a member BACK to an older version with no source change
#         (a version bump undone): a no-op test or clippy run writes nothing
#         into the target dir, so the old version's harnesses and clippy units
#         are older than the abandoned version's and lose their slots. A
#         switch that changes sources rebuilds those units in place, which
#         refreshes them. The uplift roots only the LAST build of each
#         artifact, so a build switched back without a rebuild is in the same
#         position as its harnesses: its slot keeps it while it is the newest
#         there.
#     --keep 2 kept both shapes whole on the probe; build-remote.sh passes 3.
#     Builds that differ in a dependency's unified features (`-p a` against
#     the workspace, build against test) are NOT among them: those are
#     separate slots (2), so alternating between them evicts nothing.
#   * a change to what a unit is built from (a dependency added or dropped)
#     opens a new slot rather than superseding the old one, so the old
#     variant and its closure stay until the slot ages out (STALE_DAYS).
#   * a dependency dropped from every build is the newest of its slot and of
#     its group, and nothing newer ever joins either, so it stays until the
#     target dir is cleaned.
#   * an uplift removed or renamed under the SAME toolchain (a bin deleted
#     from the workspace) stays a root, with its closure, until the next
#     toolchain bump or a `cargo clean`: cargo never deletes a stale uplift,
#     and nothing in the target dir tells it from a current one.
#   * a target dir shared by two toolchains roots only the uplifts of the
#     majority one; a one-off `cargo +nightly build -p x` there loses its own
#     units (x and what only x's nightly build names) at the next gc.
#   * a build variant run less often than every STALE_DAYS (a weekly
#     `cargo test` whose dev-dependencies unify other features) loses its
#     slot root when a sibling variant of the same target ran within the
#     window, and that run rebuilds what only it used.
#   * a unit rebuilt IN PLACE gets a new fingerprint hash, so a dependent that
#     was not rebuilt with it names a hash nothing carries; that dependent is
#     dirty for cargo anyway and the dependency is still kept by its own slot.
#   * nothing is pruned inside the hour's grace, so a run right after a build
#     reclaims only what is older.
# =============================================================================

set -euo pipefail

# Repo root from THIS script location, NOT `git rev-parse`: the remote build
# hosts receive the tree as a tarball of the files git tracks (the orchestrators
# via scripts/lib/src-sync.sh), which never includes .git itself, so there is no
# git dir there. rev-parse then fails, and
# because `cd ""` is a silent no-op the script would carry on against whatever
# directory the caller happened to be in -- pruning the wrong tree, or nothing,
# and reporting success either way.
cd "$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=lib/build-locks.sh
. scripts/lib/build-locks.sh

GRN='\033[0;32m'; YEL='\033[1;33m'; CYA='\033[0;36m'; NC='\033[0m'
say() { echo -e "${CYA}=== $* ===${NC}"; }
ok()  { echo -e "${GRN}  [ok]${NC} $*"; }
warn(){ echo -e "${YEL}  [warn]${NC} $*" >&2; }

DRY_RUN=0
KEEP=1
KEEP_INCREMENTAL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --keep) KEEP="${2:?--keep needs a count}"; shift ;;
    --keep-incremental) KEEP_INCREMENTAL=1 ;;
    -h|--help) awk 'NR==1{next} !/^#/{exit} {sub(/^# ?/, ""); print}' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 1 ;;
  esac
  shift
done
case "$KEEP" in ''|*[!0-9]*) echo "--keep needs a number, got '$KEEP'" >&2; exit 1 ;; esac
[ "$KEEP" -ge 1 ] || { echo "--keep must be >= 1 (0 would delete the current build)" >&2; exit 1; }

# One x64-host tree per emulated platform, not just Windows.
#
# target/x64-host/release is the win-x64 leg: release-local.sh builds it WITHOUT
# --target (see the comment there), so its output is not under a triple
# subdirectory. The triple-suffixed Windows path stays listed so a tree left
# behind by a pre-0.11 release still gets collected.
TREES=(
  target/debug
  target/release
  target/x64-host/release
  target/x64-host/x86_64-pc-windows-msvc/release
  target/x64-host/x86_64-apple-darwin/release
)
# OAM_GC_TREES replaces that list, whitespace-separated and relative to the repo
# root -- for the suite and for probing a layout by hand. Not a release knob.
# Either way the target-triple trees of every listed tree's target dir are
# added below (add_triple_trees).
if [ -n "${OAM_GC_TREES:-}" ]; then
  read -r -a TREES <<<"$OAM_GC_TREES"
fi

# How far a slot or an uplift may trail the newest of its kind before it stops
# rooting anything (see "Units" in the header). Days, a named knob so the
# header, the awk and the suite agree on one number.
STALE_DAYS=7

# tree_root <tree> -- the CARGO_TARGET_DIR a tree belongs to: the tree less its
# profile, less a target triple under that. A triple has at least two dashes
# (x86_64-pc-windows-msvc, aarch64-apple-darwin); x64-host, a target dir of
# its own (build-remote.sh, release-local.sh), has one.
#   target/debug                                    -> target
#   target/aarch64-pc-windows-msvc/debug            -> target
#   target/x64-host/release                         -> target/x64-host
#   target/x64-host/x86_64-pc-windows-msvc/release  -> target/x64-host
tree_root() {
  local r="${1%/*}"
  case "${r##*/}" in *-*-*) r="${r%/*}" ;; esac
  echo "$r"
}

# add_triple_trees -- append to TREES every <root>/<triple>/<profile> that
# holds cargo 1.100 units, for each target dir a listed tree belongs to. A
# plain `cargo build --target <triple>` into target/ writes its units to
# target/<triple>/debug, which the list does not carry, while the host
# build-script compiles and proc-macros it names land in target/debug/build,
# which the list does: pruning target/ without the triple tree would take
# those as unreached. Same triple rule as tree_root (two dashes or more), so
# target/x64-host -- a target dir of its own -- is never taken for one.
add_triple_trees() {
  local t r d p roots=()
  for t in "${TREES[@]}"; do
    r="$(tree_root "$t")"
    case " ${roots[*]-} " in *" $r "*) ;; *) roots+=("$r") ;; esac
  done
  for r in ${roots[@]+"${roots[@]}"}; do
    [ -d "$r" ] || continue
    for d in "$r"/*/*; do
      [ -d "$d" ] || continue
      p="${d%/*}"
      case "${p##*/}" in *-*-*) ;; *) continue ;; esac
      has_units "$d" || continue
      case " ${TREES[*]} " in *" $d "*) ;; *) TREES+=("$d") ;; esac
    done
  done
}

# deps/ pruning needs each file's mtime and size. GNU find prints both with
# -printf; macOS has BSD find, which has no -printf, so there BSD stat -f prints
# them instead. That fallback is not optional: the Mac build leg skipped this
# prune for its whole life, and ~/oam-build/target on the Air reached 93GB
# (2026-09-25). A host with neither still degrades LOUDLY below rather than
# reporting a clean prune that collected nothing.
LISTER=none
if find . -maxdepth 0 -printf '' >/dev/null 2>&1; then
  LISTER=gnu
elif stat -f '%m' . >/dev/null 2>&1; then
  # GNU stat reads -f as --file-system and '%m' as a file operand, so this
  # probe only succeeds on BSD stat.
  LISTER=bsd
fi

# list_files <dir>  -- one "<mtime>\t<KB>\t<path>" line per regular file.
# BSD stat's %m is whole seconds and %b is 512-byte blocks, halved up to KB to
# match GNU find's %k. Whole seconds cannot misorder two generations of one
# family: those come from separate builds, minutes apart.
list_files() {
  case "$LISTER" in
    gnu) find "$1" -maxdepth 1 -type f -printf '%T@\t%k\t%p\n' 2>/dev/null ;;
    bsd)
      find "$1" -maxdepth 1 -type f -exec stat -f '%m%t%b%t%N' {} + 2>/dev/null |
        awk -F'\t' -v OFS='\t' '{ $2 = int(($2 + 1) / 2); print }'
      ;;
  esac
}

# list_unit_tree <tree>  -- "<type>\t<mtime>\t<KB>\t<dev>:<inode>\t<path>"
# lines for everything the unit prune reads in one tree:
#   t  a file at the top of the tree or of its examples/ (an uplift target)
#   f  a file in build/, at least three levels down (build/<pkg>/<hash>/...)
#   d  a directory in build/ at least two levels down (the GNU walk prints the
#      deeper ones too; the awk keeps only build/<pkg>/<hash>), which is how a
#      unit holding no file at all is seen
# A fixed handful of walks per tree, not a find or du per unit: a toolchain
# bump supersedes every unit at once, and process spawn on the Windows dev box
# runs ~1s per hundred. The inode is kept as a string -- NTFS file ids under
# MSYS run past 2^53, where an awk number would round two files together.
list_unit_tree() {
  local t="$1" top=("$1")
  # examples/ only when it is there: a find over a missing operand exits
  # non-zero, and under set -e (inherited by this subshell) that would end
  # the listing before build/ was walked at all.
  [ -d "$t/examples" ] && top+=("$t/examples")
  case "$LISTER" in
    gnu)
      find "${top[@]}" -maxdepth 1 -type f -printf 't\t%T@\t%k\t%D:%i\t%p\n' 2>/dev/null || true
      find "$t/build" -mindepth 2 \( -type f -printf 'f\t%T@\t%k\t%D:%i\t%p\n' \) \
        -o \( -type d -printf 'd\t%T@\t%k\t%D:%i\t%p\n' \) 2>/dev/null || true
      ;;
    bsd)
      {
        find "${top[@]}" -maxdepth 1 -type f -exec stat -f 't%t%m%t%b%t%d:%i%t%N' {} + 2>/dev/null || true
        find "$t/build" -mindepth 3 -type f -exec stat -f 'f%t%m%t%b%t%d:%i%t%N' {} + 2>/dev/null || true
        find "$t/build" -mindepth 2 -maxdepth 2 -type d -exec stat -f 'd%t%m%t%b%t%d:%i%t%N' {} + 2>/dev/null || true
      } | awk -F'\t' -v OFS='\t' '{ $3 = int(($3 + 1) / 2); print }'
      ;;
  esac
  return 0
}

# A cargo hash as a glob. Spelled out because bash `case`/glob patterns have no
# interval form either.
H='[0-9a-f]'
HEX16="$H$H$H$H$H$H$H$H$H$H$H$H$H$H$H$H"

# has_units <tree> -- true once cargo >= 1.100 has built into this tree.
has_units() {
  compgen -G "$1/build/*/$HEX16/fingerprint" >/dev/null 2>&1
}

# dir_kb <path> -- size in KB, 0 when absent. Only for the handful of old-layout
# dirs; units are sized from the single walk above.
dir_kb() {
  local kb
  kb="$(du -sk "$1" 2>/dev/null | awk '{print $1; exit}')"
  echo "${kb:-0}"
}

# gc_rmdir <dir> -- gc_rm for a directory tree, same never-fails contract.
gc_rmdir() {
  if [ "$DRY_RUN" = "1" ]; then return 0; fi
  rm -rf "$1" 2>/dev/null || true
}

# migrate_old_layout <tree> -- drop the <= 1.99 layout from a tree 1.100 owns.
migrate_old_layout() {
  local tree="$1" d n=0 kb=0 held=0 dkb
  local old=("$tree/deps" "$tree/.fingerprint")
  # shellcheck disable=SC2231  # $HEX16 is a glob and must stay unquoted
  for d in "$tree"/build/*-$HEX16; do
    # build/<pkg>-<hash>/ is the old build-script dir. A dir holding 1.100
    # units is a package dir, never removed here, whatever its name.
    [ -d "$d" ] || continue
    compgen -G "$d/$HEX16/fingerprint" >/dev/null 2>&1 && continue
    old+=("$d")
  done
  for d in "${old[@]}"; do
    [ -e "$d" ] || continue
    dkb="$(dir_kb "$d")"
    gc_rmdir "$d"
    # Counted only when really gone, as in the deps/ prune: an in-use file
    # survives rm -rf, and reporting the dir as reclaimed would be a lie.
    if [ "$DRY_RUN" = "1" ] || [ ! -e "$d" ]; then
      n=$((n + 1)); kb=$((kb + dkb))
    else
      held=$((held + 1))
    fi
  done
  if [ "$n" -gt 0 ]; then
    if [ "$DRY_RUN" = "1" ]; then
      warn "would remove $n pre-1.100 layout dirs from $tree (deps/, .fingerprint/, build/<pkg>-<hash>/: $((kb / 1024))MB)"
    else
      ok "removed $n pre-1.100 layout dirs from $tree (deps/, .fingerprint/, build/<pkg>-<hash>/: $((kb / 1024))MB)"
    fi
  fi
  if [ "$held" -gt 0 ]; then
    warn "$held pre-1.100 layout dirs in $tree could not be fully deleted -- something still holds files in them; a later run reclaims them"
  fi
  return 0
}

# prune_root <root> <tree>... -- the unit mark-and-sweep described in the
# header, over every unit of every listed tree (all of them under <root>).
prune_root() {
  local root="$1"; shift
  local line kb unit tag
  local stale_count=0 stale_kb=0 held_count=0 held_kb=0 stats="" t
  while IFS= read -r line; do
    tag="${line%% *}"; line="${line#* }"
    if [ "$tag" = "S" ]; then stats="$line"; continue; fi
    kb="${line%% *}"; unit="${line#* }"
    gc_rmdir "$unit"
    if [ "$DRY_RUN" = "1" ] || [ ! -e "$unit" ]; then
      stale_count=$((stale_count + 1)); stale_kb=$((stale_kb + kb))
    else
      held_count=$((held_count + 1)); held_kb=$((held_kb + kb))
    fi
  done < <(
    for t in "$@"; do list_unit_tree "$t"; done |
      awk -F'\t' -v trees="$*" -v keep="$KEEP" -v now="$(date +%s)" \
          -v maxage="$((STALE_DAYS * 86400))" '
        BEGIN {
          # A dynamic string regex, never an interval expression: see the
          # mawk note in the deps/ prune below.
          hex = ""
          for (i = 0; i < 16; i++) hex = hex "[0-9a-f]"
          hex = "^" hex "$"
          ntr = split(trees, tr, " ")
          for (i = 1; i <= ntr; i++) { tp[i] = tr[i] "/"; tr[i] = tr[i] "/build/" }
          # Rounds of slot-key refinement before giving up on a fixed point.
          # Each round settles one more level of the dependency graph; a
          # workspace this size is nowhere near this deep.
          REFINE_MAX = 64
        }
        # lehex(<decimal u64>) -- the 16 hex digits cargo writes into a
        # unit`s fingerprint/<kind> file for the hash its dependents` json
        # carries in decimal: the little-endian bytes. Past 2^53, so it is
        # long-divided by 256 as a digit string; an awk number would round.
        function lehex(d,   out, i, j, nd, r, q, digit) {
          out = ""
          for (i = 0; i < 8; i++) {
            r = 0; q = ""; nd = length(d)
            for (j = 1; j <= nd; j++) {
              r = r * 10 + substr(d, j, 1)
              digit = int(r / 256); r = r % 256
              if (q != "" || digit > 0) q = q digit
            }
            out = out sprintf("%02x", r)
            d = (q == "" ? "0" : q)
          }
          return out
        }
        # An uplift target: which tree it is in, and its mtime (the inode`s,
        # so the unit file`s too).
        $1 == "t" {
          for (i = 1; i <= ntr; i++)
            if (index($5, tp[i]) == 1) { top[$4] = i; topmt[$4] = $2; break }
          next
        }
        {
          rel = ""
          for (i = 1; i <= ntr; i++)
            if (index($5, tr[i]) == 1) { pre = tr[i]; ti = i; rel = substr($5, length(pre) + 1); break }
          if (rel == "") next
          n = split(rel, c, "/")
          if (n < 2 || c[1] == "" || c[2] !~ hex) next
          unit = pre c[1] "/" c[2]
          utr[unit] = ti
          if ($1 == "d") {
            if (n != 2) next
            seen[unit] = 1; dmt[unit] = $2
            next
          }
          if (n < 3) next
          seen[unit] = 1; nf[unit]++
          kb[unit] += $3
          if (!(unit in newest) || $2 + 0 > newest[unit] + 0) newest[unit] = $2
          if (c[3] == "out") ino[$4] = unit
          if (n != 4 || c[3] != "fingerprint") next
          if (c[4] !~ /\.json$/) {
            # fingerprint/<kind> (no extension) holds the unit`s own hash,
            # which is what a dependent`s json names.
            if (c[4] ~ /^(dep-|output-)/ || c[4] == "invoked.timestamp") next
            h = ""
            if ((getline h < $5) > 0 && h ~ hex) own[h] = (h in own) ? own[h] " " unit : unit
            close($5)
            next
          }
          json = ""
          while ((getline ln < $5) > 0) json = json ln
          # Closed every time: a tree holds thousands of units, and awk keeps
          # each file it reads open until told otherwise.
          close($5)
          # Two jsons in one unit is no layout this script knows.
          if (unit in parsed) { delete parsed[unit]; delete slotted[unit]; delete deps[unit]; odd[unit] = 1; next }
          if (unit in odd) next
          kind = substr(c[4], 1, length(c[4]) - 5)
          # "deps": every entry [<pkg id>,"<name>",<bool>,<fingerprint hash>].
          # A path inside the json ("local" carries some) cannot fake a field:
          # a quote inside a JSON string is escaped, and every pattern needs a
          # bare one before the colon. A json without these fields is not a
          # fingerprint this script understands, so the unit is left alone.
          p = index(json, "\"deps\":[")
          if (p == 0) { odd[unit] = 1; next }
          rest = substr(json, p + 8); rf = ""; rn = ""
          while (match(rest, /^\[[0-9]+,"[^"]*",(true|false),[0-9]+\],?/)) {
            ent = substr(rest, 1, RLENGTH); rest = substr(rest, RLENGTH + 1)
            sub(/\],?$/, "", ent)
            nm = ent; sub(/^[^"]*"/, "", nm); sub(/".*/, "", nm)
            if (nm == "") nm = "-"
            sub(/.*,/, "", ent)
            rf = rf " " lehex(ent); rn = rn " " nm
          }
          if (substr(rest, 1, 1) != "]") { odd[unit] = 1; next }
          if (!match(json, /"rustc":[0-9]+/)) { odd[unit] = 1; next }
          rustc = substr(json, RSTART + 8, RLENGTH - 8)
          if (!match(json, /"profile":[0-9]+/)) { odd[unit] = 1; next }
          prof = substr(json, RSTART + 10, RLENGTH - 10)
          if (!match(json, /"compile_kind":[^,}]*/)) { odd[unit] = 1; next }
          ck = substr(json, RSTART + 15, RLENGTH - 15)
          if (!match(json, /"features":"([^"\\]|\\.)*"/)) { odd[unit] = 1; next }
          feat = substr(json, RSTART + 12, RLENGTH - 13)
          parsed[unit] = 1
          deps[unit] = substr(rf, 2); dnm[unit] = substr(rn, 2)
          rc[unit] = rustc
          # The leaf slot before refinement, and the group its age is judged
          # in (see the header).
          base[unit] = c[1] "|" kind "|" prof "|" ck "|" feat
          grp[unit] = ti "|" c[1] "|" kind "|" ck
          # A build-script run is no root of its own (see the header).
          if (kind !~ /^run-/) slotted[unit] = 1
        }
        END {
          # Each tree`s toolchain: the rustc MOST of its uplifts were linked
          # by (ties to the one with the newest uplift). Not the rustc of the
          # newest unit: one `cargo +nightly build -p x` into the pinned
          # target dir would then disown every uplift the pinned toolchain
          # wrote, and with them their slots and closures.
          for (i in top) {
            if (!(i in ino)) continue
            u = ino[i]
            if (!(u in parsed)) continue
            t = top[i]; r = t SUBSEP rc[u]
            nrc[r]++
            if (!(r in rcnew) || topmt[i] + 0 > rcnew[r] + 0) rcnew[r] = topmt[i]
            if (!(t in trc) || nrc[r] > nrc[t, trc[t]] \
                || (nrc[r] == nrc[t, trc[t]] && rcnew[r] + 0 > rcnew[t, trc[t]] + 0)) trc[t] = rc[u]
          }
          # Root 1: uplifted. A top-level file and an out/ file that are one
          # inode; the walk order does not matter, so it is joined here. cargo
          # rewrites an uplift on every build of its artifact, so an uplift IS
          # that artifact`s current build however old it is -- age is no
          # reason to drop one (a member untouched for weeks while another is
          # rebuilt with -p is still current). The one uplift cargo would not
          # write today is one linked by another toolchain than the tree`s
          # (see the header): it roots nothing.
          nq = 0; nup = 0
          for (i in top) {
            if (!(i in ino)) continue
            u = ino[i]
            if ((u in parsed) && rc[u] != trc[top[i]]) { stl[u] = 1; continue }
            upok[u] = 1
            if (u in live) continue
            live[u] = 1; q[++nq] = u; nup++
          }
          nstale = 0
          for (u in stl) if (!(u in upok)) nstale++
          # Leaf slots, refined by what each unit was built from: a unit`s
          # class is its base key plus the sorted classes of the units its
          # "deps" names, iterated to a fixed point -- two builds of one
          # member that differ only in a dependency`s unified features are
          # two slots, while a version or source rebuild (neither is in any
          # key) stays one. Classes only ever split (each new key starts with
          # the old class), so an unchanged count is the fixed point.
          ncls = 0
          for (u in parsed) {
            k = base[u]
            if (!(k in cid)) cid[k] = ++ncls
            cls[u] = cid[k]
            nd[u] = split(deps[u], hs, " "); split(dnm[u], ns, " ")
            for (j = 1; j <= nd[u]; j++) { dto[u, j] = (hs[j] in own) ? own[hs[j]] : ""; dname[u, j] = ns[j] }
          }
          for (round = 1; round <= REFINE_MAX; round++) {
            split("", cid); n2 = 0
            for (u in parsed) {
              na = 0
              for (j = 1; j <= nd[u]; j++) {
                nv = split(dto[u, j], vs, " ")
                if (nv == 0) { ar[++na] = "?" dname[u, j]; continue }
                # A dep unit this script cannot read is named, not keyed.
                for (v = 1; v <= nv; v++) ar[++na] = (vs[v] in cls) ? "#" cls[vs[v]] : "?" dname[u, j]
              }
              for (a = 2; a <= na; a++) {
                x = ar[a]
                for (b = a - 1; b >= 1 && ar[b] > x; b--) ar[b + 1] = ar[b]
                ar[b + 1] = x
              }
              k = cls[u]
              for (a = 1; a <= na; a++) k = k "," ar[a]
              if (!(k in cid)) cid[k] = ++n2
              ncl[u] = cid[k]
            }
            for (u in parsed) cls[u] = ncl[u]
            if (n2 == ncls) break
            ncls = n2
          }
          # Root 2: the --keep newest units of each leaf slot -- unless the
          # slot is stale: its newest unit trails the newest unit of its
          # group (tree, package, target, compile kind; every profile hash,
          # feature set and dependency variant) by more than maxage, or is an
          # uplift rejected above.
          for (u in slotted) {
            s = cls[u]; mem[s, ++cnt[s]] = u
            if (!(s in snew) || newest[u] + 0 > snew[s] + 0) { snew[s] = newest[u]; sn[s] = u }
            g = grp[u]
            if (!(g in gnew) || newest[u] + 0 > gnew[g] + 0) gnew[g] = newest[u]
          }
          nslot = 0; nexp = 0
          for (s in cnt) {
            if (snew[s] + 0 < gnew[grp[sn[s]]] - maxage) { nexp++; continue }
            if ((sn[s] in stl) && !(sn[s] in upok)) { nexp++; continue }
            for (k = 1; k <= keep; k++) {
              best = ""
              for (i = 1; i <= cnt[s]; i++) {
                m = mem[s, i]
                if (m in picked) continue
                if (best == "" || newest[m] + 0 > newest[best] + 0) best = m
              }
              if (best == "") break
              picked[best] = 1
              if (!(best in live)) { live[best] = 1; q[++nq] = best; nslot++ }
            }
          }
          # The closure: a work queue, so one pass reaches the fixed point.
          # Every tree of the root resolves a hash -- a run in a --target tree
          # names its build-script compile in the host tree.
          for (qi = 1; qi <= nq; qi++) {
            u = q[qi]
            if (!(u in deps) || deps[u] == "") continue
            m = split(deps[u], hs, " ")
            for (j = 1; j <= m; j++) {
              if (!(hs[j] in own)) continue
              nv = split(own[hs[j]], vs, " ")
              for (v = 1; v <= nv; v++)
                if (!(vs[v] in live)) { live[vs[v]] = 1; q[++nq] = vs[v] }
            }
          }
          nlive = 0; nfresh = 0; nodd = 0
          for (u in seen) {
            if (u in live) { nlive++; continue }
            t = (u in newest) ? newest[u] : dmt[u]
            if (t + 0 >= now - 3600) { nfresh++; continue }
            if (u in parsed) { print "P " kb[u] + 0 " " u; continue }
            if (!(u in nf)) { print "P 0 " u; continue }
            nodd++
          }
          print "S " nup " " nslot " " nlive " " nfresh " " nodd " " nexp " " nstale
        }
      '
  )
  # The awk's tally: uplift roots, slot roots, live units, unreached but
  # inside the hour, unreadable, slots aged out, uplifts aged out.
  local nup="" nslot="" nlive="" nfresh="" nodd="" nexp="" nstale=""
  read -r nup nslot nlive nfresh nodd nexp nstale <<<"$stats" || true
  if [ "$stale_count" -gt 0 ]; then
    if [ "$DRY_RUN" = "1" ]; then
      warn "would prune $stale_count unreachable units under $root ($((stale_kb / 1024))MB)"
    else
      ok "pruned $stale_count unreachable units under $root ($((stale_kb / 1024))MB)"
    fi
  fi
  if [ -n "$nstale" ]; then
    ok "kept $nlive live units under $root ($nup uplifted, $nslot newest in their slot, the rest reached from those)"
    if [ "$nexp" -gt 0 ] || [ "$nstale" -gt 0 ]; then
      ok "rooted nothing from $nexp slots and $nstale uplifted units under $root left behind (slots over ${STALE_DAYS} days behind their group, uplifts by another toolchain)"
    fi
    if [ "$nfresh" -gt 0 ]; then
      ok "kept $nfresh unreached units under $root that are under an hour old (a build may still be writing them)"
    fi
    if [ "$nodd" -gt 0 ]; then
      warn "kept $nodd units under $root whose fingerprint this script cannot read"
    fi
  else
    warn "the unit walk under $root reported no tally -- nothing was pruned there"
  fi
  if [ "$held_count" -gt 0 ]; then
    warn "$held_count unreachable units under $root could not be fully deleted ($((held_kb / 1024))MB left in place) -- likely still in use; a later run reclaims them"
  fi
  return 0
}

before_gb="$(oam_dir_gb target)"
say "target/ is ${before_gb}GB before pruning (keep=$KEEP, dry-run=$DRY_RUN)"

# rm that never fails the script: a running image cannot be unlinked on
# Windows, and that is an expected outcome here, not an error.
gc_rm() {
  if [ "$DRY_RUN" = "1" ]; then return 0; fi
  rm -f "$@" 2>/dev/null || true
}

add_triple_trees

UNIT_TREES=()
for tree in "${TREES[@]}"; do
  [ -d "$tree" ] || continue

  if [ "$KEEP_INCREMENTAL" = "0" ] && [ -d "$tree/incremental" ]; then
    inc_gb="$(oam_dir_gb "$tree/incremental")"
    if [ "$DRY_RUN" = "1" ]; then
      warn "would delete $tree/incremental (${inc_gb}GB)"
    else
      rm -rf "$tree/incremental" 2>/dev/null || true
      # Report what happened, not what was asked for: the rm above cannot
      # fail the script, so only the directory's absence says it worked.
      if [ -e "$tree/incremental" ]; then
        warn "$tree/incremental could not be fully deleted (was ${inc_gb}GB, $(oam_dir_gb "$tree/incremental")GB left) -- something still holds files in it"
      else
        ok "deleted $tree/incremental (${inc_gb}GB)"
      fi
    fi
  fi

  if has_units "$tree"; then
    # Migration first: it needs no mtimes, so it runs even where the unit
    # prune cannot.
    migrate_old_layout "$tree"
    # The unit prune itself runs once per target dir, after this loop: a
    # tree's units can be named from a sibling tree.
    UNIT_TREES+=("$tree")
    # deps/ was handed to the migration above. The family prune below must
    # not walk it as well: under --dry-run it is still there, and the plan
    # reported its files twice (once as the dir, once per superseded file);
    # for real, whatever survived the rm is a held file the next run takes.
    continue
  fi

  [ -d "$tree/deps" ] || continue
  if [ "$LISTER" = "none" ]; then
    warn "$tree/deps: per-family prune skipped -- needs GNU find -printf or BSD stat -f, and this host has neither; incremental/ was still reclaimed"
    oam_reap_parked "$tree"; oam_reap_parked "$tree/deps"
    continue
  fi
  # Newest-first globally, so the FIRST time awk sees a family it is that
  # family's current artifact. Anything past --keep in the same family is a
  # superseded copy. Newline-delimited, not NUL: a space in a path is still
  # safe (only the tab and the newline separate, and cargo names outputs
  # <crate>-<hash>[.ext], which carry neither), while macOS's BSD awk takes
  # RS='\0' as an empty string and loses every record after the first.
  # Counted AFTER the rm, and only when the path is really gone: gc_rm cannot
  # fail, so counting beforehand reported an image a live process still holds
  # as reclaimed. A dry run counts the plan, which is what it reports.
  stale_count=0
  stale_kb=0
  held_count=0
  held_kb=0
  while IFS= read -r line; do
    kb="${line%% *}"; path="${line#* }"
    gc_rm "$path"
    if [ "$DRY_RUN" = "1" ] || [ ! -e "$path" ]; then
      stale_count=$((stale_count + 1))
      stale_kb=$((stale_kb + kb))
    else
      held_count=$((held_count + 1))
      held_kb=$((held_kb + kb))
    fi
  done < <(
    list_files "$tree/deps" |
      sort -rn |
      awk -v keep="$KEEP" -v now="$(date +%s)" -F'\t' '
        BEGIN {
          # Interval expressions ({16}) are NOT portable. mawk is the default
          # awk on Debian and Ubuntu -- the linux leg builds in a debian:12
          # container -- and the mawk 1.3.4 this was found on matches
          # NOTHING for /-[0-9a-f]{16}/ rather than erroring, so this whole
          # prune was a silent no-op on Linux: it reported a clean deps/ while
          # collecting zero of 2535 files (21GB). A dynamic string regex
          # behaves identically on mawk, gawk and BSD awk.
          hex = ""
          for (i = 0; i < 16; i++) hex = hex "[0-9a-f]"
          bare   = "-" hex "$"
          dotted = "-" hex "\\."
        }
        {
          path = $3
          n = split(path, seg, "/")
          base = seg[n]
          # cargo hash-names outputs in TWO shapes and both must be handled:
          #   <name>-<16 hex>.<ext>   rlib / rmeta / d, and .exe on Windows
          #   <name>-<16 hex>         bare test + bin executables on unix
          # Matching only the dotted shape blinded this prune to exactly the
          # files that dominate the tree: on the Linux builder 2026-08-22,
          # 107 extensionless binaries held 16GB of a 21GB deps/.
          if (match(base, bare)) {
            family = substr(base, 1, RSTART - 1)
            stem = base
          } else if (match(base, dotted)) {
            stem = substr(base, 1, RSTART + 16)
            # A codegen-unit object, <name>-<hash>.<cgu>.rcgu.o, is not a family
            # of its own: every build names its units afresh, so each object
            # was a family of one and none was ever collected. macOS leaves
            # them all in deps/ (an executable`s debug info lives in its own
            # unit`s objects), and on the Air 2026-09-25 they held most of a
            # 49GB deps/, 57 generations of oam_engine alone. So an object
            # stays while its unit survives in this tree -- as an executable
            # (whose debug info it is) or as a <name>-<hash>.d -- and goes once
            # neither does (decided in END, once every file is seen). Never on
            # the .d alone: clippy and cargo check write .d files into the same
            # family, so a test harness loses its .d generations before the
            # harness itself goes. Deciding that from what THIS run pruned
            # took a live xtask binary`s debug info in one version, and
            # orphaned every harness`s objects for good in the next. An object
            # younger than an hour whose unit is not here yet is left alone:
            # that is a build still writing it.
            if (base ~ /\.rcgu\.o$/) {
              obj[++nobj] = $2 " " path; objstem[nobj] = stem; objmt[nobj] = $1 + 0
              next
            }
            family = substr(base, 1, RSTART - 1) substr(base, RSTART + RLENGTH)
          } else {
            next
          }
          # A crate version has several LIVE .rmeta files at once: one per
          # target cargo check ran (lib, lib test, bin, bin test) beside the
          # build`s own, and oam links getrandom at three versions. Evicting
          # any of them re-checks or rebuilds that crate and everything above
          # it on the next run, every run. They are metadata only (0.44GB for
          # the Air`s whole tree), so they get four times the keep.
          k = keep
          if (base ~ /\.rmeta$/) k = keep * 4
          if (++seen[family] > k) {
            print $2 " " path
          } else {
            if (stem == base || base ~ /\.exe$/) exe[stem] = 1
            if (base ~ /\.d$/) keptd[stem] = 1
          }
        }
        END {
          for (i = 1; i <= nobj; i++) {
            s = objstem[i]
            if (!(s in exe) && !(s in keptd) && objmt[i] < now - 3600) print obj[i]
          }
        }
      '
  )
  if [ "$stale_count" -gt 0 ]; then
    if [ "$DRY_RUN" = "1" ]; then
      warn "would prune $stale_count superseded artifacts from $tree/deps ($((stale_kb / 1024))MB)"
    else
      ok "pruned $stale_count superseded artifacts from $tree/deps ($((stale_kb / 1024))MB)"
    fi
  fi
  if [ "$held_count" -gt 0 ]; then
    warn "$held_count superseded artifacts in $tree/deps could not be deleted ($((held_kb / 1024))MB left in place) -- likely still in use; a later run reclaims them"
  fi

  oam_reap_parked "$tree"
  oam_reap_parked "$tree/deps"
done

# The unit prune: one mark-and-sweep per target dir, over all of its trees.
# Roots in the order their first tree is listed, so the output is stable.
roots=()
for tree in ${UNIT_TREES[@]+"${UNIT_TREES[@]}"}; do
  r="$(tree_root "$tree")"
  case " ${roots[*]-} " in *" $r "*) ;; *) roots+=("$r") ;; esac
done
for r in ${roots[@]+"${roots[@]}"}; do
  members=()
  for tree in "${UNIT_TREES[@]}"; do
    if [ "$(tree_root "$tree")" = "$r" ]; then members+=("$tree"); fi
  done
  if [ "$LISTER" = "none" ]; then
    warn "$r: unit prune skipped for ${members[*]} -- needs GNU find -printf or BSD stat -f, and this host has neither; incremental/ was still reclaimed"
  else
    prune_root "$r" "${members[@]}"
  fi
done
for tree in ${UNIT_TREES[@]+"${UNIT_TREES[@]}"}; do
  # Parked copies land in each unit's out/ (scripts/lib/build-locks.sh:
  # oam_park_bin) as well as at the top of the tree.
  # shellcheck disable=SC2231  # $HEX16 is a glob and must stay unquoted
  for d in "$tree"/build/*/$HEX16/out; do
    oam_reap_parked "$d"
  done
  oam_reap_parked "$tree"
done

after_gb="$(oam_dir_gb target)"
if [ "$DRY_RUN" = "1" ]; then
  say "dry run -- nothing deleted (target/ still ${after_gb}GB)"
else
  say "target/ is ${after_gb}GB (was ${before_gb}GB)"
fi
