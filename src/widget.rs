use crate::widget_window::{Controls, WidgetWindow};
use std::ffi::c_void;
use std::ptr::{NonNull, null, null_mut};
use std::time::{Duration, Instant};
use tauri::AppHandle;
use windows_sys::{
    Win32::{
        Foundation::{HWND, LPARAM, POINT, RECT, SIZE},
        Graphics::{
            Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
            Gdi::*,
        },
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
    w,
};

const PANEL_DPI: u32 = 96;
const GRAPH_POINTS: usize = 61;
const GRAPH_WINDOW_MS: u32 = 60_000;
const WINDOW_RETRY: Duration = Duration::from_secs(5);
const TEXT_LEFT: i32 = 20;
const UNIT_GAP: i32 = 3;
type Color = [u8; 3];
type Trace = [f32; 2];

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
    pub controls: Controls,
    pub history: &'a [(f64, f64)],
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Appearance {
    visible: bool,
    graph: bool,
    graph_width: i32,
    text_size: i32,
    light_text: bool,
    horizontal_offset: i32,
    colors: [Option<Color>; 2],
    connected: bool,
}

impl Appearance {
    fn size(self, dpi: u32) -> SIZE {
        let font = self.text_size;
        let width = if self.graph {
            self.graph_width
        } else {
            (font * 7 + 18).max(96)
        };
        let height = ((font * 5 + 3) / 4 * 2 + 2).max(34);
        SIZE {
            cx: scaled(width, dpi),
            cy: scaled(height, dpi),
        }
    }

    fn palette(self) -> [Color; 2] {
        let shade = match (self.light_text, self.connected) {
            (true, true) => 249,
            (true, false) => 167,
            (false, true) => 22,
            (false, false) => 97,
        };
        if !self.connected {
            return [[shade; 3]; 2];
        }
        let standard = match (self.graph, self.light_text) {
            (true, true) => [[253, 171, 93], [81, 207, 253]],
            (true, false) => [[157, 68, 13], [0, 99, 161]],
            (false, _) => [[shade; 3]; 2],
        };
        [
            self.colors[0].unwrap_or(standard[0]),
            self.colors[1].unwrap_or(standard[1]),
        ]
    }
}

pub struct Widget {
    app: AppHandle,
    control: HWND,
    window: Option<WidgetWindow>,
    next_window_attempt: Option<Instant>,
    appearance: Option<Appearance>,
    rates: [String; 2],
    graph: Graph,
    taskbar: Taskbar,
    surface: Option<Surface>,
    position: Option<(i32, i32)>,
    controls: Controls,
    dirty: bool,
}

impl Widget {
    pub fn new(app: AppHandle, control: HWND) -> Self {
        Self {
            app,
            control,
            window: None,
            next_window_attempt: None,
            appearance: None,
            rates: std::array::from_fn(|_| String::with_capacity(20)),
            graph: Graph::new(),
            taskbar: Taskbar::default(),
            surface: None,
            position: None,
            controls: Controls::default(),
            dirty: true,
        }
    }

    pub fn dpi(&self) -> u32 {
        self.taskbar.dpi()
    }

    pub fn update(&mut self, presentation: Presentation<'_>, force: bool) {
        let appearance = Appearance {
            visible: presentation.visible,
            graph: presentation.graph,
            graph_width: presentation.graph_width.clamp(64, 200),
            text_size: presentation.text_size.clamp(8, 20),
            light_text: presentation.light_text,
            horizontal_offset: presentation.horizontal_offset,
            colors: [presentation.upload_color, presentation.download_color],
            connected: presentation.connected,
        };
        let old = self.appearance.replace(appearance);
        let placement_changed = old.is_none_or(|old| {
            old.visible != appearance.visible
                || old.horizontal_offset != appearance.horizontal_offset
                || old.graph != appearance.graph
                || old.graph_width != appearance.graph_width
                || old.text_size != appearance.text_size
        });
        self.dirty |= old.is_none_or(|old| {
            old.graph != appearance.graph
                || old.light_text != appearance.light_text
                || old.colors != appearance.colors
                || old.connected != appearance.connected
        });

        if self.controls != presentation.controls {
            self.controls = presentation.controls;
            if let Some(window) = &self.window {
                window.set_controls(self.controls);
            }
        }

        if !appearance.visible {
            self.hide();
            self.surface = None;
            return;
        }
        if appearance.graph {
            let history = if appearance.connected {
                presentation.history
            } else {
                &[]
            };
            self.dirty |= self
                .graph
                .set_history(history, presentation.sample_interval_ms);
        } else {
            for (saved, next) in self
                .rates
                .iter_mut()
                .zip([presentation.upload, presentation.download])
            {
                if saved != next {
                    saved.clear();
                    saved.push_str(next);
                    self.dirty = true;
                }
            }
        }
        if placement_changed || force || self.window.is_none() {
            self.refresh_placement();
        } else if let (Some(window), Some(position)) = (&self.window, self.position) {
            self.present(window.hwnd(), position);
        }
    }

    pub fn refresh_placement(&mut self) {
        let Some(appearance) = self.appearance.filter(|appearance| appearance.visible) else {
            return;
        };
        let own = self
            .window
            .as_ref()
            .map_or(self.control, WidgetWindow::hwnd);
        let taskbar_visible = self.taskbar.refresh(own);
        if self
            .window
            .as_ref()
            .is_some_and(|window| !window.belongs_to(self.taskbar.window))
        {
            // Explorer may destroy its owned window when it replaces the taskbar.
            self.window = None;
            self.position = None;
            self.next_window_attempt = None;
        }
        if !taskbar_visible {
            self.hide();
            return;
        }
        let dpi = self.dpi();
        let size = appearance.size(dpi);
        let Some(position) = self.taskbar.position(size, appearance.horizontal_offset) else {
            self.hide();
            return;
        };
        if self.window.is_none()
            && self
                .next_window_attempt
                .is_none_or(|time| Instant::now() >= time)
        {
            match WidgetWindow::new(&self.app, self.control, self.taskbar.window, self.controls) {
                Ok(window) => {
                    self.window = Some(window);
                    self.dirty = true;
                }
                Err(error) => {
                    eprintln!("Could not create the NetBar taskbar window: {error}");
                    self.next_window_attempt = Some(Instant::now() + WINDOW_RETRY);
                }
            }
        }
        let Some(hwnd) = self.window.as_ref().map(WidgetWindow::hwnd) else {
            return;
        };
        let font = if appearance.graph {
            0
        } else {
            scaled(appearance.text_size, dpi)
        };
        if self.surface.as_ref().is_none_or(|surface| {
            surface.width != size.cx || surface.height != size.cy || surface.font_size != font
        }) {
            // Keep the previous composed frame on screen until Windows accepts a replacement.
            let Some(surface) = Surface::create(size, font) else {
                return;
            };
            self.surface = Some(surface);
            self.dirty = true;
        }

        if self.dirty {
            self.present(hwnd, position);
        } else if self.position != Some(position)
            && unsafe {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    position.0,
                    position.1,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                )
            } != 0
        {
            self.position = Some(position);
        }
        if self.position == Some(position) && unsafe { IsWindowVisible(hwnd) } == 0 {
            unsafe {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
    }

    pub fn taskbar_recreated(&mut self) {
        self.taskbar = Taskbar::default();
        self.next_window_attempt = None;
        self.refresh_placement();
    }

    fn hide(&mut self) {
        if let Some(window) = &self.window
            && unsafe { IsWindowVisible(window.hwnd()) } != 0
        {
            unsafe {
                ShowWindow(window.hwnd(), SW_HIDE);
            }
        }
        self.position = None;
    }

    fn present(&mut self, hwnd: HWND, position: (i32, i32)) {
        if !self.dirty {
            return;
        }
        let dpi = self.dpi();
        let (Some(appearance), Some(surface)) = (self.appearance, &mut self.surface) else {
            return;
        };
        let traces = appearance
            .graph
            .then_some(&self.graph.samples[..self.graph.slots]);
        surface.paint(appearance, &self.rates, traces, dpi);
        if surface.submit(hwnd, position.0, position.1) {
            self.dirty = false;
            self.position = Some(position);
        }
    }
}

struct Graph {
    inputs: [(f64, f64); GRAPH_POINTS],
    samples: [Trace; GRAPH_POINTS],
    len: usize,
    slots: usize,
    scale: f64,
}

impl Graph {
    fn new() -> Self {
        Self {
            inputs: [(0.0, 0.0); GRAPH_POINTS],
            samples: [[0.0; 2]; GRAPH_POINTS],
            len: 0,
            slots: 0,
            scale: 1.0,
        }
    }

    fn set_history(&mut self, history: &[(f64, f64)], interval_ms: u32) -> bool {
        let slots = (GRAPH_WINDOW_MS / interval_ms.clamp(1000, 5000) + 1) as usize;
        let history = &history[history.len().saturating_sub(slots)..];
        if self.slots == slots && self.inputs[..self.len] == *history {
            return false;
        }
        self.inputs[..history.len()].copy_from_slice(history);
        self.len = history.len();
        let mut changed = self.slots != slots;
        self.slots = slots;
        let maximum = history.iter().fold(1.0_f64, |peak, &(upload, download)| {
            peak.max(upload).max(download)
        });
        if maximum > self.scale || maximum < self.scale * 0.55 {
            self.scale = maximum * 1.14;
        }
        let padding = slots - history.len();
        for (index, value) in self.samples[..slots].iter_mut().enumerate() {
            let next = if index < padding {
                [0.0; 2]
            } else {
                let (upload, download) = history[index - padding];
                [
                    (upload / self.scale).clamp(0.0, 1.0) as f32,
                    (download / self.scale).clamp(0.0, 1.0) as f32,
                ]
            };
            changed |= *value != next;
            *value = next;
        }
        changed
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

const TEXT_MASK_SCALE: i32 = 2;
const HIT_TEST_PIXEL: u32 = 0x0100_0000;

struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    pixels: NonNull<u32>,
    len: usize,
}

impl Dib {
    fn create(size: SIZE) -> Option<Self> {
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() {
                return None;
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size.cx,
                    biHeight: -size.cy,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = null_mut::<c_void>();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut pixels, null_mut(), 0);
            let pixels = NonNull::new(pixels.cast()).filter(|_| !bitmap.is_null());
            let Some(pixels) = pixels else {
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                DeleteDC(dc);
                return None;
            };
            if SelectObject(dc, bitmap).is_null() {
                DeleteObject(bitmap);
                DeleteDC(dc);
                return None;
            }
            Some(Self {
                dc,
                bitmap,
                pixels,
                len: size.cx as usize * size.cy as usize,
            })
        }
    }

    fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.pixels.as_ptr(), self.len) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            DeleteDC(self.dc);
            DeleteObject(self.bitmap);
        }
    }
}

