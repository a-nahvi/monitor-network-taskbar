#![windows_subsystem = "windows"]

mod application;
mod network;
mod preferences;
mod settings_window;
mod startup;
mod update;
mod widget;

use application::ApplicationTraffic;
use network::{Monitor, RateLabel, Rates};
use preferences::Settings;
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
use tauri::{
    AppHandle, RunEvent, Wry,
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
};
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
pub const SETTINGS_OPENED: u32 = WM_APP + 2;
pub const SETTINGS_CLOSED: u32 = WM_APP + 3;
pub const SETTINGS_RELOAD: u32 = WM_APP + 4;
const POSITION_CHANGED: u32 = WM_APP + 5;
const MENU_ACTION: u32 = WM_APP + 6;
const OPEN_SETTINGS: u32 = WM_APP + 7;
const SAMPLE_TIMER: usize = 1;
const PLACEMENT_TIMER: usize = 2;
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
        show_error(&error.to_string());
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
            .setup(move |app| Ok(settings_window::open(app.handle(), path, 0, true)?))
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
            unsafe {
                PostMessageW(FindWindowW(WINDOW_CLASS, WINDOW_TITLE), OPEN_SETTINGS, 0, 0);
            }
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
        let state = match App::new(tauri, hwnd, settings, path, custom) {
            Ok(state) => Box::new(RefCell::new(state)),
            Err(error) => {
                DestroyWindow(hwnd);
                return Err(error);
            }
        };
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
            let mut app = state.borrow_mut();
            app.refresh_network();
            app.arm_sample();
            app.refresh_widget(true);
            app.update_tray();
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

struct Tray {
    icon: Option<TrayIcon<Wry>>,
    menu: Menu<Wry>,
    visible: CheckMenuItem<Wry>,
    locked: CheckMenuItem<Wry>,
    reset: MenuItem<Wry>,
    tip: String,
}

const ACTION_SHOW: usize = 1;
const ACTION_LOCK: usize = 2;
const ACTION_RESET: usize = 3;

impl Tray {
    fn new(app: &AppHandle, control: HWND, settings: &Settings) -> tauri::Result<Self> {
        let preferences = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
        let visible = CheckMenuItem::with_id(
            app,
            "visible",
            "Show on taskbar",
            true,
            settings.visible,
            None::<&str>,
        )?;
        let locked = CheckMenuItem::with_id(
            app,
            "locked",
            "Lock position",
            true,
            settings.locked,
            None::<&str>,
        )?;
        let reset = MenuItem::with_id(
            app,
            "reset",
            "Reset position",
            settings.offset != 0,
            None::<&str>,
        )?;
        let quit = MenuItem::with_id(app, "quit", "Exit NetBar", true, None::<&str>)?;
        let menu = Menu::with_items(
            app,
            &[
                &preferences,
                &PredefinedMenuItem::separator(app)?,
                &visible,
                &locked,
                &reset,
                &PredefinedMenuItem::separator(app)?,
                &quit,
            ],
        )?;
        let destination = control as usize;
        app.on_menu_event(move |_, event| {
            let (message, action) = match event.id().as_ref() {
                "settings" => (OPEN_SETTINGS, 0),
                "visible" => (MENU_ACTION, ACTION_SHOW),
                "locked" => (MENU_ACTION, ACTION_LOCK),
                "reset" => (MENU_ACTION, ACTION_RESET),
                "quit" => (WM_CLOSE, 0),
                _ => return,
            };
            unsafe {
                PostMessageW(destination as HWND, message, action, 0);
            }
        });
        Ok(Self {
            icon: None,
            menu,
            visible,
            locked,
            reset,
            tip: String::new(),
        })
    }

