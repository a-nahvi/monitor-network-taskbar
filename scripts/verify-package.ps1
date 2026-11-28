param(
    [string]$InstallerPath = '',
    [string]$ExecutablePath = (Join-Path $PSScriptRoot '..\target\x86_64-pc-windows-msvc\release\NetBar.exe'),
    [string]$ActionsDllPath = (Join-Path $PSScriptRoot '..\build\installer-actions\x86_64-pc-windows-msvc\release\NetBarInstallerActions.dll')
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$executable = [IO.Path]::GetFullPath($ExecutablePath)
$version = (Get-Item -LiteralPath $executable).VersionInfo.ProductVersion
if (!$InstallerPath) { $InstallerPath = Join-Path $root ('dist\NetBar-' + $version + '-Setup-x64.msi') }
$msi = [IO.Path]::GetFullPath($InstallerPath)
$stage = Join-Path $root ('build\package-verification-' + [Guid]::NewGuid().ToString('N'))
$payload = Join-Path $stage 'payload'
$reportDirectory = Join-Path $root 'artifacts'
New-Item -ItemType Directory -Path $stage,$payload,$reportDirectory -Force | Out-Null
$checks = [Collections.Generic.List[string]]::new()
function Check([bool]$Condition, [string]$Message) {
    if (!$Condition) { throw $Message }
    $checks.Add($Message)
    Write-Output ('PASS: ' + $Message)
}
$installer = New-Object -ComObject WindowsInstaller.Installer
$database = $installer.OpenDatabase($msi, 0) # Read-only: no installation or registration changes.
function Read-Rows([string]$Statement, [string[]]$Fields) {
    $view = $database.OpenView($Statement)
    try {
        $view.Execute($null) | Out-Null
        while ($record = $view.Fetch()) {
            try {
                $row = [ordered]@{}
                for ($index = 0; $index -lt $Fields.Count; ++$index) {
                    $row[$Fields[$index]] = $record.GetType().InvokeMember('StringData',
                        [Reflection.BindingFlags]::GetProperty, $null, $record, @(($index + 1)))
                }
                [pscustomobject]$row
            } finally { [Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) | Out-Null }
        }
    } finally {
        $view.Close() | Out-Null
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) | Out-Null
    }
}
function Extract-Stream([string]$Table, [string]$Name, [string]$Destination) {
    $view = $database.OpenView('SELECT `Data` FROM `' + $Table + '` WHERE `Name` = ''' + $Name + '''')
    $record = $null
    $stream = $null
    try {
        $view.Execute($null) | Out-Null
        $record = $view.Fetch()
        if (!$record) { throw ('Missing MSI stream: ' + $Name) }
        $stream = [IO.File]::Create($Destination)
        while ($chunk = $record.GetType().InvokeMember('ReadStream', [Reflection.BindingFlags]::InvokeMethod,
                                                     $null, $record, @(1, 65536, 1))) {
            # msiReadStreamBytes returns one byte per BSTR character, without ANSI translation.
            $bytes = [byte[]]::new($chunk.Length)
            for ($index = 0; $index -lt $chunk.Length; ++$index) { $bytes[$index] = [byte][int]$chunk[$index] }
            $stream.Write($bytes, 0, $bytes.Length)
        }
    } finally {
        if ($stream) { $stream.Dispose() }
        if ($record) { [Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) | Out-Null }
        $view.Close() | Out-Null
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) | Out-Null
    }
}
try {
    $properties = @{}
    foreach ($row in Read-Rows 'SELECT `Property`, `Value` FROM `Property`' @('name','value')) {
        $properties[$row.name] = $row.value
    }
    Check ($properties.ProductVersion -eq $version) 'MSI product version matches the release executable'
    Check ($properties.ProductLanguage -eq '1033') 'MSI uses English language metadata'
    Check ($properties.UpgradeCode -eq '{2CC4926F-08F7-4628-9B44-22B9FC00B18C}') 'MSI retains the existing upgrade family'
    $summary = $database.GetType().InvokeMember('SummaryInformation', [Reflection.BindingFlags]::GetProperty,
                                              $null, $database, @(0))
    try {
        $platform = $summary.GetType().InvokeMember('Property', [Reflection.BindingFlags]::GetProperty,
                                                   $null, $summary, @(7))
        Check ($platform -eq 'x64;1033') 'MSI platform is Windows x64 with English dialogs'
    } finally { [Runtime.InteropServices.Marshal]::FinalReleaseComObject($summary) | Out-Null }
    $files = @(Read-Rows 'SELECT `File`, `FileName`, `FileSize`, `Component_` FROM `File`' @('id','name','bytes','component'))
    Check ($files.Count -eq 3 -and @($files | Where-Object { $_.name -in @('NetBar.exe','README.txt','LICENSE.txt') }).Count -eq 3) 'Only the application, README, and license are installed'
    $appRow = @($files | Where-Object { $_.id -eq 'NetBarExe' })
    Check ($appRow.Count -eq 1 -and [long]$appRow[0].bytes -eq (Get-Item -LiteralPath $executable).Length) 'MSI file metadata matches the executable size'
    $license = Get-Item -LiteralPath (Join-Path $root 'LICENSE')
    $licenseRow = @($files | Where-Object { $_.id -eq 'NetBarLicense' })
    $licenseComponent = @(Read-Rows 'SELECT `ComponentId`, `Directory_`, `Attributes`, `KeyPath` FROM `Component` WHERE `Component` = ''License''' @('guid','directory','attributes','key'))
    $licenseRegistry = @(Read-Rows 'SELECT `Root`, `Component_` FROM `Registry` WHERE `Registry` = ''LicenseInstalled''' @('root','component'))
    Check ($licenseRow.Count -eq 1 -and $licenseRow[0].component -eq 'License' -and [long]$licenseRow[0].bytes -eq $license.Length -and
           $licenseComponent.Count -eq 1 -and $licenseComponent[0].guid -eq '{F648A61E-23B7-4289-BAA5-D3C22DFE9AF4}' -and
           $licenseComponent[0].directory -eq 'INSTALLDIR' -and [int]$licenseComponent[0].attributes -eq 260 -and $licenseComponent[0].key -eq 'LicenseInstalled' -and
           $licenseRegistry.Count -eq 1 -and [int]$licenseRegistry[0].root -eq 1 -and $licenseRegistry[0].component -eq 'License') 'MSI gives the license a stable per-user component and key path'
    $controls = @(Read-Rows 'SELECT `Text` FROM `Control`' @('text'))
    Check ($controls.Count -ge 30 -and !($controls | Where-Object { $_.text -match '[\u0600-\u06ff]' })) 'Authored installer controls remain in English'
    $conditions = @(Read-Rows 'SELECT `Condition` FROM `LaunchCondition`' @('condition'))
    Check (@($conditions | Where-Object { $_.condition -eq 'NOT ALLUSERS' }).Count -eq 1) 'Installer requires current-account installation'
    $cabinet = Join-Path $stage 'netbar.cab'
    Extract-Stream '_Streams' 'netbar.cab' $cabinet
    & (Join-Path $env:WINDIR 'System32\expand.exe') '-F:*' $cabinet $payload | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Embedded MSI cabinet could not be extracted.' }
    Check (@(Get-ChildItem -LiteralPath $payload -File).Count -eq 3) 'Embedded cabinet contains exactly three application payload files'
    $hash = (Get-FileHash -LiteralPath $executable).Hash
    Check ((Get-FileHash -LiteralPath (Join-Path $payload 'NetBarExe')).Hash -eq $hash) 'Actual embedded executable matches the release by SHA-256'
    $embeddedReadme = [IO.File]::ReadAllText((Join-Path $payload 'NetBarReadme')).Replace("`r`n", "`n")
    $sourceReadme = [IO.File]::ReadAllText((Join-Path $root 'README.md')).Replace("`r`n", "`n")
    Check ($embeddedReadme -eq $sourceReadme) 'Actual embedded README matches the current English documentation'
    $licenseHash = (Get-FileHash -LiteralPath $license.FullName -Algorithm SHA256).Hash
    Check ((Get-FileHash -LiteralPath (Join-Path $payload 'NetBarLicense') -Algorithm SHA256).Hash -eq $licenseHash) 'Actual embedded license matches the repository license by SHA-256'
    $action = Join-Path $stage 'NetBar.InstallerActions.dll'
    Extract-Stream 'Binary' 'NetBarActions' $action
    Check ((Get-FileHash -LiteralPath $action).Hash -eq (Get-FileHash -LiteralPath $ActionsDllPath).Hash) 'Embedded installer actions match the built DLL by SHA-256'
    [ordered]@{ version = $version; installer = $msi; bytes = (Get-Item -LiteralPath $msi).Length;
        executable_sha256 = $hash; license_sha256 = $licenseHash; product_code = $properties.ProductCode;
        checks = $checks.ToArray(); note = 'Read-only metadata and actual embedded-payload verification; does not install or run extracted files.' } |
        ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $reportDirectory ('package-verification-' + $version + '.json')) -Encoding utf8
} finally {
    [Runtime.InteropServices.Marshal]::FinalReleaseComObject($database) | Out-Null
    [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
}
Write-Output ('PASS: ' + $checks.Count + ' installer package integrity checks, with no installation changes.')
