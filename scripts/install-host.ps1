# Offline installer for a verified Windows host release or a card's lab build.
# Run from an elevated PowerShell. Never pass a host token on the command line.
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$BinaryPath,
    [Parameter(Mandatory=$true)][ValidatePattern('^[0-9a-fA-F]{64}$')][string]$Sha256,
    [string]$ConfigPath,
    [switch]$NoStart
)
$ErrorActionPreference = 'Stop'
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (!$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this installer as an administrator.'
}
$root = Join-Path $env:ProgramData 'Terrarium\host'
$binary = Join-Path $root 'terrarium-host.exe'
$config = Join-Path $root 'config.toml'
if ((Get-FileHash -LiteralPath $BinaryPath -Algorithm SHA256).Hash -ine $Sha256) {
    throw 'Binary SHA-256 does not match. Nothing installed.'
}
# Do not follow an attacker-created junction anywhere in the install path.
$path = $root
while ($path) {
    if (Test-Path -LiteralPath $path) {
        $item = Get-Item -LiteralPath $path -Force
        if (!$item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'Install path contains a reparse point or non-directory.'
        }
    }
    $parent = Split-Path -Parent $path
    if ($parent -eq $path) { break }
    $path = $parent
}
New-Item -ItemType Directory -Path $root -Force | Out-Null
$acl = New-Object Security.AccessControl.DirectorySecurity
$acl.SetAccessRuleProtection($true, $false)
$acl.SetOwner((New-Object Security.Principal.SecurityIdentifier('S-1-5-32-544')))
foreach ($sid in @('S-1-5-18', 'S-1-5-32-544')) {
    $id = New-Object Security.Principal.SecurityIdentifier($sid)
    $rule = New-Object Security.AccessControl.FileSystemAccessRule($id, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
    $acl.AddAccessRule($rule)
}
Set-Acl -LiteralPath $root -AclObject $acl
$service = Get-Service -Name TerrariumHost -ErrorAction SilentlyContinue
if ($service -and $service.Status -ne 'Stopped') {
    Stop-Service -Name TerrariumHost
    $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
}
# The stopped service may still be releasing its executable image.
$staged = Join-Path $root 'install.new.exe'
Copy-Item -LiteralPath $BinaryPath -Destination $staged -Force
if ((Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash -ine $Sha256) { throw 'Staged binary changed.' }
$deadline = [DateTime]::UtcNow.AddSeconds(15)
while ($true) {
    try { Move-Item -LiteralPath $staged -Destination $binary -Force; break }
    catch { if ([DateTime]::UtcNow -ge $deadline) { throw }; Start-Sleep -Milliseconds 200 }
}
if ($ConfigPath) {
    if ([IO.Path]::GetFullPath($ConfigPath) -ine [IO.Path]::GetFullPath($config)) {
        Copy-Item -LiteralPath $ConfigPath -Destination $config -Force
    }
} elseif (!(Test-Path -LiteralPath $config)) {
    if (!$NoStart) { throw 'Provide a config file containing daemon_url, name and token_file.' }
    '# Provision daemon_url, name and token_file before starting.' | Set-Content -LiteralPath $config -Encoding UTF8
}
$command = '"' + $binary + '" --config "' + $config + '" service'
if (!$service) {
    # Omitted credentials select LocalSystem. No interactive logon or desktop.
    New-Service -Name TerrariumHost -DisplayName 'Terrarium Host' -BinaryPathName $command -StartupType Automatic | Out-Null
} else {
    & sc.exe config TerrariumHost binPath= $command obj= LocalSystem start= auto | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Cannot update service configuration.' }
}
& sc.exe failure TerrariumHost reset= 86400 actions= restart/5000/restart/15000/restart/30000 | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot set service recovery.' }
& sc.exe failureflag TerrariumHost 1 | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot enable service recovery.' }
if (!$NoStart) {
    Start-Service -Name TerrariumHost
    (Get-Service TerrariumHost).WaitForStatus('Running', [TimeSpan]::FromSeconds(30))
}
Write-Output ('Installed TerrariumHost as LocalSystem. Logs: ' + (Join-Path $root 'service.log'))
