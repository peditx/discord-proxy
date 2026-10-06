pub mod dial;
pub mod relay;
pub mod store;
pub mod sys;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use serde::Serialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::relay::Relay;
use crate::store::{ProxyEntry, Store};

pub struct App {
    pub dir: PathBuf,
    pub store: Mutex<Store>,
    pub relay: Mutex<Relay>,
    /// Clock of the system-proxy session the updater dialog started. Memory
    /// only: a restart loses it, and the stale-proxy reconcile already puts
    /// Windows back, so nothing here has to survive a crash.
    pub session: Mutex<Session>,
}

#[derive(Default)]
pub struct Session {
    /// When the session was started from the dialog.
    pub since: Option<std::time::Instant>,
    /// Last moment Update.exe was actually seen running.
    pub updater_seen: Option<std::time::Instant>,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub proxies: Vec<ProxyEntry>,
    pub settings: store::Settings,
    pub relay_running: bool,
    pub relay_port: u16,
    pub connections: usize,
    pub system_proxy: bool,
    pub discord_path: Option<String>,
    /// Squirrel's Update.exe, shown so it is visible that the updater was found.
    pub updater_path: Option<String>,
    /// Whether the Discord client is up - Launch turns into Kill while it is.
    pub discord_running: bool,
}

fn with_store<T>(app: &AppHandle, f: impl FnOnce(&mut Store) -> T) -> T {
    let state = app.state::<App>();
    let mut store = state.store.lock().unwrap();
    f(&mut store)
}

fn persist(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<App>();
    let store = state.store.lock().unwrap();
    store.save(&state.dir)
}

fn relay_running(app: &AppHandle) -> bool {
    app.state::<App>().relay.lock().unwrap().running()
}

async fn sync_upstream(app: &AppHandle) {
    let state = app.state::<App>();
    let entry = {
        let store = state.store.lock().unwrap();
        store.active().cloned()
    };
    let upstream = { state.relay.lock().unwrap().upstream.clone() };
    *upstream.write().await = entry;
}

// ------------------------------------------------------------------ queries

#[tauri::command]
fn get_state(app: AppHandle) -> Result<Snapshot, String> {
    let state = app.state::<App>();
    let (proxies, settings) = {
        let store = state.store.lock().unwrap();
        (store.proxies.clone(), store.settings.clone())
    };
    let relay_running = state.relay.lock().unwrap().running();
    let connections = state.relay.lock().unwrap().conns.load(Ordering::SeqCst);
    Ok(Snapshot {
        discord_path: settings
            .discord_path
            .clone()
            .or_else(sys::find_discord),
        updater_path: sys::find_updater(),
        discord_running: sys::discord_running(),
        system_proxy: sys::system_proxy_on(),
        relay_running,
        relay_port: settings.listen_port,
        connections,
        proxies,
        settings,
    })
}

#[tauri::command]
fn detect_discord(app: AppHandle) -> Result<Option<String>, String> {
    let found = sys::find_discord();
    if let Some(path) = &found {
        with_store(&app, |store| store.settings.discord_path = Some(path.clone()));
        persist(&app)?;
    }
    Ok(found)
}

#[tauri::command]
fn set_discord_path(app: AppHandle, path: String) -> Result<(), String> {
    with_store(&app, |store| {
        let trimmed = path.trim().to_string();
        store.settings.discord_path = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        };
    });
    persist(&app)
}

// -------------------------------------------------------------------- editor

#[tauri::command]
async fn save_proxy(app: AppHandle, entry: ProxyEntry) -> Result<Snapshot, String> {
    with_store(&app, |store| store.upsert(entry));
    persist(&app)?;
    sync_upstream(&app).await;
    get_state(app)
}

#[tauri::command]
async fn delete_proxy(app: AppHandle, id: u64) -> Result<Snapshot, String> {
    with_store(&app, |store| store.remove(id));
    persist(&app)?;
    sync_upstream(&app).await;
    get_state(app)
}

