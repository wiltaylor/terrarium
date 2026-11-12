# Native ACL adversarial tests. Operates only on the unique test directory.
param([Parameter(Mandatory=$true)][string]$Installer, [string]$OutputPath = 'windows-installer-results.xml')
$ErrorActionPreference = 'Stop'
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($Installer, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'Installer does not parse.' }
foreach ($function in $ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst]}, $false)) {
    . ([scriptblock]::Create($function.Extent.Text))
}
$root = Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) ('Ter271AclTest-' + [guid]::NewGuid().ToString('N'))
$cases = New-Object 'System.Collections.Generic.List[string]'
$failed = 0
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
    Test-Case 'precreated_non_admin_owned_parent_refused' {
        $path = Join-Path $root 'owned'
        New-TrustedDirectory $path
        & icacls.exe $path /setowner '*S-1-5-32-545' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot assign test owner.' }
        Expect-Refusal { New-TrustedDirectory $path } 'untrusted owner'
    }
    Test-Case 'trusted_owner_with_world_delete_child_refused' {
        $path = Join-Path $root 'delete-child'
        New-TrustedDirectory $path
        & icacls.exe $path /grant '*S-1-1-0:(OI)(CI)F' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot assign adversarial ACE.' }
        Expect-Refusal { New-TrustedDirectory $path } 'untrusted ACL'
    }
    Test-Case 'junction_inside_host_refused' {
        $hostRoot = Join-Path $root 'host'
        $target = Join-Path $root 'target'
        New-TrustedDirectory $hostRoot
        New-TrustedDirectory $target
        $junction = Join-Path $hostRoot 'junction'
        New-Item -ItemType Junction -Path $junction -Target $target | Out-Null
        try { Expect-Refusal { Assert-TrustedTree $hostRoot } 'reparse point' }
        finally { [IO.Directory]::Delete($junction) }
    }
    Test-Case 'explicit_untrusted_file_ace_refused' {
        $hostRoot = Join-Path $root 'files'
        New-TrustedDirectory $hostRoot
        $file = Join-Path $hostRoot 'config.toml'
        [IO.File]::WriteAllText($file, 'non-secret fixture')
        & icacls.exe $file /grant '*S-1-1-0:R' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot assign file ACE.' }
        Expect-Refusal { Assert-TrustedTree $hostRoot } 'untrusted ACL'
    }
} finally {
    if (Test-Path -LiteralPath $root) {
        & icacls.exe $root /setowner '*S-1-5-32-544' /T /C | Out-Null
        Remove-Item -LiteralPath $root -Recurse -Force
    }
    ('<testsuite name="windows-installer-paths" tests="' + $cases.Count + '" failures="' + $failed + '">' + ($cases -join '') + '</testsuite>') | Set-Content -LiteralPath $OutputPath -Encoding UTF8
}
if ($failed) { exit 1 }
