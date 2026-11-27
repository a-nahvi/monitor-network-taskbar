use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=NETBAR_UPDATE_REPO");
    println!("cargo:rerun-if-changed=resources/app.ico");
    println!("cargo:rerun-if-changed=resources/app.manifest");
    println!("cargo:rerun-if-changed=resources/settings-v2.rc");

    let version = env!("CARGO_PKG_VERSION");
    let numeric = version.replace('.', ",") + ",0";
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("netbar.rc");
    let resources = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("resources");
    let resources = resources.to_str().unwrap().replace('\\', "/");
    fs::write(
        &output,
        format!(
            r#"
#pragma code_page(65001)
#include <windows.h>
#include "{resources}/settings-v2.rc"
101 ICON "{resources}/app.ico"
1 RT_MANIFEST "{resources}/app.manifest"
1 VERSIONINFO
 FILEVERSION {numeric}
 PRODUCTVERSION {numeric}
 FILEFLAGSMASK 0x3fL
 FILEOS VOS_NT_WINDOWS32
 FILETYPE VFT_APP
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "040904b0"
  BEGIN
   VALUE "CompanyName", "NetBar\0"
   VALUE "FileDescription", "NetBar network speed monitor\0"
   VALUE "FileVersion", "{version}\0"
   VALUE "OriginalFilename", "NetBar.exe\0"
   VALUE "ProductName", "NetBar\0"
   VALUE "ProductVersion", "{version}\0"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x0409, 1200
 END
END
"#
        ),
    )
    .unwrap();
    embed_resource::compile_for(&output, ["NetBar"], embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