    fn ensure_icon(&mut self, app: &AppHandle, control: HWND) {
        if self.icon.is_some() {
            return;
        }
        let destination = control as usize;
        let mut builder = TrayIconBuilder::with_id("netbar")
            .menu(&self.menu)
            .tooltip("NetBar")
            .show_menu_on_left_click(false)
            .on_tray_icon_event(move |_, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    unsafe {
                        PostMessageW(destination as HWND, OPEN_SETTINGS, 0, 0);
                    }
                }
            });
        if let Some(icon) = app.default_window_icon() {
            builder = builder.icon(icon.clone());
        }
        self.icon = builder.build(app).ok();
    }

    fn sync(&self, settings: &Settings, settings_open: bool) {
        let _ = self.visible.set_checked(settings.visible);
        let _ = self.locked.set_checked(settings.locked);
        let _ = self.visible.set_enabled(!settings_open);
        let _ = self.locked.set_enabled(!settings_open);
        let _ = self
            .reset
            .set_enabled(settings.offset != 0 && !settings_open);
    }

    fn set_tip(&mut self, text: String) {
        if text != self.tip
            && self
                .icon
                .as_ref()
                .is_some_and(|icon| icon.set_tooltip(Some(&text)).is_ok())
        {
            self.tip = text;
        }
    }
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
    settings_open: bool,
    tray: Tray,
    tray_updated: Instant,
    light_text: bool,
    widget: Widget,
    network: Monitor,
    last_network: Option<Instant>,
    rates: Rates,
    history: Vec<(f64, f64)>,
    app_traffic: ApplicationTraffic,
    hovering: bool,
    dragging: Option<Drag>,
    suspended: bool,
}

impl App {
    fn new(
        ui: AppHandle,
        hwnd: HWND,
        settings: Settings,
        path: PathBuf,
        custom: bool,
    ) -> io::Result<Self> {
        let light_text = preferences::light_text(settings.theme);
        let tray = Tray::new(&ui, hwnd, &settings).map_err(io::Error::other)?;
        let widget = Widget::new(ui.clone(), hwnd);
        Ok(Self {
            ui,
            hwnd,
            settings,
            path,
            custom,
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            settings_open: false,
            tray,
            tray_updated: Instant::now(),
            light_text,
            widget,
            network: Monitor::default(),
            last_network: None,
            rates: Rates::default(),
            history: Vec::new(),
            app_traffic: ApplicationTraffic::new(),
            hovering: false,
            dragging: None,
            suspended: false,
        })
    }

