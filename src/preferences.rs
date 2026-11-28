use crate::{wide, wide_path};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
    ptr::null_mut,
};
use windows_sys::Win32::{
    Storage::FileSystem::*,
    System::{Registry::*, WindowsProgramming::*},
};
use windows_sys::w;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    English,
    Persian,
}

impl Language {
    pub fn text(self, english: &'static str, persian: &'static str) -> &'static str {
        match self {
            Self::English => english,
            Self::Persian => persian,
        }
    }

    fn ini_value(self) -> &'static str {
        self.text("english", "persian")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub schema: i32,
    pub interval: u32,
    pub font: i32,
    pub offset: i32,
    pub locked: bool,
    pub visible: bool,
    pub theme: i32,
    pub bits: bool,
    pub graph: bool,
    pub width: i32,
    pub custom_up: bool,
    pub custom_down: bool,
    pub up_color: u32,
    pub down_color: u32,
    pub adapter: String,
    #[serde(default)]
    pub language: Language,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema: 1,
            interval: 1000,
            font: 12,
            offset: 0,
            locked: true,
            visible: true,
            theme: 0,
            bits: false,
            graph: false,
            width: 88,
            custom_up: false,
            custom_down: false,
            up_color: 0xFFAB45,
            down_color: 0x42C2FF,
            adapter: String::new(),
            language: Language::default(),
        }
    }
}

impl Settings {
    pub fn validate(&mut self) {
        self.schema = self.schema.max(1);
        if ![1000, 2000, 5000].contains(&self.interval) {
            self.interval = 1000;
        }
        if ![11, 12, 14].contains(&self.font) {
            self.font = 12;
        }
        if !(0..=2).contains(&self.theme) {
            self.theme = 0;
        }
        self.offset = self.offset.clamp(-16000, 16000);
        self.width = self.width.clamp(64, 200);
        self.up_color &= 0xffffff;
        self.down_color &= 0xffffff;
    }

    pub fn load(path: &Path) -> Self {
        let path = wide_path(path);
        let int = |key, default| unsafe {
            GetPrivateProfileIntW(w!("NetBar"), key, default, path.as_ptr()) as i32
        };
        let string = |key| {
            let mut buffer = [0u16; 80];
            let chars = unsafe {
                GetPrivateProfileStringW(
                    w!("NetBar"),
                    key,
                    w!(""),
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    path.as_ptr(),
                )
            };
            String::from_utf16_lossy(&buffer[..chars as usize])
        };
        let mut settings = Self {
            schema: int(w!("SchemaVersion"), 1),
            interval: int(w!("IntervalMs"), 1000) as u32,
            font: int(w!("FontPixels"), 12),
            offset: int(w!("HorizontalOffset"), 0),
            locked: int(w!("PositionLocked"), 1) != 0,
            visible: int(w!("Visible"), 1) != 0,
            theme: int(w!("Theme"), 0),
            bits: int(w!("Units"), 0) == 1,
            graph: int(w!("DisplayMode"), 0) == 1,
            width: int(w!("GraphWidth"), 88),
            custom_up: int(w!("CustomUploadColor"), 0) != 0,
            custom_down: int(w!("CustomDownloadColor"), 0) != 0,
            up_color: int(w!("UploadColor"), 0xFFAB45) as u32,
            down_color: int(w!("DownloadColor"), 0x42C2FF) as u32,
            adapter: string(w!("AdapterId")),
            language: if string(w!("Language")).eq_ignore_ascii_case(Language::Persian.ini_value())
            {
                Language::Persian
            } else {
                Language::English
            },
        };
        settings.validate();
        settings
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        let result = self.write_copy(path, &temp);
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }

