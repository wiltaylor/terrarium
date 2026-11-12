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
function Assert-TrustedPath([string]$Path, [bool]$Strict) {
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw ('Refusing reparse point: ' + $Path) }
    $acl = Get-Acl -LiteralPath $Path
    $trusted = @('S-1-5-18', 'S-1-5-32-544')
    $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
    if ($owner -notin $trusted) { throw ('Refusing untrusted owner on ' + $Path) }
    $dangerous = [Security.AccessControl.FileSystemRights]::Write -bor [Security.AccessControl.FileSystemRights]::Delete -bor [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles -bor [Security.AccessControl.FileSystemRights]::ChangePermissions -bor [Security.AccessControl.FileSystemRights]::TakeOwnership
    foreach ($ace in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($ace.IdentityReference.Value -notin $trusted -and ($Strict -or ($ace.AccessControlType -eq 'Allow' -and ($ace.FileSystemRights -band $dangerous)))) {
            throw ('Refusing untrusted ACL on ' + $Path)
        }
    }
}
function New-TrustedDirectory([string]$Path) {
    if (Test-Path -LiteralPath $Path) {
        if (!(Get-Item -LiteralPath $Path -Force).PSIsContainer) { throw ('Not a directory: ' + $Path) }
        Assert-TrustedPath $Path $false
    }
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true, $false)
    $acl.SetOwner((New-Object Security.Principal.SecurityIdentifier('S-1-5-18')))
    foreach ($sid in @('S-1-5-18', 'S-1-5-32-544')) {
        $rule = New-Object Security.AccessControl.FileSystemAccessRule((New-Object Security.Principal.SecurityIdentifier($sid)), 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
        $acl.AddAccessRule($rule)
    }
    # Pass the DACL at creation, not after creating a user-writable directory.
    [IO.Directory]::CreateDirectory($Path, $acl) | Out-Null
    Assert-TrustedPath $Path $false
    Set-Acl -LiteralPath $Path -AclObject $acl
}
function Assert-TrustedTree([string]$Path) {
    if (!(Test-Path -LiteralPath $Path)) { return }
    Assert-TrustedPath $Path $true
    if ((Get-Item -LiteralPath $Path -Force).PSIsContainer) {
        foreach ($child in Get-ChildItem -LiteralPath $Path -Force) { Assert-TrustedTree $child.FullName }
    }
}
$stateParent = Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) 'Terrarium'
$programParent = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Terrarium'
$root = Join-Path $stateParent 'host'
$programRoot = Join-Path $programParent 'host'
$config = Join-Path $root 'config.toml'
if ((Get-FileHash -LiteralPath $BinaryPath -Algorithm SHA256).Hash -ine $Sha256) {
    throw 'Binary SHA-256 does not match. Nothing installed.'
}
# Do not follow an attacker-created junction anywhere in the install path.
foreach ($start in @($root, $programRoot)) {
 $path = $start
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
}
# Reject existing contents before changing any ACL or copying privileged files.
foreach ($path in @($stateParent, $programParent)) { if (Test-Path -LiteralPath $path) { Assert-TrustedPath $path $false } }
Assert-TrustedTree $root
Assert-TrustedTree $programRoot
foreach ($path in @($stateParent, $root, $programParent, $programRoot)) { New-TrustedDirectory $path }
$version = ((& $BinaryPath --version | Select-Object -First 1) -split '\s+')[1]
if ($LASTEXITCODE -ne 0 -or $version -notmatch '^[a-zA-Z0-9.+_-]+$') { throw 'Invalid binary version.' }
$versions = Join-Path $programRoot 'versions'
New-TrustedDirectory $versions
$versionRoot = Join-Path $versions $version
New-TrustedDirectory $versionRoot
$binary = Join-Path $versionRoot 'terrarium-host.exe'
$service = Get-Service -Name TerrariumHost -ErrorAction SilentlyContinue
if ($service -and $service.Status -ne 'Stopped') {
    Stop-Service -Name TerrariumHost
    $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
}
# The stopped service may still be releasing its executable image.
$staged = Join-Path $versionRoot 'install.new.exe'
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
    $instance = Get-CimInstance Win32_Service -Filter "Name='TerrariumHost'"
    $changed = Invoke-CimMethod -InputObject $instance -MethodName Change -Arguments @{PathName=$command; StartName='LocalSystem'; StartMode='Automatic'}
    if ($changed.ReturnValue -ne 0) { throw 'Cannot update service configuration.' }
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
