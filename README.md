# NetBar

NetBar is a Windows 11 x64 taskbar monitor for upload and download activity. Tauri powers the taskbar widget, tray menu, and settings. The Rust backend reads adapter counters through Windows IP Helper.

## Install and upgrade

Open the MSI from the private repository’s Releases page. The installer closes a running per-user NetBar MSI installation and upgrades it in place. It preserves preferences and the Windows Startup shortcut, and can launch the new version on completion. Install and uninstall require no administrator rights. Preferences are kept in `%LocalAppData%\NetBar\NetBar.ini`.

NetBar uses the Microsoft Edge WebView2 Runtime included with Windows 11. If it was removed, install the Evergreen Runtime before launching NetBar. [Microsoft Edge WebView2](https://developer.microsoft.com/microsoft-edge/webview2/)

Portable copies can be run directly. To switch from a portable copy to an MSI installation, exit that copy first. If its settings live in an adjacent `NetBar.ini`, copy that file to `%LocalAppData%\NetBar\` before installing if the destination does not already exist.

## Use NetBar

Double-click the widget or select **Settings** from the tray menu. **Display** controls the widget, colors, position, and startup. **Network** controls the adapter, units, and refresh interval. Manual adapter selection remains selected when the adapter is disconnected. Automatic selection prefers an active physical adapter and avoids counting VPN and physical traffic together.

Feather icons mark upload on the top row and download on the bottom row. The optional graph covers the last minute with a shared scale. Unlock the position to drag the widget. The widget is anchored near the tray on the primary horizontal taskbar and hides for fullscreen applications or while the taskbar is out of view. [Feather Icons](https://feathericons.com/)

## Updates

1. Create a fine-grained GitHub token with **Only select repositories** set to `monitor-network-taskbar` and repository **Contents** set to **Read-only**. [Create a GitHub token](https://github.com/settings/personal-access-tokens/new?name=NetBar%20Updates&target_name=a-nahvi&contents=read)
2. Open **Settings → Updates**. Paste the token and choose **Save access**. NetBar stores the token in Windows Credential Manager for your Windows account.
3. Choose **Check for updates**. When a newer version is available, choose **Install update**. NetBar verifies the release asset and opens Windows Installer. Choose **Finish** to launch the new version when prompted.

If the token expires, save its replacement in the same tab. **Clear token** removes the saved access. The app contacts GitHub only when you check for or install an update. Downloads are restricted to the repository configured at build time.

## Build

Use Rust 1.90 or later on Windows with the `x86_64-pc-windows-msvc` target, Visual Studio C++ Build Tools, and a Windows SDK. Use PowerShell 7 to produce the MSI. The HTML, CSS, and JavaScript in `frontend` are bundled directly by Cargo and Tauri.

```powershell
cargo test --locked
pwsh -File .\scripts\release.ps1
```

The MSI and its SHA-256 checksum are written to `dist`. Git tags matching the package version trigger the private repository’s release workflow. The workflow sets `NETBAR_UPDATE_REPO` to the repository that publishes the MSI.

To isolate settings during local verification, launch the executable with `--config <path>`. Use `--inspect-ui --config <path>` to preview settings alongside an installed copy or `--diagnose <path>` to write a JSON diagnostic and exit. The installer fixture script checks an upgrade from the original 1.3.0 MSI in a clean test account.