struct TextMask {
    dib: Dib,
    font: HFONT,
    previous_font: HGDIOBJ,
    size: SIZE,
    value_width: i32,
}

impl TextMask {
    fn create(size: SIZE, font_size: i32) -> Option<Self> {
        let size = SIZE {
            cx: size.cx * TEXT_MASK_SCALE,
            cy: size.cy * TEXT_MASK_SCALE,
        };
        let font_size = font_size * TEXT_MASK_SCALE;
        let dib = Dib::create(size)?;
        let dc = dib.dc;
        unsafe {
            let font = CreateFontW(
                -font_size,
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                0,
                0,
                ANTIALIASED_QUALITY as u32,
                0,
                w!("Segoe UI"),
            );
            if font.is_null() {
                return None;
            }
            let previous_font = SelectObject(dc, font);
            if previous_font.is_null() {
                DeleteObject(font);
                return None;
            }
            let mut value_size = SIZE::default();
            if GetTextExtentPoint32W(dc, w!("0000"), 4, &mut value_size) == 0 {
                value_size.cx = font_size * 3;
            }
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00FF_FFFF);
            Some(Self {
                dib,
                font,
                previous_font,
                size,
                value_width: value_size.cx,
            })
        }
    }

    fn draw(&mut self, rates: &[String; 2], dpi: u32) -> &[u32] {
        self.dib.pixels().fill(0);
        let half = self.size.cy / TEXT_MASK_SCALE / 2 * TEXT_MASK_SCALE;
        let value_left = scaled(TEXT_LEFT, dpi) * TEXT_MASK_SCALE;
        let unit_left = value_left + self.value_width + scaled(UNIT_GAP, dpi) * TEXT_MASK_SCALE;
        for (row, value) in rates.iter().enumerate() {
            let (number, unit) = value.rsplit_once(' ').unwrap_or((value, ""));
            let bounds = RECT {
                left: value_left,
                top: half * row as i32,
                right: unit_left,
                bottom: if row == 0 { half } else { self.size.cy },
            };
            self.draw_field(number, bounds);
            if !unit.is_empty() {
                self.draw_field(
                    unit,
                    RECT {
                        left: unit_left,
                        right: self.size.cx - scaled(2, dpi) * TEXT_MASK_SCALE,
                        ..bounds
                    },
                );
            }
        }
        unsafe { GdiFlush() };
        self.dib.pixels()
    }

    fn draw_field(&self, text: &str, mut bounds: RECT) {
        let mut utf16 = [0u16; 32];
        let mut length = 0;
        for (next, slot) in text.encode_utf16().zip(&mut utf16) {
            *slot = next;
            length += 1;
        }
        unsafe {
            DrawTextW(
                self.dib.dc,
                utf16.as_ptr(),
                length,
                &mut bounds,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }
    }
}

