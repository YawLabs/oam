# The oam wedge demo, human edition. Self-verifying: exits non-zero if any
# act doesn't behave as advertised. Works on a temp copy of project/, which is
# removed on exit along with the daemon it started; the two environment
# variables it sets are put back, so the calling session is left as it was.
#
#   .\demo.ps1                # uses target\release\oam.exe (or debug)
#   .\demo.ps1 -OamBin <path>
param([string]$OamBin = "")

# Continue, not Stop: PowerShell 5.1 wraps native stderr in error records,
# and the typed-loop trailer IS stderr — that's the demo, not a failure.
$ErrorActionPreference = "Continue"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $OamBin) {
    $release = Join-Path $here "..\..\target\release\oam.exe"
    $debug = Join-Path $here "..\..\target\debug\oam.exe"
    $OamBin = if (Test-Path $release) { $release } elseif (Test-Path $debug) { $debug } else { "oam" }
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("oam-wedge-" + [IO.Path]::GetRandomFileName())
Copy-Item -Recurse (Join-Path $here "project") $work
# A binary given as a relative path must survive the Push-Location below.
if ($OamBin -match '[\\/]' -and (Test-Path $OamBin)) { $OamBin = (Resolve-Path $OamBin).Path }
# $env: is the whole session's environment, not this script's: remember what
# was there and put it back in the finally block.
$prevCacheDir = $env:OAM_CACHE_DIR
$prevCheckWait = $env:OAM_CHECK_WAIT_MS
$env:OAM_CACHE_DIR = Join-Path $work ".oam-cache"   # all state under the temp copy
# Warn mode waits OAM_CHECK_WAIT_MS (default 10 s) for the checker after the
# program ends; a longer bound keeps act 1's assertion off the clock.
if (-not $env:OAM_CHECK_WAIT_MS) { $env:OAM_CHECK_WAIT_MS = "60000" }
Push-Location $work
$failed = $false

try {

Write-Host ""
Write-Host "ACT 1 - the typed loop: oam run main.ts --check=warn" -ForegroundColor Cyan
Write-Host "  (the program carries a classic silent bug: a string in a number slot." -ForegroundColor DarkGray
Write-Host "   Node runs it silently. Bun runs it silently. Watch oam.)" -ForegroundColor DarkGray
$act1 = @(& $OamBin run main.ts --check=warn 2>&1 | ForEach-Object { "$_" })
if ($LASTEXITCODE -ne 0) { $failed = $true; Write-Host "unexpected: run should succeed (types warn)" -ForegroundColor Red }
$act1 | ForEach-Object { Write-Host "  $_" }
$act1Text = $act1 -join "`n"
if ($act1Text -notmatch 'total: 410') { $failed = $true; Write-Host "unexpected: the program should have printed total: 410" -ForegroundColor Red }
if ($act1Text -notmatch 'TS2322') { $failed = $true; Write-Host "unexpected: the type error (TS2322) should have surfaced" -ForegroundColor Red }
Write-Host "  ^ executed instantly (wrong answer: 410), AND the type error surfaced." -ForegroundColor DarkGray

Write-Host ""
Write-Host "ACT 2 - the CI gate: oam run main.ts --check=block" -ForegroundColor Cyan
& $OamBin run main.ts --check=block 2>&1 | ForEach-Object { Write-Host "  $_" }
if ($LASTEXITCODE -eq 0) { $failed = $true; Write-Host "unexpected: block mode should gate" -ForegroundColor Red }
Write-Host "  ^ same bug, but CI never lets it execute." -ForegroundColor DarkGray

Write-Host ""
Write-Host "ACT 3 - the daemon: repeat checks are served from cache" -ForegroundColor Cyan
& $OamBin daemon stop . | Out-Null   # acts 1-2 already warmed it; show true cold
$first = (Measure-Command { & $OamBin check . 2>&1 | Out-Null }).TotalMilliseconds
$second = (Measure-Command { & $OamBin check . 2>&1 | Out-Null }).TotalMilliseconds
$status = (& $OamBin daemon status .) | ConvertFrom-Json
# A cold check falls back to a one-shot tsgo when the daemon is slow to come
# up, which leaves the second check as the daemon's first (a fill, not a
# hit). One more check settles it either way.
if (-not ($status.cache_hits -ge 1)) {
    & $OamBin check . 2>&1 | Out-Null
    $status = (& $OamBin daemon status .) | ConvertFrom-Json
}
Write-Host ("  cold check: {0,6:N0} ms  (daemon spawn + tsgo project load)" -f $first)
Write-Host ("  warm check: {0,6:N0} ms  (cache_hits={1}; floor is process+ipc)" -f $second, $status.cache_hits)
# The claim is "served from cache", so that is what is asserted: the daemon's
# own hit counter. The timings are shown, not gated -- on a busy box a warm
# check can lose a wall-clock race it still won on work done.
if (-not ($status.cache_hits -ge 1)) { $failed = $true; Write-Host "unexpected: the repeat check should be a daemon cache hit" -ForegroundColor Red }

Write-Host ""
Write-Host "ACT 4 - machine mode: the same diagnostics, for agents" -ForegroundColor Cyan
$act4 = @(& $OamBin check . --json 2>&1 | ForEach-Object { "$_" }) | Where-Object { $_ -match 'OAM-TS2322' } | Select-Object -First 1
Write-Host "  $act4"
if (-not ($act4 -and $act4.StartsWith("{"))) { $failed = $true; Write-Host "unexpected: --json should carry the OAM-TS2322 diagnostic as a JSON line" -ForegroundColor Red }
Write-Host "  ^ stable code + span + docs URL. The agent loop is: node agent-loop.mjs" -ForegroundColor DarkGray

} finally {
    # Every way out: stop the project's daemon, leave the temp copy, restore
    # the session's environment (assigning $null removes a variable that was
    # not set before), and remove the copy. The retry covers the moment the
    # stopped daemon still holds the directory.
    # try/catch: a binary that cannot be started must not skip the rest.
    try { & $OamBin daemon stop $work | Out-Null } catch {}
    Pop-Location
    $env:OAM_CACHE_DIR = $prevCacheDir
    $env:OAM_CHECK_WAIT_MS = $prevCheckWait
    for ($i = 0; $i -lt 20 -and (Test-Path $work); $i++) {
        Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
        if (Test-Path $work) { Start-Sleep -Milliseconds 100 }
    }
}
Write-Host ""
if ($failed) { Write-Host "DEMO FAILED" -ForegroundColor Red; exit 1 }
Write-Host "DEMO OK" -ForegroundColor Green
