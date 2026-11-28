use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::{OsString, c_void};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of_val;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::time::Duration;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Networking::WinHttp::*;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
    MSIDBOPEN_READONLY, MSIHANDLE, MsiCloseHandle, MsiDatabaseOpenViewW, MsiOpenDatabaseW,
    MsiRecordGetStringW, MsiViewExecute, MsiViewFetch,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

const DEFAULT_REPOSITORY: &str = "a-nahvi/monitor-network-taskbar";
const MSI_UPGRADE_CODE: &str = "{2CC4926F-08F7-4628-9B44-22B9FC00B18C}";
const MAX_MSI_SIZE: u64 = 64 * 1024 * 1024;
const MAX_METADATA_SIZE: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct AvailableUpdate {
    pub version: String,
    asset_id: u64,
    size: u64,
    sha256: [u8; 32],
}

pub fn check() -> Result<Option<AvailableUpdate>, String> {
    let repository = update_repository()?;
    let client = HttpClient::new()?;
    let request = client.get(
        "api.github.com",
        &format!("/repos/{repository}/releases/latest"),
        "application/vnd.github+json",
    )?;
    let status = request.status()?;
    if status != 200 {
        return Err(match status {
            403 | 429 => "GitHub temporarily restricted the update check. Try again later.".into(),
            404 => "No public release is available from the configured GitHub repository.".into(),
            status => format!("GitHub release check returned HTTP {status}."),
        });
    }
    let mut contents = Vec::new();
    request.copy_to(&mut contents, MAX_METADATA_SIZE)?;
    let release = serde_json::from_slice::<Value>(&contents)
        .map_err(|_| "GitHub returned invalid release information.")?;
    parse_release(&release, env!("CARGO_PKG_VERSION"))
}

pub fn install(update: &AvailableUpdate) -> Result<(), String> {
    let repository = update_repository()?;
    let client = HttpClient::new()?;
    let download = client.get(
        "api.github.com",
        &format!("/repos/{repository}/releases/assets/{}", update.asset_id),
        "application/octet-stream",
    )?;
    let download = match download.status()? {
        200 => download,
        301 | 302 | 303 | 307 | 308 => {
            let location = download.redirect_location()?;
            let (host, path) = asset_location(&location)
                .ok_or("GitHub redirected the download to an unrecognized address.")?;
            let request = client.get(host, path, "application/octet-stream")?;
            if request.status()? != 200 {
                return Err("GitHub's download link expired or is unavailable. Try again.".into());
            }
            request
        }
        status => return Err(format!("GitHub download returned HTTP {status}.")),
    };
    let directory = update_directory()?;
    clean_expired_downloads(&directory);
    let mut staging = tempfile::Builder::new()
        .prefix("netbar-update-")
        .suffix(".msi")
        .tempfile_in(directory)
        .map_err(|error| format!("Could not stage the update: {error}"))?;
    if download.copy_to(&mut staging, update.size)? != update.size {
        return Err("The downloaded installer has an unexpected size.".into());
    }
    staging
        .as_file()
        .sync_all()
        .map_err(|error| format!("Could not save the downloaded installer: {error}"))?;
    let mut staging = staging.into_temp_path();
    let mut locked = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&staging)
        .map_err(|error| {
            format!("Could not open the downloaded installer for verification: {error}")
        })?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let size = locked
            .read(&mut buffer)
            .map_err(|error| format!("Could not verify the downloaded installer: {error}"))?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    if <[u8; 32]>::from(digest.finalize()) != update.sha256 {
        return Err("The installer failed GitHub's SHA-256 verification.".into());
    }
    verify_package(&staging, &update.version)?;
    let installer = msiexec_path()?;
    let launch = Command::new(installer)
        .arg("/i")
        .arg(staging.as_os_str())
        .arg("/norestart")
        .spawn();
    drop(locked);
    launch.map_err(|error| format!("Could not start Windows Installer: {error}"))?;
    staging.disable_cleanup(true);
    Ok(())
}

