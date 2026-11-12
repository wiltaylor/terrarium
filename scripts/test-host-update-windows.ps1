# Production update path fixture. Run only in the approved lab service window.
# Enrollment config and tokens are never read or changed.
param([string]$BinaryPath, [string]$Installer, [string]$Stub, [string]$GoodAsset, [string]$BadAsset, [string]$OutputPath='windows-update-results.xml')
$ErrorActionPreference='Stop'
$cases=New-Object 'System.Collections.Generic.List[string]'
$failed=0; $stubProcess=$null
function Service { Get-CimInstance Win32_Service -Filter "Name='TerrariumHost'" }
function Read-Log {
    $stream=[IO.File]::Open($script:log,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::ReadWrite)
    $reader=New-Object IO.StreamReader($stream)
    try { return $reader.ReadToEnd() } finally { $reader.Dispose() }
}
function Wait-For([scriptblock]$condition,[int]$seconds) {
    $until=[DateTime]::UtcNow.AddSeconds($seconds)
    do { if (& $condition) { return }; Start-Sleep -Milliseconds 250 } while ([DateTime]::UtcNow -lt $until)
    throw 'Service state timeout.'
}
function Set-Image([string]$command) {
    $changed=Invoke-CimMethod -InputObject (Service) -MethodName Change -Arguments @{PathName=$command}
    if ($changed.ReturnValue -ne 0) { throw 'SCM config failed.' }
}
function Version([string]$image) {
    $text=& $image --version | Select-Object -First 1
    if ($LASTEXITCODE -ne 0) { throw 'Fixture version failed.' }
    return ($text -split '\s+')[1]
}
function Start-Stub([string]$asset,[string]$version,[string]$mode,[int]$port) {
    $script:log=Join-Path $root ($mode+'.log')
    $script:stubProcess=Start-Process -FilePath $Stub -ArgumentList @($port,('"'+$asset+'"'),$version,$mode) -PassThru -RedirectStandardOutput $script:log -RedirectStandardError (Join-Path $root ($mode+'.err'))
    Wait-For { (Test-Path $script:log) -and (Read-Log).Contains('READY') } 15
    $config=Join-Path $root 'fixture.toml'
    # Public loopback fixture credential, not an enrolled host token.
    [IO.File]::WriteAllText($config,('daemon_url = "ws://127.0.0.1:'+$port+'"'+"`n"+'token = "local-loopback-fixture"'+"`n"+'name = "ter271-update-fixture"'+"`n"))
    return $config
}
function Case([string]$name,[scriptblock]$body) {
    try { & $body; $cases.Add('<testcase name="'+$name+'"/>'); Write-Output ('PASS '+$name) }
    catch { $script:failed++; $cases.Add('<testcase name="'+$name+'"><failure message="'+[Security.SecurityElement]::Escape($_.Exception.Message)+'"/></testcase>'); throw }
}
$original=(Service).PathName
$state=Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) 'Terrarium\host'
$journal=Join-Path $state 'update.json'
$root=Join-Path $state ('ter271-update-'+[guid]::NewGuid().ToString('N'))
try {
    if (Test-Path $journal) { throw 'Existing update transaction. Refusing test.' }
    $good=Version $GoodAsset; $bad=Version $BadAsset
    if ($good -eq $bad -or $good -eq (Version $BinaryPath)) { throw 'Distinct build versions required.' }
    & $Installer -BinaryPath $BinaryPath -Sha256 (Get-FileHash $BinaryPath -Algorithm SHA256).Hash -NoStart
    New-Item -ItemType Directory -Path $root | Out-Null
    $baseline=((Service).PathName -split '"')[1]
    $versions=Split-Path (Split-Path $baseline -Parent) -Parent
    $goodImage=Join-Path (Join-Path $versions $good) 'terrarium-host.exe'
    $config=Start-Stub $GoodAsset $good 'accept' 18192
    Set-Image ('"'+$baseline+'" --config "'+$config+'" service')
    Case 'offer_download_switch_restart_welcome' {
        Start-Service TerrariumHost
        Wait-For { (Test-Path $journal) -and (Get-Content $journal -Raw | ConvertFrom-Json).confirmed -and (Service).PathName.Contains($goodImage) -and (Read-Log).Contains(('WELCOME '+$good)) } 150
        if ((Get-FileHash $goodImage -Algorithm SHA256).Hash -ne (Get-FileHash $GoodAsset -Algorithm SHA256).Hash) { throw 'Wrong installed bytes.' }
        if (!(Read-Log).Contains(('DOWNLOAD '+$good))) { throw 'Download not observed.' }
        if (!(Test-Path $baseline)) { throw 'Old image collected too early.' }
    }
    Case 'forced_exit_and_later_start_cleanup' {
        $before=[int](Service).ProcessId
        Stop-Process -Id $before -Force
        Wait-For { (Service).State -eq 'Running' -and (Service).ProcessId -ne $before -and !(Test-Path $journal) } 60
        if ((Test-Path $baseline) -or !(Test-Path $goodImage)) { throw 'Incorrect image cleanup.' }
    }
    Stop-Service TerrariumHost
    Stop-Process -Id $stubProcess.Id -Force; $stubProcess=$null
    $config=Start-Stub $BadAsset $bad 'reject' 18193
    Set-Image ('"'+$goodImage+'" --config "'+$config+'" service')
    Case 'failed_new_build_health_rolls_back' {
        Start-Service TerrariumHost
        $record=$goodImage+'.failed'
        Wait-For { (Test-Path $record) -and !(Test-Path $journal) -and (Service).State -eq 'Running' -and (Service).PathName.Contains($goodImage) -and (Read-Log).Contains(('HELLO '+$bad)) } 240
        if ([IO.File]::ReadAllText($record).Trim() -ne $bad) { throw 'Wrong failed version.' }
    }
} finally {
    if ($stubProcess -and !$stubProcess.HasExited) { Stop-Process -Id $stubProcess.Id -Force }
    Stop-Service TerrariumHost -ErrorAction SilentlyContinue
    Set-Image $original
    Start-Service TerrariumHost
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force }
    [IO.File]::WriteAllText([IO.Path]::GetFullPath($OutputPath),('<testsuite name="windows-production-update" tests="'+$cases.Count+'" failures="'+$failed+'">'+($cases -join '')+'</testsuite>'))
    Write-Output ('Restored service state: '+(Service).State)
}
if ($failed) { exit 1 }
