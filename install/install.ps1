# oam installer (Windows). Canonical home: https://oamjs.org/install.ps1
#
#   irm https://oamjs.org/install.ps1 | iex
#
# Downloads the release binary for this arch from GitHub Releases, verifies it,
# installs it to %LOCALAPPDATA%\oam\bin, and adds that dir to the user PATH. No
# admin. From v0.18.0 the binaries are also Authenticode-signed, which this
# script does not check yet (see the hook below); what it verifies is the
# signed release, exactly as install.sh checks it:
#
#   v0.18.0 and later   RELEASE-MANIFEST must carry a RELEASE-MANIFEST.sig that
#                       `ssh-keygen -Y verify` accepts against the release keys
#                       EMBEDDED below, be signed for exactly the tag being
#                       installed, by a key whose range covers that tag. The
#                       binary's sha256 comes from that signed manifest.
#   before v0.18.0      cut before signing existed. The release's SHA256SUMS
#                       must hash to the digest pinned below for its tag. A
#                       pre-v0.18.0 tag that is not pinned is refused.
#
# A signature that is present but does not verify always fails; so does a
# v0.18.0+ release with no manifest. release-keys/README.md describes the keys.
#
# Env overrides:
#   OAM_VERSION       install a specific tag (e.g. v0.18.0); default: latest
#   OAM_INSTALL_DIR   install location; default: %LOCALAPPDATA%\oam\bin
#   OAM_INSTALL_BASE  asset base URL for a mirror or CDN; default: GitHub
#                     Releases. Requires OAM_VERSION: the signature is checked
#                     against the tag you asked for, and a mirror cannot be
#                     asked which tag it serves.
#   OAM_INSECURE_SKIP_SIGNATURE=1
#                     install a v0.18.0+ release on a host with no usable
#                     ssh-keygen, WITHOUT checking its signature. Only a
#                     missing tool is skipped: with ssh-keygen present the
#                     signature is always checked, and a bad one always fails.
#   OAM_SKIP_PATH_SHADOW=1
#                     install even though another oam earlier on PATH would
#                     still be the one that runs (a dev build, or another
#                     product's bundled copy). Loud: this install does not
#                     take effect until PATH is reordered.
#   GH_TOKEN          Optional GitHub token (GITHUB_TOKEN is also accepted).
#                     The repo and its releases are public, so a normal
#                     install needs none. It is only a fallback, tried when
#                     the unauthenticated download fails -- e.g. a GitHub
#                     Enterprise mirror (OAM_GH_API) that requires auth, on a
#                     headless host that has a token but no gh CLI.
#   OAM_GH_API        GitHub API base; default https://api.github.com
#
# Windows PowerShell 5.1 compatible, ASCII only. Two 5.1 traps shape the code:
# a native command's stderr under $ErrorActionPreference = 'Stop' turns into a
# terminating error once redirected, and PowerShell pipes text, not bytes, to
# a native command's stdin -- so ssh-keygen runs through Invoke-Native below.
$ErrorActionPreference = 'Stop'
# 5.1's Invoke-WebRequest redraws its progress bar per chunk, which makes a
# download many times slower than the transfer itself (measured here: three
# minutes for a run that takes thirty seconds without it).
$ProgressPreference = 'SilentlyContinue'
Set-StrictMode -Version Latest

$ownerRepo = 'YawLabs/oam'
$installDir = if ($env:OAM_INSTALL_DIR) { $env:OAM_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'oam\bin' }
$ghApi = if ($env:OAM_GH_API) { $env:OAM_GH_API } else { 'https://api.github.com' }
# gh CLI convention first, then the Actions-provided name.
$token = if ($env:GH_TOKEN) { $env:GH_TOKEN } elseif ($env:GITHUB_TOKEN) { $env:GITHUB_TOKEN } else { '' }
# Declared up front: StrictMode makes reading an unset variable a hard error.
$script:relJson = $null

# The first tag cut with a signed RELEASE-MANIFEST: every tag from here on must
# carry one that verifies, every tag before it is checked against the pinned
# table. install.sh carries the same cutoff (scripts/test-scripts.sh checks).
$firstManifestSigTag = 'v0.18.0'
$signNamespace = 'oam-release'
$manifestHeader = 'oam-release-manifest v1'
$principalPrefix = 'oam-release-'

function Say($m) { Write-Host "oam-install: $m" }
function Warn($m) { Write-Warning "oam-install: $m" }
# Never call Die inside a try/catch: under 'Stop' it throws, and a catch would
# swallow the refusal.
function Die($m) { Write-Error "oam-install: error: $m"; exit 1 }

