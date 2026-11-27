use std::cell::Cell;
use std::io;
use std::ptr::null;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use tauri::window::WindowBuilder;
use tauri::{AppHandle, Window, Wry};
use windows_sys::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, SetLastError, WPARAM},
    Graphics::Gdi::ValidateRect,
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
const SUBCLASS_ID: usize = 1;

pub struct WidgetWindow {
    window: Window<Wry>,
    hwnd: usize,
    draggable: Arc<AtomicBool>,
}

impl WidgetWindow {
    pub fn new(app: &AppHandle, owner: HWND) -> tauri::Result<Self> {
        let window = WindowBuilder::new(app, "widget")
            .title("NetBar widget")
            .window_classname("NetBar.Tauri.Widget.v2")
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
        let owner = owner as usize;
        let draggable = Arc::new(AtomicBool::new(false));
        let callback_draggable = Arc::clone(&draggable);
        let (done, ready) = mpsc::sync_channel(1);
        let result = app.run_on_main_thread(move || {
            let _ = done.send(install_subclass(hwnd as HWND, owner, callback_draggable));
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
            draggable,
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd as HWND
    }

    pub fn set_draggable(&self, draggable: bool) {
        if self.draggable.swap(draggable, Ordering::Relaxed) && !draggable {
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
    draggable: Arc<AtomicBool>,
    native_double_clicks: bool,
    pressed: Cell<Option<Click>>,
    previous: Cell<Option<Click>>,
    drag_x: Cell<Option<i32>>,
}

impl Pointer {
    fn post(&self, action: usize, x: i32) {
        unsafe {
            PostMessageW(self.owner as HWND, WIDGET_ACTION, action, x as LPARAM);
        }
    }

    fn start(&self, hwnd: HWND) {
        let Some(click) = cursor_click() else {
            return;
        };
        if !self.native_double_clicks {
            let previous = self.previous.take();
            if previous.is_some_and(|prior| {
                click.time.wrapping_sub(prior.time) <= unsafe { GetDoubleClickTime() }
                    && click.nearby(prior)
            }) {
                self.pressed.set(None);
                self.post(ACTION_SETTINGS, 0);
                return;
            }
            self.pressed.set(Some(click));
        }
        if self.draggable.load(Ordering::Relaxed) {
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
        if let (Some(last), Some(click)) = (self.drag_x.get(), cursor_click())
            && last != click.point.x
        {
            self.drag_x.set(Some(click.point.x));
            self.post(ACTION_DRAG_MOVE, click.point.x);
        }
    }

    fn finish(&self, hwnd: HWND, commit: bool) {
        if commit {
            self.move_drag();
        }
        if let Some(pressed) = self.pressed.take() {
            self.previous.set(
                cursor_click()
                    .filter(|released| commit && pressed.nearby(*released))
                    .map(|_| pressed),
            );
        }
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
    }
}

fn cursor_click() -> Option<Click> {
    let mut point = POINT::default();
    (unsafe { GetCursorPos(&mut point) } != 0).then(|| Click {
        point,
        time: unsafe { GetMessageTime() } as u32,
    })
}

fn install_subclass(hwnd: HWND, owner: usize, draggable: Arc<AtomicBool>) -> io::Result<()> {
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
            draggable,
            native_double_clicks: GetClassLongPtrW(hwnd, GCL_STYLE) as u32 & CS_DBLCLKS != 0,
            pressed: Cell::new(None),
            previous: Cell::new(None),
            drag_x: Cell::new(None),
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
            WM_LBUTTONDOWN => pointer.start(hwnd),
            WM_LBUTTONDBLCLK => {
                pointer.finish(hwnd, false);
                pointer.post(ACTION_SETTINGS, 0);
            }
            WM_MOUSEMOVE => pointer.move_drag(),
            WM_LBUTTONUP => pointer.finish(hwnd, true),
            WM_CANCELMODE | WM_CAPTURECHANGED => pointer.finish(hwnd, false),
            WM_RBUTTONDOWN => {
                pointer.finish(hwnd, false);
                pointer.previous.set(None);
            }
            WM_RBUTTONUP => pointer.post(ACTION_PAUSE, 0),
            WM_NCDESTROY => {
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
