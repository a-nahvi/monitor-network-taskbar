param([switch]$Offline)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$previousTarget = $env:CARGO_TARGET_DIR

Push-Location $root
try {
    $cargoOptions = @('--locked')
    if ($Offline) { $cargoOptions += '--offline' }
    $metadata = cargo metadata @cargoOptions --no-deps --format-version 1 | ConvertFrom-Json
    $version = ($metadata.packages | Where-Object name -eq 'netbar').version
    if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'Set a three-part release version in Cargo.toml.' }

    $env:CARGO_TARGET_DIR = Join-Path $root 'target'
    cargo build @cargoOptions --release --target x86_64-pc-windows-msvc --bin NetBar
    $executable = Join-Path $env:CARGO_TARGET_DIR 'x86_64-pc-windows-msvc\release\NetBar.exe'
    if ((Get-Item -LiteralPath $executable).VersionInfo.ProductVersion -ne $version) {
        throw 'The executable version differs from Cargo.toml.'
    }

    $output = Join-Path $root 'dist'
    & (Join-Path $PSScriptRoot 'build-installer.ps1') -ExecutablePath $executable -PackageVersion $version -OutputDirectory $output
    $installer = Get-Item -LiteralPath (Join-Path $output "NetBar-$version-Setup-x64.msi")
    $hash = (Get-FileHash -LiteralPath $installer.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText((Join-Path $output "NetBar-$version-SHA256.txt"), "$hash  $($installer.Name)`n")
} finally {
    $env:CARGO_TARGET_DIR = $previousTarget
    Pop-Location
}