# --- The trust root, embedded ---------------------------------------------------
# Byte-identical copies of release-keys/allowed_signers, release-keys/ranges and
# release-keys/presigning-sums; scripts/test-scripts.sh fails on any drift.
# Embedded, never fetched: a key list downloaded from where the release came from
# is whatever whoever controls the release says it is. There is deliberately no
# environment variable that points these anywhere else -- that is exactly the
# knob an attacker would set. (The test suite swaps these blocks in a COPY of
# this file.) Single-quoted here-strings: the bodies are verbatim.
$embeddedAllowedSigners = @'
# oam release keys -- the PUBLIC halves only (ssh-keygen allowed_signers format).
#
# Every oam release from the first signed one on carries RELEASE-MANIFEST and
# RELEASE-MANIFEST.sig; this file names the keys allowed to have made that
# signature. Read by scripts/lib/signing.sh (release_verify_manifest), and the
# source of the key set the installers and `oam self-update` embed.
#
# One line per key:
#
#   oam-release-<id> namespaces="oam-release" ssh-ed25519 <base64> [comment]
#
#   - the principal is exactly "oam-release-<id>"; <id> is the name its line in
#     ./ranges uses (k1, k2, ...)
#   - namespaces="oam-release" is required: it stops a signature this key made
#     for anything else from verifying as a release
#   - ssh-ed25519 (or sk-ssh-ed25519@openssh.com for a hardware key)
#
# k1 is the current release key (signs from v0.18.0, see ./ranges); k2 is the
# staged next key, with no range yet. Because key lines exist,
# every release must sign; there is no knob that skips it.
#
#   k1  SHA256:zB7Aq4Ky/U90VJ4sAEp0e2A65KfQpiyQXJI4FuT2oss
#   k2  SHA256:Uy7nugF5mDzfM/8/fcUti9K+sQMbn/LdRAk+sIbWYs4
#
# Never remove a key line to retire a key: old tags it signed must keep
# verifying. Close its range in ./ranges instead.
oam-release-k1 namespaces="oam-release" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJuD0+abFvf+zbUclRzT4uqdebLr6CU8Ps2PE+Qev0/k
oam-release-k2 namespaces="oam-release" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDdFy96BmdBfcHwXjWDTod9i5sfvJk7wu5MI7j4SnyoR
'@
$embeddedRanges = @'
# Which tags each release key may sign. Read by scripts/lib/signing.sh
# (release_tag_in_range); the installers and `oam self-update` embed it.
#
# At most one line per key in ./allowed_signers:
#
#   <id> <from-tag> <to-tag>
#
#   - <id> is the principal's suffix: "k1" for oam-release-k1
#   - <from-tag> is inclusive; <to-tag> is inclusive, or "-" for open-ended
#   - tags are plain vMAJOR.MINOR.PATCH, compared numerically per field
#   - a key with no line (the staged "next" key) may sign nothing yet
#
# Example: k1 signs from v0.18.0 on; at rotation k1 is closed at the last tag
# it signed and k2 opens at the next one:
#
#   k1 v0.18.0 v0.21.3
#   k2 v0.21.4 -
#
# A verifier accepts a manifest only when the key that signed it has a range
# covering the tag INSIDE the signed manifest. That is why retiring a key is an
# edit here, not a deletion from allowed_signers, and why it never breaks the
# old tags the key legitimately signed.
#
# k1 signs from v0.18.0, the first signed release. k2 is staged: no line yet.
k1 v0.18.0 -
'@
$embeddedPresigningSums = @'
# SHA-256 of each pre-signing release's published SHA256SUMS (tags before v0.18.0).
# Bounded and immutable: captured 2026-10-02 from the published releases.
# A pre-v0.18.0 tag not listed here is refused. Format: <tag> <sha256-hex>
v0.6.1 2d0082d28b70b171468bd2562300a7b33b847bf0bf3c9e93d0c2e0cc3613f5d3
v0.7.0 28e52f688c4269240d3baebbc4376ea03d21091fb2b5b9ae9068870e5354ba6c
v0.8.0 a6bfd6e74f1a8c0710c7846be9ec67206caa28fee7cdeba359ddd1f7af1eb0af
v0.8.1 d3e806ca0f5b20ec3bd0a480e55d3b2941b687358261196bf00fd7877d86d403
v0.8.2 fb85c1c2e0b514e50748b72937dc1f9d5f675ad120cce57ceb27f93611ab5f75
v0.8.3 b142260e172646b3e704e4a1a85e1a810cdc590e6d4a4f1fcb8633e753d417da
v0.9.0 6a7cbbc21b464525885254ff729f1f927546c3c88952d2f867c3d2167fa4743e
v0.9.1 abc9607cf134c0a3123618700d892eda5a20b7759f90efd68c5b70e91bc37c36
v0.10.2 1623ff13605ca4181e7262489e0d2a01baf9e99040cb1bb60fae926d9c339bd9
v0.11.0 149fc07a00aefe35f8730195cdc9199a26e4d45cda6c9b884dab7ce5ae16e118
v0.12.0 0942ec0eeb56074c71129cb857f05104194e9c98ae100bb023e037780b937581
v0.12.1 3bd908edc17737a68dacbe26e4dd28fdfdf7ce779971ade1b6cc7dcfd14cbde7
v0.13.0 f28c6aa95c7a173e5318ef809ecdf69cbf1c84945d04e6d9f3011765146f8265
v0.13.1 78e1eeea71a51c4076677d344859fe0cbe905196e852a14992adc8bdae5a7e45
v0.14.0 a92fc06ab7bfde6c15bada68dedd2964630f2ed09c5c1f48ffa8b097e07fa6e4
v0.15.0 7d3f57342cea6e96a72f61e25690f56a3b052baf3ee4b8a391d493a7f3285e7c
v0.15.1 ddb1ea652623ac8e74cf7b173ea9d6460883a06d7605907075a00f758e98182c
v0.15.2 1a1e6d63bd49d0741eea7cd9ffdca06780b2cf00c496595b71f3755a8b9b5137
v0.15.3 653760ba45a1dc636662dde47f951bbbfd714aa5f658dcc0a8ac8e8b0efc7afd
v0.16.1 a35db0c44321f4f003a259989eb12efc522fdeb1871b53835ef28f561921fe39
v0.16.2 2b50575f440e747bd69b30af08a1113431b70823cf7a801a594164836c41f540
v0.16.3 ea11b31eb8b92aa8576545de66a0e1f9b65db3b8deba4428f0f9b2b1d14087f6
v0.16.4 28f7e3485703f7381111d8c49dba096dc0714e018eb264ac3d0d29d61c3ec19f
v0.17.0 78c4b2249f42e1959f6f6d2ca29d613c32a8808360c170702d211d26cc104765
v0.17.1 d13c587307519995fabe05a999abdfae9292921b261e5808082b53e7acbe79a9
'@

