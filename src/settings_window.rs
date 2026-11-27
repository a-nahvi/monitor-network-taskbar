use crate::{network::Adapter, preferences::Settings, startup, update, wide};
use std::{
    cell::RefCell,
    io,
    path::PathBuf,
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver},
    thread,
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::{Dialogs::*, *},
            Input::KeyboardAndMouse::EnableWindow,
            WindowsAndMessaging::*,
        },
    },
    w,
};
use zeroize::Zeroizing;

const TAB: i32 = 217;
const DISPLAY: i32 = 302;
const WIDTH: i32 = 304;
const FONT: i32 = 307;
const THEME: i32 = 309;
const ADAPTER: i32 = 402;
const UNITS: i32 = 404;
const INTERVAL: i32 = 406;
const CHECK_UPDATE: i32 = 501;
const INSTALL_UPDATE: i32 = 502;
const UPDATE_STATUS: i32 = 503;
const REPOSITORY: i32 = 506;
const TOKEN: i32 = 508;
const SAVE_ACCESS: i32 = 509;
const CLEAR_ACCESS: i32 = 510;
const JOB_TIMER: usize = 1;
const DIALOG_USER: i32 = (size_of::<LRESULT>() + size_of::<DLGPROC>()) as i32;

type Save = Box<dyn Fn(Settings, Option<bool>) -> Result<(), String> + Send>;

struct Dialog {
    settings: Settings,
    adapters: Vec<Adapter>,
    custom: bool,
    installed: bool,
    owner: usize,
    save: Save,
    font: HFONT,
    job: Option<Receiver<Job>>,
    available: Option<update::AvailableUpdate>,
}

enum Job {
    Check(Result<Option<update::AvailableUpdate>, String>),
    Install(Result<(), String>),
}

pub fn open(
    owner: HWND,
    settings: Settings,
    adapters: Vec<Adapter>,
    custom: bool,
    save: Save,
) -> std::io::Result<()> {
    let owner = owner as usize;
    thread::Builder::new()
        .name("NetBar settings".into())
        .spawn(move || {
            let result = run_dialog(owner, settings, adapters, custom, save);
            unsafe {
                if result.is_err() {
                    MessageBoxW(
                        null_mut(),
                        w!("The settings window could not open."),
                        w!("NetBar"),
                        MB_OK | MB_ICONERROR,
                    );
                }
                PostMessageW(owner as HWND, crate::SETTINGS_CLOSED, 0, 0);
            }
        })?;
    Ok(())
}

pub fn preview(path: PathBuf) -> io::Result<()> {
    let settings = Settings::load(&path);
    run_dialog(
        0,
        settings,
        crate::network::adapters(),
        true,
        Box::new(move |settings, _| {
            settings
                .save(&path)
                .map_err(|error| format!("Could not save the test settings. {error}"))
        }),
    )
}

fn run_dialog(
    owner: usize,
    settings: Settings,
    adapters: Vec<Adapter>,
    custom: bool,
    save: Save,
) -> io::Result<()> {
    let state = RefCell::new(Dialog {
        settings,
        adapters,
        custom,
        installed: !custom && crate::preferences::installed(),
        owner,
        save,
        font: null_mut(),
        job: None,
        available: None,
    });
    unsafe {
        let controls = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_TAB_CLASSES,
        };
        InitCommonControlsEx(&controls);
        let result = DialogBoxParamW(
            GetModuleHandleW(null()),
            210usize as *const u16,
            null_mut(),
            Some(proc),
            (&raw const state) as isize,
        );
        let error = (result == -1).then(io::Error::last_os_error);
        let state = state.into_inner();
        if !state.font.is_null() {
            DeleteObject(state.font);
        }
        error.map_or(Ok(()), Err)
    }
}

