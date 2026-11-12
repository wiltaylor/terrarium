# Native ACL adversarial tests. Operates only on the unique test directory.
param([Parameter(Mandatory=$true)][string]$Installer, [Parameter(Mandatory=$true)][string]$BinaryPath, [string]$OutputPath = 'windows-installer-results.xml')
$ErrorActionPreference = 'Stop'
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($Installer, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'Installer does not parse.' }
function New-TrustedDirectory([string]$Path) {
    New-Item -ItemType Directory -Path $Path | Out-Null
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true,$false)
    foreach ($sid in @('S-1-5-18','S-1-5-32-544')) {
        $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule((New-Object Security.Principal.SecurityIdentifier($sid)),'FullControl','ContainerInherit,ObjectInherit','None','Allow')))
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
    Set-NewFileOwner $Path
}
function Set-NewFileOwner([string]$Path) {
    & icacls.exe $Path /setowner '*S-1-5-18' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Cannot assign fixture owner.' }
}
function Preflight([string]$StateParent) {
    & $Installer -BinaryPath $BinaryPath -Sha256 $hash -TestStateParent $StateParent -TestProgramParent $program -PreflightOnly
}
$root = Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) ('Ter271AclTest-' + [guid]::NewGuid().ToString('N'))
$cases = New-Object 'System.Collections.Generic.List[string]'
$failed = 0
$hash = (Get-FileHash -LiteralPath $BinaryPath -Algorithm SHA256).Hash
$program = Join-Path $root 'program'
function Expect-Refusal([scriptblock]$action, [string]$reason) {
    try { & $action } catch { if ($_.Exception.Message.Contains($reason)) { return }; throw }
    throw ('Expected refusal: ' + $reason)
}
function Test-Case([string]$name, [scriptblock]$action) {
    try { & $action; $cases.Add('<testcase name="' + $name + '"/>'); Write-Output ('PASS ' + $name) }
    catch {
        $script:failed++
        $cases.Add('<testcase name="' + $name + '"><failure message="' + [Security.SecurityElement]::Escape($_.Exception.Message) + '"/></testcase>')
        Write-Output ('FAIL ' + $name + ': ' + $_.Exception.Message)
    }
}
try {
    New-TrustedDirectory $root
    New-TrustedDirectory $program
    Test-Case 'safe_actual_preflight_passes' { Preflight (Join-Path $root 'safe') }
    Test-Case 'precreated_non_admin_owned_parent_refused' {
        $path = Join-Path $root 'owned'
        New-TrustedDirectory $path
        & icacls.exe $path /setowner '*S-1-5-32-545' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot assign test owner.' }
        Expect-Refusal { Preflight $path } 'ancestor owner'
    }
    Test-Case 'trusted_owner_with_world_delete_child_refused' {
        $path = Join-Path $root 'delete-child'
        New-TrustedDirectory $path
        & icacls.exe $path /grant '*S-1-1-0:(OI)(CI)F' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot assign adversarial ACE.' }
        Expect-Refusal { Preflight $path } 'ancestor replacement permission'
    }
    Test-Case 'junction_inside_host_refused' {
        $parent = Join-Path $root 'junction-test'
        New-TrustedDirectory $parent
        $hostRoot = Join-Path $parent 'host'
        $target = Join-Path $root 'target'
        New-TrustedDirectory $hostRoot
        New-TrustedDirectory $target
        $junction = Join-Path $hostRoot 'junction'
        New-Item -ItemType Junction -Path $junction -Target $target | Out-Null
        try { Expect-Refusal { Preflight $parent } 'reparse point' }
        finally { [IO.Directory]::Delete($junction) }
    }
    Test-Case 'explicit_untrusted_file_ace_refused' {
        $parent = Join-Path $root 'files'
        New-TrustedDirectory $parent
        $hostRoot = Join-Path $parent 'host'
        New-TrustedDirectory $hostRoot
        $file = Join-Path $hostRoot 'config.toml'
        [IO.File]::WriteAllText($file, 'non-secret fixture')
        Set-NewFileOwner $file
        & icacls.exe $file /grant '*S-1-1-0:R' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot assign file ACE.' }
        Expect-Refusal { Preflight $parent } 'untrusted ACL'
    }
    Test-Case 'protected_child_under_users_delete_child_ancestor_refused' {
        $ancestor = Join-Path $root 'ancestor'
        New-TrustedDirectory $ancestor
        $protected = Join-Path $ancestor 'protected'
        New-TrustedDirectory $protected
        & icacls.exe $ancestor /grant '*S-1-5-32-545:(DC)' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot assign ancestor delete-child ACE.' }
        Expect-Refusal { Preflight $protected } 'ancestor replacement permission'
    }
} finally {
    if (Test-Path -LiteralPath $root) {
        & icacls.exe $root /setowner '*S-1-5-32-544' /T /C | Out-Null
        Remove-Item -LiteralPath $root -Recurse -Force
    }
    ('<testsuite name="windows-installer-paths" tests="' + $cases.Count + '" failures="' + $failed + '">' + ($cases -join '') + '</testsuite>') | Set-Content -LiteralPath $OutputPath -Encoding UTF8
}
if ($failed) { exit 1 }
