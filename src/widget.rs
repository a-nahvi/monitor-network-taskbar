use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromWindow,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const WIDGET_LABEL: &str = "widget";
const READING_EVENT: &str = "netbar-widget-reading";
const PANEL_DPI: u32 = 96;
const SURFACE_RETRY: Duration = Duration::from_secs(30);

pub const WIDGET_ACTION: u32 = WM_APP + 8;
pub const ACTION_SETTINGS: WPARAM = 1;
pub const ACTION_CONTEXT: WPARAM = 2;
pub const ACTION_DRAG_BEGIN: WPARAM = 3;
pub const ACTION_DRAG_MOVE: WPARAM = 4;
pub const ACTION_DRAG_END: WPARAM = 5;
pub const ACTION_DRAG_CANCEL: WPARAM = 6;

type Color = [u8; 3];

pub struct Presentation<'a> {
    pub visible: bool,
    pub graph: bool,
    pub graph_width: i32,
    pub text_size: i32,
    pub light_text: bool,
    pub horizontal_offset: i32,
    pub sample_interval_ms: u32,
    pub upload: &'a str,
    pub download: &'a str,
    pub upload_color: Option<Color>,
    pub download_color: Option<Color>,
    pub connected: bool,
    pub locked: bool,
    pub history: &'a [(f64, f64)],
}

#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Reading {
    graph: bool,
    text_size: i32,
    locked: bool,
    connected: bool,
    interval: u32,
    upload: String,
    download: String,
    colors: [Color; 2],
    history: Vec<(f64, f64)>,
}

#[derive(Clone, Default, Serialize)]
pub struct WidgetSnapshot {
    revision: u64,
    reading: Option<Reading>,
}

pub struct WidgetChannel {
    control: usize,
    data: Arc<Mutex<WidgetSnapshot>>,
}

#[tauri::command]
pub fn widget_snapshot(
    window: WebviewWindow,
    state: State<'_, WidgetChannel>,
) -> Result<WidgetSnapshot, String> {
    if window.label() != WIDGET_LABEL {
        return Err("This window cannot access the taskbar display.".into());
    }
    Ok(lock(&state.data).clone())
}

#[tauri::command]
pub fn widget_action(
    window: WebviewWindow,
    state: State<'_, WidgetChannel>,
    action: String,
) -> Result<(), String> {
    if window.label() != WIDGET_LABEL {
        return Err("This window cannot control the taskbar display.".into());
    }
    let id = match action.as_str() {
        "settings" => ACTION_SETTINGS,
        "context" => ACTION_CONTEXT,
        "drag-begin" => ACTION_DRAG_BEGIN,
        "drag-move" => ACTION_DRAG_MOVE,
        "drag-end" => ACTION_DRAG_END,
        "drag-cancel" => ACTION_DRAG_CANCEL,
        _ => return Err("Unknown taskbar action.".into()),
    };
    let value = if matches!(id, ACTION_DRAG_BEGIN | ACTION_DRAG_MOVE) {
        let mut point = POINT::default();
        if unsafe { GetCursorPos(&mut point) } == 0 {
            return Err("The pointer location could not be read.".into());
        }
        point.x as LPARAM
    } else {
        0
    };
    if unsafe { PostMessageW(state.control as HWND, WIDGET_ACTION, id, value) } == 0 {
        return Err("The NetBar monitor has closed.".into());
    }
    Ok(())
}

pub struct Widget {
    app: AppHandle,
    data: Arc<Mutex<WidgetSnapshot>>,
    placement: Option<Placement>,
    taskbar: Taskbar,
    surface: Option<Surface>,
    next_build: Option<Instant>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Placement {
    visible: bool,
    width: i32,
    height: i32,
    offset: i32,
}

impl Widget {
    pub fn new(app: AppHandle, control: HWND) -> Self {
        let data = Arc::new(Mutex::new(WidgetSnapshot::default()));
        app.manage(WidgetChannel {
            control: control as usize,
            data: data.clone(),
        });
        Self {
            app,
            data,
            placement: None,
            taskbar: Taskbar::default(),
            surface: None,
            next_build: None,
        }
    }

