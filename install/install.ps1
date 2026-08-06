# oam installer (Windows). Canonical home: https://oamjs.org/install.ps1
#
#   irm https://oamjs.org/install.ps1 | iex
#
# Downloads the release binary for this arch from GitHub Releases, verifies it
# against the published SHA256SUMS, installs it to %LOCALAPPDATA%\oam\bin, and
# adds that dir to the user PATH. No admin, no signing (binaries are unsigned +
# checksummed -- see docs/design).
#
# Env overrides:
#   OAM_VERSION       install a specific tag (e.g. v0.7.0); default: latest
#   OAM_INSTALL_DIR   install location; default: %LOCALAPPDATA%\oam\bin
#   OAM_INSTALL_BASE  asset base URL; default: GitHub Releases
#                     (oamjs.org sets this to proxy downloads through the CDN)
#   GH_TOKEN          GitHub token for private-repo installs (GITHUB_TOKEN is
#                     also accepted). Needed on headless hosts -- CI, a fresh
#                     VM -- that have a token but no gh CLI. While the repo is
#                     private, unauthenticated asset URLs return 404.
#   OAM_GH_API        GitHub API base; default https://api.github.com
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ownerRepo = 'YawLabs/oam'
$installDir = if ($env:OAM_INSTALL_DIR) { $env:OAM_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'oam\bin' }
$ghApi = if ($env:OAM_GH_API) { $env:OAM_GH_API } else { 'https://api.github.com' }
# gh CLI convention first, then the Actions-provided name.
$token = if ($env:GH_TOKEN) { $env:GH_TOKEN } elseif ($env:GITHUB_TOKEN) { $env:GITHUB_TOKEN } else { '' }
# Declared up front: StrictMode makes reading an unset variable a hard error.
$script:relJson = $null

function Say($m) { Write-Host "oam-install: $m" }
function Die($m) { Write-Error "oam-install: error: $m"; exit 1 }

# Map arch -> Rust target triple (must match release.yml asset names).
$arch = $env:PROCESSOR_ARCHITECTURE
switch ($arch) {
  'AMD64' { $target = 'x86_64-pc-windows-msvc' }
  'ARM64' { $target = 'aarch64-pc-windows-msvc' }
  default { Die "unsupported Windows arch: $arch" }
}
$asset = "oam-$target.exe"

# Resolve the asset base (pinned tag -> /download/<tag>/, else /latest/download/).
if ($env:OAM_INSTALL_BASE) {
  $base = $env:OAM_INSTALL_BASE
} elseif ($env:OAM_VERSION) {
  $base = "https://github.com/$ownerRepo/releases/download/$($env:OAM_VERSION)"
} else {
  $base = "https://github.com/$ownerRepo/releases/latest/download"
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("oam-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
  $binPath = Join-Path $tmp $asset
  $sumsPath = Join-Path $tmp 'SHA256SUMS'

  # Authenticated fallback: while the repo is private, unauthenticated release
  # URLs 404. If the direct download fails and the gh CLI is available
  # (internal machines), fetch the same assets through the caller's GitHub auth.
  function Get-ViaGh($pattern, $outFile) {
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { return $false }
    $tag = $env:OAM_VERSION
    if (-not $tag) {
      try { $tag = (gh release view --repo $ownerRepo --json tagName -q .tagName) }
      catch { return $false }
      if (-not $tag) { return $false }
    }
    gh release download $tag --repo $ownerRepo --pattern $pattern --output $outFile --clobber
    return (Test-Path $outFile)
  }

  # Token fallback: works on headless hosts with no gh CLI. A private-repo asset
  # is NOT reachable via its browser_download_url even with a token -- GitHub
  # only serves the bytes from the assets endpoint with an octet-stream Accept
  # -- so resolve the numeric asset id first.
  function Get-ViaToken($assetName, $outFile) {
    if (-not $token) { return $false }
    try {
      if (-not $script:relJson) {
        $relUrl = if ($env:OAM_VERSION) { "$ghApi/repos/$ownerRepo/releases/tags/$($env:OAM_VERSION)" }
                  else { "$ghApi/repos/$ownerRepo/releases/latest" }
        $script:relJson = Invoke-RestMethod -Uri $relUrl -UseBasicParsing -Headers @{
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

  # Direct first (public releases + the oamjs.org CDN), then token, then gh CLI.
  function Get-Asset($assetName, $outFile) {
    try { Invoke-WebRequest -Uri "$base/$assetName" -OutFile $outFile -UseBasicParsing; return $true } catch { }
    if ($token) {
      Say 'direct download failed; retrying with $GH_TOKEN'
      if (Get-ViaToken $assetName $outFile) { return $true }
    }
    Say 'retrying via gh CLI (private repo needs auth)'
    return (Get-ViaGh $assetName $outFile)
  }

  Say "downloading $asset from $base"
  if (-not (Get-Asset $asset $binPath)) { Die "download failed: $asset (private repo? set GH_TOKEN or install the gh CLI)" }
  if (-not (Get-Asset 'SHA256SUMS' $sumsPath)) { Die 'could not fetch SHA256SUMS' }

  # Verify the checksum for our asset only (the manifest covers all targets).
  # Match field 2 exactly, stripping sha256sum's binary-mode "*" marker -- the
  # manifest is written as "<hash> *<asset>", so matching on " <asset>$" never
  # succeeds and every install dies with "no checksum for ...".
  $expected = $null
  foreach ($l in (Get-Content $sumsPath)) {
    $parts = $l -split '\s+'
    if ($parts.Count -ge 2 -and ($parts[1] -replace '^\*', '') -eq $asset) { $expected = $parts[0].ToLower(); break }
  }
  if (-not $expected) { Die "no checksum for $asset in SHA256SUMS" }
  $actual = (Get-FileHash -Algorithm SHA256 -Path $binPath).Hash.ToLower()
  if ($expected -ne $actual) { Die "checksum mismatch for $asset (expected $expected, got $actual)" }
  Say 'checksum ok'

  New-Item -ItemType Directory -Path $installDir -Force | Out-Null
  $dest = Join-Path $installDir 'oam.exe'
  # A running oam.exe can't be overwritten in place; move the old one aside.
  if (Test-Path $dest) {
    try { Move-Item -Path $dest -Destination "$dest.old" -Force }
    catch { Remove-Item -Path "$dest.old" -Force -ErrorAction SilentlyContinue; Move-Item -Path $dest -Destination "$dest.old" -Force }
  }
  Move-Item -Path $binPath -Destination $dest -Force
  Remove-Item -Path "$dest.old" -Force -ErrorAction SilentlyContinue
  Say "installed oam to $dest"

  # Add install dir to the USER PATH (persistent) if it isn't already there.
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (-not $userPath) { $userPath = '' }
  $onPath = $userPath.Split(';') | Where-Object { $_ -eq $installDir }
  if (-not $onPath) {
    $newPath = if ($userPath.TrimEnd(';')) { "$($userPath.TrimEnd(';'));$installDir" } else { $installDir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    $env:Path = "$env:Path;$installDir"
    Say "added $installDir to your user PATH (restart your terminal to pick it up)"
  }

  & $dest --version 2>$null
} finally {
  Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
