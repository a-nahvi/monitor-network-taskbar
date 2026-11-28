#![windows_subsystem = "windows"]

mod network;
mod preferences;
mod settings_window;
mod startup;
mod update;
mod widget;
mod widget_window;

use network::{Monitor, RateLabel, Rates};
use preferences::{Language, Settings};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    io,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, RunEvent};
use widget::{Presentation, Widget};
use windows_sys::{
    Win32::{
        Foundation::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::*},
        UI::{Accessibility::*, WindowsAndMessaging::*},
    },
    w,
};

pub const NETWORK_CHANGED: u32 = WM_APP + 1;
pub const SETTINGS_CLOSED: u32 = WM_APP + 3;
pub const SETTINGS_RELOAD: u32 = WM_APP + 4;
const POSITION_CHANGED: u32 = WM_APP + 5;
const OPEN_SETTINGS: u32 = WM_APP + 7;
const SAMPLE_TIMER: usize = 1;
const PLACEMENT_TIMER: usize = 2;
const NETWORK_TIMER: usize = 5;
const HISTORY_POINTS: usize = 61;
const WINDOW_CLASS: *const u16 = w!("NetBar.Monitor.v1");
const WINDOW_TITLE: *const u16 = w!("NetBar");

thread_local! { static HOOK_TARGET: Cell<HWND> = const { Cell::new(null_mut()) }; }

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

pub fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[derive(Default)]
struct Options {
    config: Option<PathBuf>,
    diagnose: Option<PathBuf>,
    quit: bool,
    settings: bool,
    inspect_ui: bool,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = OsString>) -> io::Result<Self> {
        let mut options = Self::default();
        while let Some(argument) = args.next() {
            match argument.to_str() {
                Some("--quit") => options.quit = true,
                Some("--settings") => options.settings = true,
                Some("--inspect-ui") => options.inspect_ui = true,
                Some("--config" | "--diagnose") => {
                    let next = args.next().ok_or_else(|| {
                        io::Error::other("Missing path for a command-line option.")
                    })?;
                    let path = std::path::absolute(next)?;
                    if argument == "--config" {
                        options.config = Some(path);
                    } else {
                        options.diagnose = Some(path);
                    }
                }
                _ => return Err(io::Error::other("Invalid command-line option.")),
            }
        }
        Ok(options)
    }
}