# The non-comment, non-blank lines of an embedded file.
function Get-DataLines([string]$text) {
  @($text -split "`n" | ForEach-Object { $_.TrimEnd("`r") } | Where-Object { $_ -notmatch '^[ \t]*(#|$)' })
}

# vMAJOR.MINOR.PATCH and nothing else. [0-9], not \d: \d matches any Unicode
# digit in .NET. -cmatch: the v is lower-case in every tag. \z, not $: .NET's
# $ also matches before a final newline, so "v0.17.1`n" would pass.
function Test-PlainTag([string]$t) { return ($t -cmatch '^v[0-9]+\.[0-9]+\.[0-9]+\z') }

# 0 when plain tag a <= b, numerically per field.
function Test-TagLe([string]$a, [string]$b) {
  return ([version]$a.Substring(1)) -le ([version]$b.Substring(1))
}

# Map arch -> Rust target triple (must match the asset names
# scripts/release-local.sh publishes; install/README.md lists them). A 32-bit
# PowerShell on 64-bit Windows reports x86 here and the real arch in
# PROCESSOR_ARCHITEW6432; there is no 32-bit oam, so the real one decides.
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
switch ($arch) {
  'AMD64' { $target = 'x86_64-pc-windows-msvc' }
  'ARM64' { $target = 'aarch64-pc-windows-msvc' }
  default { Die "unsupported Windows arch: $arch" }
}
$asset = "oam-$target.exe"

# Resolve the tag ONCE. Every asset below comes from /download/<tag>/, never
# from /latest/download/: two separate "latest" fetches can straddle a release
# and pair one tag's binary with another's checksums -- and the signature check
# needs to know which tag it is checking.
function Get-LatestTag {
  # The /releases/latest redirect names the tag; HEAD, with the redirect not
  # followed, so no page body is fetched. HttpWebRequest rather than
  # Invoke-WebRequest, whose redirect handling differs between 5.1 and 7.
  try {
    $req = [System.Net.HttpWebRequest]::Create("https://github.com/$ownerRepo/releases/latest")
    $req.Method = 'HEAD'
    $req.AllowAutoRedirect = $false
    $req.UserAgent = 'oam-install'
    $resp = $req.GetResponse()
    try { $loc = $resp.Headers['Location'] } finally { $resp.Close() }
    if ($loc -and $loc -match '/releases/tag/([^/?#]+)$') { return $Matches[1] }
  } catch { }
  # Fallbacks for when that redirect gives no tag (a host that requires auth,
  # a proxy that eats the redirect): the API with the token, then the gh CLI.
  if ($token) {
    try {
      $rel = Invoke-RestMethod -Uri "$ghApi/repos/$ownerRepo/releases/latest" -UseBasicParsing -Headers @{
        Authorization = "Bearer $token"; Accept = 'application/vnd.github+json'; 'User-Agent' = 'oam-install'
      }
      if ($rel.tag_name) { return [string]$rel.tag_name }
    } catch { }
  }
  if (Get-Command gh -ErrorAction SilentlyContinue) {
    try { $t = (gh release view --repo $ownerRepo --json tagName -q .tagName) } catch { $t = $null }
    if ($t) { return [string]$t }
  }
  return $null
}

# Refuse to install where another oam would win: whatever Get-Command resolves
# for `oam` (PATHEXT included, so oam.cmd/oam.bat count) is still the oam that
# runs after this install, because this script only APPENDS the install dir to
# PATH -- appending cannot fix a shadow, only reordering can. Checked before
# tag resolution so a shadowed host fails instantly, with no network round
# trip. (install.sh PREPENDS in its guidance, so its check is gated on the dir
# already being a PATH entry; this one is not.)
$shadowCmd = Get-Command oam -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
if ($shadowCmd -and "$($shadowCmd.Path)" -ne (Join-Path $installDir 'oam.exe')) {
  if ($env:OAM_SKIP_PATH_SHADOW -eq '1') {
    Warn "OAM_SKIP_PATH_SHADOW=1: installing even though another oam would win on PATH ($($shadowCmd.Path) resolves ahead of $installDir) -- this install is not the oam that runs until PATH is reordered"
  } else {
    Die "another oam would win on PATH: $($shadowCmd.Path) resolves ahead of $installDir -- remove it (typically a dev build or a stale copy), put $installDir earlier in PATH, or set OAM_SKIP_PATH_SHADOW=1 to install anyway"
  }
}