#[tauri::command]
async fn set_active(app: AppHandle, id: Option<u64>) -> Result<Snapshot, String> {
    with_store(&app, |store| store.settings.active_id = id);
    persist(&app)?;
    sync_upstream(&app).await;
    get_state(app)
}

#[tauri::command]
async fn test_proxy(app: AppHandle, id: u64) -> Result<String, String> {
    let entry = with_store(&app, |store| {
        store.proxies.iter().find(|p| p.id == id).cloned()
    })
    .ok_or("proxy not found")?;
    let ms = dial::probe(Some(&entry)).await?;
    Ok(format!("{} · {} ms", entry.kind.label(), ms))
}

// -------------------------------------------------------------------- relay

#[tauri::command]
async fn start_relay(app: AppHandle) -> Result<u16, String> {
    sync_upstream(&app).await;
    let port = with_store(&app, |store| store.settings.listen_port);
    let listener = relay::bind(port).await?;

    let state = app.state::<App>();
    let mut relay = state.relay.lock().unwrap();
    relay.install(listener, port);
    Ok(port)
}

#[tauri::command]
fn stop_relay(app: AppHandle) -> Result<(), String> {
    app.state::<App>().relay.lock().unwrap().stop();
    // A system proxy pointing at a relay that is no longer there breaks everything.
    if with_store(&app, |store| store.settings.system_proxy) {
        return disable_system_proxy(&app);
    }
    Ok(())
}

#[tauri::command]
async fn set_listen_port(app: AppHandle, port: u16) -> Result<u16, String> {
    if port < 1024 {
        return Err("pick a port above 1024".to_string());
    }
    let was_running = relay_running(&app);
    let previous = with_store(&app, |store| store.settings.listen_port);
    with_store(&app, |store| store.settings.listen_port = port);
    // A restart that fails must not leave the store claiming a port nobody is
    // listening on - the old relay is still the one running.
    let restarted = if was_running {
        start_relay(app.clone()).await
    } else {
        Ok(port)
    };
    if let Err(e) = restarted {
        with_store(&app, |store| store.settings.listen_port = previous);
        return Err(e);
    }
    persist(&app)?;
    // Windows still points at the old port, and a proxy aimed at a port nobody
    // listens on takes the whole PC offline.
    if with_store(&app, |store| store.settings.system_proxy) {
        sys::set_proxy_server(port)?;
    }
    Ok(port)
}

#[tauri::command]
fn set_strict(app: AppHandle, strict: bool) -> Result<(), String> {
    with_store(&app, |store| store.settings.strict_udp = strict);
    persist(&app)
}

#[tauri::command]
fn set_close_to_tray(app: AppHandle, on: bool) -> Result<(), String> {
    with_store(&app, |store| store.settings.close_to_tray = on);
    persist(&app)
}

// -------------------------------------------------------------- system proxy

fn disable_system_proxy(app: &AppHandle) -> Result<(), String> {
    let saved = with_store(app, |store| {
        store.settings.system_proxy = false;
        store.settings.sys_session = false;
        store.settings.saved_sys.take()
    });
    if let Some(saved) = saved {
        sys::restore_system_proxy(&saved)?;
    }
    persist(app)
}

#[tauri::command]
fn set_system_proxy(app: AppHandle, on: bool) -> Result<(), String> {
    if !on {
        return disable_system_proxy(&app);
    }
    if !relay_running(&app) {
        return Err("start the local relay first - it is what the system proxy points at".into());
    }
    let (port, already) = with_store(&app, |store| {
        (store.settings.listen_port, store.settings.system_proxy)
    });
    if already {
        return Ok(());
    }
    let saved = sys::apply_system_proxy(port)?;
    with_store(&app, |store| {
        store.settings.system_proxy = true;
        store.settings.saved_sys = Some(saved);
    });
    persist(&app)
}

// -------------------------------------------------------------------- launch