unsafe extern "system" fn proc(hwnd: HWND, message: u32, word: WPARAM, param: LPARAM) -> isize {
    if message == WM_INITDIALOG {
        let state = unsafe { &*(param as *const RefCell<Dialog>) };
        unsafe {
            initialize(hwnd, &mut state.borrow_mut());
            SetWindowLongPtrW(hwnd, DIALOG_USER, param);
        }
        return 1;
    }
    // Standard controls may send synchronous notifications while being updated.
    let command = (word & 0xffff) as i32;
    let notify = (word >> 16) as u16;
    if message == WM_COMMAND
        && notify != 0
        && (command != DISPLAY || notify as u32 != CBN_SELCHANGE)
    {
        return 0;
    }
    if message == WM_NOTIFY
        && (param == 0 || unsafe { (*(param as *const NMHDR)).code } != TCN_SELCHANGE)
    {
        return 0;
    }
    if ![WM_NOTIFY, WM_COMMAND, WM_TIMER, WM_CLOSE].contains(&message) {
        return 0;
    }
    let state = unsafe { GetWindowLongPtrW(hwnd, DIALOG_USER) } as *const RefCell<Dialog>;
    let Some(state) = (unsafe { state.as_ref() }) else {
        return 0;
    };
    let Ok(mut state) = state.try_borrow_mut() else {
        return 0;
    };
    let state = &mut *state;
    unsafe {
        match message {
            WM_NOTIFY => page(hwnd),
            WM_TIMER if word == JOB_TIMER => state.finish_job(hwnd),
            WM_CLOSE | WM_COMMAND if message == WM_CLOSE || command == IDCANCEL => {
                EndDialog(hwnd, 0);
            }
            WM_COMMAND => match command {
                IDOK => {
                    let settings = state.read(hwnd);
                    let startup = (!state.custom).then(|| checked(hwnd, 316));
                    match (state.save)(settings, startup) {
                        Ok(()) => {
                            EndDialog(hwnd, 1);
                        }
                        Err(message) => {
                            MessageBoxW(
                                hwnd,
                                wide(&message).as_ptr(),
                                w!("NetBar settings"),
                                MB_OK | MB_ICONERROR,
                            );
                        }
                    }
                }
                DISPLAY => update_display_fields(hwnd),
                310 => choose_color(
                    hwnd,
                    &mut state.settings.up_color,
                    &mut state.settings.custom_up,
                ),
                311 => choose_color(
                    hwnd,
                    &mut state.settings.down_color,
                    &mut state.settings.custom_down,
                ),
                312 => {
                    state.settings.custom_up = false;
                    state.settings.custom_down = false;
                    state.settings.up_color = Settings::default().up_color;
                    state.settings.down_color = Settings::default().down_color;
                }
                315 => {
                    state.settings.offset = 0;
                    EnableWindow(GetDlgItem(hwnd, 315), 0);
                }
                218 => {
                    state.settings = Settings {
                        schema: state.settings.schema,
                        ..Settings::default()
                    };
                    state.fill(hwnd);
                }
                SAVE_ACCESS | CLEAR_ACCESS => {
                    let repository = get_text(hwnd, REPOSITORY);
                    let token = Zeroizing::new(get_text(hwnd, TOKEN));
                    let result = update::configure(
                        repository.trim(),
                        if command == CLEAR_ACCESS {
                            Some("")
                        } else if token.is_empty() {
                            None
                        } else {
                            Some(token.trim())
                        },
                    );
                    SetDlgItemTextW(hwnd, TOKEN, w!(""));
                    state.available = None;
                    state.buttons(hwnd, false);
                    set_text(
                        hwnd,
                        UPDATE_STATUS,
                        &result
                            .map(|()| {
                                if command == CLEAR_ACCESS {
                                    "GitHub token cleared.".to_owned()
                                } else {
                                    "GitHub access saved.".to_owned()
                                }
                            })
                            .unwrap_or_else(|error| error),
                    );
                }
                CHECK_UPDATE if state.job.is_none() => state.launch_job(hwnd, false),
                INSTALL_UPDATE if state.job.is_none() => state.launch_job(hwnd, true),
                _ => return 0,
            },
            _ => return 0,
        }
    }
    1
}

