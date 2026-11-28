use crate::{
    network,
    preferences::{self, Settings},
    startup, update,
};
use serde::Serialize;
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tauri::{
    AppHandle, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent, Wry,
    webview::NewWindowResponse,
};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{GetWindowThreadProcessId, PostMessageW},
};

const LABEL: &str = "settings";

#[derive(Default)]
struct SettingsRuntime {
    window_gate: Mutex<()>,
    session: Mutex<Option<Session>>,
    counter: AtomicU64,
    installing: Arc<AtomicBool>,
}

struct Session {
    id: u64,
    owner: usize,
    path: PathBuf,
    custom: bool,
    updates_allowed: bool,
    release: Option<update::AvailableUpdate>,
    update_epoch: u64,
}

struct InstallLease(Arc<AtomicBool>);

impl Drop for InstallLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsSnapshot {
    settings: Settings,
    defaults: Settings,
    adapters: Vec<AdapterOption>,
    startup_enabled: bool,
    startup_allowed: bool,
    updates_allowed: bool,
    version: &'static str,
    preview: bool,
}

#[derive(Serialize)]
struct AdapterOption {
    id: String,
    name: String,
    connected: bool,
}

#[derive(Serialize)]
struct UpdateResult {
    version: Option<String>,
}

pub fn builder() -> tauri::Builder<Wry> {
    tauri::Builder::default()
        .manage(SettingsRuntime::default())
        .invoke_handler(tauri::generate_handler![
            settings_state,
            save_settings,
            check_updates,
            install_update,
            close_settings,
        ])
}

pub fn context() -> tauri::Context<Wry> {
    tauri::tauri_build_context!()
}

pub fn open(app: &AppHandle, path: PathBuf, owner: usize, custom: bool) -> io::Result<()> {
    let runtime = app.state::<SettingsRuntime>();
    let _gate = runtime
        .window_gate
        .lock()
        .map_err(|_| io::Error::other("NetBar settings are unavailable"))?;
    if let Some(window) = app.get_webview_window(LABEL) {
        window.unminimize().map_err(io::Error::other)?;
        window.show().map_err(io::Error::other)?;
        return window.set_focus().map_err(io::Error::other);
    }

    let id = runtime.counter.fetch_add(1, Ordering::Relaxed);
    {
        let mut state = runtime
            .session
            .lock()
            .map_err(|_| io::Error::other("NetBar settings are unavailable"))?;
        *state = Some(Session {
            id,
            owner,
            path,
            custom,
            updates_allowed: !custom && preferences::installed(),
            release: None,
            update_epoch: 0,
        });
    }

    let creation = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title("NetBar settings")
        .inner_size(940.0, 680.0)
        .min_inner_size(740.0, 560.0)
        .center()
        .visible(false)
        .on_navigation(|url| {
            matches!(
                (url.scheme(), url.host_str()),
                ("http", Some("tauri.localhost")) | ("tauri", Some("localhost"))
            )
        })
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .build();

    let window = match creation {
        Ok(window) => window,
        Err(error) => {
            clear_session(&runtime, id);
            return Err(io::Error::other(error));
        }
    };
    let app = app.clone();
    let hwnd = window.hwnd().map_or(0, |handle| handle.0 as usize);
    window.on_window_event(move |event| {
        if matches!(event, WindowEvent::Destroyed) {
            clear_session(&app.state::<SettingsRuntime>(), id);
            notify_parent(owner, crate::SETTINGS_CLOSED, hwnd);
        }
    });
    if let Err(error) = window.show() {
        let _ = window.destroy();
        return Err(io::Error::other(error));
    }
    let _ = window.set_focus();
    notify_parent(owner, crate::SETTINGS_OPENED, hwnd);
    Ok(())
}

fn clear_session(runtime: &SettingsRuntime, id: u64) {
    if let Ok(mut session) = runtime.session.lock()
        && session.as_ref().is_some_and(|session| session.id == id)
    {
        *session = None;
    }
}

fn notify_parent(owner: usize, message: u32, value: usize) {
    if owner == 0 {
        return;
    }
    let window = owner as HWND;
    let mut process = 0;
    unsafe {
        GetWindowThreadProcessId(window, &mut process);
        if process == std::process::id() {
            PostMessageW(window, message, value, 0);
        }
    }
}

fn session<'a>(
    window: &WebviewWindow,
    runtime: &'a SettingsRuntime,
) -> Result<MutexGuard<'a, Option<Session>>, String> {
    if window.label() != LABEL {
        return Err("Settings commands are available only in NetBar settings.".into());
    }
    let session = runtime
        .session
        .lock()
        .map_err(|_| "NetBar settings are unavailable.")?;
    if session.is_none() {
        return Err("NetBar settings are closed.".into());
    }
    Ok(session)
}

