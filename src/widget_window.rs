use crate::preferences::Language;
use std::cell::Cell;
use std::io;
use std::ptr::null;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};

use tauri::window::WindowBuilder;
use tauri::{AppHandle, Window, Wry};
use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, SetLastError, WPARAM},
    Graphics::Gdi::{ClientToScreen, PtInRect, ValidateRect},
    UI::{
        Input::KeyboardAndMouse::{GetCapture, GetDoubleClickTime, ReleaseCapture, SetCapture},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

pub const WIDGET_ACTION: u32 = WM_APP + 8;
pub const ACTION_SETTINGS: usize = 1;
pub const ACTION_PAUSE: usize = 2;
pub const ACTION_DRAG_BEGIN: usize = 3;
pub const ACTION_DRAG_MOVE: usize = 4;
pub const ACTION_DRAG_END: usize = 5;
pub const ACTION_DRAG_CANCEL: usize = 6;
pub const ACTION_RESUME: usize = 7;
pub const ACTION_LOCK: usize = 8;
pub const ACTION_RESET_POSITION: usize = 9;
pub const ACTION_HIDE: usize = 10;
pub const ACTION_EXIT: usize = 11;
const SUBCLASS_ID: usize = 1;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Controls {
    pub language: Language,
    pub paused: bool,
    pub unlocked: bool,
    pub settings_open: bool,
    pub can_reset: bool,
}

impl Controls {
    fn draggable(self) -> bool {
        self.unlocked && !self.settings_open
    }
}

pub struct WidgetWindow {
    window: Window<Wry>,
    hwnd: usize,
    controls: Arc<Mutex<Controls>>,
}

impl WidgetWindow {
    pub fn new(
        app: &AppHandle,
        control: HWND,
        taskbar: HWND,
        controls: Controls,
    ) -> tauri::Result<Self> {
        let window = WindowBuilder::new(app, "widget")
            .title("NetBar widget")
            .window_classname("NetBar.Tauri.Widget.v2")
            // Windows keeps the popup above its owner when Explorer reorders the taskbar.
            .owner_raw(windows::Win32::Foundation::HWND(taskbar))
            .inner_size(1.0, 1.0)
            .decorations(false)
            .resizable(false)
            .focusable(false)
            .focused(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .shadow(false)
            .visible(false)
            .build()?;
        let hwnd = window.hwnd()?.0 as usize;
        let control = control as usize;
        let controls = Arc::new(Mutex::new(controls));
        let callback_controls = Arc::clone(&controls);
        let (done, ready) = mpsc::sync_channel(1);
        let result = app.run_on_main_thread(move || {
            let _ = done.send(install_subclass(hwnd as HWND, control, callback_controls));
        });
        let prepared = result.and_then(|()| {
            ready
                .recv()
                .map_err(|error| io::Error::other(error.to_string()))?
                .map_err(Into::into)
        });
        if let Err(error) = prepared {
            let _ = window.destroy();
            return Err(error);
        }
        Ok(Self {
            window,
            hwnd,
            controls,
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd as HWND
    }

    pub fn belongs_to(&self, taskbar: HWND) -> bool {
        unsafe {
            !taskbar.is_null()
                && IsWindow(self.hwnd()) != 0
                && GetWindow(self.hwnd(), GW_OWNER) == taskbar
        }
    }

    pub fn set_controls(&self, controls: Controls) {
        let cancel_drag = if let Ok(mut previous) = self.controls.lock() {
            let cancel = previous.draggable() && !controls.draggable();
            *previous = controls;
            cancel
        } else {
            false
        };
        if cancel_drag {
            // ReleaseCapture must run on the UI thread if a drag was in progress.
            unsafe {
                PostMessageW(self.hwnd(), WM_CANCELMODE, 0, 0);
            }
        }
    }
}

impl Drop for WidgetWindow {
    fn drop(&mut self) {
        let _ = self.window.destroy();
    }
}

#[derive(Clone, Copy)]
struct Click {
    point: POINT,
    time: u32,
}

impl Click {
    fn nearby(self, other: Self) -> bool {
        let close_in = |metric| unsafe { GetSystemMetrics(metric).max(2) as u32 / 2 };
        self.point.x.abs_diff(other.point.x) <= close_in(SM_CXDOUBLECLK)
            && self.point.y.abs_diff(other.point.y) <= close_in(SM_CYDOUBLECLK)
    }
}

struct Pointer {
    owner: usize,
    controls: Arc<Mutex<Controls>>,
    pressed: Cell<Option<Click>>,
    previous: Cell<Option<Click>>,
    drag_x: Cell<Option<i32>>,
    moved: Cell<bool>,
    right_pressed: Cell<bool>,
    menu_open: Cell<bool>,
}

impl Pointer {
    fn post(&self, action: usize, x: i32) {
        unsafe {
            PostMessageW(self.owner as HWND, WIDGET_ACTION, action, x as LPARAM);
        }
    }

    fn start(&self, hwnd: HWND) {
        if self.menu_open.get() || self.right_pressed.get() {
            return;
        }
        let Some(click) = cursor_click() else {
            return;
        };
        if self.pressed.get().is_some() {
            self.finish(hwnd, false);
        }
        self.pressed.set(Some(click));
        if self
            .controls
            .lock()
            .is_ok_and(|controls| controls.draggable())
        {
            unsafe {
                SetCapture(hwnd);
                if GetCapture() == hwnd {
                    self.drag_x.set(Some(click.point.x));
                    self.post(ACTION_DRAG_BEGIN, click.point.x);
                }
            }
        }
    }

    fn move_drag(&self) {
        if self.pressed.get().is_none() {
            return;
        }
        let Some(click) = cursor_click() else { return };
        if self
            .pressed
            .get()
            .is_some_and(|pressed| !pressed.nearby(click))
        {
            self.moved.set(true);
        }
        if self.drag_x.get().is_some_and(|last| last != click.point.x) {
            self.drag_x.set(Some(click.point.x));
            self.post(ACTION_DRAG_MOVE, click.point.x);
        }
    }

    fn finish(&self, hwnd: HWND, commit: bool) {
        if commit {
            self.move_drag();
        }
        let pressed = self.pressed.take();
        let moved = self.moved.replace(false);
        if self.drag_x.take().is_some() {
            self.post(
                if commit {
                    ACTION_DRAG_END
                } else {
                    ACTION_DRAG_CANCEL
                },
                0,
            );
            unsafe {
                if GetCapture() == hwnd {
                    ReleaseCapture();
                }
            }
        }
        if let Some(pressed) = pressed {
            let click =
                cursor_click().filter(|released| commit && !moved && pressed.nearby(*released));
            let previous = self.previous.take();
            if let Some(click) = click {
                if previous.is_some_and(|previous| {
                    click.time.wrapping_sub(previous.time) <= unsafe { GetDoubleClickTime() }
                        && click.nearby(previous)
                }) {
                    self.post(ACTION_SETTINGS, 0);
                } else {
                    self.previous.set(Some(click));
                }
            }
        }
    }

    fn show_menu(&self, hwnd: HWND, position: POINT) {
        if self.menu_open.replace(true) {
            return;
        }
        self.previous.set(None);
        let controls = self.controls.lock().map(|controls| *controls);
        if let Ok(controls) = controls
            && let Some(action) = show_popup(hwnd, position, controls)
        {
            self.post(action, 0);
        }
        self.menu_open.set(false);
    }
}

struct PopupMenu(HMENU);

impl Drop for PopupMenu {
    fn drop(&mut self) {
        unsafe { DestroyMenu(self.0) };
    }
}

fn show_popup(hwnd: HWND, point: POINT, controls: Controls) -> Option<usize> {
    let handle = unsafe { CreatePopupMenu() };
    if handle.is_null() {
        return None;
    }
    let menu = PopupMenu(handle);
    let language = controls.language;
    let position_flags = if controls.settings_open {
        MF_GRAYED
    } else {
        MF_ENABLED
    };
    let lock_flags = position_flags
        | if controls.unlocked {
            MF_UNCHECKED
        } else {
            MF_CHECKED
        };
    let reset_flags = if controls.can_reset {
        position_flags
    } else {
        MF_GRAYED
    };
    let pause = if controls.paused {
        (
            ACTION_RESUME,
            language.text("Resume readings", "ادامهٔ نمایش سرعت"),
            MF_ENABLED,
        )
    } else {
        (
            ACTION_PAUSE,
            language.text("Pause readings", "توقف نمایش سرعت"),
            MF_ENABLED,
        )
    };
    for entry in [
        Some((
            ACTION_SETTINGS,
            language.text("Settings…", "تنظیمات…"),
            MF_ENABLED,
        )),
        Some(pause),
        None,
        Some((
            ACTION_LOCK,
            language.text("Lock position", "قفل کردن موقعیت"),
            lock_flags,
        )),
        Some((
            ACTION_RESET_POSITION,
            language.text("Reset position", "بازنشانی موقعیت"),
            reset_flags,
        )),
        Some((
            ACTION_HIDE,
            language.text("Hide from taskbar", "پنهان کردن از نوار وظیفه"),
            position_flags,
        )),
        None,
        Some((
            ACTION_EXIT,
            language.text("Exit NetBar", "خروج از نت‌بار"),
            MF_ENABLED,
        )),
    ] {
        let added = unsafe {
            match entry {
                Some((action, label, flags)) => {
                    AppendMenuW(menu.0, flags, action, crate::wide(label).as_ptr())
                }
                None => AppendMenuW(menu.0, MF_SEPARATOR, 0, null()),
            }
        };
        if added == 0 {
            return None;
        }
    }

    let direction = match language {
        Language::English => TPM_LEFTALIGN,
        Language::Persian => TPM_LAYOUTRTL | TPM_RIGHTALIGN,
    };
    let selected = unsafe {
        SetForegroundWindow(hwnd);
        let result = TrackPopupMenuEx(
            menu.0,
            direction
                | TPM_BOTTOMALIGN
                | TPM_RIGHTBUTTON
                | TPM_NONOTIFY
                | TPM_RETURNCMD
                | TPM_NOANIMATION,
            point.x,
            point.y,
            hwnd,
            null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        result
    };
    (selected > 0).then_some(selected as usize)
}

fn mouse_point(param: LPARAM) -> POINT {
    POINT {
        x: i32::from(param as i16),
        y: i32::from((param >> 16) as i16),
    }
}

fn cursor_click() -> Option<Click> {
    let mut point = POINT::default();
    (unsafe { GetCursorPos(&mut point) } != 0).then(|| Click {
        point,
        time: unsafe { GetMessageTime() } as u32,
    })
}

fn install_subclass(hwnd: HWND, owner: usize, controls: Arc<Mutex<Controls>>) -> io::Result<()> {
    unsafe {
        // Tao gives windows a private DC. A layered window cannot use that class style.
        // The dedicated widget class keeps this change isolated from the settings window.
        let class_style = GetClassLongPtrW(hwnd, GCL_STYLE) as u32;
        let layered_class = class_style & !(CS_OWNDC | CS_CLASSDC);
        if layered_class != class_style {
            SetLastError(0);
            if SetClassLongPtrW(hwnd, GCL_STYLE, layered_class as isize) == 0 && GetLastError() != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let style =
            (style | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) & !WS_EX_APPWINDOW;
        SetLastError(0);
        if SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize) == 0 && GetLastError() != 0 {
            return Err(io::Error::last_os_error());
        }
        let pointer = Box::new(Rc::new(Pointer {
            owner,
            controls,
            pressed: Cell::new(None),
            previous: Cell::new(None),
            drag_x: Cell::new(None),
            moved: Cell::new(false),
            right_pressed: Cell::new(false),
            menu_open: Cell::new(false),
        }));
        let data = Box::into_raw(pointer);
        if SetWindowSubclass(hwnd, Some(widget_proc), SUBCLASS_ID, data as usize) == 0 {
            drop(Box::from_raw(data));
            return Err(io::Error::other(
                "Could not attach the NetBar widget input handler",
            ));
        }
    }
    Ok(())
}

unsafe extern "system" fn widget_proc(
    hwnd: HWND,
    message: u32,
    word: WPARAM,
    param: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    unsafe {
        // Capture changes can reenter this subclass before the first message returns.
        let data = data as *const Rc<Pointer>;
        let pointer = Rc::clone(&*data);
        match message {
            WM_ERASEBKGND => return 1,
            WM_PAINT => {
                ValidateRect(hwnd, null());
                return 0;
            }
            WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => pointer.start(hwnd),
            WM_MOUSEMOVE => pointer.move_drag(),
            WM_LBUTTONUP => pointer.finish(hwnd, true),
            WM_CANCELMODE | WM_CAPTURECHANGED => {
                let right_pressed = pointer.right_pressed.replace(false);
                pointer.finish(hwnd, false);
                if right_pressed && message == WM_CANCELMODE && GetCapture() == hwnd {
                    ReleaseCapture();
                }
            }
            WM_RBUTTONDOWN | WM_RBUTTONDBLCLK if !pointer.menu_open.get() => {
                pointer.finish(hwnd, false);
                pointer.right_pressed.set(true);
                pointer.previous.set(None);
                SetCapture(hwnd);
            }
            WM_RBUTTONUP if pointer.right_pressed.replace(false) => {
                if GetCapture() == hwnd {
                    ReleaseCapture();
                }
                let mut point = mouse_point(param);
                let mut client = RECT::default();
                if GetClientRect(hwnd, &mut client) != 0
                    && PtInRect(&client, point) != 0
                    && ClientToScreen(hwnd, &mut point) != 0
                {
                    pointer.show_menu(hwnd, point);
                }
            }
            WM_RBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => {}
            WM_CONTEXTMENU if word as HWND == hwnd => {
                let mut point = mouse_point(param);
                if param == -1 {
                    let mut bounds = RECT::default();
                    if GetWindowRect(hwnd, &mut bounds) == 0 {
                        return 0;
                    }
                    point = POINT {
                        x: bounds.left + (bounds.right - bounds.left) / 2,
                        y: bounds.top,
                    };
                }
                pointer.show_menu(hwnd, point);
            }
            WM_NCDESTROY => {
                if pointer.menu_open.get() {
                    EndMenu();
                }
                RemoveWindowSubclass(hwnd, Some(widget_proc), id);
                drop(Box::from_raw(data.cast_mut()));
                pointer.finish(hwnd, false);
                return DefSubclassProc(hwnd, message, word, param);
            }
            _ => return DefSubclassProc(hwnd, message, word, param),
        }
        0
    }
}
