# Destructive lab-only SCM recovery checks. Run in an approved maintenance window.
# Never reads the service config or token. Only the queried service PID is killed.
param(
    [Parameter(Mandatory=$true)][string]$BinaryPath,
    [Parameter(Mandatory=$true)][string]$Sha256,
    [string]$OutputPath = 'windows-service-results.xml'
)
$ErrorActionPreference = 'Stop'
$cases = New-Object 'System.Collections.Generic.List[string]'
$failed = 0
function Get-HostService { Get-CimInstance Win32_Service | Where-Object Name -eq 'TerrariumHost' }
function Wait-Healthy([int]$seconds, [int]$previousPid) {
    $deadline = [DateTime]::UtcNow.AddSeconds($seconds)
    do {
        $service = Get-HostService
        if ($service.State -eq 'Running' -and $service.ProcessId -ne $previousPid -and !(Test-Path -LiteralPath ($BinaryPath + '.pending'))) { return }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'Service did not restart and resolve its pending journal.'
}
function Stage-And-Kill([string]$version) {
    $service = Get-HostService
    if ($service.State -ne 'Running' -or $service.StartName -ne 'LocalSystem') { throw 'Expected running LocalSystem lab service.' }
    if ((Get-FileHash -LiteralPath $BinaryPath -Algorithm SHA256).Hash -ne $Sha256) { throw 'Installed binary checksum mismatch.' }
    Copy-Item -LiteralPath $BinaryPath -Destination ($BinaryPath + '.new') -Force
    Copy-Item -LiteralPath $BinaryPath -Destination ($BinaryPath + '.prev') -Force
    if (Test-Path -LiteralPath ($BinaryPath + '.old')) { Remove-Item -LiteralPath ($BinaryPath + '.old') -Force }
    # Match the service's journal-before-rename order, flush before image changes.
    $bytes = [Text.Encoding]::UTF8.GetBytes($version + "`n")
    $file = [IO.File]::Open(($BinaryPath + '.pending'), [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try { $file.Write($bytes, 0, $bytes.Length); $file.Flush($true) } finally { $file.Dispose() }
    Move-Item -LiteralPath $BinaryPath -Destination ($BinaryPath + '.old')
    try { Move-Item -LiteralPath ($BinaryPath + '.new') -Destination $BinaryPath }
    catch { Move-Item -LiteralPath ($BinaryPath + '.old') -Destination $BinaryPath; throw }
    # Force the exact stopped-between-install-and-exit case. No process pattern.
    Stop-Process -Id $service.ProcessId -Force
    return [int]$service.ProcessId
}
function Test-Case([string]$name, [scriptblock]$body) {
    try {
        & $body
        $cases.Add('<testcase name="' + $name + '"/>')
        Write-Output ('PASS ' + $name)
    } catch {
        $script:failed++
        $message = [Security.SecurityElement]::Escape($_.Exception.Message)
        $cases.Add('<testcase name="' + $name + '"><failure message="' + $message + '"/></testcase>')
        Write-Output ('FAIL ' + $name + ': ' + $_.Exception.Message)
        throw # Never continue another destructive case after recovery failed.
    }
}
try {
    $version = ((& $BinaryPath --version | Select-Object -First 1) -split '\s+')[1]
    if ($LASTEXITCODE -ne 0 -or !$version) { throw 'Could not read host version.' }
    Test-Case 'mapped_image_swap_and_forced_exit_recovers' {
        $oldPid = Stage-And-Kill $version
        Wait-Healthy 90 $oldPid
    }
    Test-Case 'failed_health_check_rolls_back_and_restarts' {
        $bad = '99.0.0+ter271-health-failure'
        $oldPid = Stage-And-Kill $bad
        Wait-Healthy 180 $oldPid
        $recorded = [IO.File]::ReadAllText($BinaryPath + '.failed').Trim()
        if ($recorded -ne $bad) { throw 'Rollback did not record the failed version.' }
        if ((Get-FileHash -LiteralPath $BinaryPath -Algorithm SHA256).Hash -ne $Sha256) { throw 'Rollback restored the wrong image.' }
    }
} finally {
    ('<testsuite name="windows-service" tests="' + $cases.Count + '" failures="' + $failed + '">' + ($cases -join '') + '</testsuite>') | Set-Content -LiteralPath $OutputPath -Encoding UTF8
    Get-Service TerrariumHost | Select-Object Name,Status
}
if ($failed -ne 0) { exit 1 }
