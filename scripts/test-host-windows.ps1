# Native lab checks. Runs only child processes created by this script.
param([Parameter(Mandatory=$true)][string]$BinaryPath, [string]$OutputPath = 'host-windows-results.xml')
$ErrorActionPreference = 'Stop'
$cases = New-Object 'System.Collections.Generic.List[string]'
$failed = 0
function Write-Frame($process, $value) {
    $data = [Text.Encoding]::UTF8.GetBytes(($value | ConvertTo-Json -Depth 20 -Compress))
    $length = [BitConverter]::GetBytes([uint32]$data.Length)
    [Array]::Reverse($length)
    $stream = $process.StandardInput.BaseStream
    $stream.Write($length, 0, 4)
    $stream.Write($data, 0, $data.Length)
    $stream.Flush()
}
function Read-Bytes($stream, [int]$length) {
    $buffer = New-Object byte[] $length
    $offset = 0
    while ($offset -lt $length) {
        $task = $stream.ReadAsync($buffer, $offset, ($length - $offset))
        if (!$task.Wait(15000)) { throw 'Runner output timed out.' }
        $n = $task.Result
        if ($n -eq 0) { throw 'Runner closed output before exit frame.' }
        $offset += $n
    }
    return ,$buffer
}
function Read-Frame($process) {
    $stream = $process.StandardOutput.BaseStream
    $length = Read-Bytes $stream 4
    [Array]::Reverse($length)
    $n = [BitConverter]::ToUInt32($length, 0)
    if ($n -gt 1048576) { throw 'Oversized output frame.' }
    $data = Read-Bytes $stream $n
    return ([Text.Encoding]::UTF8.GetString($data) | ConvertFrom-Json)
}
function Start-Runner($program, $arguments, $pty) {
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $BinaryPath
    $info.Arguments = 'session-runner'
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $info
    if (!$process.Start()) { throw 'Cannot start runner.' }
    Write-Frame $process @{ program=$program; args=@($arguments); env=@{}; cwd=$env:TEMP; pty=$pty }
    return $process
}
function Stop-OwnedRunner($process) {
    if ($process -and !$process.HasExited) { $process.Kill(); $process.WaitForExit(5000) | Out-Null }
    if ($process) { $process.Dispose() }
}
function Collect-Runner($process) {
    $out = ''; $err = ''; $exitFrame = $null
    while (!$exitFrame) {
        $frame = Read-Frame $process
        if ($frame.Error) { throw $frame.Error }
        if ($frame.Data) {
            $data = [Text.Encoding]::UTF8.GetString([byte[]]$frame.Data[1])
            if ($frame.Data[0] -eq 'stdout') { $out += $data } else { $err += $data }
        }
        if ($frame.Exit) { $exitFrame = $frame.Exit }
    }
    if (!$process.WaitForExit(5000)) { throw 'Runner did not exit after its command.' }
    return @{out=$out; err=$err; result=$exitFrame}
}
function Assert-Dead([int[]]$ids) {
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $alive = @($ids | Where-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue })
        if ($alive.Count -eq 0) { return }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    throw ('Owned descendant survived its job: ' + ($alive -join ','))
}
function Test-Case($name, [scriptblock]$body) {
    try {
        & $body
        $cases.Add('<testcase name="' + $name + '"/>')
        Write-Output ('PASS ' + $name)
    } catch {
        $script:failed++
        $message = [Security.SecurityElement]::Escape($_.Exception.Message)
        $cases.Add('<testcase name="' + $name + '"><failure message="' + $message + '"/></testcase>')
        Write-Output ('FAIL ' + $name + ': ' + $_.Exception.Message)
    }
}
Test-Case 'pipes_streams_and_exit_code' {
    $p = $null
    try {
        $p = Start-Runner $env:COMSPEC @('/d', '/c', 'echo pipe-out & echo pipe-err 1>&2 & exit /b 7') $null
        $r = Collect-Runner $p
        if ($r.result.code -ne 7 -or !$r.out.Contains('pipe-out') -or !$r.err.Contains('pipe-err')) { throw 'Pipes or exit code mismatch.' }
    } finally { Stop-OwnedRunner $p }
}
Test-Case 'conpty_output_and_resize' {
    $p = $null
    try {
        $p = Start-Runner $env:COMSPEC @('/d', '/c', 'echo pty-ok') @{rows=24; cols=80}
        Write-Frame $p @{Resize=@{rows=30; cols=100}}
        $r = Collect-Runner $p
        if ($r.result.code -ne 0 -or !$r.out.Contains('pty-ok')) { throw 'ConPTY output mismatch.' }
    } finally { Stop-OwnedRunner $p }
}
Test-Case 'stdin_and_eof' {
    $p = $null
    try {
        $p = Start-Runner 'powershell.exe' @('-NoProfile', '-NonInteractive', '-Command', '[Console]::WriteLine([Console]::ReadLine())') $null
        Write-Frame $p @{Stdin=@([Text.Encoding]::UTF8.GetBytes("input-ok`r`n"))}
        Write-Frame $p 'Eof'
        $r = Collect-Runner $p
        if (!$r.out.Contains('input-ok')) { throw 'Stdin was not relayed.' }
    } finally { Stop-OwnedRunner $p }
}
Test-Case 'runner_termination_kills_detached_grandchild' {
    $p = $null
    try {
        $command = '$child = Start-Process powershell.exe -ArgumentList ''-NoProfile -NonInteractive -Command "Start-Sleep -Seconds 120"'' -WindowStyle Hidden -PassThru; [Console]::WriteLine("OWNED=$PID,$($child.Id)"); Start-Sleep -Seconds 120'
        $p = Start-Runner 'powershell.exe' @('-NoProfile', '-NonInteractive', '-Command', $command) $null
        $text = ''
        do {
            $frame = Read-Frame $p
            if ($frame.Error) { throw $frame.Error }
            if ($frame.Data) { $text += [Text.Encoding]::UTF8.GetString([byte[]]$frame.Data[1]) }
        } while ($text -notmatch 'OWNED=(\d+),(\d+)')
        $ids = @([int]$Matches[1], [int]$Matches[2])
        $p.Kill()
        $p.WaitForExit(5000) | Out-Null
        Assert-Dead $ids
    } finally { Stop-OwnedRunner $p }
}
('<testsuite name="windows-host-runner" tests="' + $cases.Count + '" failures="' + $failed + '">' + ($cases -join '') + '</testsuite>') | Set-Content -LiteralPath $OutputPath -Encoding UTF8
if ($failed -ne 0) { exit 1 }