if ($env:OAM_VERSION) {
  $tag = $env:OAM_VERSION
} elseif ($env:OAM_INSTALL_BASE) {
  Die "OAM_INSTALL_BASE needs OAM_VERSION too: the signature is checked against the tag you ask for, and a mirror cannot be asked which tag it serves (e.g. OAM_VERSION=$firstManifestSigTag)"
} else {
  $tag = Get-LatestTag
  if (-not $tag) { Die 'could not resolve the latest release tag (network or proxy? set OAM_VERSION to name the tag; if your host requires GitHub auth, set GH_TOKEN or install the gh CLI)' }
}
if (-not (Test-PlainTag $tag)) { Die "'$tag' is not a release tag -- oam tags look like $firstManifestSigTag" }

$base = if ($env:OAM_INSTALL_BASE) { $env:OAM_INSTALL_BASE } else { "https://github.com/$ownerRepo/releases/download/$tag" }

# Authenticated fallback. The releases are public, so this is not the normal
# path: if the direct download fails and the gh CLI is available, fetch the
# same assets through the caller's GitHub auth.
function Get-ViaGh($pattern, $outFile) {
  if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { return $false }
  # gh's exit status decides, not the file alone: a failed download leaves
  # whatever was already at $outFile, and that would read as a success.
  # Out-Null: anything gh prints on stdout would otherwise join this
  # function's return value. $LASTEXITCODE is set first: StrictMode makes
  # reading it a hard error when no native command has run yet (a gh that is
  # a script or function never sets it).
  $global:LASTEXITCODE = 0
  try { gh release download $tag --repo $ownerRepo --pattern $pattern --output $outFile --clobber | Out-Null } catch { return $false }
  if ($LASTEXITCODE -ne 0) { return $false }
  return (Test-Path $outFile)
}

# Token fallback: works on headless hosts with no gh CLI. An asset that requires auth
# is NOT reachable via its browser_download_url even with a token -- GitHub
# only serves the bytes from the assets endpoint with an octet-stream Accept
# -- so resolve the numeric asset id first.
function Get-ViaToken($assetName, $outFile) {
  if (-not $token) { return $false }
  try {
    if (-not $script:relJson) {
      $script:relJson = Invoke-RestMethod -Uri "$ghApi/repos/$ownerRepo/releases/tags/$tag" -UseBasicParsing -Headers @{
        Authorization = "Bearer $token"; Accept = 'application/vnd.github+json'; 'User-Agent' = 'oam-install'
      }
    }
    $a = $script:relJson.assets | Where-Object { $_.name -eq $assetName } | Select-Object -First 1
    if (-not $a) { return $false }
    Invoke-WebRequest -Uri "$ghApi/repos/$ownerRepo/releases/assets/$($a.id)" -OutFile $outFile -UseBasicParsing -Headers @{
      Authorization = "Bearer $token"; Accept = 'application/octet-stream'; 'User-Agent' = 'oam-install'
    }
    return (Test-Path $outFile)
  } catch { return $false }
}

# Direct first (public releases + a mirror), then token, then gh CLI. All three
# fetch the same tag, and the manifest, the checksums, the binary and the
# license and attribution texts are verified the same way whichever of them
# served the bytes.
function Get-Asset($assetName, $outFile) {
  try { Invoke-WebRequest -Uri "$base/$assetName" -OutFile $outFile -UseBasicParsing; return $true } catch { }
  if ($token) {
    Say "direct download of $assetName failed; retrying with `$GH_TOKEN"
    if (Get-ViaToken $assetName $outFile) { return $true }
  }
  if (Get-Command gh -ErrorAction SilentlyContinue) {
    Say "retrying $assetName via gh CLI"
    return (Get-ViaGh $assetName $outFile)
  }
  return $false
}