fn main() {
    if let Err(error) = run() {
        show_error(&error.to_string(), Language::default());
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let options = Options::parse(std::env::args_os().skip(1))?;
    if options.inspect_ui {
        let path = options.config.ok_or_else(|| {
            io::Error::other("The settings preview requires a separate --config path.")
        })?;
        settings_window::builder()
            .setup(move |app| Ok(settings_window::open(app.handle(), path, 0, true).map(|_| ())?))
            .run(settings_window::context())
            .map_err(io::Error::other)?;
        return Ok(());
    }
    if options.quit {
        return quit_existing();
    }
    if let Some(report) = options.diagnose {
        let config = match options.config {
            Some(path) => path,
            None => preferences::default_path()?,
        };
        return diagnostics(&config, &report);
    }

    // The running monitor and MSI upgrade share this identity with NetBar 1.x.
    let mutex = Handle(unsafe { CreateMutexW(null(), 0, w!("Local\\NetBar.Monitor.v1")) });
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if mutex.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    if already_running {
        if options.settings {
            return request_existing_settings();
        }
        return Ok(());
    }
    let custom = options.config.is_some();
    let config = match options.config {
        Some(path) => path,
        None => preferences::default_path()?,
    };
    let worker = Arc::new(Mutex::new(None));
    let worker_slot = worker.clone();
    let app = settings_window::builder()
        .setup(move |app| {
            let app = app.handle().clone();
            let monitor =
                thread::Builder::new()
                    .name("NetBar monitor".into())
                    .spawn(move || {
                        let result = run_monitor(app.clone(), config, custom, options.settings);
                        app.exit(i32::from(result.is_err()));
                        result
                    })?;
            *worker_slot.lock().unwrap() = Some(monitor);
            Ok(())
        })
        .build(settings_window::context())
        .map_err(io::Error::other)?;
    let code = app.run_return(|_, event| {
        if let RunEvent::ExitRequested {
            code: None, api, ..
        } = event
        {
            api.prevent_exit();
        }
    });
    if let Some(worker) = worker.lock().unwrap().take() {
        worker
            .join()
            .map_err(|_| io::Error::other("The background monitor stopped unexpectedly."))??;
    }
    if code != 0 {
        return Err(io::Error::other("NetBar stopped unexpectedly."));
    }
    Ok(())
}

fn run_monitor(
    tauri: AppHandle,
    path: PathBuf,
    custom: bool,
    open_settings: bool,
) -> io::Result<()> {
    let settings = Settings::load(&path);
    unsafe {
        let instance = GetModuleHandleW(null());
        let definition = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&definition) == 0 {
            return Err(io::Error::last_os_error());
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            WINDOW_CLASS,
            WINDOW_TITLE,
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return Err(io::Error::last_os_error());
        }
        let state = Box::new(RefCell::new(App::new(tauri, hwnd, settings, path, custom)));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&raw const *state) as isize);
        HOOK_TARGET.set(hwnd);
        let foreground = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            null_mut(),
            Some(foreground_changed),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        let notifications = network::Notifications::new(hwnd);
        {
            let app = &mut *state.borrow_mut();
            if app.is_sampling() {
                app.refresh_network();
            }
            app.arm_sample();
            app.refresh_widget(true);
            timer(hwnd, PLACEMENT_TIMER, 2000, 150);
            if open_settings {
                app.open_settings();
            }
        }
        let mut message = MSG::default();
        let result = loop {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result <= 0 {
                break result;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        };
        HOOK_TARGET.set(null_mut());
        if !foreground.is_null() {
            UnhookWinEvent(foreground);
        }
        drop(notifications);
        if IsWindow(hwnd) != 0 {
            DestroyWindow(hwnd);
        }
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn request_existing_settings() -> io::Result<()> {
    // The single-instance mutex can be visible before the first launch has a message window.
    for _ in 0..40 {
        unsafe {
            let hwnd = FindWindowW(WINDOW_CLASS, WINDOW_TITLE);
            if !hwnd.is_null() {
                let mut process = 0;
                GetWindowThreadProcessId(hwnd, &mut process);
                if process != 0 {
                    AllowSetForegroundWindow(process);
                    if PostMessageW(hwnd, OPEN_SETTINGS, 0, 0) != 0 {
                        return Ok(());
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::other(
        "Could not reach the running NetBar window.",
    ))
}

fn quit_existing() -> io::Result<()> {
    unsafe {
        let hwnd = FindWindowW(WINDOW_CLASS, WINDOW_TITLE);
        if hwnd.is_null() {
            return Ok(());
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let process = Handle(OpenProcess(PROCESS_SYNCHRONIZE, 0, pid));
        if PostMessageW(hwnd, WM_CLOSE, 0, 0) == 0 {
            return Err(io::Error::last_os_error());
        }
        if !process.0.is_null() && WaitForSingleObject(process.0, 5000) != WAIT_OBJECT_0 {
            return Err(io::Error::other("NetBar did not close in time."));
        }
    }
    Ok(())
}

fn diagnostics(config: &Path, report: &Path) -> io::Result<()> {
    let settings = Settings::load(config);
    let adapters = network::adapters();
    let adapter = network::select(&adapters, &settings.adapter);
    let mut monitor = Monitor::default();
    monitor.select(adapter);
    let data = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"), "config": config, "connected": monitor.sample().connected,
        "adapter_id": settings.adapter, "interval_ms": settings.interval, "show_on_taskbar": settings.visible,
        "start_with_windows": startup::enabled(), "display_mode": if settings.graph { "graph" } else { "numerical" },
        "adapters": adapters.iter().map(|item| serde_json::json!({ "id": item.id, "name": item.name, "connected": item.connected })).collect::<Vec<_>>()
    });
    std::fs::write(report, serde_json::to_vec_pretty(&data)?)
}

struct Drag {
    origin: i32,
    offset: i32,
}

struct App {
    ui: AppHandle,
    hwnd: HWND,
    settings: Settings,
    path: PathBuf,
    custom: bool,
    taskbar_created: u32,
    settings_window: Option<usize>,
    light_text: bool,
    widget: Widget,
    network: Monitor,
    last_network: Option<Instant>,
    rates: Rates,
    history: Vec<(f64, f64)>,
    dragging: Option<Drag>,
    paused: bool,
    suspended: bool,
}

impl App {
    fn new(ui: AppHandle, hwnd: HWND, settings: Settings, path: PathBuf, custom: bool) -> Self {
        let light_text = preferences::light_text(settings.theme);
        let widget = Widget::new(ui.clone(), hwnd);
        Self {
            ui,
            hwnd,
            settings,
            path,
            custom,
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            settings_window: None,
            light_text,
            widget,
            network: Monitor::default(),
            last_network: None,
            rates: Rates::default(),
            history: Vec::new(),
            dragging: None,
            paused: false,
            suspended: false,
        }
    }

    fn select_network(&mut self) -> bool {
        self.last_network = Some(Instant::now());
        let adapters = network::adapters();
        self.network
            .select(network::select(&adapters, &self.settings.adapter))
    }

    fn refresh_network(&mut self) -> bool {
        let changed = self.select_network();
        if changed {
            self.history.clear();
            self.rates = self.network.sample();
        }
        changed
    }

    fn sample(&mut self) {
        let age = self
            .last_network
            .map_or(Duration::MAX, |time| time.elapsed());
        let retry = if self.rates.connected {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(5)
        };
        let changed = age >= retry && self.refresh_network();
        let rates = if changed {
            self.rates
        } else {
            self.network.sample()
        };
        if self.rates.connected != rates.connected {
            self.history.clear();
        }
        self.rates = rates;
        if self.settings.graph && rates.connected && self.settings.visible {
            let points = ((60_000 / self.settings.interval + 1) as usize).min(HISTORY_POINTS);
            if self.history.len() >= points {
                self.history.remove(0);
            }
            self.history.push((rates.up, rates.down));
        }
        self.refresh_widget(false);
    }

    fn refresh_widget(&mut self, force: bool) {
        let s = &self.settings;
        let up = RateLabel::new(self.rates.up, s.bits);
        let down = RateLabel::new(self.rates.down, s.bits);
        fn color(rgb: u32) -> [u8; 3] {
            [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8]
        }
        self.widget.update(
            Presentation {
                visible: s.visible && !self.suspended,
                graph: s.graph,
                graph_width: s.width,
                text_size: s.font,
                light_text: self.light_text,
                horizontal_offset: s.offset,
                sample_interval_ms: s.interval,
                controls: widget_window::Controls {
                    language: s.language,
                    paused: self.paused,
                    unlocked: !s.locked,
                    settings_open: self.settings_window.is_some(),
                    can_reset: s.offset != 0,
                },
                upload: if self.rates.connected {
                    up.as_str()
                } else {
                    "–"
                },
                download: if self.rates.connected {
                    down.as_str()
                } else {
                    "–"
                },
                upload_color: s.custom_up.then(|| color(s.up_color)),
                download_color: s.custom_down.then(|| color(s.down_color)),
                connected: self.rates.connected,
                history: &self.history,
            },
            force,
        );
    }

    fn is_sampling(&self) -> bool {
        self.settings.visible && !self.paused && !self.suspended
    }

    fn arm_sample(&self) {
        unsafe {
            KillTimer(self.hwnd, SAMPLE_TIMER);
            if self.is_sampling() {
                timer(self.hwnd, SAMPLE_TIMER, self.settings.interval, 75);
            } else {
                KillTimer(self.hwnd, NETWORK_TIMER);
            }
        }
    }

    fn prime_network(&mut self) {
        self.network.reset();
        if self.select_network() {
            self.history.clear();
        }
        // The next full sampling interval replaces the displayed reading.
        self.network.sample();
    }

    fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        if self.is_sampling() {
            self.prime_network();
        }
        self.arm_sample();
        self.refresh_widget(false);
    }

    fn open_settings(&mut self) {
        self.finish_drag(false);
        match settings_window::open(&self.ui, self.path.clone(), self.hwnd as usize, self.custom) {
            Ok(window) => {
                self.settings_window = Some(window);
                self.refresh_widget(false);
            }
            Err(error) => {
                let language = self.settings.language;
                let message =
                    language.text("NetBar settings could not open.", "تنظیمات نت‌بار باز نشد.");
                show_error(&format!("{message}\n{error}"), language);
            }
        }
    }

    fn apply(&mut self, settings: Settings) {
        if self.settings.interval != settings.interval
            || self.settings.graph != settings.graph
            || !settings.visible
        {
            self.history.clear();
        }
        let was_sampling = self.is_sampling();
        let interval_changed = self.settings.interval != settings.interval;
        let adapter_changed = self.settings.adapter != settings.adapter;
        self.settings = settings;
        self.light_text = preferences::light_text(self.settings.theme);
        let sampling = self.is_sampling();
        if sampling && (!was_sampling || adapter_changed) {
            self.prime_network();
        }
        if interval_changed || was_sampling != sampling {
            self.arm_sample();
        }
        self.refresh_widget(true);
    }

    fn menu_action(&mut self, action: usize) {
        if self.settings_window.is_some() {
            return;
        }
        let mut settings = self.settings.clone();
        match action {
            widget_window::ACTION_HIDE => settings.visible = false,
            widget_window::ACTION_LOCK => settings.locked = !settings.locked,
            widget_window::ACTION_RESET_POSITION => settings.offset = 0,
            _ => return,
        }
        if settings == self.settings {
            return;
        }
        match settings.save(&self.path) {
            Ok(()) => self.apply(settings),
            Err(error) => {
                let language = self.settings.language;
                let message = language.text(
                    "Could not save NetBar settings.",
                    "تنظیمات نت‌بار ذخیره نشد.",
                );
                show_error(&format!("{message}\n{error}"), language);
            }
        }
    }

    fn widget_action(&mut self, action: usize, value: isize) {
        match action {
            widget_window::ACTION_SETTINGS => self.open_settings(),
            widget_window::ACTION_PAUSE => self.set_paused(true),
            widget_window::ACTION_RESUME => self.set_paused(false),
            widget_window::ACTION_HIDE
            | widget_window::ACTION_LOCK
            | widget_window::ACTION_RESET_POSITION => self.menu_action(action),
            widget_window::ACTION_EXIT => unsafe {
                PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
            },
            widget_window::ACTION_DRAG_BEGIN
                if !self.settings.locked && self.settings_window.is_none() =>
            {
                self.dragging = Some(Drag {
                    origin: value as i32,
                    offset: self.settings.offset,
                });
            }
            widget_window::ACTION_DRAG_MOVE => {
                if let Some(drag) = &self.dragging {
                    let distance = i64::from(value as i32) - i64::from(drag.origin);
                    let offset = (i64::from(drag.offset)
                        + distance * 96 / i64::from(self.widget.dpi().max(1)))
                    .clamp(-16000, 16000) as i32;
                    if offset != self.settings.offset {
                        self.settings.offset = offset;
                        self.refresh_widget(false);
                    }
                }
            }
            widget_window::ACTION_DRAG_END => self.finish_drag(true),
            widget_window::ACTION_DRAG_CANCEL => self.finish_drag(false),
            _ => {}
        }
    }

    fn finish_drag(&mut self, save: bool) {
        let Some(drag) = self.dragging.take() else {
            return;
        };
        if self.settings.offset == drag.offset {
            return;
        }
        if !save {
            self.settings.offset = drag.offset;
        } else if let Err(error) = self.settings.save(&self.path) {
            self.settings.offset = drag.offset;
            let language = self.settings.language;
            let message = language.text(
                "Could not save the taskbar position.",
                "موقعیت روی نوار وظیفه ذخیره نشد.",
            );
            show_error(&format!("{message}\n{error}"), language);
        }
        self.refresh_widget(true);
    }

    fn message(&mut self, message: u32, word: WPARAM, param: LPARAM) -> Option<LRESULT> {
        unsafe {
            if self.taskbar_created != 0 && message == self.taskbar_created {
                self.widget.taskbar_recreated();
                return Some(0);
            }
            match message {
                WM_TIMER => match word {
                    SAMPLE_TIMER if self.is_sampling() => self.sample(),
                    PLACEMENT_TIMER if !self.suspended => self.widget.refresh_placement(),
                    NETWORK_TIMER => {
                        KillTimer(self.hwnd, NETWORK_TIMER);
                        if self.is_sampling() {
                            self.refresh_network();
                            self.refresh_widget(false);
                        }
                    }
                    _ => {}
                },
                NETWORK_CHANGED if self.is_sampling() => timer(self.hwnd, NETWORK_TIMER, 200, 30),
                POSITION_CHANGED | WM_DISPLAYCHANGE | WM_DPICHANGED => {
                    self.widget.refresh_placement()
                }
                WM_SETTINGCHANGE | WM_THEMECHANGED => {
                    self.light_text = preferences::light_text(self.settings.theme);
                    self.refresh_widget(true);
                }
                WM_POWERBROADCAST => {
                    match word as u32 {
                        PBT_APMSUSPEND => {
                            self.suspended = true;
                            self.arm_sample();
                            self.refresh_widget(true);
                        }
                        PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_APMRESUMECRITICAL
                            if self.suspended =>
                        {
                            self.suspended = false;
                            if self.is_sampling() {
                                self.history.clear();
                                self.prime_network();
                            }
                            self.refresh_widget(true);
                            self.arm_sample();
                        }
                        _ => {}
                    }
                    return Some(1);
                }
                OPEN_SETTINGS => self.open_settings(),
                SETTINGS_CLOSED => {
                    if self.settings_window == Some(word) {
                        self.settings_window = None;
                        self.refresh_widget(false);
                    }
                }
                SETTINGS_RELOAD => self.apply(Settings::load(&self.path)),
                widget_window::WIDGET_ACTION => self.widget_action(word, param),
                WM_DESTROY => {
                    PostQuitMessage(0);
                }
                _ => return None,
            }
        }
        Some(0)
    }
}

fn show_error(message: &str, language: Language) {
    // A native error is also available when the system WebView cannot start.
    let direction = match language {
        Language::English => 0,
        Language::Persian => MB_RIGHT | MB_RTLREADING,
    };
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(message).as_ptr(),
            WINDOW_TITLE,
            MB_OK | MB_ICONERROR | direction,
        );
    }
}

unsafe fn timer(hwnd: HWND, id: usize, interval: u32, tolerance: u32) {
    unsafe {
        if SetCoalescableTimer(hwnd, id, interval, None, tolerance) == 0 {
            SetTimer(hwnd, id, interval, None);
        }
    }
}

unsafe extern "system" fn foreground_changed(
    _: HWINEVENTHOOK,
    _: u32,
    _: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    let hwnd = HOOK_TARGET.get();
    if !hwnd.is_null() {
        unsafe {
            PostMessageW(hwnd, POSITION_CHANGED, 0, 0);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    word: WPARAM,
    param: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_CLOSE {
            DestroyWindow(hwnd);
            return 0;
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<App>;
        if let Some(state) = state.as_ref() {
            if let Ok(mut app) = state.try_borrow_mut() {
                if let Some(result) = app.message(message, word, param) {
                    return result;
                }
            } else if message == WM_DESTROY {
                PostQuitMessage(0);
            }
        }
        DefWindowProcW(hwnd, message, word, param)
    }
}