impl Drop for TextMask {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dib.dc, self.previous_font);
            DeleteObject(self.font);
        }
    }
}

struct Surface {
    frame: Dib,
    text: Option<TextMask>,
    width: i32,
    height: i32,
    font_size: i32,
}

impl Surface {
    fn create(size: SIZE, font_size: i32) -> Option<Self> {
        Some(Self {
            frame: Dib::create(size)?,
            text: if font_size > 0 {
                Some(TextMask::create(size, font_size)?)
            } else {
                None
            },
            width: size.cx,
            height: size.cy,
            font_size,
        })
    }

    fn paint(
        &mut self,
        appearance: Appearance,
        rates: &[String; 2],
        traces: Option<&[Trace]>,
        dpi: u32,
    ) {
        let palette = appearance.palette();
        let width = self.width as usize;
        let pixels = self.frame.pixels();
        if let Some(text) = &mut self.text {
            // GDI can switch tiny glyphs to monochrome. Average the 2× grayscale mask
            // into physical pixels so edge coverage survives without subpixel colors.
            let mask = text.draw(rates, dpi);
            let stride = width * TEXT_MASK_SCALE as usize;
            for (y, (row, samples)) in pixels
                .chunks_exact_mut(width)
                .zip(mask.chunks_exact(stride * TEXT_MASK_SCALE as usize))
                .enumerate()
            {
                let color = palette[usize::from(y as i32 >= self.height / 2)];
                let (top, bottom) = samples.split_at(stride);
                let (top, _) = top.as_chunks::<2>();
                let (bottom, _) = bottom.as_chunks::<2>();
                let samples = top.iter().zip(bottom);
                for (pixel, (a, b)) in row.iter_mut().zip(samples) {
                    let coverage = u16::from(a[0] as u8)
                        + u16::from(a[1] as u8)
                        + u16::from(b[0] as u8)
                        + u16::from(b[1] as u8);
                    let alpha = ((coverage + 2) / 4) as u8;
                    *pixel = if alpha == 0 {
                        HIT_TEST_PIXEL
                    } else {
                        premultiplied(color, alpha)
                    };
                }
            }
        } else {
            pixels.fill(HIT_TEST_PIXEL);
        }

        let mut painter = Painter {
            pixels,
            width: self.width,
            height: self.height,
        };
        let factor = dpi as f32 / PANEL_DPI as f32;
        if let Some(traces) = traces {
            let left = 4.0 * factor;
            let top = 4.0 * factor;
            let right = self.width as f32 - 4.0 * factor;
            let bottom = self.height as f32 - 4.0 * factor;
            let guide = if appearance.light_text {
                [255; 3]
            } else {
                [20; 3]
            };
            painter.line(
                (left, bottom + factor),
                (right, bottom + factor),
                0.5 * factor,
                guide,
                0.16,
            );
            if appearance.connected && traces.len() >= 2 {
                for (direction, color) in palette.into_iter().enumerate() {
                    let mut last = (left, bottom - (bottom - top) * traces[0][direction]);
                    for (column, point) in traces.iter().enumerate().skip(1) {
                        let x = left + (right - left) * column as f32 / (traces.len() - 1) as f32;
                        let next = (x, bottom - (bottom - top) * point[direction]);
                        painter.line(last, next, 0.72 * factor, color, 0.85);
                        last = next;
                    }
                    painter.line(last, last, 1.3 * factor, color, 1.0);
                }
            }
        } else {
            // Draw the two Feather icons with the same 24-unit geometry as the local SVG sprite.
            let scale = 13.0 * factor / 24.0;
            let x = 5.0 * factor + 12.0 * scale;
            for (direction, color) in palette.into_iter().enumerate() {
                let center = self.height as f32 * (0.25 + direction as f32 * 0.5);
                let offset = if direction == 0 {
                    -7.0 * scale
                } else {
                    7.0 * scale
                };
                let tip = (x, center + offset);
                let width = scale;
                painter.line(tip, (x, center - offset), width, color, 1.0);
                painter.line(tip, (x - 7.0 * scale, center), width, color, 1.0);
                painter.line(tip, (x + 7.0 * scale, center), width, color, 1.0);
            }
        }
    }

