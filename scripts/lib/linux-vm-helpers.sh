# shellcheck shell=bash
# =============================================================================
# Pure decisions of the linux leg (build-platforms-mac-linux.sh), lifted out so
# they can be TESTED.
# =============================================================================
# The linux leg builds in an x86_64 Debian 12 container under Rosetta, inside a
# Lima VM on the build Mac. The orchestrator runs top-to-bottom against that
# Mac, so a test cannot source it; these are the parts of it that are pure --
# a text in, a verdict out:
#
#   version_le            dotted-number comparison, for the glibc floor
#   glibc_max_version     the newest GLIBC_x.y symbol version a binary needs,
#                         from what `objdump -T` printed for it
#   lima_name_valid       an instance or container name that is safe to put on
#                         the Mac's command line
#   kv_field              one key=value field of what the Mac-side driver
#                         (scripts/mac-linux-vm.sh) printed
#   uint_valid            a whole number of CPUs / GiB, 1 or more
#
# Same convention as lib/host-helpers.sh: sourced, never executed. All
# functions RETURN status rather than exiting; the caller owns fail()/warn().
# =============================================================================

# version_le <a> <b>
# 0 when dotted version <a> is at or below <b>, compared field by field as
# numbers (2.9 < 2.17 < 2.34; a missing field counts as 0, so 2.34 = 2.34.0).
# 1 when <a> is newer. 2, without comparing, when either is not digits and
# dots: the glibc gate must never read "I could not tell" as "old enough".
version_le() {
  local a="${1//$'\r'/}" b="${2//$'\r'/}" i x y
  [[ "$a" =~ ^[0-9]+(\.[0-9]+)*$ ]] || return 2
  [[ "$b" =~ ^[0-9]+(\.[0-9]+)*$ ]] || return 2
  local -a av bv
  IFS=. read -r -a av <<<"$a"
  IFS=. read -r -a bv <<<"$b"
  for ((i = 0; i < ${#av[@]} || i < ${#bv[@]}; i++)); do
    # 10# keeps a leading zero from being read as octal.
    x=$((10#${av[i]:-0})); y=$((10#${bv[i]:-0}))
    [ "$x" -lt "$y" ] && return 0
    [ "$x" -gt "$y" ] && return 1
  done
  return 0
}

# glibc_max_version <objdump-text>
# Echoes the newest version among the GLIBC_<n>.<n>[.<n>] tokens in the text
# (`objdump -T` prints one per dynamic symbol, "(GLIBC_2.34) pthread_create"
# and the like), without the GLIBC_ prefix. GLIBC_PRIVATE and other non-numeric
# tags are not versions and are skipped. Non-zero, echoing nothing, when there
# is no version at all: a dynamically linked oam always needs some, so an
# empty answer means objdump failed or read the wrong file, and the gate must
# fail on it rather than pass.
glibc_max_version() {
  local t="${1//$'\r'/}" v max=""
  while IFS= read -r v; do
    v="${v#GLIBC_}"
    if [ -z "$max" ] || ! version_le "$v" "$max"; then max="$v"; fi
  done < <(grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' <<<"$t" || true)
  [ -n "$max" ] || return 1
  printf '%s' "$max"
}

# lima_name_valid <name>
# 0 for a name made of letters, digits and inner hyphens: what Lima accepts
# for an instance, and nothing a shell on the Mac or in the VM would read as
# more than one word.
lima_name_valid() {
  [[ "$1" =~ ^[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?$ ]]
}

# kv_field <text> <key>
# Echoes the value of the first `<key>=<value>` line of <text>, CR stripped
# (ssh on Windows). Non-zero when no line carries the key.
kv_field() {
  local t="${1//$'\r'/}" key="$2" line
  while IFS= read -r line; do
    case "$line" in
      "$key="*) printf '%s' "${line#"$key="}"; return 0 ;;
    esac
  done <<<"$t"
  return 1
}

# uint_valid <value>   -- 0 for a whole number, 1 or more.
uint_valid() {
  case "$1" in '' | *[!0-9]* | 0 | 0*) return 1 ;; esac
  return 0
}