# Run a native program with a file as its stdin, BYTE for byte, and return
# its exit code and combined output. Start-Process opens the files itself and
# hands the child their handles as its standard handles, so nothing of
# PowerShell's or .NET's sits between the file and the program.
#
# Not `Get-Content | & exe`: PowerShell re-encodes piped text and appends a
# newline. And not [Diagnostics.Process] with RedirectStandardInput: .NET
# Framework wraps that pipe in a StreamWriter using the console's input
# encoding, and under a UTF-8 console (code page 65001 -- Windows Terminal,
# `chcp 65001`) that encoding carries a byte-order mark, which the writer
# emits the moment the process starts: three bytes ahead of the message, and
# a signature that no longer verifies. Named $argv, not $args: $args is
# automatic.
function Invoke-Native([string]$exe, [string[]]$argv, [string]$stdinFile) {
  $io = Join-Path ([System.IO.Path]::GetTempPath()) ("oam-install-io-" + [System.Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $io -Force | Out-Null
  try {
    $outFile = Join-Path $io 'out'
    $errFile = Join-Path $io 'err'
    # stdin is always a file: a child that reads nothing gets an empty one
    # rather than this console's input.
    $inFile = $stdinFile
    if (-not $inFile) { $inFile = Join-Path $io 'in'; [System.IO.File]::WriteAllBytes($inFile, [byte[]]@()) }
    $start = @{
      FilePath = $exe
      RedirectStandardInput = $inFile
      RedirectStandardOutput = $outFile
      RedirectStandardError = $errFile
      # Hidden, not -NoNewWindow: under a host with no console of its own
      # (PowerShell ISE, an embedded engine) -NoNewWindow lets each
      # console-subsystem child open a visible window; a hidden new console
      # behaves the same with and without a parent console.
      WindowStyle = 'Hidden'
      Wait = $true
      PassThru = $true
    }
    if ($argv.Count -gt 0) { $start.ArgumentList = (@($argv | ForEach-Object { '"' + $_ + '"' }) -join ' ') }
    $p = Start-Process @start
    $text = [System.Text.Encoding]::UTF8.GetString([System.IO.File]::ReadAllBytes($outFile)) +
            [System.Text.Encoding]::UTF8.GetString([System.IO.File]::ReadAllBytes($errFile))
    return @{ Code = $p.ExitCode; Output = $text }
  } finally {
    Remove-Item -Path $io -Recurse -Force -ErrorAction SilentlyContinue
  }
}

# An ssh-keygen that understands -Y verify (OpenSSH 8.1+). Sysnative first: a
# 32-bit PowerShell on 64-bit Windows sees System32 redirected to SysWOW64,
# which has no OpenSSH, and reaches the real one only through Sysnative. Then
# System32, then PATH (a removed inbox feature, Git for Windows, scoop). The
# probe asks the tool itself: an unknown -Y operation. 8.2 and later answer
# "Unsupported operation for -Y"; 8.1 (the Windows 10 inbox client) has no
# such text and, given a namespace, falls through to its usage, which lists
# "ssh-keygen -Y verify"; a tool from before -Y rejects the flag and prints a
# usage without that line. -n is passed because 8.1 checks for it BEFORE it
# looks at the operation ("Too few arguments for sign/verify: missing
# namespace").
$sshKeygenCandidates = @("$env:windir\Sysnative\OpenSSH\ssh-keygen.exe", "$env:windir\System32\OpenSSH\ssh-keygen.exe")
function Find-SshKeygen {
  $found = @($sshKeygenCandidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf })
  $cmd = Get-Command ssh-keygen -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($cmd) { $found += $cmd.Path }
  foreach ($kg in $found) {
    try { $r = Invoke-Native $kg @('-Y', 'oam-probe', '-n', $signNamespace) '' } catch { continue }
    if ($r.Output -match 'Unsupported operation for -Y|ssh-keygen -Y verify') { return $kg }
  }
  return $null
}
$keygenFix = 'install the OpenSSH client (OpenSSH 8.1 or later), from an elevated PowerShell: Add-WindowsCapability -Online -Name OpenSSH.Client~~~~0.0.1.0'

# Comparing hashes: lower-case hex, from Get-FileHash.
function Get-Sha256([string]$path) { (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLower() }

# Every sha256 a SHA256SUMS text lists for $name. Field 2 matched exactly,
# sha256sum's binary-mode "*" stripped.
function Get-SumsHits([string]$text, [string]$name) {
  $hits = @()
  foreach ($l in ($text -split "`n")) {
    $parts = $l -split '[ \t]+'
    if ($parts.Count -ge 2 -and ($parts[1] -replace '^\*', '') -ceq $name) { $hits += $parts[0].ToLower() }
  }
  return ,$hits
}
# The sha256 a SHA256SUMS text lists for $name (default $asset), when it lists
# it exactly once. Two entries are refused rather than resolved: the first of a
# duplicate pair has been the stale one.
function Get-HashFromSums([string]$text, [string]$name = $asset) {
  $hits = Get-SumsHits $text $name
  if ($hits.Count -ne 1) { Die "the verified checksums for $tag list $name $($hits.Count) times, not once" }
  return $hits[0]
}

# The user PATH, exactly as HKCU\Environment stores it. Not
# [Environment]::GetEnvironmentVariable('Path', 'User') and its Set twin: that
# read returns the value with every %VAR% reference expanded, and that write
# stores a plain string, so appending one directory through the pair would
# permanently flatten an entry such as %USERPROFILE%\bin into today's
# expansion. (That is .NET's documented behaviour, taken from its reference,
# not measured on a box here.) Read unexpanded; written back with the kind
# the value already had, REG_EXPAND_SZ for a new one.
$userEnvKey = 'Environment'
$noExpand = [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
function Get-UserPathRaw {
  $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($userEnvKey)
  if (-not $k) { return '' }
  try { return [string]$k.GetValue('Path', '', $noExpand) } finally { $k.Close() }
}
function Set-UserPath([string]$value) {
  $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($userEnvKey)
  try {
    $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
    if ($null -ne $k.GetValue('Path', $null, $noExpand)) {
      $had = $k.GetValueKind('Path')
      if ($had -eq [Microsoft.Win32.RegistryValueKind]::String) { $kind = $had }
    }
    $k.SetValue('Path', $value, $kind)
  } finally { $k.Close() }
  # A raw registry write tells no running program the environment changed.
  # SetEnvironmentVariable broadcasts WM_SETTINGCHANGE after every user-scope
  # write, so clearing a variable that does not exist sends that one
  # broadcast and changes nothing (again from the .NET reference, not
  # measured here). Best-effort: without it only a new sign-in sees the PATH.
  try { [Environment]::SetEnvironmentVariable('OAM_INSTALL_PATH_NOTIFY', $null, 'User') } catch { }
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("oam-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
  $binPath = Join-Path $tmp $asset

  # --- Verify the release BEFORE the binary is downloaded -----------------------
  if (Test-TagLe $firstManifestSigTag $tag) {
    $keygen = Find-SshKeygen
    if (-not $keygen) {
      if ($env:OAM_INSECURE_SKIP_SIGNATURE -eq '1') {
        Warn "no ssh-keygen with -Y found, so the signature of $tag CANNOT be checked."
        Warn 'OAM_INSECURE_SKIP_SIGNATURE=1: installing WITHOUT signature verification. The checksum below only proves the download matches what the release says, not that we published it.'
        Warn "to verify instead: $keygenFix"
      } else {
        Die "no ssh-keygen with -Y found, and oam $tag is verified by its signature (ssh-keygen -Y verify). Fix: $keygenFix. (To install anyway, unverified: set OAM_INSECURE_SKIP_SIGNATURE=1.)"
      }
    }
    $mPath = Join-Path $tmp 'RELEASE-MANIFEST'
    $sPath = Join-Path $tmp 'RELEASE-MANIFEST.sig'
    if (-not (Get-Asset 'RELEASE-MANIFEST' $mPath)) { Die "could not fetch RELEASE-MANIFEST for $tag -- every release from $firstManifestSigTag on is signed, so a missing manifest means the download failed or the release was tampered with" }
    if (-not (Get-Asset 'RELEASE-MANIFEST.sig' $sPath)) { Die "could not fetch RELEASE-MANIFEST.sig for $tag -- every release from $firstManifestSigTag on is signed, so a missing signature means the download failed or the release was tampered with" }

    $principal = $null
    if ($keygen) {
      Say "verifying the signature with $keygen"
      # 1. The signature, against each embedded principal in turn with -I.
      #    The key file is written LF-only and without a BOM, as committed.
      $asPath = Join-Path $tmp 'allowed_signers'
      [System.IO.File]::WriteAllText($asPath, (($embeddedAllowedSigners -replace "`r`n", "`n") + "`n"), (New-Object System.Text.ASCIIEncoding))
      #    Every key's first error line is kept, by principal: the last key
      #    tried is almost never the signer, and its "Could not verify
      #    signature." says nothing about why the signer's own attempt failed.
      $why = @()
      foreach ($line in (Get-DataLines $embeddedAllowedSigners)) {
        $p = ($line -split '[ \t]+')[0]
        if (-not $p.StartsWith($principalPrefix, [System.StringComparison]::Ordinal)) { continue }
        $r = Invoke-Native $keygen @('-Y', 'verify', '-f', $asPath, '-I', $p, '-n', $signNamespace, '-s', $sPath) $mPath
        if ($r.Code -eq 0) { $principal = $p; break }
        $why += "${p}: " + ($r.Output -split "`n")[0].Trim() + " (exit $($r.Code))"
      }
      if (-not $principal) { Die "RELEASE-MANIFEST.sig for $tag does not verify against any oam release key ($($why -join '; ')) -- this is not a release we signed; refusing it" }
    }

    # 2. The header, only now that the bytes are known to be ours: line 1 the
    #    v1 header, line 2 exactly "tag <the tag we resolved>", LF-only.
    $mText = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($mPath))
    $prefix = "$manifestHeader`ntag $tag`n"
    if (-not $mText.StartsWith($prefix, [System.StringComparison]::Ordinal)) {
      $lines = $mText -split "`n"
      $line2 = if ($lines.Count -ge 2) { $lines[1] } else { '' }
      if (($lines[0] + $line2).Contains("`r")) { Die "RELEASE-MANIFEST for $tag has CR line endings -- it is LF-only, byte for byte; refusing it" }
      if ($lines[0] -ceq $manifestHeader -and $line2.StartsWith('tag ', [System.StringComparison]::Ordinal)) {
        Die "RELEASE-MANIFEST is signed for tag '$($line2.Substring(4))', not $tag -- a replayed or misfiled release; refusing it"
      }
      Die "RELEASE-MANIFEST for $tag does not start with '$manifestHeader' / 'tag $tag' -- refusing it"
    }

    # 3. The signing key's range covers the tag. A key with no range line (the
    #    staged next key) signs nothing.
    if ($principal) {
      $id = $principal.Substring($principalPrefix.Length)
      $range = Get-DataLines $embeddedRanges | Where-Object { ($_ -split '[ \t]+')[0] -ceq $id } | Select-Object -First 1
      if (-not $range) { Die "RELEASE-MANIFEST for $tag is signed by $principal, which has no range in release-keys/ranges -- a staged key signs nothing yet; refusing it" }
      $f = $range -split '[ \t]+'
      if (-not (Test-TagLe $f[1] $tag)) { Die "RELEASE-MANIFEST for $tag is signed by $principal, which may sign only from $($f[1]) on; refusing it" }
      if ($f[2] -ne '-' -and -not (Test-TagLe $tag $f[2])) { Die "RELEASE-MANIFEST for $tag is signed by $principal, which was retired after $($f[2]); refusing it" }
      Say "signature ok: $tag, signed by $principal"
    }
    $sumsText = $mText.Substring($prefix.Length)
  } else {
    # Cut before signing existed: the pinned digest of its SHA256SUMS is the
    # proof, and ssh-keygen plays no part. Said anyway when it is missing, so
    # the next (signed) install does not come as a surprise.
    if (-not (Find-SshKeygen)) {
      Warn "no ssh-keygen with -Y found. Not needed for $tag (verified by its pinned digest), but releases from $firstManifestSigTag on are verified by signature: $keygenFix"
    }
    $pinned = $null
    foreach ($line in (Get-DataLines $embeddedPresigningSums)) {
      $f = $line -split '[ \t]+'
      if ($f[0] -ceq $tag) { $pinned = $f[1].ToLower(); break }
    }
    if (-not $pinned) { Die "$tag predates signed releases ($firstManifestSigTag) and is not in the pinned table of pre-signing releases -- there is no such oam release; refusing it" }
    $sumsPath = Join-Path $tmp 'SHA256SUMS'
    if (-not (Get-Asset 'SHA256SUMS' $sumsPath)) { Die "could not fetch SHA256SUMS for $tag" }
    $got = Get-Sha256 $sumsPath
    if ($got -ne $pinned) { Die "SHA256SUMS for $tag hashes to $got, but ${tag}'s pinned digest is $pinned -- it is not the file that release published; refusing it" }
    Say "SHA256SUMS ok: $tag matches its pinned digest (released before signing)"
    $sumsText = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($sumsPath))
  }
  $expected = Get-HashFromSums $sumsText

  # --- Authenticode hook (INACTIVE) ---------------------------------------------
  # The release signs both .exe assets with Authenticode from v0.18.0 (signing
  # plan 4.1 / rollout step 5), but this check stays off until a signed release
  # has shipped and its publisher and intermediate have been confirmed against
  # it, so today this checks nothing. Then: set
  # $firstAuthenticodeTag to the first tag signed with it, and every tag from
  # there on must be Valid, signed by oam's publisher, under the "Microsoft ID
  # Verified Code Signing PCA 2021" intermediate. Never pin the leaf
  # thumbprint: those certificates rotate every few days.
  $firstAuthenticodeTag = ''
  function Test-OamAuthenticode([string]$path) {
    if (-not $firstAuthenticodeTag) { return }
    if (-not (Test-TagLe $firstAuthenticodeTag $tag)) { return }
    $sig = Get-AuthenticodeSignature -LiteralPath $path
    if ($sig.Status -ne 'Valid') { Die "the $tag binary's Authenticode signature is $($sig.Status); refusing it" }
    # Publisher and intermediate checks go here with the go-live values.
  }

  # --- Download, check, replace -------------------------------------------------
  Say "downloading $asset $tag from $base"
  if (-not (Get-Asset $asset $binPath)) { Die "download failed: $asset (network or proxy? does $tag carry this asset? if your host requires GitHub auth, set GH_TOKEN or install the gh CLI)" }
  $actual = Get-Sha256 $binPath
  if ($expected -ne $actual) { Die "checksum mismatch for $asset (expected $expected, got $actual)" }
  Say 'checksum ok'
  Test-OamAuthenticode $binPath

  # The binary is a binary redistribution of V8, ICU, the Node streams port and
  # ~380 Rust crates, whose licenses require their notices travel with it. They
  # go beside the binary so the copy on THIS machine carries its attribution,
  # not just the repo it came from. Each one is checked against the same
  # verified checksums as the binary, and BEFORE the binary is put in place: a
  # file whose bytes differ from what the release lists stops the install with
  # nothing changed. Otherwise best-effort, as install.sh:
  #   - a release whose checksums do not list a file (every release cut before
  #     the license files were checksummed; before v0.8.1 there were none at
  #     all) does not get that file installed -- unchecked text is not put
  #     beside the binary -- and a warning says where to read it instead;
  #   - a listed file that cannot be fetched is not worth failing an otherwise
  #     good install over, and is reported, not swallowed.
  # Get-Asset returns a bool (it never throws), so that is what is tested.
  $licenseNames = @('LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.md')
  $licOk = @(); $licUnlisted = @(); $licFailed = @()
  foreach ($f in $licenseNames) {
    $hits = Get-SumsHits $sumsText $f
    if ($hits.Count -eq 0) { $licUnlisted += $f; continue }
    if ($hits.Count -ne 1) { Die "the verified checksums for $tag list $f $($hits.Count) times, not once" }
    $staged = Join-Path $tmp "lic.$f"
    Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
    if (-not (Get-Asset $f $staged)) { $licFailed += $f; continue }
    $got = Get-Sha256 $staged
    if ($got -ne $hits[0]) { Die "checksum mismatch for $f (expected $($hits[0]), got $got) -- refusing to install $tag; nothing was changed" }
    $licOk += $f
  }
  if ($licOk.Count -gt 0) { Say "checksum ok: $($licOk -join ' ')" }

  New-Item -ItemType Directory -Path $installDir -Force | Out-Null
  $dest = Join-Path $installDir 'oam.exe'
  # A running oam.exe can't be overwritten in place; move the old one aside.
  if (Test-Path $dest) {
    try { Move-Item -Path $dest -Destination "$dest.old" -Force }
    catch {
      Remove-Item -Path "$dest.old" -Force -ErrorAction SilentlyContinue
      if (Test-Path "$dest.old") {
        # oam.exe.old is itself still in use: an oam started before the LAST
        # upgrade (an `oam mcp` server, say) still runs from it, and Windows
        # replaces or deletes no running image. Move aside to a name of this
        # run's own instead -- the <target>.old-<pid>-<secs> shape
        # `oam self-update` uses, so its sweep and the one below both
        # recognize it.
        $secs = [int64][Math]::Floor(([DateTime]::UtcNow - (New-Object DateTime 1970, 1, 1)).TotalSeconds)
        Move-Item -Path $dest -Destination "$dest.old-$PID-$secs" -Force
      } else {
        Move-Item -Path $dest -Destination "$dest.old" -Force
      }
    }
  }
  Move-Item -Path $binPath -Destination $dest -Force
  # The move-asides are removed best-effort -- this run's and any an earlier
  # install or `oam self-update` left (oam.exe.old, oam.exe.old-<digits and
  # dashes>): a running copy (an `oam mcp` server, a second session) keeps its
  # file until a later install or `oam self-update`, whose sweep clears it. Say
  # so when one survives -- a stray oam.exe beside the install is exactly the
  # residue that must not go unnoticed.
  $asideLeft = @()
  $asides = @(Get-ChildItem -LiteralPath $installDir -Force -ErrorAction SilentlyContinue |
    Where-Object { -not $_.PSIsContainer -and $_.Name -match '^oam\.exe\.old(-[0-9-]+)?\z' })
  foreach ($aside in $asides) {
    Remove-Item -LiteralPath $aside.FullName -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $aside.FullName) { $asideLeft += $aside.FullName }
  }
  if ($asideLeft.Count -gt 0) { Warn "$($asideLeft -join ', ') could not be removed -- most likely the previous oam.exe is still running (an ACL refusal looks the same); it is not on PATH and a later install or 'oam self-update' will clear it" }
  Say "installed oam $tag to $dest"

  # Whatever the install this one replaced left in licenses/ describes a
  # different binary: those three names go first, then the checked files go
  # in, and the directory goes when nothing is left in it.
  $licenseDir = Join-Path $installDir 'licenses'
  foreach ($f in $licenseNames) {
    Remove-Item -LiteralPath (Join-Path $licenseDir $f) -Force -ErrorAction SilentlyContinue
  }
  $gotLicenses = $false
  foreach ($f in $licOk) {
    try {
      New-Item -ItemType Directory -Path $licenseDir -Force | Out-Null
      Move-Item -LiteralPath (Join-Path $tmp "lic.$f") -Destination (Join-Path $licenseDir $f) -Force
      $gotLicenses = $true
    } catch { Warn "could not put $f in ${licenseDir}: $($_.Exception.Message)" }
  }
  if ((Test-Path -LiteralPath $licenseDir) -and -not @(Get-ChildItem -LiteralPath $licenseDir -Force -ErrorAction SilentlyContinue).Count) {
    Remove-Item -LiteralPath $licenseDir -Force -ErrorAction SilentlyContinue
  }
  if ($gotLicenses) { Say "license and attribution files in $licenseDir" }
  if ($licUnlisted.Count -gt 0) {
    Warn "the verified checksums for $tag do not list: $($licUnlisted -join ' ') -- not installed (only checked license files are copied); read them at https://github.com/$ownerRepo/tree/$tag"
  }
  if ($licFailed.Count -gt 0) {
    Warn "could not fetch $($licFailed -join ' ') for $tag; see https://github.com/$ownerRepo/tree/$tag"
  }

  # Add install dir to the USER PATH (persistent) if it isn't already there.
  # Read and written as the registry stores it (Get-UserPathRaw above): an
  # entry such as %USERPROFILE%\bin stays a reference instead of being
  # rewritten as today's expansion. An entry counts as the install dir either
  # as written or once expanded.
  $userPath = Get-UserPathRaw
  if (-not $userPath) { $userPath = '' }
  $onPath = $userPath.Split(';') | Where-Object { $_ -eq $installDir -or [Environment]::ExpandEnvironmentVariables($_) -eq $installDir }
  if (-not $onPath) {
    $newPath = if ($userPath.TrimEnd(';')) { "$($userPath.TrimEnd(';'));$installDir" } else { $installDir }
    Set-UserPath $newPath
    $env:Path = "$env:Path;$installDir"
    Say "added $installDir to your user PATH (restart your terminal to pick it up)"
  }

  # A smoke, not a check: whatever it prints on stderr must not turn into a
  # terminating error (the 5.1 trap above), so 'Stop' is lifted around it.
  $ErrorActionPreference = 'Continue'
  try { & $dest --version 2>$null } catch { }
  $ErrorActionPreference = 'Stop'
} finally {
  Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