    fn submit(&self, hwnd: HWND, x: i32, y: i32) -> bool {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
            ..Default::default()
        };
        unsafe {
            UpdateLayeredWindow(
                hwnd,
                null_mut(),
                &POINT { x, y },
                &SIZE {
                    cx: self.width,
                    cy: self.height,
                },
                self.frame.dc,
                &POINT::default(),
                0,
                &blend,
                ULW_ALPHA,
            ) != 0
        }
    }
}

struct Painter<'a> {
    pixels: &'a mut [u32],
    width: i32,
    height: i32,
}

impl Painter<'_> {
    fn line(&mut self, from: (f32, f32), to: (f32, f32), radius: f32, color: Color, opacity: f32) {
        let fringe = radius + 0.6;
        let left = ((from.0.min(to.0) - fringe).floor() as i32).max(0);
        let right = ((from.0.max(to.0) + fringe).ceil() as i32).min(self.width - 1);
        let top = ((from.1.min(to.1) - fringe).floor() as i32).max(0);
        let bottom = ((from.1.max(to.1) + fringe).ceil() as i32).min(self.height - 1);
        let dx = to.0 - from.0;
        let dy = to.1 - from.1;
        let length_squared = dx * dx + dy * dy;
        for y in top..=bottom {
            for x in left..=right {
                let px = x as f32 + 0.5 - from.0;
                let py = y as f32 + 0.5 - from.1;
                let t = if length_squared > 0.0 {
                    ((px * dx + py * dy) / length_squared).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let ex = px - t * dx;
                let ey = py - t * dy;
                let distance = (ex * ex + ey * ey).sqrt();
                let coverage = (fringe - distance).clamp(0.0, 1.0);
                let alpha = (coverage * opacity * 255.0) as u8;
                if alpha > 0 {
                    let pixel = &mut self.pixels[(y * self.width + x) as usize];
                    *pixel = over(*pixel, color, alpha);
                }
            }
        }
    }
}

