#![allow(non_snake_case)]

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSTALL_FAILURE, ERROR_SUCCESS, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::ApplicationInstallationAndServicing::*;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowThreadProcessId, IsWindow, PostMessageW, WM_CLOSE,
};
use windows_sys::core::{PCWSTR, w};

#[allow(dead_code)] // The settings window uses the rest of the shared module.
#[path = "../../../src/startup.rs"]
mod startup;

struct InstallerHandle(MSIHANDLE);

impl Drop for InstallerHandle {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { MsiCloseHandle(self.0) };
        }
    }
}

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn CloseNetBar(session: MSIHANDLE) -> u32 {
    status(installed_executable(session).is_some_and(|path| close_netbar(&path)))
}

#[unsafe(no_mangle)]
pub extern "system" fn PrepareStartupRemoval(session: MSIHANDLE) -> u32 {
    status(prepare_startup_removal(session))
}

fn status(success: bool) -> u32 {
    if success {
        ERROR_SUCCESS
    } else {
        ERROR_INSTALL_FAILURE
    }
}

fn property(session: MSIHANDLE, name: PCWSTR) -> Option<PathBuf> {
    let mut buffer = vec![0; 32_768];
    let mut count = buffer.len() as u32;
    let result = unsafe { MsiGetPropertyW(session, name, buffer.as_mut_ptr(), &mut count) };
    if result != ERROR_SUCCESS || count == 0 {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(
        buffer.get(..count as usize)?,
    )))
}

fn installed_executable(session: MSIHANDLE) -> Option<PathBuf> {
    Some(property(session, w!("INSTALLDIR"))?.join("NetBar.exe"))
}

fn close_netbar(executable: &Path) -> bool {
    let window = unsafe { FindWindowW(w!("NetBar.Monitor.v1"), w!("NetBar")) };
    if window.is_null() {
        return true;
    }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, &mut pid) };
    let process = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if process.is_null() {
        return unsafe { IsWindow(window) } == 0;
    }
    let process = ProcessHandle(process);
    let mut buffer = vec![0; 32_768];
    let mut count = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut count) } == 0 {
        return unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0;
    }
    let Some(image) = buffer.get(..count as usize) else {
        return false;
    };
    let image = OsString::from_wide(image);
    match startup::same_path(Path::new(&image), executable) {
        Ok(true) => {
            unsafe { PostMessageW(window, WM_CLOSE, 0, 0) };
            (unsafe { WaitForSingleObject(process.0, 5_000) }) == WAIT_OBJECT_0
        }
        Ok(false) => true,
        Err(_) => false,
    }
}

fn prepare_startup_removal(session: MSIHANDLE) -> bool {
    let Some(folder) = property(session, w!("StartupFolder")) else {
        return false;
    };
    let Some(executable) = installed_executable(session) else {
        return false;
    };
    if !matches!(
        startup::owned_shortcut(&folder.join("NetBar.lnk"), &executable),
        Ok(true)
    ) {
        return true;
    }

    // Schedule the matching shortcut in MSI's normal removal transaction.
    let database = InstallerHandle(unsafe { MsiGetActiveDatabase(session) });
    let mut view = 0;
    let query = w!(
        "SELECT `FileKey`, `Component_`, `FileName`, `DirProperty`, `InstallMode` FROM `RemoveFile`"
    );
    if database.0 == 0
        || unsafe { MsiDatabaseOpenViewW(database.0, query, &mut view) } != ERROR_SUCCESS
    {
        return false;
    }
    let view = InstallerHandle(view);
    let record = InstallerHandle(unsafe { MsiCreateRecord(5) });
    if record.0 == 0 {
        return false;
    }
    let values = [
        (1, w!("OwnedStartupRemoval")),
        (2, w!("Application")),
        (3, w!("NetBar.lnk")),
        (4, w!("StartupFolder")),
    ];
    let prepared = values.into_iter().all(|(field, value)| {
        (unsafe { MsiRecordSetStringW(record.0, field, value) }) == ERROR_SUCCESS
    }) && unsafe { MsiRecordSetInteger(record.0, 5, 2) } == ERROR_SUCCESS;
    if !prepared || unsafe { MsiViewExecute(view.0, 0) } != ERROR_SUCCESS {
        return false;
    }
    let result = unsafe { MsiViewModify(view.0, MSIMODIFY_INSERT_TEMPORARY, record.0) };
    unsafe { MsiViewClose(view.0) };
    result == ERROR_SUCCESS
}