unsafe fn initialize(hwnd: HWND, state: &mut Dialog) {
    unsafe {
        let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96);
        state.font = CreateFontW(
            -(25 * dpi as i32 / 96),
            0,
            0,
            0,
            FW_SEMIBOLD as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            w!("Segoe UI"),
        );
        if !state.font.is_null() {
            SendDlgItemMessageW(hwnd, 215, WM_SETFONT, state.font as usize, 1);
        }
        set_text(hwnd, 216, concat!("Version ", env!("CARGO_PKG_VERSION")));
        for (index, title) in [w!("Taskbar"), w!("Network"), w!("Updates")]
            .into_iter()
            .enumerate()
        {
            let item = TCITEMW {
                mask: TCIF_TEXT,
                pszText: title.cast_mut(),
                ..Default::default()
            };
            SendDlgItemMessageW(
                hwnd,
                TAB,
                TCM_INSERTITEMW,
                index,
                (&raw const item) as isize,
            );
        }
        insert_choices(hwnd, DISPLAY, &["Numbers", "Graph"]);
        insert_choices(hwnd, FONT, &["Small", "Normal", "Large"]);
        insert_choices(hwnd, THEME, &["Follow Windows", "Light text", "Dark text"]);
        insert_choices(hwnd, UNITS, &["Bytes per second", "Bits per second"]);
        insert_choices(hwnd, INTERVAL, &["1 second", "2 seconds", "5 seconds"]);
        insert_choices(hwnd, ADAPTER, &["Automatic"]);
        if !state.settings.adapter.is_empty()
            && !state
                .adapters
                .iter()
                .any(|item| item.id.eq_ignore_ascii_case(&state.settings.adapter))
        {
            let missing = Adapter::missing(state.settings.adapter.clone());
            state.adapters.push(missing);
        }
        for item in &state.adapters {
            insert_choices(hwnd, ADAPTER, &[&item.name]);
        }
        state.fill(hwnd);
        check(hwnd, 316, !state.custom && startup::enabled());
        EnableWindow(GetDlgItem(hwnd, 316), i32::from(!state.custom));
        SendDlgItemMessageW(hwnd, WIDTH, EM_SETLIMITTEXT, 3, 0);
        SendDlgItemMessageW(hwnd, REPOSITORY, EM_SETLIMITTEXT, 140, 0);
        SendDlgItemMessageW(hwnd, TOKEN, EM_SETLIMITTEXT, 256, 0);
        let repository = update::configured_repository();
        set_text(hwnd, REPOSITORY, repository.as_deref().unwrap_or(""));
        for id in [SAVE_ACCESS, CLEAR_ACCESS, REPOSITORY, TOKEN] {
            EnableWindow(GetDlgItem(hwnd, id), i32::from(!state.custom));
        }
        state.buttons(hwnd, false);
        set_text(
            hwnd,
            UPDATE_STATUS,
            if state.installed {
                "Check for the latest available version."
            } else {
                "Install NetBar with its MSI to update from this window."
            },
        );
        page(hwnd);
        PostMessageW(
            state.owner as HWND,
            crate::SETTINGS_OPENED,
            hwnd as usize,
            0,
        );
    }
}

impl Dialog {
    unsafe fn fill(&self, hwnd: HWND) {
        let s = &self.settings;
        unsafe {
            for (id, selection) in [
                (DISPLAY, s.graph as usize),
                (
                    FONT,
                    match s.font {
                        11 => 0,
                        14 => 2,
                        _ => 1,
                    },
                ),
                (THEME, s.theme as usize),
                (UNITS, s.bits as usize),
                (
                    INTERVAL,
                    match s.interval {
                        2000 => 1,
                        5000 => 2,
                        _ => 0,
                    },
                ),
                (
                    ADAPTER,
                    self.adapters
                        .iter()
                        .position(|item| item.id.eq_ignore_ascii_case(&s.adapter))
                        .map_or(0, |index| index + 1),
                ),
            ] {
                SendDlgItemMessageW(hwnd, id, CB_SETCURSEL, selection, 0);
            }
            set_text(hwnd, WIDTH, &s.width.to_string());
            check(hwnd, 313, s.visible);
            check(hwnd, 314, s.locked);
            check(hwnd, 407, s.app_usage);
            EnableWindow(GetDlgItem(hwnd, 315), i32::from(s.offset != 0));
            update_display_fields(hwnd);
        }
    }

    unsafe fn read(&self, hwnd: HWND) -> Settings {
        unsafe {
            let mut settings = self.settings.clone();
            settings.graph = selected(hwnd, DISPLAY) == 1;
            settings.width = get_text(hwnd, WIDTH).parse().unwrap_or(settings.width);
            settings.font = match selected(hwnd, FONT) {
                0 => 11,
                2 => 14,
                _ => 12,
            };
            settings.theme = selected(hwnd, THEME) as i32;
            settings.visible = checked(hwnd, 313);
            settings.locked = checked(hwnd, 314);
            settings.app_usage = checked(hwnd, 407);
            settings.bits = selected(hwnd, UNITS) == 1;
            settings.interval = match selected(hwnd, INTERVAL) {
                1 => 2000,
                2 => 5000,
                _ => 1000,
            };
            settings.adapter = usize::try_from(selected(hwnd, ADAPTER) - 1)
                .ok()
                .and_then(|index| self.adapters.get(index))
                .map(|item| item.id.clone())
                .unwrap_or_default();
            settings.validate();
            settings
        }
    }

    unsafe fn buttons(&self, hwnd: HWND, busy: bool) {
        unsafe {
            EnableWindow(
                GetDlgItem(hwnd, CHECK_UPDATE),
                i32::from(!busy && self.installed),
            );
            EnableWindow(
                GetDlgItem(hwnd, INSTALL_UPDATE),
                i32::from(!busy && self.installed && self.available.is_some()),
            );
            for id in [REPOSITORY, TOKEN, SAVE_ACCESS, CLEAR_ACCESS] {
                EnableWindow(GetDlgItem(hwnd, id), i32::from(!busy && !self.custom));
            }
        }
    }

