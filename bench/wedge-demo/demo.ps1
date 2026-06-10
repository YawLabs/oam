# The oam wedge demo, human edition. Self-verifying: exits non-zero if any
# act doesn't behave as advertised. Works on a temp copy of project/.
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
$env:OAM_CACHE_DIR = Join-Path $work ".oam-cache"   # demo leaves no residue
Push-Location $work
$failed = $false

Write-Host ""
Write-Host "ACT 1 - the typed loop: oam run main.ts" -ForegroundColor Cyan
Write-Host "  (the program carries a classic silent bug: a string in a number slot." -ForegroundColor DarkGray
Write-Host "   Node runs it silently. Bun runs it silently. Watch oam.)" -ForegroundColor DarkGray
& $OamBin run main.ts 2>&1 | ForEach-Object { Write-Host "  $_" }
if ($LASTEXITCODE -ne 0) { $failed = $true; Write-Host "unexpected: run should succeed (types warn)" -ForegroundColor Red }
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
Write-Host ("  cold check: {0,6:N0} ms  (daemon spawn + tsgo project load)" -f $first)
Write-Host ("  warm check: {0,6:N0} ms  (cache_hits={1}; floor is process+ipc)" -f $second, $status.cache_hits)
if ($second -ge $first) { $failed = $true; Write-Host "unexpected: warm check should beat cold" -ForegroundColor Red }

Write-Host ""
Write-Host "ACT 4 - machine mode: the same diagnostics, for agents" -ForegroundColor Cyan
& $OamBin check . --json 2>&1 | Select-Object -First 1 | ForEach-Object { Write-Host "  $_" }
Write-Host "  ^ stable code + span + docs URL. The agent loop is: node agent-loop.mjs" -ForegroundColor DarkGray

& $OamBin daemon stop . | Out-Null
Pop-Location
Write-Host ""
if ($failed) { Write-Host "DEMO FAILED" -ForegroundColor Red; exit 1 }
Write-Host "DEMO OK" -ForegroundColor Green
