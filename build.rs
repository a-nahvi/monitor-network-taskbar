const COMMANDS: &[&str] = &[
    "settings_state",
    "save_settings",
    "check_updates",
    "install_update",
    "open_releases",
    "close_settings",
];

fn main() {
    println!("cargo:rerun-if-env-changed=NETBAR_UPDATE_REPO");
    println!("cargo:rerun-if-changed=resources/app.ico");
    println!("cargo:rerun-if-changed=resources/app.manifest");
    let windows = tauri_build::WindowsAttributes::new()
        .window_icon_path("resources/app.ico")
        .app_manifest(include_str!("resources/app.manifest"));
    let attributes = tauri_build::Attributes::new()
        .windows_attributes(windows)
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS))
        .codegen(tauri_build::CodegenContext::new());
    tauri_build::try_build(attributes).expect("could not build NetBar's Tauri application");
}