    fn refresh_network(&mut self) -> bool {
        self.last_network = Some(Instant::now());
        let adapters = network::adapters();
        let changed = self
            .network
            .select(network::select(&adapters, &self.settings.adapter));
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
        if self.hovering && !self.settings.app_usage {
            self.refresh_tooltip();
        }
        if self.tray_updated.elapsed() >= Duration::from_secs(5) {
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
                locked: s.locked || self.settings_open,
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
        if !self.widget.visible() {
            self.end_hover();
        }
    }

    fn refresh_placement(&mut self) {
        self.widget.refresh_placement(self.hwnd);
        if !self.widget.visible() {
            self.end_hover();
        }
    }

    fn arm_sample(&self) {
        unsafe {
            KillTimer(self.hwnd, SAMPLE_TIMER);
            timer(self.hwnd, SAMPLE_TIMER, self.settings.interval, 75);
        }
    }

    fn update_tray(&mut self) {
        self.tray_updated = Instant::now();
        self.tray.ensure_icon(&self.ui, self.hwnd);
        let tip = if self.rates.connected {
            let up = RateLabel::new(self.rates.up, self.settings.bits);
            let down = RateLabel::new(self.rates.down, self.settings.bits);
            format!("NetBar\n↑ {}    ↓ {}", up.as_str(), down.as_str())
        } else {
            "NetBar\nNetwork adapter disconnected".into()
        };
        self.tray.set_tip(tip);
    }

    fn open_settings(&mut self) {
        self.end_hover();
        if let Err(error) =
            settings_window::open(&self.ui, self.path.clone(), self.hwnd as usize, self.custom)
        {
            show_error(&format!("NetBar settings could not open. {error}"));
            return;
        }
        self.set_settings_open(true);
    }

    fn set_settings_open(&mut self, open: bool) {
        self.settings_open = open;
        self.tray.sync(&self.settings, open);
        self.refresh_widget(false);
    }

    fn apply(&mut self, settings: Settings) {
        if self.settings.interval != settings.interval
            || self.settings.graph != settings.graph
            || !settings.visible
        {
            self.history.clear();
        }
        let interval_changed = self.settings.interval != settings.interval;
        let adapter_changed = self.settings.adapter != settings.adapter;
        if self.settings.app_usage != settings.app_usage {
            self.end_hover();
        }
        self.settings = settings;
        self.light_text = preferences::light_text(self.settings.theme);
        if interval_changed {
            self.arm_sample();
        }
        if adapter_changed {
            self.refresh_network();
        }
        self.tray.sync(&self.settings, self.settings_open);
        self.refresh_widget(true);
        self.update_tray();
    }

    fn menu_action(&mut self, action: usize) {
        if self.settings_open {
            self.tray.sync(&self.settings, true);
            return;
        }
        let mut settings = self.settings.clone();
        match action {
            ACTION_SHOW => settings.visible = !settings.visible,
            ACTION_LOCK => settings.locked = !settings.locked,
            ACTION_RESET => settings.offset = 0,
            _ => return,
        }
        match settings.save(&self.path) {
            Ok(()) => self.apply(settings),
            Err(error) => {
                self.tray.sync(&self.settings, self.settings_open);
                show_error(&format!("Could not save NetBar settings. {error}"));
            }
        }
    }

    fn widget_action(&mut self, action: usize, value: isize) {
        match action {
            widget::ACTION_SETTINGS => self.open_settings(),
            widget::ACTION_CONTEXT => {
                self.end_hover();
                if let Some(window) = self.widget.window() {
                    let _ = window.popup_menu(&self.tray.menu);
                }
            }
            widget::ACTION_DRAG_BEGIN if !self.settings.locked && !self.settings_open => {
                self.end_hover();
                self.dragging = Some(Drag {
                    origin: value as i32,
                    offset: self.settings.offset,
                });
            }
            widget::ACTION_DRAG_MOVE => {
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
            widget::ACTION_DRAG_END => self.finish_drag(true),
            widget::ACTION_DRAG_CANCEL => self.finish_drag(false),
            widget::ACTION_HOVER_ENTER if self.dragging.is_none() => self.show_tooltip(),
            widget::ACTION_HOVER_LEAVE => self.end_hover(),
            _ => {}
        }
    }

    fn show_tooltip(&mut self) {
        if self.hovering || !self.widget.visible() {
            return;
        }
        self.hovering = true;
        if self.settings.app_usage {
            self.app_traffic.start();
            unsafe {
                timer(self.hwnd, TOOLTIP_TIMER, 1000, 100);
            }
        }
        self.refresh_tooltip();
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
        self.widget.update_tooltip(&text);
    }

    fn end_hover(&mut self) {
        if !self.hovering {
            return;
        }
        self.hovering = false;
        self.app_traffic.stop();
        unsafe {
            KillTimer(self.hwnd, TOOLTIP_TIMER);
        }
        self.widget.hide_tooltip();
    }

    fn finish_drag(&mut self, save: bool) {
        let Some(drag) = self.dragging.take() else {
            return;
        };
        if !save {
            self.settings.offset = drag.offset;
        } else if let Err(error) = self.settings.save(&self.path) {
            self.settings.offset = drag.offset;
            show_error(&format!("Could not save the taskbar position. {error}"));
        }
        self.tray.sync(&self.settings, self.settings_open);
        self.refresh_widget(true);
    }

    fn message(&mut self, message: u32, word: WPARAM, param: LPARAM) -> Option<LRESULT> {
        unsafe {
            if message == self.taskbar_created {
                self.refresh_placement();
                self.update_tray();
                return Some(0);
            }
            match message {
                WM_TIMER => match word {
                    SAMPLE_TIMER if !self.suspended => self.sample(),
                    PLACEMENT_TIMER if !self.suspended => self.refresh_placement(),
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
                NETWORK_CHANGED => timer(self.hwnd, NETWORK_TIMER, 200, 30),
                POSITION_CHANGED | WM_DISPLAYCHANGE | WM_DPICHANGED => self.refresh_placement(),
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
                SETTINGS_OPENED => self.set_settings_open(true),
                SETTINGS_CLOSED => self.set_settings_open(false),
                SETTINGS_RELOAD => self.apply(Settings::load(&self.path)),
                MENU_ACTION => self.menu_action(word),
                widget::WIDGET_ACTION => self.widget_action(word, param),
                WM_DESTROY => {
                    self.end_hover();
                    PostQuitMessage(0);
                }
                _ => return None,
            }
        }
        Some(0)
    }
}

fn show_error(message: &str) {
    // A native error is also available when the system WebView cannot start.
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(message).as_ptr(),
            WINDOW_TITLE,
            MB_OK | MB_ICONERROR,
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