    fn write_copy(&self, path: &Path, temp: &Path) -> io::Result<()> {
        match fs::copy(path, temp) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::write(temp, [0xff, 0xfe])?,
            Err(error) => return Err(error),
        }
        let target = wide_path(temp);
        let key_values = [
            ("SchemaVersion", self.schema),
            ("IntervalMs", self.interval as i32),
            ("FontPixels", self.font),
            ("HorizontalOffset", self.offset),
            ("PositionLocked", self.locked.into()),
            ("Visible", self.visible.into()),
            ("Theme", self.theme),
            ("Units", self.bits.into()),
            ("DisplayMode", self.graph.into()),
            ("GraphWidth", self.width),
            ("CustomUploadColor", self.custom_up.into()),
            ("CustomDownloadColor", self.custom_down.into()),
            ("UploadColor", self.up_color as i32),
            ("DownloadColor", self.down_color as i32),
        ];
        for (key, value) in key_values
            .into_iter()
            .map(|(k, v)| (k, v.to_string()))
            .chain([
                ("AdapterId", self.adapter.clone()),
                ("Language", self.language.ini_value().to_owned()),
            ])
        {
            if unsafe {
                WritePrivateProfileStringW(
                    w!("NetBar"),
                    wide(key).as_ptr(),
                    wide(&value).as_ptr(),
                    target.as_ptr(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        if unsafe {
            MoveFileExW(
                target.as_ptr(),
                wide_path(path).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

pub fn default_path() -> io::Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| io::Error::other("Windows local app data is unavailable"))?;
    let destination = PathBuf::from(local).join("NetBar").join("NetBar.ini");
    let source = std::env::current_exe()?.with_file_name("NetBar.ini");
    if !destination.exists() && source.is_file() {
        fs::create_dir_all(destination.parent().unwrap())?;
        use windows_sys::Win32::Foundation::GetLastError;
        if unsafe {
            CopyFileW(
                wide_path(&source).as_ptr(),
                wide_path(&destination).as_ptr(),
                1,
            )
        } == 0
        {
            let error = unsafe { GetLastError() };
            if !destination.exists() {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
    }
    Ok(destination)
}

pub fn light_text(theme: i32) -> bool {
    if theme != 0 {
        return theme == 1;
    }
    let mut uses_light = 0u32;
    let mut bytes = 4;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut uses_light as *mut u32).cast(),
            &mut bytes,
        );
    }
    uses_light == 0
}

pub fn installed() -> bool {
    let mut path = [0u16; 32768];
    let mut size = std::mem::size_of_val(&path) as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\NetBar"),
            w!("InstallLocation"),
            RRF_RT_REG_SZ,
            null_mut(),
            path.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != 0 {
        return false;
    }
    let directory =
        String::from_utf16_lossy(&path[..path.iter().position(|&c| c == 0).unwrap_or(path.len())]);
    let expected = PathBuf::from(directory).join("NetBar.exe");
    fs::canonicalize(expected)
        .ok()
        .zip(
            std::env::current_exe()
                .ok()
                .and_then(|path| fs::canonicalize(path).ok()),
        )
        .is_some_and(|(expected, running)| {
            expected
                .to_string_lossy()
                .eq_ignore_ascii_case(&running.to_string_lossy())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_and_invalid_preferences_use_english() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("NetBar.ini");
        for contents in [
            "[NetBar]\r\nIntervalMs=2000\r\n",
            "[NetBar]\r\nIntervalMs=2000\r\nLanguage=unknown\r\n",
        ] {
            fs::write(&path, contents).unwrap();
            let settings = Settings::load(&path);
            assert_eq!(settings.language, Language::English);
            assert_eq!(settings.interval, 2000);
        }
    }

    #[test]
    fn language_round_trips_through_ipc_and_ini_without_losing_other_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("NetBar.ini");
        fs::write(
            &path,
            "[NetBar]\r\nSchemaVersion=7\r\nFutureFeature=keep me\r\n[Other]\r\nOwner=preserve\r\n",
        )
        .unwrap();

        let mut settings = Settings::load(&path);
        for (language, value) in [
            (Language::Persian, "persian"),
            (Language::English, "english"),
        ] {
            settings.language = language;
            let payload = serde_json::to_value(&settings).unwrap();
            assert_eq!(payload["language"], value);
            let received: Settings = serde_json::from_value(payload).unwrap();
            received.save(&path).unwrap();

            let reloaded = Settings::load(&path);
            assert_eq!(reloaded.language, language);
            assert_eq!(reloaded.schema, 7);
            let contents = fs::read_to_string(&path).unwrap();
            assert!(contents.contains(&format!("Language={value}")));
            assert!(contents.contains("FutureFeature=keep me"));
            assert!(contents.contains("[Other]\r\nOwner=preserve"));
        }
    }
}
