param(
    [string]$InstallerPath = '',
    [string]$ExecutablePath = (Join-Path $PSScriptRoot '..\target\x86_64-pc-windows-msvc\release\NetBar.exe'),
    [string]$PreviousInstallerPath = (Join-Path $PSScriptRoot '..\tests\fixtures\NetBar-1.3.0-Setup-x64.msi')
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$releaseExe = Get-Item -LiteralPath $ExecutablePath
if (!$InstallerPath) {
    $releaseVersion = $releaseExe.VersionInfo.ProductVersion
    $InstallerPath = Join-Path $root ('dist\NetBar-' + $releaseVersion + '-Setup-x64.msi')
}
$msi = [IO.Path]::GetFullPath($InstallerPath)
$fixture = [IO.Path]::GetFullPath($PreviousInstallerPath)
if (!(Test-Path -LiteralPath $fixture -PathType Leaf)) { throw 'Supply the historical NetBar installer for the upgrade test.' }
$fixtureVersion = '1.3.0'
$fixtureMsiHash = 'FC931DEEEC2EEA2736A2B00AFF71DDB86EC83BAB2013FBAD90AA36CAA1FA4915'
$fixtureExeHash = 'DDFF8AE5E84B037FA858531D8F9C77D10D9AE7771A4643390DDED6F74AD14CB0'
if ((Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash -ne $fixtureMsiHash) {
    throw 'The historical NetBar installer does not match its expected SHA-256 digest.'
}
if ([version]$fixtureVersion -ge [version]($releaseExe.VersionInfo.ProductVersion)) { throw 'The upgrade fixture must be older than the release.' }
$installer = New-Object -ComObject WindowsInstaller.Installer
function Product-Code([string]$Path) {
    $db = $installer.OpenDatabase($Path, 0)
    $view = $db.OpenView('SELECT `Value` FROM `Property` WHERE `Property` = ''ProductCode''')
    $record = $null
    try {
        $view.Execute($null) | Out-Null; $record = $view.Fetch()
        return $record.GetType().InvokeMember('StringData', [Reflection.BindingFlags]::GetProperty, $null, $record, @(1))
    } finally {
        if ($record) { [Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) | Out-Null }
        $view.Close() | Out-Null; [Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) | Out-Null
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($db) | Out-Null
    }
}
function State([string]$Code) {
    return [int]$installer.GetType().InvokeMember('ProductState', [Reflection.BindingFlags]::GetProperty, $null, $installer, @($Code))
}
function Product-Info([string]$Code, [string]$Attribute) {
    return $installer.GetType().InvokeMember('ProductInfo', [Reflection.BindingFlags]::GetProperty, $null, $installer, @($Code, $Attribute))
}
$currentCode = Product-Code $msi
$fixtureCode = Product-Code $fixture
# Never interfere with an existing installation or running user copy.
if (!('NetBar.InstallerTest' -as [type])) {
    Add-Type -Namespace NetBar -Name InstallerTest -MemberDefinition @'
    [System.Runtime.InteropServices.DllImport("msi.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
    public static extern uint MsiEnumRelatedProducts(string upgradeCode, uint reserved, uint index,
        System.Text.StringBuilder productCode);
    [System.Runtime.InteropServices.DllImport("user32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
    public static extern System.IntPtr FindWindow(string windowClass, string title);
    [System.Runtime.InteropServices.DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(System.IntPtr window, out uint processId);
'@
}
$relatedCode = [Text.StringBuilder]::new(39)
$relatedResult = [NetBar.InstallerTest]::MsiEnumRelatedProducts('{2CC4926F-08F7-4628-9B44-22B9FC00B18C}', 0, 0, $relatedCode)
if ($relatedResult -eq 0) {
    [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
    throw 'An existing NetBar installation must be preserved; use a clean test account.'
}
if ($relatedResult -ne 259) { throw "Windows Installer could not check for existing products ($relatedResult)." }
if ((Test-Path -LiteralPath 'HKCU:\Software\NetBar') -or
    (Test-Path -LiteralPath (Join-Path $env:LOCALAPPDATA 'Programs\NetBar\NetBar.exe'))) {
    throw 'Existing NetBar installation files or registration must be preserved.'
}
$currentState = State $currentCode
$fixtureState = State $fixtureCode
if ($currentState -ne -1 -or $fixtureState -ne -1) {
    throw ('A test product is already registered. States: ' + $currentState + ', ' + $fixtureState + '; codes: ' + $currentCode + ', ' + $fixtureCode)
}
if (@(Get-Process -Name NetBar -ErrorAction SilentlyContinue).Count) { throw 'Close NetBar before this installer integration test.' }
$menu = Join-Path ([Environment]::GetFolderPath('Programs')) 'NetBar\NetBar.lnk'
if (Test-Path -LiteralPath $menu) { throw 'An existing NetBar shortcut must be preserved; use a clean test account.' }
$startupShortcut = Join-Path ([Environment]::GetFolderPath('Startup')) 'NetBar.lnk'
function Startup-Value {
    if (!(Test-Path -LiteralPath $startupShortcut)) { return $null }
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($startupShortcut)
    try { return $shortcut.TargetPath }
    finally {
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shortcut) | Out-Null
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) | Out-Null
    }
}
function Set-TestStartup([string]$Value) {
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($startupShortcut)
    try { $shortcut.TargetPath = $Value; $shortcut.WorkingDirectory = Split-Path -Parent $Value; $shortcut.Save() }
    finally {
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shortcut) | Out-Null
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) | Out-Null
    }
}
$originalStartup = Startup-Value
if ($null -ne $originalStartup) { throw 'An existing NetBar startup entry must be preserved; use a clean test account.' }
$testRoot = Join-Path $root ('build\installer-test-' + [Guid]::NewGuid().ToString('N'))
$reportDirectory = Join-Path $root 'artifacts'
$appDirectory = Join-Path $testRoot 'app'
$app = Join-Path $appDirectory 'NetBar.exe'
$config = Join-Path $testRoot 'preferences\NetBar.ini'
New-Item -ItemType Directory -Path $testRoot,(Split-Path -Parent $config),$reportDirectory -Force | Out-Null
$ownedStartup = $app
$foreignStartup = $releaseExe.FullName
$checks = [Collections.Generic.List[string]]::new()
$steps = [Collections.Generic.List[object]]::new()
function Check([bool]$Condition, [string]$Message) {
    if (!$Condition) { throw $Message }
    $checks.Add($Message); Write-Output ('PASS: ' + $Message)
}
function Msi-Step([string]$Name, [string]$Arguments, [int]$Expected = 0) {
    $log = Join-Path $testRoot ($Name + '.log')
    $process = Start-Process -FilePath (Join-Path $env:WINDIR 'system32\msiexec.exe') `
        -ArgumentList ($Arguments + ' /qn /norestart /l*v "' + $log + '"') -WindowStyle Hidden -PassThru
    if (!$process.WaitForExit(45000)) { throw ('Windows Installer timed out: ' + $Name) }
    $steps.Add([pscustomobject]@{ name = $Name; exit_code = $process.ExitCode; log = $log })
    if ($process.ExitCode -ne $Expected) { throw ($Name + ' returned ' + $process.ExitCode + '; inspect ' + $log) }
}
function Start-Installed {
    $process = Start-Process -FilePath $app -ArgumentList ('--config "' + $config + '"') -WindowStyle Hidden -PassThru
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    $windowProcess = [uint32]0
    do {
        $window = [NetBar.InstallerTest]::FindWindow('NetBar.Monitor.v1', 'NetBar')
        if ($window -ne [IntPtr]::Zero) {
            [void][NetBar.InstallerTest]::GetWindowThreadProcessId($window, [ref]$windowProcess)
        }
        if ($windowProcess -eq $process.Id -or $process.HasExited) { break }
        Start-Sleep -Milliseconds 100
    } while ($deadline.ElapsedMilliseconds -lt 7000)
    Check ($windowProcess -eq $process.Id -and !$process.HasExited) 'installed application starts and registers its monitor window'
}
function Quit-Installed {
    $process = Start-Process -FilePath $app -ArgumentList '--quit' -WindowStyle Hidden -Wait -PassThru
    Check ($process.ExitCode -eq 0) 'installed application closes cleanly'
}
$succeeded = $false
try {
    Msi-Step 'install-fixture' ('/i "' + $fixture + '" INSTALLDIR="' + $appDirectory + '"')
    Check ((State $fixtureCode) -eq 5) 'per-user upgrade fixture is registered'
    $expectedHash = (Get-FileHash -LiteralPath $releaseExe.FullName).Hash
    Check ((Get-FileHash -LiteralPath $app).Hash -eq $fixtureExeHash) 'installed fixture matches the historical release by SHA-256'
    Check ((Product-Info $fixtureCode 'ProductName') -eq 'NetBar' -and
           (Product-Info $fixtureCode 'VersionString') -eq $fixtureVersion -and
           (Product-Info $fixtureCode 'AssignmentType') -eq '0') 'Windows Installer publishes the app name and version with per-user assignment'
    Check (Test-Path -LiteralPath $menu) 'Start-menu shortcut is created'
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($menu)
    Check ($shortcut.TargetPath -eq $app) 'Start-menu shortcut targets the installed executable'
    [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shortcut) | Out-Null
    [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) | Out-Null
    Check ($null -eq (Startup-Value)) 'automatic startup remains disabled by default'
    $preferences = @'
[NetBar]
SchemaVersion=7
IntervalMs=2000
FontPixels=12
HorizontalOffset=0
PositionLocked=1
Visible=0
Theme=0
Units=1
DisplayMode=1
AdapterId=
FutureFeature=keep me
[OtherSection]
Owner=preserve
'@
    [IO.File]::WriteAllText($config, $preferences, [Text.Encoding]::Unicode)
    $preferencesHash = (Get-FileHash -LiteralPath $config).Hash
    Set-TestStartup $ownedStartup
    Start-Installed
    Msi-Step 'upgrade' ('/i "' + $msi + '"')
    Check ((State $fixtureCode) -eq -1 -and (State $currentCode) -eq 5) 'major upgrade replaces the older product registration'
    Check (@(Get-Process -Name NetBar -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $app }).Count -eq 0) 'upgrade closes the running installed copy gracefully'
    Check ((Get-FileHash -LiteralPath $app).Hash -eq $expectedHash) 'upgrade reuses the original directory and installs the new executable'
    Check ((Startup-Value) -eq $ownedStartup) 'upgrade preserves the startup choice'
    Check ((Get-FileHash -LiteralPath $config).Hash -eq $preferencesHash) 'upgrade preserves preferences and future fields byte-for-byte'
    $diagnosticPath = Join-Path $testRoot 'upgraded-diagnostics.json'
    $diagnose = Start-Process -FilePath $app -ArgumentList ('--config "' + $config + '" --diagnose "' + $diagnosticPath + '"') `
        -WindowStyle Hidden -Wait -PassThru
    Check ($diagnose.ExitCode -eq 0) 'the upgraded app opens the existing preference file'
    $diagnostics = Get-Content -LiteralPath $diagnosticPath -Raw | ConvertFrom-Json
    Check ($diagnostics.start_with_windows -and !$diagnostics.show_on_taskbar -and
           $diagnostics.interval_ms -eq 2000 -and $diagnostics.display_mode -eq 'graph') 'the upgraded app uses the previous startup and display settings'
    Start-Installed
    Quit-Installed
    Msi-Step 'reject-downgrade' ('/i "' + $fixture + '" INSTALLDIR="' + $appDirectory + '"') 1603
    Check ((State $currentCode) -eq 5 -and (State $fixtureCode) -eq -1) 'an older installer cannot replace the newer installed version'
    $absoluteApp = [IO.Path]::GetFullPath($app)
    if (!$absoluteApp.StartsWith($testRoot + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe test deletion path.' }
    Remove-Item -LiteralPath $absoluteApp -Force
    Msi-Step 'repair-missing-file' ('/fomus "' + $msi + '"')
    Check ((Get-FileHash -LiteralPath $app).Hash -eq $expectedHash) 'Repair restores a missing executable without depending on that executable'
    $location = (Get-ItemProperty -LiteralPath 'HKCU:\Software\NetBar').InstallLocation
    Check ($location.TrimEnd('\') -eq $appDirectory) 'Repair retains the original installation directory'
    $sentinel = Join-Path $appDirectory 'personal-file.txt'
    [IO.File]::WriteAllText($sentinel, 'Keep user-created files during uninstall.')
    Msi-Step 'uninstall' ('/x ' + $currentCode)
    Check ((State $currentCode) -eq -1) 'uninstall removes Windows Installer registration'
    Check (!(Test-Path -LiteralPath $app) -and !(Test-Path -LiteralPath $menu)) 'uninstall removes program files and its Start-menu shortcut'
    $remainingStartup = Startup-Value
    Check ([string]::IsNullOrEmpty($remainingStartup)) 'uninstall removes only its owned startup entry'
    Check ((Test-Path -LiteralPath $sentinel) -and (Get-FileHash -LiteralPath $config).Hash -eq $preferencesHash) 'uninstall preserves preferences and user-created files'
    Msi-Step 'reinstall' ('/i "' + $msi + '" INSTALLDIR="' + $appDirectory + '"')
    Check ((Get-FileHash -LiteralPath $config).Hash -eq $preferencesHash) 'reinstallation retains preferences'
    Set-TestStartup $foreignStartup
    Remove-Item -LiteralPath $absoluteApp -Force
    Msi-Step 'uninstall-missing-file' ('/x ' + $currentCode)
    Check ((State $currentCode) -eq -1) 'uninstall works when the executable is already missing'
    Check ((Startup-Value) -eq $foreignStartup) 'uninstall preserves a startup entry owned by another copy'
    $succeeded = $true
} finally {
    foreach ($code in @($currentCode, $fixtureCode)) {
        if ((State $code) -eq 5) { Msi-Step ('cleanup-' + $code.Trim('{}')) ('/x ' + $code) }
    }
    $startup = Startup-Value
    if ($startup -eq $ownedStartup -or $startup -eq $foreignStartup) {
        Remove-Item -LiteralPath $startupShortcut -Force
    }
    $report = [ordered]@{ succeeded = $succeeded; installer = $msi; test_directory = $testRoot;
        checks = $checks.ToArray(); steps = $steps.ToArray(); current_product_code = $currentCode;
        fixture_product_code = $fixtureCode; fixture_version = $fixtureVersion }
    $report | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $reportDirectory 'installer-tests.json') -Encoding utf8
    [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
}
Write-Output ('PASS: ' + $checks.Count + ' installer integration checks; test products and startup entries removed.')