fn parse_release(release: &Value, current: &str) -> Result<Option<AvailableUpdate>, String> {
    if release["draft"].as_bool() != Some(false) || release["prerelease"].as_bool() != Some(false) {
        return Err("GitHub returned a release outside the stable channel.".into());
    }
    let version = release["tag_name"]
        .as_str()
        .and_then(|tag| tag.strip_prefix('v'))
        .ok_or("The release tag must use the vMajor.Minor.Patch format.")?;
    let released = version_parts(version).ok_or("The release tag has an invalid version.")?;
    let installed = version_parts(current).ok_or("NetBar has an invalid build version.")?;
    if released <= installed {
        return Ok(None);
    }
    let filename = format!("NetBar-{version}-Setup-x64.msi");
    let asset = release["assets"]
        .as_array()
        .and_then(|assets| {
            assets
                .iter()
                .find(|asset| asset["name"].as_str() == Some(&filename))
        })
        .filter(|asset| asset["state"].as_str() == Some("uploaded"))
        .ok_or_else(|| format!("The release is missing {filename}."))?;
    let asset_id = asset["id"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or("The release installer is missing a valid asset ID.")?;
    let size = asset["size"]
        .as_u64()
        .filter(|size| (1..=MAX_MSI_SIZE).contains(size))
        .ok_or("The release installer has an invalid size.")?;
    let sha256 = asset["digest"]
        .as_str()
        .and_then(parse_sha256)
        .ok_or("GitHub did not provide a valid SHA-256 digest for the installer.")?;
    Ok(Some(AvailableUpdate {
        version: version.into(),
        asset_id,
        size,
        sha256,
    }))
}

fn version_parts(version: &str) -> Option<[u16; 3]> {
    let mut fields = version.split('.');
    let mut result = [0; 3];
    for (index, part) in result.iter_mut().enumerate() {
        let field = fields.next()?;
        if field.is_empty()
            || field.len() > 1 && field.starts_with('0')
            || !field.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        *part = field.parse().ok()?;
        if index < 2 && *part > 255 {
            return None;
        }
    }
    fields.next().is_none().then_some(result)
}

fn parse_sha256(digest: &str) -> Option<[u8; 32]> {
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut result = [0; 32];
    for (index, byte) in result.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(result)
}

fn asset_location(url: &str) -> Option<(&str, &str)> {
    let url = url.strip_prefix("https://")?;
    let (host, _) = url.split_once('/')?;
    let path = &url[host.len()..];
    if !matches!(
        host,
        "release-assets.githubusercontent.com"
            | "objects.githubusercontent.com"
            | "github-releases.githubusercontent.com"
    ) || path
        .bytes()
        .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'\\' | b'#'))
    {
        return None;
    }
    Some((host, path))
}

fn valid_repository(repository: &str) -> bool {
    let Some((owner, repo)) = repository.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && owner.len() <= 39
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !repo.is_empty()
        && repo.len() <= 100
        && !matches!(repo, "." | "..")
        && repo
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}

fn update_repository() -> Result<&'static str, String> {
    let repository = option_env!("NETBAR_UPDATE_REPO").unwrap_or(DEFAULT_REPOSITORY);
    if !valid_repository(repository) {
        return Err("NetBar has an invalid built-in GitHub repository.".into());
    }
    Ok(repository)
}

struct InternetHandle(*mut c_void);

impl InternetHandle {
    fn new(handle: *mut c_void, action: &str) -> Result<Self, String> {
        if handle.is_null() {
            return Err(windows_error(action));
        }
        Ok(Self(handle))
    }
}

impl Drop for InternetHandle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

struct HttpClient(InternetHandle);

impl HttpClient {
    fn new() -> Result<Self, String> {
        let agent = wide(concat!("NetBar/", env!("CARGO_PKG_VERSION")));
        let session = InternetHandle::new(
            unsafe {
                WinHttpOpen(
                    agent.as_ptr(),
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    null(),
                    null(),
                    0,
                )
            },
            "Could not start a secure connection",
        )?;
        if unsafe { WinHttpSetTimeouts(session.0, 5_000, 10_000, 10_000, 15_000) } == 0 {
            return Err(windows_error("Could not configure the update connection"));
        }
        let protocols = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3;
        if unsafe {
            WinHttpSetOption(
                session.0,
                WINHTTP_OPTION_SECURE_PROTOCOLS,
                (&raw const protocols).cast(),
                size_of_val(&protocols) as u32,
            )
        } == 0
        {
            return Err(windows_error("Could not configure TLS for updates"));
        }
        Ok(Self(session))
    }

