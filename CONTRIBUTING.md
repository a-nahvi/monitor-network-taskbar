# Contributing to NetBar

Use [GitHub Issues](https://github.com/a-nahvi/monitor-network-taskbar/issues) for bugs and feature requests. For security issues, follow the [private reporting instructions](SECURITY.md).

## Development

Build on Windows 11 x64 with the Rust MSVC toolchain, Visual Studio C++ Build Tools, a Windows SDK, and PowerShell 7. WebView2 is required when opening Settings.

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

To preview Settings with a test configuration, run `cargo run --locked -- --inspect-ui --config build\test.ini`. The taskbar widget uses a native Windows bitmap within a Tauri window. The Settings frontend in `frontend/` is bundled at build time.

If your change affects installation or upgrades, build and inspect the MSI.

```powershell
pwsh -File .\scripts\release.ps1
pwsh -File .\scripts\verify-package.ps1
```

Use `scripts/test-installer.ps1` to exercise a 1.3.0 upgrade, repair, and uninstall in a clean Windows account. The suite checks for an existing NetBar registration before installing its fixtures.

## Pull requests

Keep changes focused and describe what users will notice. Include the commands or manual Windows checks you ran. For taskbar changes, include the Windows build, scaling, and display mode you tested.
