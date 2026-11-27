#![windows_subsystem = "windows"]

mod application;
mod network;
mod preferences;
mod settings_window;
mod startup;
mod update;
mod widget;

use application::ApplicationTraffic;
use network::{Adapter, Monitor, RateLabel, Rates};
use preferences::Settings;
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    io,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver, Sender, SyncSender},
    time::{Duration, Instant},
};
use widget::{Presentation, Widget};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::ValidateRect,
        System::{LibraryLoader::GetModuleHandleW, Threading::*},
        UI::{
            Accessibility::*, Controls::*, Input::KeyboardAndMouse::*, Shell::*,
            WindowsAndMessaging::*,
        },
    },
    w,
};

pub const NETWORK_CHANGED: u32 = WM_APP + 1;
pub const SETTINGS_OPENED: u32 = WM_APP + 2;
pub const SETTINGS_CLOSED: u32 = WM_APP + 3;
const SETTINGS_SAVE: u32 = WM_APP + 4;
const POSITION_CHANGED: u32 = WM_APP + 5;
const TRAY: u32 = WM_APP + 6;
const OPEN_SETTINGS: u32 = WM_APP + 7;
const TRAY_KEY_SELECT: u32 = NIN_SELECT | NINF_KEY;
const SAMPLE_TIMER: usize = 1;
const PLACEMENT_TIMER: usize = 2;
const GRAPH_TIMER: usize = 3;
const TOOLTIP_TIMER: usize = 4;
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
        let mut opts = Self::default();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--quit") => opts.quit = true,
                Some("--settings") => opts.settings = true,
                Some("--inspect-ui") => opts.inspect_ui = true,
                Some("--config" | "--diagnose") => {
                    let path = args.next().ok_or_else(|| {
                        io::Error::other("Missing path for a command-line option.")
                    })?;
                    let path = std::path::absolute(path)?;
                    if arg == "--config" {
                        opts.config = Some(path);
                    } else {
                        opts.diagnose = Some(path);
                    }
                }
                _ => return Err(io::Error::other("Invalid command-line option.")),
            }
        }
        Ok(opts)
    }
}