    fn get(&self, host: &str, path: &str, accept: &str) -> Result<Response<'_>, String> {
        let api_version = if host == "api.github.com" {
            "X-GitHub-Api-Version: 2022-11-28\r\n"
        } else {
            ""
        };
        let headers = wide(&format!("Accept: {accept}\r\n{api_version}"));
        let host = wide(host);
        let connection = InternetHandle::new(
            unsafe { WinHttpConnect(self.0.0, host.as_ptr(), 443, 0) },
            "Could not connect to GitHub",
        )?;
        let method = wide("GET");
        let path = wide(path);
        let request = InternetHandle::new(
            unsafe {
                WinHttpOpenRequest(
                    connection.0,
                    method.as_ptr(),
                    path.as_ptr(),
                    null(),
                    null(),
                    null(),
                    WINHTTP_FLAG_SECURE | WINHTTP_FLAG_ESCAPE_DISABLE,
                )
            },
            "Could not initialize the GitHub request",
        )?;
        let no_redirects = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
        if unsafe {
            WinHttpSetOption(
                request.0,
                WINHTTP_OPTION_REDIRECT_POLICY,
                (&raw const no_redirects).cast(),
                size_of_val(&no_redirects) as u32,
            )
        } == 0
        {
            return Err(windows_error("Could not secure GitHub redirects"));
        }
        if unsafe { WinHttpSendRequest(request.0, headers.as_ptr(), u32::MAX, null(), 0, 0, 0) }
            == 0
            || unsafe { WinHttpReceiveResponse(request.0, null_mut()) } == 0
        {
            return Err(windows_error("Could not reach GitHub"));
        }
        Ok(Response {
            request,
            _connection: connection,
            _client: self,
        })
    }
}

struct Response<'a> {
    request: InternetHandle,
    _connection: InternetHandle,
    _client: &'a HttpClient,
}

impl Response<'_> {
    fn status(&self) -> Result<u32, String> {
        let mut status = 0u32;
        let mut length = size_of_val(&status) as u32;
        let found = unsafe {
            WinHttpQueryHeaders(
                self.request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                (&raw mut status).cast(),
                &mut length,
                null_mut(),
            )
        };
        if found == 0 {
            return Err(windows_error("Could not read the GitHub response status"));
        }
        Ok(status)
    }

    fn redirect_location(&self) -> Result<String, String> {
        let mut buffer = [0u16; 8192];
        let mut length = size_of_val(&buffer) as u32;
        let found = unsafe {
            WinHttpQueryHeaders(
                self.request.0,
                WINHTTP_QUERY_LOCATION,
                null(),
                buffer.as_mut_ptr().cast(),
                &mut length,
                null_mut(),
            )
        };
        if found == 0 {
            return Err("GitHub returned an invalid download redirect.".into());
        }
        String::from_utf16(&buffer[..length as usize / 2])
            .map_err(|_| "GitHub returned an invalid download address.".into())
    }

    fn copy_to(&self, output: &mut impl Write, limit: u64) -> Result<u64, String> {
        let mut buffer = [0u8; 64 * 1024];
        let mut written = 0;
        loop {
            let mut count = 0u32;
            let read = unsafe {
                WinHttpReadData(
                    self.request.0,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut count,
                )
            };
            if read == 0 {
                return Err(windows_error("The GitHub download was interrupted"));
            }
            if count == 0 {
                return Ok(written);
            }
            written += u64::from(count);
            if written > limit {
                return Err("The GitHub response exceeded its expected size.".into());
            }
            output
                .write_all(&buffer[..count as usize])
                .map_err(|error| format!("Could not save the GitHub response: {error}"))?;
        }
    }
}

struct MsiHandle(MSIHANDLE);

impl Drop for MsiHandle {
    fn drop(&mut self) {
        unsafe { MsiCloseHandle(self.0) };
    }
}