    pub fn dpi(&self) -> u32 {
        self.taskbar.dpi()
    }

    pub fn window(&self) -> Option<&WebviewWindow> {
        self.surface
            .as_ref()
            .filter(|surface| surface.live())
            .map(|surface| &surface.window)
    }

    pub fn update(&mut self, control: HWND, presentation: Presentation<'_>, force: bool) {
        invalidate_surface(&mut self.surface, &mut self.next_build);
        let font = presentation.text_size.clamp(8, 20);
        let placement = Placement {
            visible: presentation.visible,
            width: if presentation.graph {
                presentation.graph_width.clamp(64, 200)
            } else {
                (font * 7 + 18).max(96)
            },
            height: ((font * 5 + 3) / 4 * 2 + 2).max(34),
            offset: presentation.horizontal_offset,
        };
        let changed = self.placement != Some(placement);
        self.placement = Some(placement);

        let reading = Reading {
            graph: presentation.graph,
            text_size: font,
            locked: presentation.locked,
            connected: presentation.connected,
            interval: presentation.sample_interval_ms,
            upload: presentation.upload.to_owned(),
            download: presentation.download.to_owned(),
            colors: palette(&presentation),
            history: if presentation.graph && presentation.connected {
                presentation.history.to_vec()
            } else {
                Vec::new()
            },
        };
        let event = {
            let mut data = lock(&self.data);
            if data.reading.as_ref() == Some(&reading) {
                None
            } else {
                data.revision += 1;
                data.reading = Some(reading);
                Some(data.clone())
            }
        };

        if !placement.visible {
            if let Some(surface) = self.surface.take() {
                surface.destroy();
            }
            return;
        }
        if changed || force || self.surface.is_none() {
            self.refresh_placement(control);
        }
        if let (Some(window), Some(payload)) = (self.window(), event) {
            let _ = window.emit_to(WIDGET_LABEL, READING_EVENT, payload);
        }
    }

    pub fn refresh_placement(&mut self, control: HWND) {
        invalidate_surface(&mut self.surface, &mut self.next_build);
        let Some(placement) = self.placement.filter(|placement| placement.visible) else {
            return;
        };
        let own = self
            .surface
            .as_ref()
            .map_or(control, |surface| surface.hwnd);
        if !self.taskbar.refresh(own) {
            self.hide();
            return;
        }
        let dpi = self.dpi();
        let size = SIZE {
            cx: scaled(placement.width, dpi),
            cy: scaled(placement.height, dpi),
        };
        let Some((x, y)) = self.taskbar.position(size, placement.offset) else {
            self.hide();
            return;
        };
        if self.surface.is_none() && due(self.next_build) {
            match Surface::build(&self.app, WIDGET_LABEL, "NetBar taskbar", "widget.html") {
                Ok(surface) => self.surface = Some(surface),
                Err(error) => {
                    eprintln!("Could not start the NetBar taskbar display: {error}");
                    self.next_build = Some(retry_after(&error));
                }
            }
        }
        if let Some(surface) = &mut self.surface {
            let raise = self.taskbar.above(surface.hwnd);
            surface.place(
                Bounds {
                    x,
                    y,
                    width: size.cx,
                    height: size.cy,
                },
                raise,
            );
        }
    }