fn main() {
    if let Err(error) = run() {
        unsafe {
            MessageBoxW(
                null_mut(),
                wide(&error.to_string()).as_ptr(),
                WINDOW_TITLE,
                MB_OK | MB_ICONERROR,
            );
        }
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let options = Options::parse(std::env::args_os().skip(1))?;
    if options.inspect_ui {
        let path = options.config.ok_or_else(|| {
            io::Error::other("The settings preview requires a separate --config path.")
        })?;
        return settings_window::preview(path);
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

    let mutex = Handle(unsafe { CreateMutexW(null(), 0, w!("Local\\NetBar.Monitor.v1")) });
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if mutex.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    if already_running {
        if options.settings {
            unsafe {
                PostMessageW(FindWindowW(WINDOW_CLASS, WINDOW_TITLE), OPEN_SETTINGS, 0, 0);
            }
        }
        return Ok(());
    }

    let custom = options.config.is_some();
    let path = match options.config {
        Some(path) => path,
        None => preferences::default_path()?,
    };
    let settings = Settings::load(&path);
    unsafe {
        let instance = GetModuleHandleW(null());
        let icon = LoadIconW(instance, 101usize as *const u16);
        let definition = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hIcon: icon,
            hIconSm: icon,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&definition) == 0 {
            return Err(io::Error::last_os_error());
        }
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
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

        let state = Box::new(RefCell::new(App::new(hwnd, settings, path, custom, icon)));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&raw const *state) as isize);
        HOOK_TARGET.set(hwnd);
        let foreground = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            null_mut(),
            Some(foreground),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        let notifications = network::Notifications::new(hwnd);
        {
            let mut app = state.borrow_mut();
            app.add_tray();
            app.refresh_network();
            app.arm_sample();
            app.refresh_widget(true);
            app.update_tray();
            timer(hwnd, PLACEMENT_TIMER, 2000, 150);
            if options.settings {
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

struct SaveRequest {
    settings: Settings,
    startup: Option<bool>,
    result: SyncSender<Result<(), String>>,
}
struct Drag {
    origin: i32,
    offset: i32,
}

struct App {
    hwnd: HWND,
    settings: Settings,
    path: PathBuf,
    custom: bool,
    icon: HICON,
    taskbar_created: u32,
    settings_window: Option<HWND>,
    save_tx: Sender<SaveRequest>,
    save_rx: Receiver<SaveRequest>,
    tray: bool,
    tray_tip: Vec<u16>,
    tray_updated: Instant,
    light_text: bool,
    widget: Widget,
    adapters: Vec<Adapter>,
    network: Monitor,
    last_network: Option<Instant>,
    rates: Rates,
    history: Vec<(f64, f64)>,
    app_traffic: ApplicationTraffic,
    tooltip: HWND,
    tooltip_text: [u16; 1536],
    hovering: bool,
    tracking: bool,
    dragging: Option<Drag>,
    menu: bool,
    suspended: bool,
    animating: bool,
}

impl App {
    fn new(hwnd: HWND, settings: Settings, path: PathBuf, custom: bool, icon: HICON) -> Self {
        let (save_tx, save_rx) = mpsc::channel();
        let light_text = preferences::light_text(settings.theme);
        Self {
            hwnd,
            settings,
            path,
            custom,
            icon,
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            settings_window: None,
            save_tx,
            save_rx,
            tray: false,
            tray_tip: Vec::new(),
            tray_updated: Instant::now(),
            light_text,
            widget: Widget::new(),
            adapters: Vec::new(),
            network: Monitor::default(),
            last_network: None,
            rates: Rates::default(),
            history: Vec::new(),
            app_traffic: ApplicationTraffic::new(),
            tooltip: null_mut(),
            tooltip_text: [0; 1536],
            hovering: false,
            tracking: false,
            dragging: None,
            menu: false,
            suspended: false,
            animating: false,
        }
    }

    fn refresh_network(&mut self) -> bool {
        self.last_network = Some(Instant::now());
        self.adapters = network::adapters();
        let changed = self
            .network
            .select(network::select(&self.adapters, &self.settings.adapter));
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
        if !self.tray || self.tray_updated.elapsed() >= Duration::from_secs(5) {
            self.update_tray();
        }
    }

    fn refresh_widget(&mut self, force: bool) {
        let s = &self.settings;
        let up = RateLabel::new(self.rates.up, s.bits);
        let down = RateLabel::new(self.rates.down, s.bits);
        fn color(rgb: u32) -> [u8; 3] {
            [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8]
        }
        self.widget.update(
            self.hwnd,
            Presentation {
                visible: s.visible && !self.suspended,
                graph: s.graph,
                graph_width: s.width,
                text_size: s.font,
                light_text: self.light_text,
                horizontal_offset: s.offset,
                sample_interval_ms: s.interval,
                upload: if self.rates.connected {
                    up.as_str()
                } else {
                    "--"
                },
                download: if self.rates.connected {
                    down.as_str()
                } else {
                    "--"
                },
                upload_color: s.custom_up.then(|| color(s.up_color)),
                download_color: s.custom_down.then(|| color(s.down_color)),
                connected: self.rates.connected,
                history: &self.history,
            },
            force,
        );
        self.visibility_changed();
    }

    fn visibility_changed(&mut self) {
        if !self.widget.visible() {
            self.end_hover();
        }
        let animation = self.widget.animating();
        if animation != self.animating {
            self.animating = animation;
            unsafe {
                if animation {
                    timer(self.hwnd, GRAPH_TIMER, 33, 4);
                } else {
                    KillTimer(self.hwnd, GRAPH_TIMER);
                }
            }
        }
    }

    fn arm_sample(&self) {
        unsafe {
            KillTimer(self.hwnd, SAMPLE_TIMER);
            timer(self.hwnd, SAMPLE_TIMER, self.settings.interval, 75);
        }
    }

    fn tray_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        }
    }

    fn add_tray(&mut self) {
        let mut data = self.tray_data();
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        data.hIcon = self.icon;
        data.uCallbackMessage = TRAY;
        write_nul(&mut data.szTip, "NetBar");
        self.tray = unsafe { Shell_NotifyIconW(NIM_ADD, &data) } != 0;
        if self.tray {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            unsafe {
                Shell_NotifyIconW(NIM_SETVERSION, &data);
            }
            self.tray_tip.clear();
        }
    }

    fn update_tray(&mut self) {
        self.tray_updated = Instant::now();
        if !self.tray {
            self.add_tray();
        }
        if !self.tray {
            return;
        }
        let (up, down) = (
            RateLabel::new(self.rates.up, self.settings.bits),
            RateLabel::new(self.rates.down, self.settings.bits),
        );
        let tip = if self.rates.connected {
            format!("NetBar\n↑ {}    ↓ {}", up.as_str(), down.as_str())
        } else {
            "NetBar\nNetwork adapter disconnected".into()
        };
        let tip = wide(&tip);
        if tip == self.tray_tip {
            return;
        }
        let mut data = self.tray_data();
        data.uFlags = NIF_TIP | NIF_SHOWTIP;
        data.szTip[..tip.len()].copy_from_slice(&tip);
        self.tray = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } != 0;
        self.tray_tip = tip;
    }

    fn open_settings(&mut self) {
        self.end_hover();
        if let Some(hwnd) = self.settings_window {
            unsafe {
                if !hwnd.is_null() {
                    SetForegroundWindow(hwnd);
                }
            }
            return;
        }
        self.refresh_network();
        let tx = self.save_tx.clone();
        let hwnd = self.hwnd as usize;
        let result = settings_window::open(
            self.hwnd,
            self.settings.clone(),
            self.adapters.clone(),
            self.custom,
            Box::new(move |settings, startup| {
                let (result, reply) = mpsc::sync_channel(1);
                tx.send(SaveRequest {
                    settings,
                    startup,
                    result,
                })
                .map_err(|_| "NetBar has closed.".to_owned())?;
                if unsafe { PostMessageW(hwnd as HWND, SETTINGS_SAVE, 0, 0) } == 0 {
                    return Err("NetBar has closed.".into());
                }
                reply
                    .recv()
                    .unwrap_or_else(|_| Err("NetBar has closed.".into()))
            }),
        );
        if result.is_ok() {
            self.settings_window = Some(null_mut());
        } else {
            self.show_error("The settings window could not open.");
        }
    }

    fn apply(&mut self, settings: Settings, startup: Option<bool>) -> Result<(), String> {
        let changing_startup = startup.filter(|desired| *desired != startup::enabled());
        if let Some(enabled) = changing_startup {
            startup::set_enabled(enabled)
                .map_err(|error| format!("Could not change Windows startup. {error}"))?;
        }
        if let Err(error) = settings.save(&self.path) {
            if let Some(enabled) = changing_startup {
                let _ = startup::set_enabled(!enabled);
            }
            return Err(format!("Could not save NetBar settings. {error}"));
        }
        if self.settings.interval != settings.interval
            || self.settings.graph != settings.graph
            || !settings.visible
        {
            self.history.clear();
        }
        let interval_changed = self.settings.interval != settings.interval;
        let adapter_changed = self.settings.adapter != settings.adapter;
        self.settings = settings;
        self.light_text = preferences::light_text(self.settings.theme);
        if interval_changed {
            self.arm_sample();
        }
        if adapter_changed {
            self.refresh_network();
        }
        self.refresh_widget(true);
        self.update_tray();
        Ok(())
    }

    fn show_error(&self, message: &str) {
        unsafe {
            MessageBoxW(
                self.hwnd,
                wide(message).as_ptr(),
                WINDOW_TITLE,
                MB_OK | MB_ICONERROR,
            );
        }
    }

    fn show_menu(&mut self, mut point: POINT) {
        if self.menu {
            return;
        }
        self.end_hover();
        self.menu = true;
        unsafe {
            if point.x == -1 && point.y == -1 {
                GetCursorPos(&mut point);
            }
            let menu = CreatePopupMenu();
            if menu.is_null() {
                self.menu = false;
                return;
            }
            let available = if self.settings_window.is_some() {
                MF_GRAYED
            } else {
                MF_ENABLED
            };
            AppendMenuW(menu, MF_STRING, 1, w!("Settings…"));
            SetMenuDefaultItem(menu, 1, 0);
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(
                menu,
                MF_STRING | available | if self.settings.visible { MF_CHECKED } else { 0 },
                2,
                w!("Show on taskbar"),
            );
            AppendMenuW(
                menu,
                MF_STRING | available | if self.settings.locked { MF_CHECKED } else { 0 },
                3,
                w!("Lock position"),
            );
            AppendMenuW(
                menu,
                MF_STRING
                    | available
                    | if self.settings.offset == 0 {
                        MF_GRAYED
                    } else {
                        0
                    },
                4,
                w!("Reset position"),
            );
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, 5, w!("Exit NetBar"));
            SetForegroundWindow(self.hwnd);
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                self.hwnd,
                null(),
            );
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            self.menu = false;
            match selected {
                1 => self.open_settings(),
                2..=4 => {
                    let mut settings = self.settings.clone();
                    match selected {
                        2 => settings.visible = !settings.visible,
                        3 => settings.locked = !settings.locked,
                        _ => settings.offset = 0,
                    }
                    if let Err(error) = self.apply(settings, None) {
                        self.show_error(&error);
                    }
                }
                5 => {
                    PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
                }
                _ => {}
            }
        }
    }

    fn track_hover(&mut self) {
        if self.tracking || self.menu {
            return;
        }
        let mut tracking = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_HOVER | TME_LEAVE,
            hwndTrack: self.hwnd,
            dwHoverTime: 450,
        };
        self.tracking = unsafe { TrackMouseEvent(&mut tracking) } != 0;
    }

    fn tool(&mut self) -> TTTOOLINFOW {
        TTTOOLINFOW {
            cbSize: size_of::<TTTOOLINFOW>() as u32,
            uFlags: TTF_IDISHWND | TTF_TRACK | TTF_ABSOLUTE,
            hwnd: self.hwnd,
            uId: self.hwnd as usize,
            lpszText: self.tooltip_text.as_mut_ptr(),
            ..Default::default()
        }
    }

    fn show_tooltip(&mut self) {
        if self.hovering || self.menu || !self.widget.visible() {
            return;
        }
        unsafe {
            if self.tooltip.is_null() {
                self.tooltip = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
                    TOOLTIPS_CLASSW,
                    null(),
                    WS_POPUP | TTS_NOPREFIX | TTS_ALWAYSTIP,
                    0,
                    0,
                    0,
                    0,
                    self.hwnd,
                    null_mut(),
                    GetModuleHandleW(null()),
                    null(),
                );
                if self.tooltip.is_null() {
                    return;
                }
                let tool = self.tool();
                if SendMessageW(self.tooltip, TTM_ADDTOOLW, 0, (&raw const tool) as isize) == 0 {
                    DestroyWindow(self.tooltip);
                    self.tooltip = null_mut();
                    return;
                }
            }
            SendMessageW(
                self.tooltip,
                TTM_SETMAXTIPWIDTH,
                0,
                (370 * self.widget.dpi() / 96) as isize,
            );
            self.hovering = true;
            if self.settings.app_usage {
                self.app_traffic.start();
                timer(self.hwnd, TOOLTIP_TIMER, 1000, 100);
            }
            self.refresh_tooltip();
            let mut rect = RECT::default();
            GetWindowRect(self.hwnd, &mut rect);
            let tool = self.tool();
            let size = SendMessageW(
                self.tooltip,
                TTM_GETBUBBLESIZE,
                0,
                (&raw const tool) as isize,
            );
            let height = ((size >> 16) & 0xffff) as i32;
            let position =
                (rect.left as u16 as u32) | (((rect.top - height - 4) as u16 as u32) << 16);
            SendMessageW(self.tooltip, TTM_TRACKPOSITION, 0, position as isize);
            SendMessageW(
                self.tooltip,
                TTM_TRACKACTIVATE,
                1,
                (&raw const tool) as isize,
            );
        }
    }

    fn refresh_tooltip(&mut self) {
        if !self.hovering {
            return;
        }
        let up = RateLabel::new(self.rates.up, self.settings.bits);
        let down = RateLabel::new(self.rates.down, self.settings.bits);
        let mut text = if self.rates.connected {
            format!("Upload {}   •   Download {}", up.as_str(), down.as_str())
        } else {
            "Network adapter disconnected".into()
        };
        if self.settings.app_usage {
            self.app_traffic.start();
            text.push_str("\n\n");
            text.push_str(&self.app_traffic.summary(self.settings.bits));
        }
        write_nul(&mut self.tooltip_text, &text);
        let tool = self.tool();
        unsafe {
            SendMessageW(
                self.tooltip,
                TTM_UPDATETIPTEXTW,
                0,
                (&raw const tool) as isize,
            );
        }
    }

    fn end_hover(&mut self) {
        if !self.hovering {
            return;
        }
        self.hovering = false;
        self.app_traffic.stop();
        unsafe {
            KillTimer(self.hwnd, TOOLTIP_TIMER);
            let tool = self.tool();
            SendMessageW(
                self.tooltip,
                TTM_TRACKACTIVATE,
                0,
                (&raw const tool) as isize,
            );
        }
    }

    fn finish_drag(&mut self, save: bool) {
        let Some(drag) = self.dragging.take() else {
            return;
        };
        unsafe {
            if GetCapture() == self.hwnd {
                ReleaseCapture();
            }
        }
        if !save {
            self.settings.offset = drag.offset;
        } else if let Err(error) = self.settings.save(&self.path) {
            self.settings.offset = drag.offset;
            self.show_error(&format!("Could not save the taskbar position. {error}"));
        }
        self.refresh_widget(true);
    }

    fn message(&mut self, message: u32, word: WPARAM, param: LPARAM) -> Option<LRESULT> {
        unsafe {
            if message == self.taskbar_created {
                self.tray = false;
                self.update_tray();
                self.widget.refresh_placement(self.hwnd);
                self.visibility_changed();
                return Some(0);
            }
            match message {
                WM_TIMER => match word {
                    SAMPLE_TIMER if !self.suspended => self.sample(),
                    PLACEMENT_TIMER if !self.suspended => {
                        self.widget.refresh_placement(self.hwnd);
                        self.visibility_changed();
                    }
                    GRAPH_TIMER => {
                        if !self.widget.animate(self.hwnd) {
                            KillTimer(self.hwnd, GRAPH_TIMER);
                            self.animating = false;
                        }
                    }
                    TOOLTIP_TIMER => self.refresh_tooltip(),
                    NETWORK_TIMER => {
                        KillTimer(self.hwnd, NETWORK_TIMER);
                        if !self.suspended {
                            self.refresh_network();
                            self.refresh_widget(false);
                        }
                    }
                    _ => {}
                },
                NETWORK_CHANGED => {
                    timer(self.hwnd, NETWORK_TIMER, 200, 30);
                }
                POSITION_CHANGED | WM_DISPLAYCHANGE | WM_DPICHANGED => {
                    self.widget.refresh_placement(self.hwnd);
                    self.visibility_changed();
                }
                WM_SETTINGCHANGE | WM_THEMECHANGED => {
                    self.light_text = preferences::light_text(self.settings.theme);
                    self.refresh_widget(true);
                }
                WM_POWERBROADCAST => {
                    match word as u32 {
                        PBT_APMSUSPEND => {
                            self.suspended = true;
                            self.end_hover();
                            KillTimer(self.hwnd, SAMPLE_TIMER);
                            self.refresh_widget(true);
                        }
                        PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_APMRESUMECRITICAL
                            if self.suspended =>
                        {
                            self.suspended = false;
                            self.network.reset();
                            self.history.clear();
                            if !self.refresh_network() {
                                self.rates = self.network.sample();
                            }
                            self.refresh_widget(true);
                            self.arm_sample();
                        }
                        _ => {}
                    }
                    return Some(1);
                }
                OPEN_SETTINGS => self.open_settings(),
                SETTINGS_OPENED => self.settings_window = Some(word as HWND),
                SETTINGS_CLOSED => self.settings_window = None,
                SETTINGS_SAVE => {
                    while let Ok(request) = self.save_rx.try_recv() {
                        let result = self.apply(request.settings, request.startup);
                        let _ = request.result.send(result);
                    }
                }
                TRAY => match (param & 0xffff) as u32 {
                    WM_CONTEXTMENU | WM_RBUTTONUP => self.show_menu(point(word as isize)),
                    WM_LBUTTONDBLCLK | NIN_SELECT | TRAY_KEY_SELECT => self.open_settings(),
                    _ => {}
                },
                WM_CONTEXTMENU => self.show_menu(point(param)),
                WM_LBUTTONDBLCLK => self.open_settings(),
                WM_MOUSEMOVE => {
                    if let Some(drag) = &self.dragging {
                        let mut cursor = POINT::default();
                        GetCursorPos(&mut cursor);
                        let offset = (drag.offset
                            + (cursor.x - drag.origin) * 96 / self.widget.dpi().max(1) as i32)
                            .clamp(-16000, 16000);
                        if offset != self.settings.offset {
                            self.settings.offset = offset;
                            self.refresh_widget(false);
                        }
                    } else {
                        self.track_hover();
                    }
                }
                WM_LBUTTONDOWN if !self.settings.locked && self.settings_window.is_none() => {
                    self.end_hover();
                    let mut cursor = POINT::default();
                    GetCursorPos(&mut cursor);
                    self.dragging = Some(Drag {
                        origin: cursor.x,
                        offset: self.settings.offset,
                    });
                    SetCapture(self.hwnd);
                }
                WM_LBUTTONUP => self.finish_drag(true),
                WM_CAPTURECHANGED | WM_CANCELMODE => self.finish_drag(false),
                WM_MOUSEHOVER if self.dragging.is_none() => self.show_tooltip(),
                WM_MOUSELEAVE => {
                    self.tracking = false;
                    self.end_hover();
                }
                WM_DESTROY => {
                    Shell_NotifyIconW(NIM_DELETE, &self.tray_data());
                    self.end_hover();
                    PostQuitMessage(0);
                }
                _ => return None,
            }
        }
        Some(0)
    }
}

fn write_nul(destination: &mut [u16], text: &str) {
    let mut offset = 0;
    let capacity = destination.len() - 1;
    for (output, value) in destination
        .iter_mut()
        .take(capacity)
        .zip(text.encode_utf16())
    {
        *output = value;
        offset += 1;
    }
    destination[offset] = 0;
}

fn point(packed: isize) -> POINT {
    POINT {
        x: packed as i16 as i32,
        y: (packed >> 16) as i16 as i32,
    }
}

unsafe fn timer(hwnd: HWND, id: usize, interval: u32, tolerance: u32) {
    unsafe {
        if SetCoalescableTimer(hwnd, id, interval, None, tolerance) == 0 {
            SetTimer(hwnd, id, interval, None);
        }
    }
}

unsafe extern "system" fn foreground(
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
        match message {
            WM_PAINT => {
                ValidateRect(hwnd, null());
                return 0;
            }
            WM_ERASEBKGND => return 1,
            WM_MOUSEACTIVATE => return MA_NOACTIVATE as isize,
            WM_CLOSE => {
                DestroyWindow(hwnd);
                return 0;
            }
            _ => {}
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
