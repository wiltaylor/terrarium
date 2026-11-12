# Run cross-built native integration tests. No service or configuration changes.
param(
    [Parameter(Mandatory=$true)][string]$HostTests,
    [Parameter(Mandatory=$true)][string]$WinapiTests,
    [string]$OutputDirectory = '.'
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$env:RUSTC_BOOTSTRAP = '1'
$failed = $false
foreach ($test in @(@{Name='windows-host'; Path=$HostTests}, @{Name='windows-winapi'; Path=$WinapiTests})) {
    $output = Join-Path $OutputDirectory ($test.Name + '.json')
    & $test.Path --test-threads=1 -Z unstable-options --format json --report-time | Set-Content -LiteralPath $output -Encoding UTF8
    if ($LASTEXITCODE -ne 0) { $failed = $true }
}
if ($failed) { exit 1 }
