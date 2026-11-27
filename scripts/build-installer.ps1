param(
    [string]$ExecutablePath = (Join-Path $PSScriptRoot '..\target\x86_64-pc-windows-msvc\release\NetBar.exe'),
    [string]$ActionsDllPath = '',
    [string]$PackageVersion = '',
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '..\dist')
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$executable = Get-Item -LiteralPath $ExecutablePath
if (!$ActionsDllPath) {
    $actionsManifest = Join-Path $root 'installer\actions\Cargo.toml'
    $actionsTarget = Join-Path $root 'build\installer-actions'
    & cargo rustc --locked --release --target x86_64-pc-windows-msvc --manifest-path $actionsManifest `
        --target-dir $actionsTarget -- -C target-feature=+crt-static
    if ($LASTEXITCODE -ne 0) { throw 'Building the installer actions failed.' }
    $ActionsDllPath = Join-Path $actionsTarget 'x86_64-pc-windows-msvc\release\NetBarInstallerActions.dll'
}
if (!(Test-Path -LiteralPath $ActionsDllPath -PathType Leaf)) { throw 'The installer actions DLL is missing.' }
$actionsDll = [IO.Path]::GetFullPath($ActionsDllPath)
if (!$PackageVersion) { $PackageVersion = $executable.VersionInfo.ProductVersion }
if ($PackageVersion -notmatch '^\d+\.\d+\.\d+$') { throw 'Use a three-part MSI product version.' }
$parts = $PackageVersion.Split('.') | ForEach-Object { [int]$_ }
if ($parts[0] -gt 255 -or $parts[1] -gt 255 -or $parts[2] -gt 65535) { throw 'The version exceeds MSI limits.' }
$output = [IO.Path]::GetFullPath($OutputDirectory)
if (!$output.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Output must be inside this project.' }
$stage = Join-Path $root ('build\installer-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stage,$output -Force | Out-Null

# Stable product identity for each version; one upgrade family and stable components.
function Version-ProductCode([string]$Version) {
    $bytes = [Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes('NetBar.win-x64|' + $Version))
    $guidBytes = [byte[]]$bytes[0..15]
    return [Guid]::new($guidBytes).ToString('B').ToUpperInvariant()
}
$productCode = Version-ProductCode $PackageVersion
$upgradeCode = '{2CC4926F-08F7-4628-9B44-22B9FC00B18C}'
$msiPath = Join-Path $stage ('NetBar-' + $PackageVersion + '-Setup-x64.msi')
Copy-Item -LiteralPath $executable.FullName -Destination (Join-Path $stage 'NetBarExe')
$readme = [IO.File]::ReadAllText((Join-Path $root 'README.md')).Replace("`r`n", "`n").Replace("`n", "`r`n")
[IO.File]::WriteAllText((Join-Path $stage 'NetBarReadme'), $readme, [Text.UTF8Encoding]::new($true))
$ddf = @'
.OPTION EXPLICIT
.Set Cabinet=on
.Set Compress=on
.Set CompressionType=LZX
.Set CompressionMemory=21
.Set CabinetNameTemplate=netbar.cab
.Set DiskDirectoryTemplate=.
.Set MaxDiskSize=0
"NetBarExe" "NetBarExe"
"NetBarReadme" "NetBarReadme"
'@
[IO.File]::WriteAllText((Join-Path $stage 'payload.ddf'), $ddf, [Text.Encoding]::ASCII)
Push-Location $stage
try {
    & makecab.exe /F payload.ddf | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Payload cabinet creation failed.' }
} finally { Pop-Location }

$installer = New-Object -ComObject WindowsInstaller.Installer
$database = $installer.OpenDatabase($msiPath, 3)
$columns = @{}
function Sql([string]$Statement) {
    try { $view = $database.OpenView($Statement) }
    catch { throw ('Invalid MSI schema statement: ' + $Statement + ' — ' + $_.Exception.Message) }
    try { $view.Execute($null) | Out-Null } finally { $view.Close() | Out-Null; [Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) | Out-Null }
}
function Row([string]$Table, [object[]]$Values) {
    $placeholders = (@('?') * $Values.Count) -join ','
    $statement = 'INSERT INTO `' + $Table + '` (' + $columns[$Table] + ') VALUES (' + $placeholders + ')'
    $view = $database.OpenView($statement)
    $record = $installer.CreateRecord($Values.Count)
    try {
        for ($index = 0; $index -lt $Values.Count; $index++) {
            $value = $Values[$index]
            if ($null -eq $value) { continue }
            $property = if ($value -is [int]) { 'IntegerData' } else { 'StringData' }
            $record.GetType().InvokeMember($property, [Reflection.BindingFlags]::SetProperty, $null, $record,
                                          @(($index + 1), $value)) | Out-Null
        }
        $view.Execute($record) | Out-Null
    } finally {
        $view.Close() | Out-Null
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) | Out-Null
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) | Out-Null
    }
}
function Stream-Row([string]$Table, [string]$Name, [string]$Path) {
    $view = $database.OpenView('SELECT `Name`, `Data` FROM `' + $Table + '`')
    $record = $installer.CreateRecord(2)
    try {
        $record.GetType().InvokeMember('StringData', [Reflection.BindingFlags]::SetProperty, $null, $record, @(1, $Name)) | Out-Null
        $record.SetStream(2, $Path) | Out-Null
        $view.Execute($null) | Out-Null
        $view.Modify(1, $record) | Out-Null
    } finally {
        $view.Close() | Out-Null
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) | Out-Null
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) | Out-Null
    }
}
try {
    $schemas = [ordered]@{
        Property = '`Property` CHAR(72) NOT NULL, `Value` CHAR(0) NOT NULL LOCALIZABLE PRIMARY KEY `Property`'
        Directory = '`Directory` CHAR(72) NOT NULL, `Directory_Parent` CHAR(72), `DefaultDir` CHAR(255) NOT NULL LOCALIZABLE PRIMARY KEY `Directory`'
        Component = '`Component` CHAR(72) NOT NULL, `ComponentId` CHAR(38), `Directory_` CHAR(72) NOT NULL, `Attributes` SHORT NOT NULL, `Condition` CHAR(255), `KeyPath` CHAR(72) PRIMARY KEY `Component`'
        Feature = '`Feature` CHAR(38) NOT NULL, `Feature_Parent` CHAR(38), `Title` CHAR(64) LOCALIZABLE, `Description` CHAR(255) LOCALIZABLE, `Display` SHORT, `Level` SHORT NOT NULL, `Directory_` CHAR(72), `Attributes` SHORT NOT NULL PRIMARY KEY `Feature`'
        FeatureComponents = '`Feature_` CHAR(38) NOT NULL, `Component_` CHAR(72) NOT NULL PRIMARY KEY `Feature_`, `Component_`'
        File = '`File` CHAR(72) NOT NULL, `Component_` CHAR(72) NOT NULL, `FileName` CHAR(255) NOT NULL LOCALIZABLE, `FileSize` LONG NOT NULL, `Version` CHAR(72), `Language` CHAR(20), `Attributes` SHORT, `Sequence` SHORT NOT NULL PRIMARY KEY `File`'
        Media = '`DiskId` SHORT NOT NULL, `LastSequence` SHORT NOT NULL, `DiskPrompt` CHAR(64) LOCALIZABLE, `Cabinet` CHAR(255), `VolumeLabel` CHAR(32), `Source` CHAR(72) PRIMARY KEY `DiskId`'
        Registry = '`Registry` CHAR(72) NOT NULL, `Root` SHORT NOT NULL, `Key` CHAR(255) NOT NULL LOCALIZABLE, `Name` CHAR(255) LOCALIZABLE, `Value` CHAR(0) LOCALIZABLE, `Component_` CHAR(72) NOT NULL PRIMARY KEY `Registry`'
        Shortcut = '`Shortcut` CHAR(72) NOT NULL, `Directory_` CHAR(72) NOT NULL, `Name` CHAR(128) NOT NULL LOCALIZABLE, `Component_` CHAR(72) NOT NULL, `Target` CHAR(255) NOT NULL, `Arguments` CHAR(255), `Description` CHAR(255) LOCALIZABLE, `Hotkey` SHORT, `Icon_` CHAR(72), `IconIndex` SHORT, `ShowCmd` SHORT, `WkDir` CHAR(72) PRIMARY KEY `Shortcut`'
        RemoveFile = '`FileKey` CHAR(72) NOT NULL, `Component_` CHAR(72) NOT NULL, `FileName` CHAR(255) LOCALIZABLE, `DirProperty` CHAR(72) NOT NULL, `InstallMode` SHORT NOT NULL PRIMARY KEY `FileKey`'
        Upgrade = '`UpgradeCode` CHAR(38) NOT NULL, `VersionMin` CHAR(20), `VersionMax` CHAR(20), `Language` CHAR(255), `Attributes` LONG NOT NULL, `Remove` CHAR(255), `ActionProperty` CHAR(72) NOT NULL PRIMARY KEY `UpgradeCode`, `VersionMin`, `VersionMax`, `Language`, `Attributes`'
        CustomAction = '`Action` CHAR(72) NOT NULL, `Type` SHORT NOT NULL, `Source` CHAR(72), `Target` CHAR(255) PRIMARY KEY `Action`'
        InstallExecuteSequence = '`Action` CHAR(72) NOT NULL, `Condition` CHAR(255), `Sequence` SHORT PRIMARY KEY `Action`'
        InstallUISequence = '`Action` CHAR(72) NOT NULL, `Condition` CHAR(255), `Sequence` SHORT PRIMARY KEY `Action`'
        LaunchCondition = '`Condition` CHAR(255) NOT NULL, `Description` CHAR(255) NOT NULL LOCALIZABLE PRIMARY KEY `Condition`'
        AppSearch = '`Property` CHAR(72) NOT NULL, `Signature_` CHAR(72) NOT NULL PRIMARY KEY `Property`, `Signature_`'
        Signature = '`Signature` CHAR(72) NOT NULL, `FileName` CHAR(255) NOT NULL, `MinVersion` CHAR(20), `MaxVersion` CHAR(20), `MinSize` LONG, `MaxSize` LONG, `MinDate` LONG, `MaxDate` LONG, `Languages` CHAR(255) PRIMARY KEY `Signature`'
        RegLocator = '`Signature_` CHAR(72) NOT NULL, `Root` SHORT NOT NULL, `Key` CHAR(255) NOT NULL, `Name` CHAR(255), `Type` SHORT PRIMARY KEY `Signature_`'
        Icon = '`Name` CHAR(72) NOT NULL, `Data` OBJECT NOT NULL PRIMARY KEY `Name`'
        Binary = '`Name` CHAR(72) NOT NULL, `Data` OBJECT NOT NULL PRIMARY KEY `Name`'
        Dialog = '`Dialog` CHAR(72) NOT NULL, `HCentering` SHORT NOT NULL, `VCentering` SHORT NOT NULL, `Width` SHORT NOT NULL, `Height` SHORT NOT NULL, `Attributes` LONG NOT NULL, `Title` CHAR(128) LOCALIZABLE, `Control_First` CHAR(50), `Control_Default` CHAR(50), `Control_Cancel` CHAR(50) PRIMARY KEY `Dialog`'
        Control = '`Dialog_` CHAR(72) NOT NULL, `Control` CHAR(50) NOT NULL, `Type` CHAR(20) NOT NULL, `X` SHORT NOT NULL, `Y` SHORT NOT NULL, `Width` SHORT NOT NULL, `Height` SHORT NOT NULL, `Attributes` LONG, `Property` CHAR(72), `Text` CHAR(0) LOCALIZABLE, `Control_Next` CHAR(50), `Help` CHAR(50) LOCALIZABLE PRIMARY KEY `Dialog_`, `Control`'
        ControlEvent = '`Dialog_` CHAR(72) NOT NULL, `Control_` CHAR(50) NOT NULL, `Event` CHAR(50) NOT NULL, `Argument` CHAR(255) NOT NULL, `Condition` CHAR(255), `Ordering` SHORT PRIMARY KEY `Dialog_`, `Control_`, `Event`, `Argument`, `Condition`'
        EventMapping = '`Dialog_` CHAR(72) NOT NULL, `Control_` CHAR(50) NOT NULL, `Event` CHAR(50) NOT NULL, `Attribute` CHAR(50) NOT NULL PRIMARY KEY `Dialog_`, `Control_`, `Event`'
        ControlCondition = '`Dialog_` CHAR(72) NOT NULL, `Control_` CHAR(50) NOT NULL, `Action` CHAR(50) NOT NULL, `Condition` CHAR(255) NOT NULL PRIMARY KEY `Dialog_`, `Control_`, `Action`, `Condition`'
        CheckBox = '`Property` CHAR(72) NOT NULL, `Value` CHAR(64) PRIMARY KEY `Property`'
        TextStyle = '`TextStyle` CHAR(72) NOT NULL, `FaceName` CHAR(32) NOT NULL, `Size` SHORT NOT NULL, `Color` LONG, `StyleBits` SHORT PRIMARY KEY `TextStyle`'
    }
    foreach ($name in $schemas.Keys) {
        Sql ('CREATE TABLE `' + $name + '` (' + $schemas[$name] + ')')
        $columns[$name] = ([regex]::Matches($schemas[$name], '`[^`]+` (?=CHAR|SHORT|LONG|OBJECT)') |
                          ForEach-Object { $_.Value.Trim() }) -join ','
    }
    $properties = [ordered]@{
        ProductCode = $productCode; UpgradeCode = $upgradeCode; ProductName = 'NetBar'; ProductVersion = $PackageVersion
        ProductLanguage = '1033'; Manufacturer = 'NetBar'; ARPPRODUCTICON = 'NetBar.ico'; ARPNOMODIFY = '1'
        ARPCOMMENTS = 'Lightweight taskbar upload and download monitor.'; DefaultUIFont = 'DlgFont'
        SecureCustomProperties = 'OLDPRODUCTS;NEWPRODUCTS;NETBARWINDOWSBUILD'; LAUNCHNETBAR = '1'
    }
    foreach ($name in $properties.Keys) { Row Property @($name, $properties[$name]) }
    Row Directory @('TARGETDIR', $null, 'SourceDir')
    Row Directory @('LocalAppDataFolder', 'TARGETDIR', '.')
    Row Directory @('ProgramsDirectory', 'LocalAppDataFolder', 'Programs')
    Row Directory @('INSTALLDIR', 'ProgramsDirectory', 'NetBar')
    Row Directory @('ProgramMenuFolder', 'TARGETDIR', '.')
    Row Directory @('StartupFolder', 'TARGETDIR', '.')
    Row Directory @('NetBarMenu', 'ProgramMenuFolder', 'NetBar')
    Row Component @('Application', '{7D8CB6D5-A2BC-47E4-8C2E-51A0E40B8D36}', 'INSTALLDIR', 256, $null, 'NetBarExe')
    Row Component @('StartMenu', '{E5C6349D-2871-424D-9623-4C90EA22D59E}', 'NetBarMenu', 260, $null, 'MenuInstalled')
    Row Feature @('NetBar', $null, 'NetBar', 'Taskbar upload and download speeds.', 1, 1, 'INSTALLDIR', 0)
    Row FeatureComponents @('NetBar', 'Application')
    Row FeatureComponents @('NetBar', 'StartMenu')
    Row File @('NetBarExe', 'Application', 'NetBar.exe', [int]$executable.Length, $executable.VersionInfo.FileVersion, '1033', 512, 1)
    Row File @('NetBarReadme', 'Application', 'README.txt', [int](Get-Item -LiteralPath (Join-Path $stage 'NetBarReadme')).Length, $null, $null, 512, 2)
    Row Media @(1, 2, $null, '#netbar.cab', $null, $null)
    Stream-Row '_Streams' 'netbar.cab' (Join-Path $stage 'netbar.cab')
    Stream-Row 'Icon' 'NetBar.ico' (Join-Path $root 'resources\app.ico')
    Stream-Row 'Binary' 'NetBarActions' $actionsDll
    Row Registry @('InstallLocation', 1, 'Software\NetBar', 'InstallLocation', '[INSTALLDIR]', 'Application')
    Row Registry @('InstallVersion', 1, 'Software\NetBar', 'Version', $PackageVersion, 'Application')
    Row Registry @('MenuInstalled', 1, 'Software\NetBar', 'MenuInstalled', '#1', 'StartMenu')
    Row Shortcut @('NetBarShortcut', 'NetBarMenu', 'NetBar', 'StartMenu', '[#NetBarExe]', $null,
                   'Show upload and download speeds beside the clock.', $null, $null, $null, 1, 'INSTALLDIR')
    Row RemoveFile @('RemoveMenuFolder', 'StartMenu', $null, 'NetBarMenu', 2)
    Row RemoveFile @('RemoveApplicationFolder', 'Application', $null, 'INSTALLDIR', 2)
    Row Upgrade @($upgradeCode, '0.0.0', $PackageVersion, $null, 256, $null, 'OLDPRODUCTS')
    Row Upgrade @($upgradeCode, $PackageVersion, $null, $null, 2, $null, 'NEWPRODUCTS')
    Row RegLocator @('WindowsBuild', 2, 'Software\Microsoft\Windows NT\CurrentVersion', 'CurrentBuildNumber', 18)
    Row AppSearch @('NETBARWINDOWSBUILD', 'WindowsBuild')
    # MSI maintenance must recover the original directory before calculating paths.
    Row RegLocator @('RegisteredFolder', 1, 'Software\NetBar', 'InstallLocation', 18)
    Row AppSearch @('INSTALLDIR', 'RegisteredFolder')
    Row LaunchCondition @('VersionNT64 AND NETBARWINDOWSBUILD >= 22000', 'NetBar requires 64-bit Windows 11.')
    Row LaunchCondition @('NOT ALLUSERS', 'NetBar installs for the current Windows account only.')
    Row CustomAction @('PreventDowngrade', 19, $null, 'A newer version of NetBar is already installed.')
    Row CustomAction @('SetArpLocation', 51, 'ARPINSTALLLOCATION', '[INSTALLDIR]')
    Row CustomAction @('CloseNetBar', 1, 'NetBarActions', 'CloseNetBar')
    Row CustomAction @('PrepareStartupRemoval', 1, 'NetBarActions', 'PrepareStartupRemoval')
    # EXE + async + continue: the installer must not wait for the monitor to exit.
    Row CustomAction @('LaunchNetBar', 210, 'NetBarExe', '')

    $actions = @(
        @('FindRelatedProducts', $null, 25), @('AppSearch', $null, 50), @('PreventDowngrade', 'NEWPRODUCTS', 75),
        @('LaunchConditions', $null, 100), @('CostInitialize', $null, 800), @('FileCost', $null, 900),
        @('CostFinalize', $null, 1000), @('SetArpLocation', $null, 1001), @('MigrateFeatureStates', $null, 1200)
    )
    foreach ($action in $actions) { Row InstallExecuteSequence $action; Row InstallUISequence $action }
    foreach ($action in @(
        @('CloseNetBar', 'Installed OR OLDPRODUCTS', 1300), @('InstallValidate', $null, 1400), @('InstallInitialize', $null, 1500),
        @('RemoveExistingProducts', 'OLDPRODUCTS', 1600), @('ProcessComponents', $null, 1700),
        @('UnpublishFeatures', $null, 1800),
        @('PrepareStartupRemoval', 'REMOVE~="ALL" AND NOT UPGRADINGPRODUCTCODE', 2100),
        @('RemoveRegistryValues', $null, 2600), @('RemoveShortcuts', $null, 3200),
        @('RemoveFiles', $null, 3500),
        @('InstallFiles', $null, 4000), @('CreateShortcuts', $null, 4500), @('WriteRegistryValues', $null, 5000),
        @('RegisterUser', $null, 6000), @('RegisterProduct', $null, 6100), @('PublishFeatures', $null, 6300),
        @('PublishProduct', $null, 6400), @('InstallFinalize', $null, 6600)
    )) { Row InstallExecuteSequence $action }

    Row TextStyle @('DlgFont', 'Segoe UI', 9, 0, 0)
    Row TextStyle @('DlgTitle', 'Segoe UI', 12, 0, 1)
    function Dialog([string]$Name, [string]$First, [string]$Default, [string]$Cancel, [int]$Attributes = 3) {
        Row Dialog @($Name, 50, 50, 370, 230, $Attributes, 'NetBar Setup', $First, $Default, $Cancel)
    }
    function Control([string]$Dialog, [string]$Name, [string]$Type, [int]$X, [int]$Y, [int]$Width,
                     [int]$Height, [string]$Text, [string]$Next = '', [object]$Property = $null) {
        Row Control @($Dialog, $Name, $Type, $X, $Y, $Width, $Height, 3, $Property, $Text, $Next, $null)
    }
    function Event([string]$Dialog, [string]$Control, [string]$Event, [string]$Argument,
                   [string]$Condition = '1', [int]$Order = 1) {
        Row ControlEvent @($Dialog, $Control, $Event, $Argument, $Condition, $Order)
    }
    Dialog 'WelcomeDlg' 'Install' 'Install' 'Cancel'
    Control 'WelcomeDlg' 'Title' 'Text' 20 15 330 24 '{\DlgTitle}Install NetBar [ProductVersion]'
    Control 'WelcomeDlg' 'Description' 'Text' 20 50 330 60 "Show upload and download speeds beside your clock.`r`n`r`nInstall for your Windows account. No administrator access is needed."
    Control 'WelcomeDlg' 'Location' 'Text' 20 120 330 38 'Destination: [INSTALLDIR]'
    Control 'WelcomeDlg' 'Startup' 'Text' 20 164 330 24 'You can enable Start with Windows in the NetBar menu.'
    Control 'WelcomeDlg' 'Install' 'PushButton' 210 200 66 18 'Install' 'Cancel'
    Control 'WelcomeDlg' 'Cancel' 'PushButton' 284 200 66 18 'Cancel' 'Install'
    Event 'WelcomeDlg' 'Install' 'EndDialog' 'Return'
    Event 'WelcomeDlg' 'Cancel' 'EndDialog' 'Exit'
    Dialog 'MaintenanceDlg' 'Repair' 'Repair' 'Cancel'
    Control 'MaintenanceDlg' 'Title' 'Text' 20 15 330 24 '{\DlgTitle}Manage NetBar'
    Control 'MaintenanceDlg' 'Description' 'Text' 20 55 330 82 "NetBar [ProductVersion] is already installed.`r`n`r`nRepair restores missing program files. Uninstall removes the application and its shortcuts. Your preferences are kept."
    Control 'MaintenanceDlg' 'Repair' 'PushButton' 136 200 66 18 'Repair' 'Remove'
    Control 'MaintenanceDlg' 'Remove' 'PushButton' 210 200 66 18 'Uninstall' 'Cancel'
    Control 'MaintenanceDlg' 'Cancel' 'PushButton' 284 200 66 18 'Cancel' 'Repair'
    Event 'MaintenanceDlg' 'Repair' '[REINSTALL]' 'ALL'
    Event 'MaintenanceDlg' 'Repair' '[REINSTALLMODE]' 'omus' '1' 2
    Event 'MaintenanceDlg' 'Repair' 'EndDialog' 'Return' '1' 3
    Event 'MaintenanceDlg' 'Remove' '[REMOVE]' 'ALL'
    Event 'MaintenanceDlg' 'Remove' 'EndDialog' 'Return' '1' 2
    Event 'MaintenanceDlg' 'Cancel' 'EndDialog' 'Exit'
    Dialog 'ProgressDlg' 'Cancel' 'Cancel' 'Cancel' 1
    Control 'ProgressDlg' 'Title' 'Text' 20 15 330 24 '{\DlgTitle}Setting up NetBar'
    Control 'ProgressDlg' 'Description' 'Text' 20 55 330 36 'Please wait while Windows completes the selected operation.'
    Control 'ProgressDlg' 'Status' 'Text' 20 112 330 36 ''
    Control 'ProgressDlg' 'Progress' 'ProgressBar' 20 160 330 12 ''
    Control 'ProgressDlg' 'Cancel' 'PushButton' 284 200 66 18 'Cancel' 'Cancel'
    Row EventMapping @('ProgressDlg', 'Progress', 'SetProgress', 'Progress')
    Row EventMapping @('ProgressDlg', 'Status', 'ActionText', 'Text')
    Event 'ProgressDlg' 'Cancel' 'SpawnDialog' 'ConfirmCancelDlg'
    Dialog 'ConfirmCancelDlg' 'No' 'No' 'No'
    Control 'ConfirmCancelDlg' 'Question' 'Text' 20 70 330 48 'Cancel NetBar setup?'
    Control 'ConfirmCancelDlg' 'Yes' 'PushButton' 210 200 66 18 'Yes' 'No'
    Control 'ConfirmCancelDlg' 'No' 'PushButton' 284 200 66 18 'No' 'Yes'
    Event 'ConfirmCancelDlg' 'Yes' 'EndDialog' 'Exit'
    Event 'ConfirmCancelDlg' 'No' 'EndDialog' 'Return'
    Dialog 'ExitDlg' 'Launch' 'Finish' 'Finish'
    Control 'ExitDlg' 'Title' 'Text' 20 15 330 24 '{\DlgTitle}Setup complete'
    Control 'ExitDlg' 'Description' 'Text' 20 55 330 72 "NetBar setup completed successfully.`r`n`r`nRight-click the readings or the NetBar tray icon to adjust the display."
    Control 'ExitDlg' 'Removed' 'Text' 20 55 330 72 "NetBar has been uninstalled.`r`n`r`nYour preferences have been kept for a later reinstall."
    Control 'ExitDlg' 'Launch' 'CheckBox' 20 155 260 20 'Launch NetBar' 'Finish' 'LAUNCHNETBAR'
    Control 'ExitDlg' 'Finish' 'PushButton' 284 200 66 18 'Finish' 'Launch'
    Row CheckBox @('LAUNCHNETBAR', '1')
    foreach ($name in @('Description','Launch')) {
        Row ControlCondition @('ExitDlg', $name, 'Hide', 'REMOVE~="ALL"')
        Row ControlCondition @('ExitDlg', $name, 'Show', 'NOT REMOVE~="ALL"')
    }
    Row ControlCondition @('ExitDlg', 'Removed', 'Hide', 'NOT REMOVE~="ALL"')
    Row ControlCondition @('ExitDlg', 'Removed', 'Show', 'REMOVE~="ALL"')
    Event 'ExitDlg' 'Finish' 'DoAction' 'LaunchNetBar' 'LAUNCHNETBAR=1 AND NOT REMOVE~="ALL"'
    Event 'ExitDlg' 'Finish' 'EndDialog' 'Return' '1' 2
    foreach ($name in @('FatalDlg','UserExitDlg')) {
        Dialog $name 'Finish' 'Finish' 'Finish'
        $title = if ($name -eq 'FatalDlg') { '{\DlgTitle}Setup did not complete' } else { '{\DlgTitle}Setup canceled' }
        Control $name 'Title' 'Text' 20 15 330 24 $title
        Control $name 'Description' 'Text' 20 60 330 75 'NetBar setup did not complete. You can run this installer again.'
        Control $name 'Finish' 'PushButton' 284 200 66 18 'Finish' 'Finish'
        Event $name 'Finish' 'EndDialog' 'Return'
    }
    foreach ($action in @(
        @('WelcomeDlg', 'NOT Installed', 1230), @('MaintenanceDlg', 'Installed AND NOT REMOVE', 1240),
        @('ProgressDlg', $null, 1250), @('ExecuteAction', $null, 1300),
        @('ExitDlg', $null, -1), @('UserExitDlg', $null, -2), @('FatalDlg', $null, -3)
    )) { Row InstallUISequence $action }

    $summary = $database.GetType().InvokeMember('SummaryInformation', [Reflection.BindingFlags]::GetProperty, $null, $database, @(20))
    try {
        $summaryProperties = @{ 2 = 'NetBar Setup'; 3 = 'NetBar Windows 11 x64 installation'; 4 = 'NetBar';
            5 = 'NetBar, network, taskbar'; 7 = 'x64;1033'; 9 = [Guid]::NewGuid().ToString('B').ToUpperInvariant();
            14 = 500; 15 = 10; 18 = 'NetBar native MSI builder'; 19 = 2 }
        foreach ($key in $summaryProperties.Keys) {
            $summary.GetType().InvokeMember('Property', [Reflection.BindingFlags]::SetProperty, $null, $summary,
                                            @([int]$key, $summaryProperties[$key])) | Out-Null
        }
        $summary.Persist() | Out-Null
    } finally { [Runtime.InteropServices.Marshal]::FinalReleaseComObject($summary) | Out-Null }
    $database.Commit() | Out-Null
} finally {
    [Runtime.InteropServices.Marshal]::FinalReleaseComObject($database) | Out-Null
    [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
}
$destination = Join-Path $output ([IO.Path]::GetFileName($msiPath))
Copy-Item -LiteralPath $msiPath -Destination $destination -Force
Write-Output ('Installer: ' + $destination)
Write-Output ('Product code: ' + $productCode)
Write-Output ('Installer bytes: ' + (Get-Item -LiteralPath $destination).Length)
Write-Output ('SHA-256: ' + (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash)