fn premultiplied(color: Color, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    let [red, green, blue] = color.map(|component| (u32::from(component) * alpha + 127) / 255);
    u32::from_be_bytes([alpha as u8, red as u8, green as u8, blue as u8])
}

fn over(background: u32, color: Color, alpha: u8) -> u32 {
    let foreground = premultiplied(color, alpha).to_be_bytes();
    let remaining = 255 - u32::from(alpha);
    let background = background.to_be_bytes();
    u32::from_be_bytes(std::array::from_fn(|channel| {
        foreground[channel] + ((u32::from(background[channel]) * remaining + 127) / 255) as u8
    }))
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn number_and_unit_edges_stay_smooth_and_aligned_at_taskbar_dpi() {
        let rates = ["7.6 KB/s".to_owned(), "58.4 KB/s".to_owned()];
        for dpi in [96, 120, 144, 192] {
            for light_text in [false, true] {
                let appearance = Appearance {
                    visible: true,
                    graph: false,
                    graph_width: 88,
                    text_size: 11,
                    light_text,
                    horizontal_offset: 0,
                    colors: [None; 2],
                    connected: true,
                };
                let mut surface = Surface::create(appearance.size(dpi), scaled(11, dpi)).unwrap();
                surface.paint(appearance, &rates, None, dpi);
                let width = surface.width as usize;
                let half = surface.height as usize / 2;
                let value_column = scaled(TEXT_LEFT, dpi) as usize;
                let unit_column = value_column
                    + (surface.text.as_ref().unwrap().value_width / TEXT_MASK_SCALE) as usize
                    + scaled(UNIT_GAP, dpi) as usize;
                let pixels = surface.frame.pixels();
                assert_eq!(pixels[0], HIT_TEST_PIXEL);

                let first_drawn = |row: &[u32], x: usize| {
                    row.chunks_exact(width)
                        .filter_map(|line| {
                            line[x..].iter().position(|&pixel| pixel != HIT_TEST_PIXEL)
                        })
                        .min()
                };
                let (upload, download) = pixels.split_at(half * width);
                assert!(first_drawn(upload, unit_column).is_some(), "DPI {dpi}");
                assert_eq!(
                    first_drawn(upload, unit_column),
                    first_drawn(download, unit_column),
                    "DPI {dpi}"
                );
                for row in [upload, download] {
                    for field in [value_column..unit_column, unit_column..width] {
                        let mut smoothed = false;
                        for line in row.chunks_exact(width) {
                            for pixel in &line[field.clone()] {
                                let [alpha, red, green, blue] = pixel.to_be_bytes();
                                smoothed |= alpha > 1 && alpha < 255;
                                assert!(red == green && green == blue && blue <= alpha);
                            }
                        }
                        assert!(
                            smoothed,
                            "DPI {dpi}, light text {light_text}, field {field:?}"
                        );
                    }
                }
            }
        }
    }
}