#[tauri::command]
fn launch_discord(app: AppHandle) -> Result<String, String> {
    if !relay_running(&app) {
        return Err("start the local relay first".to_string());
    }
    let (stored, port, strict) = with_store(&app, |store| {
        (
            store.settings.discord_path.clone(),
            store.settings.listen_port,
            store.settings.strict_udp,
        )
    });
    // Squirrel deletes the app-* folder an update replaces, so a path pinned
    // months ago is usually gone by the time Launch is pressed. Re-detect and
    // remember where Discord moved instead of failing.
    let path = match stored.filter(|p| Path::new(p).is_file()) {
        Some(p) => p,
        None => {
            let found = sys::find_discord().ok_or_else(|| {
                "Discord was not found. Install it, or paste its Discord.exe path here.".to_string()
            })?;
            with_store(&app, |store| {
                store.settings.discord_path = Some(found.clone())
            });
            persist(&app)?;
            found
        }
    };
    // Squirrel's Update.exe is .NET, not Chromium: it follows only the Windows
    // system proxy, never --proxy-server. Turning that on changes routing for
    // every app on the machine, so it stays the user's switch - say where
    // things stand and leave it alone.
    let note = if sys::proxy_points_at(port) {
        "Discord started through the relay - the Windows system proxy is on, so Update.exe rides it too"
            .to_string()
    } else {
        "Discord started through the relay. Update.exe only follows the Windows system proxy - turn that switch on yourself if you want updates routed too"
            .to_string()
    };
    sys::launch_discord(&path, port, strict)?;
    Ok(note)
}

/// Stop Discord and its updater. Works whether or not the relay is up - the
/// client may well be running without it.
#[tauri::command]
fn kill_discord() -> Result<String, String> {
    sys::kill_discord()
}

/// What the dialog's "Start with system proxy" button runs. That click is the
/// consent - nothing here turns the machine-wide proxy on by itself - and
/// `updater_session_watch` is what turns it back off again.
#[tauri::command]
fn start_updater_session(app: AppHandle) -> Result<(), String> {
    set_system_proxy(app.clone(), true)?;
    with_store(&app, |store| store.settings.sys_session = true);
    persist(&app)?;
    // The guard borrows the State, so the State has to outlive this statement.
    let state = app.state::<App>();
    let mut session = state.session.lock().unwrap();
    *session = Session {
        since: Some(std::time::Instant::now()),
        updater_seen: None,
    };
    Ok(())
}

/// Wind the dialog's session down by itself: the proxy goes back off two
/// minutes after Update.exe was last seen running (two minutes from the start
/// if it never ran). Counting the quiet stretch rather than the whole session
/// is what lets an update that takes half an hour finish.
async fn updater_session_watch(app: AppHandle) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let state = app.state::<App>();
        let (session_on, proxy_on) = {
            let store = state.store.lock().unwrap();
            (store.settings.sys_session, store.settings.system_proxy)
        };
        let mut session = state.session.lock().unwrap();
        // Not ours, or not ours any more: a manual switch, a restart, a second
        // instance that never started a session.
        if !session_on || !proxy_on || session.since.is_none() {
            *session = Session::default();
            continue;
        }
        if sys::updater_running() {
            session.updater_seen = Some(std::time::Instant::now());
            continue;
        }
        let quiet_since = session.updater_seen.or(session.since);
        drop(session);
        if let Some(at) = quiet_since {
            if at.elapsed() >= std::time::Duration::from_secs(120) {
                if let Err(e) = disable_system_proxy(&app) {
                    eprintln!("updater session: {e}");
                }
            }
        }
    }
}

// ------------------------------------------------------------ tray + close

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Relay up. The Windows-wide proxy sits behind the user's own switch and is
/// never flipped from here - it changes routing for every app on the machine.
async fn tray_connect(app: AppHandle) -> Result<(), String> {
    if !relay_running(&app) {
        start_relay(app.clone()).await?;
    }
    Ok(())
}

/// Stop routing. `stop_relay` is what puts the Windows settings back.
async fn tray_disconnect(app: AppHandle) -> Result<(), String> {
    stop_relay(app.clone())
}

