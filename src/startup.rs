use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows_sys::Win32::Storage::FileSystem::{GetFullPathNameW, WIN32_FIND_DATAW};
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, STGM_READ,
};
use windows_sys::Win32::UI::Shell::{FOLDERID_Startup, SHGetKnownFolderPath, SLGP_RAWPATH};
use windows_sys::core::{GUID, HRESULT, PCWSTR, PWSTR, w};

const SHELL_LINK_CLASS: GUID = GUID::from_u128(0x00021401_0000_0000_c000_000000000046);
const SHELL_LINK_W: GUID = GUID::from_u128(0x000214f9_0000_0000_c000_000000000046);
const PERSIST_FILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);
const PATH_CAPACITY: usize = 32_768;

// windows-sys exposes the Shell APIs but omits COM interface types. The unused
// slots retain the documented IShellLinkW and IPersistFile vtable positions.
#[repr(C)]
struct UnknownVtable {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct ShellLinkVtable {
    base: UnknownVtable,
    get_path:
        unsafe extern "system" fn(*mut c_void, PWSTR, i32, *mut WIN32_FIND_DATAW, u32) -> HRESULT,
    get_id_list: usize,
    set_id_list: usize,
    get_description: usize,
    set_description: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    get_working_directory: usize,
    set_working_directory: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    get_arguments: unsafe extern "system" fn(*mut c_void, PWSTR, i32) -> HRESULT,
    set_arguments: usize,
    get_hotkey: usize,
    set_hotkey: usize,
    get_show_command: usize,
    set_show_command: usize,
    get_icon_location: usize,
    set_icon_location: usize,
    set_relative_path: usize,
    resolve: usize,
    set_path: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
}

#[repr(C)]
struct PersistFileVtable {
    base: UnknownVtable,
    get_class_id: usize,
    is_dirty: usize,
    load: unsafe extern "system" fn(*mut c_void, PCWSTR, u32) -> HRESULT,
    save: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> HRESULT,
}

struct ComInterface(*mut c_void);

impl ComInterface {
    // Each call pairs an interface pointer with the vtable type for its IID.
    unsafe fn vtable<T>(&self) -> &T {
        unsafe { &**self.0.cast::<*const T>() }
    }
}

impl Drop for ComInterface {
    fn drop(&mut self) {
        unsafe { (self.vtable::<UnknownVtable>().release)(self.0) };
    }
}

struct Apartment(bool);

impl Apartment {
    fn enter() -> io::Result<Self> {
        let result = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
        if result == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        check_hresult(result, "Initialize Windows COM")?;
        Ok(Self(true))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

pub fn enabled() -> bool {
    matches!(
        current_shortcut().and_then(|(path, executable)| owned_shortcut(&path, &executable)),
        Ok(true)
    )
}

pub fn set_enabled(enabled: bool) -> io::Result<()> {
    let (path, executable) = current_shortcut()?;
    if owned_shortcut(&path, &executable)? {
        return if enabled {
            Ok(())
        } else {
            std::fs::remove_file(path)
        };
    }
    if !enabled {
        return Ok(());
    }
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "A startup shortcut already belongs to a different copy of NetBar.",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    create_shortcut(&path, &executable)
}

pub(crate) fn owned_shortcut(path: &Path, executable: &Path) -> io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let _apartment = Apartment::enter()?;
    let (link, file) = shell_link()?;
    let path = wide(path.as_os_str());
    let mut target = vec![0; PATH_CAPACITY];
    let mut arguments = vec![0; PATH_CAPACITY];
    unsafe {
        let file_methods = file.vtable::<PersistFileVtable>();
        let link_methods = link.vtable::<ShellLinkVtable>();
        check_hresult(
            (file_methods.load)(file.0, path.as_ptr(), STGM_READ),
            "Read startup shortcut",
        )?;
        check_hresult(
            (link_methods.get_path)(
                link.0,
                target.as_mut_ptr(),
                PATH_CAPACITY as i32,
                null_mut(),
                SLGP_RAWPATH as u32,
            ),
            "Read startup target",
        )?;
        check_hresult(
            (link_methods.get_arguments)(link.0, arguments.as_mut_ptr(), PATH_CAPACITY as i32),
            "Read startup arguments",
        )?;
    }
    let target = OsString::from_wide(nul_terminated(&target));
    if target.is_empty() || arguments[0] != 0 {
        return Ok(false);
    }
    same_path(Path::new(&target), executable)
}

pub(crate) fn same_path(left: &Path, right: &Path) -> io::Result<bool> {
    fn absolute(path: &Path) -> io::Result<Vec<u16>> {
        let path = wide(path.as_os_str());
        let mut buffer = vec![0; PATH_CAPACITY];
        let size = unsafe {
            GetFullPathNameW(
                path.as_ptr(),
                PATH_CAPACITY as u32,
                buffer.as_mut_ptr(),
                null_mut(),
            )
        } as usize;
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        if size >= PATH_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows path is too long",
            ));
        }
        buffer.truncate(size);
        Ok(buffer)
    }
    let left = absolute(left)?;
    let right = absolute(right)?;
    Ok(unsafe {
        CompareStringOrdinal(
            left.as_ptr(),
            left.len() as i32,
            right.as_ptr(),
            right.len() as i32,
            1,
        )
    } == CSTR_EQUAL)
}