    fn hide(&mut self) {
        if let Some(surface) = &mut self.surface {
            surface.hide();
        }
    }
}

impl Drop for Widget {
    fn drop(&mut self) {
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
    }
}

fn lock<T>(data: &Mutex<T>) -> MutexGuard<'_, T> {
    data.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn due(time: Option<Instant>) -> bool {
    time.is_none_or(|time| Instant::now() >= time)
}

fn invalidate_surface(surface: &mut Option<Surface>, retry: &mut Option<Instant>) {
    if surface.as_ref().is_some_and(|surface| !surface.live()) {
        *surface = None;
        *retry = None;
    }
}

fn retry_after(error: &tauri::Error) -> Instant {
    let delay = match error {
        tauri::Error::InvalidWindowHandle
        | tauri::Error::WindowLabelAlreadyExists(_)
        | tauri::Error::WebviewLabelAlreadyExists(_) => Duration::from_millis(300),
        _ => SURFACE_RETRY,
    };
    Instant::now() + delay
}

fn palette(presentation: &Presentation<'_>) -> [Color; 2] {
    let shade = match (presentation.light_text, presentation.connected) {
        (true, true) => 249,
        (true, false) => 167,
        (false, true) => 22,
        (false, false) => 97,
    };
    if !presentation.connected {
        return [[shade; 3]; 2];
    }
    let standard = match (presentation.graph, presentation.light_text) {
        (true, true) => [[253, 171, 93], [81, 207, 253]],
        (true, false) => [[157, 68, 13], [0, 99, 161]],
        (false, _) => [[shade; 3]; 2],
    };
    [
        presentation.upload_color.unwrap_or(standard[0]),
        presentation.download_color.unwrap_or(standard[1]),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Bounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

struct Surface {
    window: WebviewWindow,
    hwnd: HWND,
    shown: bool,
    bounds: Option<Bounds>,
}

impl Surface {
    fn build(app: &AppHandle, label: &str, title: &str, page: &str) -> tauri::Result<Self> {
        let window = if let Some(window) = app.get_webview_window(label) {
            window
        } else {
            WebviewWindowBuilder::new(app, label, WebviewUrl::App(page.into()))
                .title(title)
                .decorations(false)
                .shadow(false)
                .resizable(false)
                .minimizable(false)
                .maximizable(false)
                .transparent(true)
                .no_redirection_bitmap(true)
                .focusable(false)
                .focused(false)
                .skip_taskbar(true)
                .always_on_top(true)
                .visible(false)
                .devtools(false)
                .disable_drag_drop_handler()
                .inner_size(120.0, 34.0)
                .build()?
        };
        let hwnd = window.hwnd()?.0;
        if unsafe { IsWindow(hwnd) } == 0 {
            return Err(tauri::Error::InvalidWindowHandle);
        }
        Ok(Self {
            window,
            hwnd,
            shown: false,
            bounds: None,
        })
    }

    fn live(&self) -> bool {
        unsafe { IsWindow(self.hwnd) != 0 }
    }

    fn place(&mut self, bounds: Bounds, raise: bool) {
        if self.bounds == Some(bounds)
            && self.shown
            && !raise
            && unsafe { IsWindowVisible(self.hwnd) } != 0
        {
            return;
        }
        if unsafe {
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                bounds.x,
                bounds.y,
                bounds.width,
                bounds.height,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW | SWP_ASYNCWINDOWPOS,
            )
        } != 0
        {
            self.shown = true;
            self.bounds = Some(bounds);
        }
    }

    fn hide(&mut self) {
        if self.shown {
            unsafe { ShowWindowAsync(self.hwnd, SW_HIDE) };
            self.shown = false;
        }
    }

    fn destroy(mut self) {
        self.hide();
        let _ = self.window.destroy();
    }
}

#[derive(Default)]
struct Taskbar {
    window: HWND,
    tray: HWND,
    bounds: RECT,
    tray_left: Option<i32>,
    dpi: u32,
}

impl Taskbar {
    fn dpi(&self) -> u32 {
        if self.dpi == 0 { PANEL_DPI } else { self.dpi }
    }

    fn refresh(&mut self, own: HWND) -> bool {
        // Handles and rectangles are read on shell events and a bounded recovery timer.
        unsafe {
            if self.window.is_null() || IsWindow(self.window) == 0 {
                self.window = FindWindowW(w!("Shell_TrayWnd"), null());
                self.tray = null_mut();
            }
            if self.window.is_null()
                || IsWindowVisible(self.window) == 0
                || GetWindowRect(self.window, &mut self.bounds) == 0
            {
                return false;
            }

            let monitor = MonitorFromWindow(self.window, MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut info) == 0
                || !encloses(info.rcMonitor, self.bounds)
                || fullscreen(own, self.window, monitor, info.rcMonitor)
            {
                return false;
            }

            self.dpi = GetDpiForWindow(self.window);
            if self.tray.is_null() || IsChild(self.window, self.tray) == 0 {
                self.tray = null_mut();
                EnumChildWindows(
                    self.window,
                    Some(locate_tray),
                    (&raw mut self.tray) as LPARAM,
                );
            }
            let mut tray = RECT::default();
            self.tray_left = (!self.tray.is_null()
                && IsWindowVisible(self.tray) != 0
                && GetWindowRect(self.tray, &mut tray) != 0
                && tray.right > tray.left)
                .then_some(tray.left);
        }
        true
    }

    fn position(&self, size: SIZE, offset: i32) -> Option<(i32, i32)> {
        let width = self.bounds.right - self.bounds.left;
        let height = self.bounds.bottom - self.bounds.top;
        let dpi = self.dpi();
        let gap = scaled(10, dpi);
        if width <= height || width < size.cx + gap * 2 || height < size.cy {
            return None;
        }
        let anchor = self
            .tray_left
            .unwrap_or(self.bounds.right - scaled(220, dpi));
        let left = (anchor - size.cx - gap + scaled(offset.clamp(-32768, 32767), dpi))
            .clamp(self.bounds.left + gap, self.bounds.right - size.cx - gap);
        let top = self.bounds.top + (height - size.cy) / 2;
        Some((left, top))
    }

    fn above(&self, own: HWND) -> bool {
        unsafe {
            let mut above = GetWindow(own, GW_HWNDPREV);
            for _ in 0..64 {
                if above.is_null() {
                    break;
                }
                if above == self.window {
                    return true;
                }
                above = GetWindow(above, GW_HWNDPREV);
            }
        }
        false
    }
}

unsafe extern "system" fn locate_tray(window: HWND, out: LPARAM) -> i32 {
    let mut name = [0u16; 64];
    let length = unsafe { GetClassNameW(window, name.as_mut_ptr(), name.len() as i32) };
    if length == 13 && class_is(&name[..13], "TrayNotifyWnd") {
        // The callback is synchronous and `out` points to the still-live tray slot.
        unsafe { *(out as *mut HWND) = window };
        return 0;
    }
    1
}

fn class_is(class: &[u16], expected: &str) -> bool {
    class.iter().copied().eq(expected.encode_utf16())
}

fn fullscreen(own: HWND, taskbar: HWND, monitor: HMONITOR, monitor_bounds: RECT) -> bool {
    unsafe {
        let window = GetForegroundWindow();
        if window.is_null()
            || window == own
            || window == taskbar
            || IsIconic(window) != 0
            || IsWindowVisible(window) == 0
            || GetAncestor(window, GA_ROOTOWNER) == own
            || GetAncestor(window, GA_ROOT) == taskbar
            || MonitorFromWindow(window, MONITOR_DEFAULTTONULL) != monitor
        {
            return false;
        }
        if IsZoomed(window) != 0
            && GetWindowLongW(window, GWL_STYLE) as u32 & WS_CAPTION == WS_CAPTION
        {
            return false;
        }
        let mut bounds = RECT::default();
        if DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&raw mut bounds).cast(),
            size_of::<RECT>() as u32,
        ) < 0
            && GetWindowRect(window, &mut bounds) == 0
        {
            return false;
        }
        if !encloses(bounds, monitor_bounds) {
            return false;
        }
        let mut name = [0u16; 96];
        let length = GetClassNameW(window, name.as_mut_ptr(), name.len() as i32).max(0) as usize;
        let name = &name[..length];
        if ["Progman", "WorkerW"]
            .iter()
            .any(|class| class_is(name, class))
        {
            return false;
        }
        let mut shell_pid = 0;
        let mut foreground_pid = 0;
        GetWindowThreadProcessId(taskbar, &mut shell_pid);
        GetWindowThreadProcessId(window, &mut foreground_pid);
        shell_pid == 0 || foreground_pid != shell_pid
    }
}

fn encloses(outer: RECT, inner: RECT) -> bool {
    outer.left <= inner.left + 1
        && outer.top <= inner.top + 1
        && outer.right >= inner.right - 1
        && outer.bottom >= inner.bottom - 1
}

fn scaled(logical: i32, dpi: u32) -> i32 {
    (logical * dpi as i32 + 48 * logical.signum()) / 96
}