#[tauri::command]
fn settings_state(
    window: WebviewWindow,
    state: State<'_, SettingsRuntime>,
) -> Result<SettingsSnapshot, String> {
    let (path, custom, updates_allowed, preview) = {
        let session = session(&window, &state)?;
        let session = session.as_ref().expect("settings session checked");
        (
            session.path.clone(),
            session.custom,
            session.updates_allowed,
            session.owner == 0,
        )
    };
    let settings = Settings::load(&path);
    let mut adapters = network::adapters();
    if !settings.adapter.is_empty()
        && !adapters
            .iter()
            .any(|adapter| adapter.id.eq_ignore_ascii_case(&settings.adapter))
    {
        adapters.push(network::Adapter::missing(settings.adapter.clone()));
    }

    Ok(SettingsSnapshot {
        defaults: Settings {
            schema: settings.schema,
            ..Settings::default()
        },
        settings,
        adapters: adapters
            .into_iter()
            .map(|adapter| AdapterOption {
                id: adapter.id,
                name: adapter.name,
                connected: adapter.connected,
            })
            .collect(),
        startup_enabled: !custom && startup::enabled(),
        startup_allowed: !custom,
        updates_allowed,
        version: env!("CARGO_PKG_VERSION"),
        preview,
    })
}

#[tauri::command]
fn save_settings(
    window: WebviewWindow,
    state: State<'_, SettingsRuntime>,
    mut settings: Settings,
    startup_enabled: Option<bool>,
) -> Result<(), String> {
    let session = session(&window, &state)?;
    let session = session.as_ref().expect("settings session checked");
    let previous = Settings::load(&session.path);
    settings.schema = previous.schema;
    if settings.adapter != previous.adapter
        && !settings.adapter.is_empty()
        && !valid_adapter_id(&settings.adapter)
    {
        return Err("Select a valid network adapter.".into());
    }
    settings.validate();
    let startup_change =
        startup_enabled.filter(|desired| !session.custom && *desired != startup::enabled());
    if let Some(enabled) = startup_change {
        startup::set_enabled(enabled)
            .map_err(|error| format!("Could not change Windows startup. {error}"))?;
    }
    if let Err(error) = settings.save(&session.path) {
        if let Some(enabled) = startup_change {
            let _ = startup::set_enabled(!enabled);
        }
        return Err(format!("Could not save NetBar settings. {error}"));
    }
    notify_parent(session.owner, crate::SETTINGS_RELOAD, 0);
    Ok(())
}

fn valid_adapter_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 38
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            0 => *byte == b'{',
            37 => *byte == b'}',
            9 | 14 | 19 | 24 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn require_updates(session: &Session) -> Result<(), String> {
    if session.updates_allowed {
        Ok(())
    } else {
        Err("Install NetBar with its MSI to update from this window.".into())
    }
}

#[tauri::command]
async fn check_updates(
    window: WebviewWindow,
    state: State<'_, SettingsRuntime>,
) -> Result<UpdateResult, String> {
    let (id, epoch) = {
        let mut session = session(&window, &state)?;
        let session = session.as_mut().expect("settings session checked");
        require_updates(session)?;
        if state.installing.load(Ordering::Acquire) {
            return Err("An update is already being installed.".into());
        }
        session.release = None;
        session.update_epoch = session.update_epoch.wrapping_add(1);
        (session.id, session.update_epoch)
    };
    let result = tauri::async_runtime::spawn_blocking(update::check)
        .await
        .map_err(|error| error.to_string())??;
    let mut session = session(&window, &state)?;
    let session = session.as_mut().expect("settings session checked");
    if session.id != id || session.update_epoch != epoch {
        return Err("This update check was superseded. Check for updates again.".into());
    }
    let version = result.as_ref().map(|release| release.version.clone());
    session.release = result;
    Ok(UpdateResult { version })
}

#[tauri::command]
async fn install_update(
    window: WebviewWindow,
    state: State<'_, SettingsRuntime>,
) -> Result<(), String> {
    let (id, release, lease) = {
        let mut session = session(&window, &state)?;
        let session = session.as_mut().expect("settings session checked");
        require_updates(session)?;
        state
            .installing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "An update is already being installed.")?;
        let lease = InstallLease(Arc::clone(&state.installing));
        let release = session
            .release
            .take()
            .ok_or("Check for an update before installing it.")?;
        (session.id, release, lease)
    };
    let attempted = release.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _lease = lease;
        update::install(&attempted)
    })
    .await
    .map_err(|error| error.to_string())
    .and_then(|result| result);
    if result.is_err()
        && let Ok(mut session) = session(&window, &state)
        && let Some(session) = session.as_mut().filter(|session| session.id == id)
    {
        session.release = Some(release);
    }
    result
}

#[tauri::command]
fn close_settings(window: WebviewWindow, state: State<'_, SettingsRuntime>) -> Result<(), String> {
    drop(session(&window, &state)?);
    window.close().map_err(|error| error.to_string())
}