    unsafe fn launch_job(&mut self, hwnd: HWND, install: bool) {
        if !self.installed {
            return;
        }
        if install && self.available.is_none() {
            return;
        }
        unsafe {
            let (tx, rx) = mpsc::channel();
            let release = self.available.take();
            let job = thread::Builder::new()
                .name("NetBar update".into())
                .spawn(move || {
                    let event = match (install, release) {
                        (true, Some(release)) => Job::Install(update::install(&release)),
                        _ => Job::Check(update::check()),
                    };
                    let _ = tx.send(event);
                });
            if job.is_ok() {
                self.job = Some(rx);
                self.buttons(hwnd, true);
                set_text(
                    hwnd,
                    UPDATE_STATUS,
                    if install {
                        "Downloading and verifying the installer…"
                    } else {
                        "Checking GitHub…"
                    },
                );
                SetTimer(hwnd, JOB_TIMER, 125, None);
            } else {
                set_text(
                    hwnd,
                    UPDATE_STATUS,
                    "The update could not start. Try again.",
                );
            }
        }
    }

    unsafe fn finish_job(&mut self, hwnd: HWND) {
        let Some(job) = &self.job else {
            return;
        };
        let result = match job.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Job::Check(Err("The update could not finish. Try again.".into()))
            }
        };
        self.job = None;
        let text = match result {
            Job::Check(Ok(Some(release))) => {
                let text = format!(
                    "NetBar {} is available. Save any open settings before installing.",
                    release.version
                );
                self.available = Some(release);
                text
            }
            Job::Check(Ok(None)) => "NetBar is up to date.".into(),
            Job::Install(Ok(())) => "The verified installer has started.".into(),
            Job::Check(Err(error)) | Job::Install(Err(error)) => error,
        };
        unsafe {
            KillTimer(hwnd, JOB_TIMER);
            self.buttons(hwnd, false);
            set_text(hwnd, UPDATE_STATUS, &text);
        }
    }
}

unsafe fn choose_color(hwnd: HWND, rgb: &mut u32, active: &mut bool) {
    let swap = |value: u32| ((value & 0xff) << 16) | (value & 0xff00) | ((value >> 16) & 0xff);
    let mut custom = [0u32; 16];
    let mut dialog = CHOOSECOLORW {
        lStructSize: size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: hwnd,
        rgbResult: swap(*rgb),
        lpCustColors: custom.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        ..Default::default()
    };
    if unsafe { ChooseColorW(&mut dialog) } != 0 {
        *active = true;
        *rgb = swap(dialog.rgbResult);
    }
}

unsafe fn page(hwnd: HWND) {
    unsafe {
        let selected = SendDlgItemMessageW(hwnd, TAB, TCM_GETCURSEL, 0, 0);
        for (page, ids) in [301..=316, 401..=408, 501..=510].into_iter().enumerate() {
            for id in ids {
                ShowWindow(
                    GetDlgItem(hwnd, id),
                    if page as isize == selected {
                        SW_SHOWNA
                    } else {
                        SW_HIDE
                    },
                );
            }
        }
    }
}

unsafe fn update_display_fields(hwnd: HWND) {
    unsafe {
        EnableWindow(
            GetDlgItem(hwnd, WIDTH),
            i32::from(selected(hwnd, DISPLAY) == 1),
        );
    }
}

unsafe fn insert_choices(hwnd: HWND, id: i32, choices: &[&str]) {
    for choice in choices {
        unsafe {
            SendDlgItemMessageW(hwnd, id, CB_ADDSTRING, 0, wide(choice).as_ptr() as isize);
        }
    }
}

unsafe fn set_text(hwnd: HWND, id: i32, value: &str) {
    unsafe {
        SetDlgItemTextW(hwnd, id, wide(value).as_ptr());
    }
}

unsafe fn get_text(hwnd: HWND, id: i32) -> String {
    let mut buffer = Zeroizing::new([0u16; 512]);
    let length = unsafe { GetDlgItemTextW(hwnd, id, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..length as usize])
}

unsafe fn checked(hwnd: HWND, id: i32) -> bool {
    unsafe { IsDlgButtonChecked(hwnd, id) == BST_CHECKED }
}
unsafe fn check(hwnd: HWND, id: i32, checked: bool) {
    unsafe {
        CheckDlgButton(hwnd, id, if checked { BST_CHECKED } else { BST_UNCHECKED });
    }
}
unsafe fn selected(hwnd: HWND, id: i32) -> isize {
    unsafe { SendDlgItemMessageW(hwnd, id, CB_GETCURSEL, 0, 0) }
}