fn verify_package(path: &Path, version: &str) -> Result<(), String> {
    let path = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let mut database = MsiHandle(0);
    if unsafe { MsiOpenDatabaseW(path.as_ptr(), MSIDBOPEN_READONLY, &mut database.0) }
        != ERROR_SUCCESS
    {
        return Err("The downloaded file is not a readable Windows Installer package.".into());
    }
    for (key, expected) in [
        ("ProductName", "NetBar"),
        ("ProductVersion", version),
        ("UpgradeCode", MSI_UPGRADE_CODE),
    ] {
        let query = wide(&format!(
            "SELECT `Value` FROM `Property` WHERE `Property` = '{key}'"
        ));
        let mut view = MsiHandle(0);
        let mut record = MsiHandle(0);
        let mut value = [0u16; 128];
        let mut length = value.len() as u32;
        if unsafe { MsiDatabaseOpenViewW(database.0, query.as_ptr(), &mut view.0) } != ERROR_SUCCESS
            || unsafe { MsiViewExecute(view.0, 0) } != ERROR_SUCCESS
            || unsafe { MsiViewFetch(view.0, &mut record.0) } != ERROR_SUCCESS
            || unsafe { MsiRecordGetStringW(record.0, 1, value.as_mut_ptr(), &mut length) }
                != ERROR_SUCCESS
            || String::from_utf16_lossy(&value[..length as usize]) != expected
        {
            return Err("The installer does not match the expected NetBar release.".into());
        }
    }
    Ok(())
}

fn update_directory() -> Result<PathBuf, String> {
    let mut folder = null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, null_mut(), &mut folder) };
    if result < 0 || folder.is_null() {
        return Err("Could not find Local AppData for the installer.".into());
    }
    let value = unsafe { OsString::from_wide(wide_slice(folder)) };
    unsafe { CoTaskMemFree(folder.cast()) };
    let directory = PathBuf::from(value).join("NetBar").join("updates");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Could not make the update directory: {error}"))?;
    Ok(directory)
}

fn clean_expired_downloads(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let filename = entry.file_name();
        let name = filename.to_string_lossy();
        if !name.starts_with("netbar-update-") || !name.ends_with(".msi") {
            continue;
        }
        if entry
            .metadata()
            .and_then(|data| data.modified())
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|elapsed| elapsed > Duration::from_secs(7 * 86_400))
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn msiexec_path() -> Result<PathBuf, String> {
    let mut path = [0u16; 260];
    let length = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
    if length == 0 || length >= path.len() {
        return Err(windows_error("Could not locate Windows Installer"));
    }
    Ok(PathBuf::from(OsString::from_wide(&path[..length])).join("msiexec.exe"))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

unsafe fn wide_slice<'a>(pointer: *const u16) -> &'a [u16] {
    let mut length = 0;
    while unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    unsafe { std::slice::from_raw_parts(pointer, length) }
}

fn windows_error(action: &str) -> String {
    format!("{action}: {}", io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_requires_the_expected_newer_installer_and_digest() {
        let mut release = serde_json::json!({
            "draft": false,
            "prerelease": false,
            "tag_name": "v2.0.0",
            "assets": [{
                "name": "NetBar-2.0.0-Setup-x64.msi",
                "state": "uploaded",
                "id": 7,
                "size": 1024,
                "digest": format!("sha256:{}", "ab".repeat(32)),
            }]
        });
        let update = parse_release(&release, "1.3.0").unwrap().unwrap();
        assert_eq!(update.version, "2.0.0");
        assert_eq!(update.sha256, [0xab; 32]);
        assert!(parse_release(&release, "2.0.0").unwrap().is_none());
        assert!(parse_release(&release, "2.1.0").unwrap().is_none());
        release["assets"][0]["digest"] = Value::Null;
        assert!(parse_release(&release, "1.3.0").is_err());
        release["assets"][0]["name"] = "NetBar-2.0.0-Setup-arm64.msi".into();
        assert!(parse_release(&release, "1.3.0").is_err());
    }

    #[test]
    fn redirects_are_limited_to_https_github_release_hosts() {
        assert_eq!(
            asset_location("https://release-assets.githubusercontent.com/path?signature=abc"),
            Some((
                "release-assets.githubusercontent.com",
                "/path?signature=abc"
            ))
        );
        for url in [
            "http://release-assets.githubusercontent.com/path",
            "https://release-assets.githubusercontent.com.attacker.test/path",
            "https://release-assets.githubusercontent.com@attacker.test/path",
            "https://release-assets.githubusercontent.com/path#fragment",
            "https://api.github.com/path",
        ] {
            assert!(asset_location(url).is_none(), "Accepted {url}");
        }
    }
}