fn current_shortcut() -> io::Result<(PathBuf, PathBuf)> {
    let executable = std::env::current_exe()?;
    let mut directory = null_mut();
    let result = unsafe { SHGetKnownFolderPath(&FOLDERID_Startup, 0, null_mut(), &mut directory) };
    if result < 0 {
        unsafe { CoTaskMemFree(directory.cast()) };
        check_hresult(result, "Find Windows Startup folder")?;
    }
    if directory.is_null() {
        return Err(io::Error::other("Windows Startup folder is unavailable"));
    }
    let length = (0..PATH_CAPACITY).find(|&index| unsafe { *directory.add(index) == 0 });
    let path = length.map(|length| {
        OsString::from_wide(unsafe { std::slice::from_raw_parts(directory, length) })
    });
    unsafe { CoTaskMemFree(directory.cast()) };
    let directory =
        PathBuf::from(path.ok_or_else(|| io::Error::other("Windows Startup path is too long"))?);
    Ok((directory.join("NetBar.lnk"), executable))
}

fn create_shortcut(path: &Path, executable: &Path) -> io::Result<()> {
    let _apartment = Apartment::enter()?;
    let (link, file) = shell_link()?;
    let working_directory = wide(
        executable
            .parent()
            .ok_or_else(|| io::Error::other("NetBar has no program folder"))?
            .as_os_str(),
    );
    let executable = wide(executable.as_os_str());
    let path = wide(path.as_os_str());
    unsafe {
        let methods = link.vtable::<ShellLinkVtable>();
        check_hresult(
            (methods.set_path)(link.0, executable.as_ptr()),
            "Set startup executable",
        )?;
        check_hresult(
            (methods.set_working_directory)(link.0, working_directory.as_ptr()),
            "Set startup folder",
        )?;
        check_hresult(
            (methods.set_description)(link.0, w!("NetBar taskbar network speed monitor")),
            "Set startup description",
        )?;
        check_hresult(
            (file.vtable::<PersistFileVtable>().save)(file.0, path.as_ptr(), 1),
            "Save startup shortcut",
        )
    }
}

fn shell_link() -> io::Result<(ComInterface, ComInterface)> {
    let mut link = null_mut();
    check_hresult(
        unsafe {
            CoCreateInstance(
                &SHELL_LINK_CLASS,
                null_mut(),
                CLSCTX_INPROC_SERVER,
                &SHELL_LINK_W,
                &mut link,
            )
        },
        "Create Windows shortcut",
    )?;
    let link = ComInterface(link);
    let mut file = null_mut();
    check_hresult(
        unsafe {
            (link.vtable::<UnknownVtable>().query_interface)(link.0, &PERSIST_FILE, &mut file)
        },
        "Open shortcut persistence interface",
    )?;
    Ok((link, ComInterface(file)))
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn nul_terminated(value: &[u16]) -> &[u16] {
    &value[..value
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(value.len())]
}

fn check_hresult(result: HRESULT, action: &str) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::other(format!(
            "{action} failed (0x{:08X})",
            result as u32
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn recognizes_only_the_target_of_a_real_shell_link() -> io::Result<()> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("netbar-{nonce}.lnk"));
        let executable = std::env::current_exe()?;
        create_shortcut(&path, &executable)?;
        let owned = owned_shortcut(&path, &executable);
        let other = owned_shortcut(&path, &executable.with_extension("other.exe"));
        std::fs::remove_file(path)?;
        assert!(owned?);
        assert!(!other?);
        Ok(())
    }
}
