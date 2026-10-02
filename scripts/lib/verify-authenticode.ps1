# verify-authenticode.ps1 -- the second, independent half of win_verify in
# scripts/lib/signing.sh. Exit 0 only when the file AS IT IS ON DISK carries
# an embedded Authenticode signature that:
#   - Get-AuthenticodeSignature calls Valid (chain to a trusted root, intact
#     digest; for an expired three-day certificate, valid via its timestamp)
#   - is timestamped (TimeStamperCertificate present). Artifact Signing
#     certificates live three days; without a timestamp the signature dies
#     with them
#   - names the expected publisher as BOTH the signer's CN and its O
#   - chains through the expected intermediate, as a CA certificate above the
#     leaf (for Artifact Signing: "Microsoft ID Verified Code Signing PCA 2021")
# Anything else exits 1 with a [fail] line saying which check gave way.
#
# Windows PowerShell 5.1, ASCII only. Every input arrives as a parameter
# (powershell -File ... -Path <p> -Publisher <n> -Intermediate <n>), never
# spliced into a command string: a file path and a publisher name are data.
# It never consults signtool, so a signtool that reports success without
# signing -- the documented failure without the x64 .NET runtime -- or a stub
# standing in for it cannot vouch for itself here.
param(
  [Parameter(Mandatory = $true)][string]$Path,
  [Parameter(Mandatory = $true)][string]$Publisher,
  [Parameter(Mandatory = $true)][string]$Intermediate
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

function Fail([string]$msg) {
  [Console]::Error.WriteLine("  [fail] $msg")
  exit 1
}

# The single RDN value for <attr> in a certificate's subject, or $null when it
# is absent or appears more than once. Decoded one RDN per line, unquoted, so
# a name with a comma ("Example, LLC") reads back whole.
function Get-RdnValue($cert, [string]$attr) {
  $flags = [System.Security.Cryptography.X509Certificates.X500DistinguishedNameFlags]'UseNewLines,DoNotUseQuotes'
  $lines = $cert.SubjectName.Decode($flags) -split "`r?`n"
  $found = @($lines | Where-Object { $_.StartsWith("$attr=") } | ForEach-Object { $_.Substring($attr.Length + 1) })
  if ($found.Count -ne 1) { return $null }
  return $found[0]
}

try {
  if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { Fail "$Path does not exist" }
  $sig = Get-AuthenticodeSignature -LiteralPath $Path
} catch {
  Fail "could not read a signature from ${Path}: $($_.Exception.Message)"
}

if ($sig.Status -ne 'Valid') {
  Fail "$Path signature status is '$($sig.Status)', not Valid ($($sig.StatusMessage))"
}
# Catalog-signed means the file itself carries nothing: the signature would
# not travel with the download.
if ($sig.SignatureType -ne 'Authenticode') {
  Fail "$Path is signed by '$($sig.SignatureType)', not an embedded Authenticode signature"
}
if ($null -eq $sig.TimeStamperCertificate) {
  Fail "$Path has no timestamp -- the signature would stop verifying when its certificate expires"
}
$signer = $sig.SignerCertificate
if ($null -eq $signer) { Fail "$Path has no signer certificate" }

$cn = Get-RdnValue $signer 'CN'
$o = Get-RdnValue $signer 'O'
if ($cn -cne $Publisher) { Fail "$Path signer CN is '$cn', expected '$Publisher' (subject: $($signer.Subject))" }
if ($o -cne $Publisher) { Fail "$Path signer O is '$o', expected '$Publisher' (subject: $($signer.Subject))" }

# The chain's elements, whatever Build() returns: a three-day leaf is usually
# past NotAfter by the time this runs again, and Status Valid above already
# answered trust (via the timestamp). This is only the WHICH-CA question.
# Revocation is not fetched; it is not what is asked here.
$chain = New-Object System.Security.Cryptography.X509Certificates.X509Chain
$chain.ChainPolicy.RevocationMode = [System.Security.Cryptography.X509Certificates.X509RevocationMode]::NoCheck
[void]$chain.Build($signer)
$names = @()
$hit = $false
for ($i = 0; $i -lt $chain.ChainElements.Count; $i++) {
  $c = $chain.ChainElements[$i].Certificate
  $n = Get-RdnValue $c 'CN'
  $names += "$n"
  if ($i -gt 0 -and $n -ceq $Intermediate) { $hit = $true }
}
if (-not $hit) {
  Fail "$Path signer chain is [$($names -join ' > ')], which does not pass through '$Intermediate'"
}

[Console]::Error.WriteLine("  [ok] $Path : Valid, timestamped, CN=O='$Publisher', via '$Intermediate'")
exit 0