fn tray_action(app: AppHandle, id: &str) {
    let connect = id == "tray-connect";
    match id {
        "tray-open" => show_main(&app),
        "tray-connect" | "tray-disconnect" => {
            let what = id.to_string();
            tauri::async_runtime::spawn(async move {
                let outcome = if connect {
                    tray_connect(app.clone()).await
                } else {
                    tray_disconnect(app.clone()).await
                };
                if let Err(e) = outcome {
                    // There is no toast out here - open the window so the
                    // relay/system-proxy pills show what actually happened.
                    eprintln!("{what}: {e}");
                    show_main(&app);
                }
            });
        }
        "tray-quit" => app.exit(0),
        _ => {}
    }
}

fn build_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "tray-open", "Open Discord Proxy", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "tray-connect", "Connect", true, None::<&str>)?,
            &MenuItem::with_id(app, "tray-disconnect", "Disconnect", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "tray-quit", "Quit", true, None::<&str>)?,
        ],
    )?;

    let mut tray = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip("Discord Proxy")
        // Right-click opens the menu; a left click just brings the window back.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| tray_action(app.clone(), event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    // The icon is held by the app's resource table, so dropping this is fine.
    tray.build(app)?;
    Ok(())
}

/// A crash or a kill from Task Manager never runs the exit handler, so Windows
/// can be left pointing at a relay that is gone - the whole PC then goes
/// offline until this runs. The store still holds what Windows looked like
/// before we touched it, so put it back on the next start.
fn reconcile_stale_proxy(store: &mut Store) -> bool {
    if !store.settings.system_proxy {
        return false;
    }
    if sys::proxy_points_at(store.settings.listen_port) {
        if let Some(saved) = store.settings.saved_sys.clone() {
            if sys::restore_system_proxy(&saved).is_err() {
                return false; // keep the record and retry on the next start
            }
        }
    }
    store.settings.system_proxy = false;
    store.settings.sys_session = false;
    store.settings.saved_sys = None;
    true
}

// --------------------------------------------------------------------- entry

pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let dir = app.path().app_config_dir()?;
            let mut store = Store::load(&dir);
            // Something already on our port means a second instance is alive
            // and owns these settings - leave them alone.
            let probe = std::net::SocketAddr::from(([127, 0, 0, 1], store.settings.listen_port));
            let busy =
                std::net::TcpStream::connect_timeout(&probe, std::time::Duration::from_millis(200))
                    .is_ok();
            let mut dirty = !busy && reconcile_stale_proxy(&mut store);
            // The watcher's clock lives in memory - after a restart there is
            // no session left to wind down, only a flag written by one.
            if !busy && store.settings.sys_session {
                store.settings.sys_session = false;
                dirty = true;
            }
            if dirty {
                let _ = store.save(&dir);
            }
            let upstream = std::sync::Arc::new(tokio::sync::RwLock::new(store.active().cloned()));
            app.manage(App {
                dir,
                store: Mutex::new(store),
                relay: Mutex::new(Relay::new(upstream)),
                session: Mutex::new(Session::default()),
            });
            build_tray(app)?;
            tauri::async_runtime::spawn(updater_session_watch(app.handle().clone()));
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let to_tray = window
                    .state::<App>()
                    .store
                    .lock()
                    .unwrap()
                    .settings
                    .close_to_tray;
                if to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            detect_discord,
            set_discord_path,
            save_proxy,
            delete_proxy,
            set_active,
            test_proxy,
            start_relay,
            stop_relay,
            set_listen_port,
            set_strict,
            set_close_to_tray,
            set_system_proxy,
            launch_discord,
            kill_discord,
            start_updater_session,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Discord Proxy");

    app.run(|app, event| {
        if let tauri::RunEvent::Exit = event {
            let state = app.state::<App>();
            state.relay.lock().unwrap().stop();
            let saved = {
                let mut store = state.store.lock().unwrap();
                store.settings.system_proxy = false;
                store.settings.saved_sys.take()
            };
            if let Some(saved) = saved {
                let _ = sys::restore_system_proxy(&saved);
            }
            let store = state.store.lock().unwrap();
            let _ = store.save(&state.dir);
        }
    });
}
