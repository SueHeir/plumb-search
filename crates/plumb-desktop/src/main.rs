//! Plumb Search for the desktop: a Plumb Search node that runs inside the
//! app, and one window with its panel.
//!
//! The window opens on a bundled "Starting..." page while the node starts on
//! Tauri's async runtime, then shows the node's panel (`/app`): what works
//! now ("Limited search is ready"), storage and downloads against their
//! limits, crawling, the Plumb network, setup progress, the settings and how
//! to search from the browser. The app is no browser: searching happens
//! in the default browser, and so does every link out of the panel,
//! including the node's search page.
//!
//! Closing the window keeps the node running, so that searches from the
//! browser keep working: the app stays in the menu bar (macOS) or the
//! notification area (Windows, Linux), whose menu opens the window again,
//! turns "Start at login" on or off, or quits. Started at login, the app
//! opens no window.
//!
//! The panel is also the control center for the person's other nodes, such
//! as a Docker container on a homelab: "Connect to a node" saves a node's
//! address and remote control token, and the panel then shows and changes
//! that node's settings, with the app's own node making the requests (see
//! `plumb_node::node::control`). The window never sees the tokens.
//!
//! No page can call into the app: the app defines no commands and grants no
//! capabilities, so Tauri's IPC refuses everything.

// Release builds on Windows are GUI programs, without a console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod logging;

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use plumb_node::node::{self, NodeConfig, NodeHandle};
use tauri::async_runtime::{self, JoinHandle};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::webview::NewWindowResponse;
use tauri::{
    AppHandle, Manager, RunEvent, Url, WebviewUrl, WebviewWindowBuilder, Window, WindowEvent,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tracing::{debug, error, info, warn};

/// Label of the app's one window.
const MAIN_WINDOW: &str = "main";

/// The argument the app is started with at login, when it opens no window.
const AT_LOGIN: &str = "--at-login";

/// The node's panel, which the window shows; its forms post under it.
const PANEL_PATH: &str = "app";

/// The port the node listens on, so that its address stays the same from
/// one launch to the next, and with it a browser search engine set up as
/// `http://127.0.0.1:7586/search?q=%s`. 7586 spells PLUM on a phone keypad.
const PORT: u16 = 7586;

/// How long the node gets to stop when the app quits, before the app exits
/// anyway. Stopping finishes an index build already under way, which takes
/// about a minute for a million sites. Exiting sooner is safe too: the node
/// writes its files atomically and clears unfinished work when it starts.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

fn main() {
    logging::init();
    let app = tauri::Builder::default()
        // First, so that a second launch hands over to the running app and
        // exits before anything else starts.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // While quitting, the window stays closed.
            let running = matches!(*app.state::<Node>().phase(), Phase::Started(_));
            if running {
                show_main_window(app);
            }
        }))
        // The plugin's click handler opens links through IPC, which the
        // node's pages may not use; `on_new_window` below does its job.
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        // A login item on macOS, a registry entry on Windows and an
        // autostart entry on Linux, which start the app with AT_LOGIN.
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![AT_LOGIN]),
        ))
        .manage(Node::default())
        .on_window_event(on_window_event)
        .setup(|app| {
            setup(app.handle())?;
            Ok(())
        })
        .build(tauri::generate_context!());
    match app {
        Ok(app) => app.run(on_run_event),
        Err(err) => {
            error!("could not start Plumb Search: {err}");
            std::process::exit(1);
        }
    }
}

/// The app's node, from starting it to stopping it.
#[derive(Default)]
struct Node {
    /// The node's page, `http://127.0.0.1:<port>/`, once it listens. Set on
    /// every start: a restart can bring the node up on another port.
    url: Mutex<Option<Url>>,
    phase: Mutex<Phase>,
    /// Whether the menu bar or notification area icon is up, so that closing
    /// the window can leave the app running there.
    in_tray: AtomicBool,
}

impl Node {
    fn phase(&self) -> MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The running node's page, as of its latest start.
    fn url(&self) -> Option<Url> {
        self.url
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_url(&self, url: Url) {
        *self.url.lock().unwrap_or_else(PoisonError::into_inner) = Some(url);
    }
}

#[derive(Default)]
enum Phase {
    /// Setup has not started the node yet.
    #[default]
    NotStarted,
    /// The node is starting or running. The task yields it, or `None` when
    /// it could not start.
    Started(JoinHandle<Option<NodeHandle>>),
    /// The app is quitting; the task ends once the node has stopped.
    Stopping(JoinHandle<()>),
    /// The node has stopped, so the app may exit.
    Stopped,
}

/// Opens the window and starts the node in the background.
fn setup(app: &AppHandle) -> Result<()> {
    match app.path().app_log_dir() {
        Ok(dir) => match logging::open_file(&dir) {
            Ok(path) => info!("log file: {}", path.display()),
            Err(err) => warn!("cannot write a log file in {}: {err}", dir.display()),
        },
        Err(err) => warn!("cannot find the log folder: {err}"),
    }
    info!(
        "Plumb Search {} on {} {}",
        app.package_info().version,
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    // `cargo tauri dev` serves the bundled page from its own dev server.
    let dev_server = if tauri::is_dev() {
        app.config().build.dev_url.clone()
    } else {
        None
    };
    let navigating = app.clone();
    let opening = app.clone();
    let mut window =
        WebviewWindowBuilder::new(app, MAIN_WINDOW, WebviewUrl::App("index.html".into()))
            .title("Plumb Search")
            .inner_size(960.0, 900.0)
            .min_inner_size(400.0, 300.0)
            .resizable(true)
            .center()
            // At login the node starts in the background; the menu bar icon
            // or launching the app again opens the window.
            .visible(!started_at_login())
            .on_navigation(move |url| {
                let node = navigating.state::<Node>();
                match destination(url, node.url().as_ref(), dev_server.as_ref()) {
                    Destination::Window => true,
                    Destination::Browser => {
                        open_in_browser(&navigating, url);
                        false
                    }
                    Destination::Nowhere => {
                        debug!("blocked a navigation to an unsupported destination");
                        false
                    }
                }
            })
            // Ctrl-click, middle-click and `target="_blank"` links.
            .on_new_window(move |url, _features| {
                let node = opening.state::<Node>();
                if is_add_to_firefox(&url, node.url().as_ref()) {
                    open_in_firefox(&opening, &url);
                } else if is_web_page(&url) {
                    open_in_browser(&opening, &url);
                }
                NewWindowResponse::Deny
            });
    // Tauri keeps the webview's cache and storage in the app's local data
    // folder, which on Linux is the node's data folder too; keep them apart.
    if let Ok(cache) = app.path().app_cache_dir() {
        window = window.data_directory(cache.join("webview"));
    }
    window.build().context("opening the window")?;
    match add_tray_icon(app) {
        Ok(()) => app.state::<Node>().in_tray.store(true, Ordering::Relaxed),
        // Closing the window then quits, as it would without the icon.
        Err(err) => warn!("cannot add the menu bar icon: {err:#}"),
    }

    let task = async_runtime::spawn(start_node(app.clone()));
    *app.state::<Node>().phase() = Phase::Started(task);
    quit_on_stop_signals(app.clone());
    Ok(())
}

/// Starts the node and shows its page, or tells the user why it could not
/// start.
async fn start_node(app: AppHandle) -> Option<NodeHandle> {
    // In a task of its own, so that a panic while starting is reported too.
    let started = match async_runtime::spawn(launch_node(app.clone())).await {
        Ok(started) => started,
        Err(err) => Err(anyhow!("the node crashed while starting: {err}")),
    };
    match started {
        Ok(node) => {
            info!("node listening on {}", node.url());
            // The panel's "Restart to apply".
            let signal = node.restart_signal();
            let restarting = app.clone();
            async_runtime::spawn(async move {
                signal.requested().await;
                restart_node(&restarting);
            });
            if let Err(err) = show_node_page(&app, &node) {
                error!("could not show the node's page: {err:#}");
            }
            if node.addr().port() != PORT {
                report_other_port(&app, node.addr().port());
            }
            Some(node)
        }
        Err(err) => {
            error!("the node could not start: {err:#}");
            report_start_failure(&app, &err);
            None
        }
    }
}

/// Stops the node and starts it again, which applies the feature settings
/// saved on the panel. The window shows the panel again once the node is
/// back.
fn restart_node(app: &AppHandle) {
    let node = app.state::<Node>();
    let mut phase = node.phase();
    // Quitting meanwhile wins.
    let Phase::Started(_) = &*phase else {
        return;
    };
    let Phase::Started(task) = std::mem::replace(&mut *phase, Phase::NotStarted) else {
        unreachable!("matched above");
    };
    info!("restarting the node");
    let app = app.clone();
    *phase = Phase::Started(async_runtime::spawn(async move {
        stop_node(task).await;
        start_node(app).await
    }));
}

async fn launch_node(app: AppHandle) -> Result<NodeHandle> {
    let data_dir = match std::env::var_os("PLUMB_DESKTOP_DATA_DIR") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => app
            .path()
            .app_data_dir()
            .context("finding the app data folder")?,
    };
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating the data folder {}", data_dir.display()))?;
    info!("data folder: {}", data_dir.display());
    let mut config = NodeConfig::desktop(data_dir);
    config.bind.set_port(PORT);
    config.network = Some(network_config());
    match node::start(config.clone()).await {
        // Something else has the port; the window works on any port.
        Err(err) if !can_listen_on(config.bind) => {
            warn!("cannot listen on port {PORT}, so using a free port instead: {err:#}");
            config.bind.set_port(0);
            node::start(config).await
        }
        started => started,
    }
}

/// How the app joins the Plumb network, which the panel's "Join the Plumb
/// network" setting turns off and on. Like any home node, it needs no open
/// port: it connects out, through the relay when nothing else reaches it.
/// It listens on ports the system picks, which never clash with another
/// program's.
fn network_config() -> plumb_net::NetConfig {
    // The node puts the network's files in its data folder.
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.listen = [
        "/ip4/0.0.0.0/tcp/0",
        "/ip4/0.0.0.0/udp/0/quic-v1",
        "/ip6/::/tcp/0",
        "/ip6/::/udp/0/quic-v1",
    ]
    .iter()
    .map(|addr| addr.parse().expect("a valid multiaddr"))
    .collect();
    // The Plumb network's first nodes, which the app connects to and learns
    // the others from: the relay on plumbsearch.org, by name and by address.
    net.bootstrap = plumb_net::default_bootstrap();
    // Fewer background rounds of bucket requests than a server's: they
    // keep searches from standing out, on home internet.
    net.round_every = Some(plumb_net::rounds::DESKTOP_ROUND_EVERY);
    net
}

/// Whether a server could listen on `addr` now.
fn can_listen_on(addr: SocketAddr) -> bool {
    TcpListener::bind(addr).is_ok()
}

/// Tells the user that the node is not on [`PORT`] this time, so a browser
/// search engine set up for Plumb Search cannot reach it.
fn report_other_port(app: &AppHandle, port: u16) {
    let message = format!(
        "Port {PORT} is taken, probably by another program, so Plumb Search is using \
         port {port} this time.\n\n\
         A browser search engine set up with http://127.0.0.1:{PORT} cannot reach Plumb \
         Search until that port is free and Plumb Search is started again."
    );
    let mut dialog = app
        .dialog()
        .message(message)
        .title("Plumb Search")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::Ok);
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        dialog = dialog.parent(&window);
    }
    dialog.show(|_| {});
}

fn show_node_page(app: &AppHandle, node: &NodeHandle) -> Result<()> {
    let url: Url = node.url().parse().context("reading the node's address")?;
    // Let the window go to the node's panel before sending it there.
    app.state::<Node>().set_url(url.clone());
    let window = app
        .get_webview_window(MAIN_WINDOW)
        .context("the window is closed")?;
    let panel = url.join(PANEL_PATH).context("finding the node's panel")?;
    window.navigate(panel).context("opening the node's panel")
}

/// Shows an error dialog, and quits when it is closed.
fn report_start_failure(app: &AppHandle, err: &anyhow::Error) {
    let folder = match app.path().app_data_dir() {
        Ok(dir) => dir.display().to_string(),
        Err(_) => "unknown".to_string(),
    };
    let mut dialog = app
        .dialog()
        .message(start_failure_message(err, &folder, logging::file_path()))
        .title("Plumb Search")
        .kind(MessageDialogKind::Error)
        .buttons(MessageDialogButtons::Ok);
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        dialog = dialog.parent(&window);
    }
    let app = app.clone();
    dialog.show(move |_| app.exit(1));
}

/// The error dialog's text: what went wrong, where the data folder is
/// (unless the error already says) and where the log is.
fn start_failure_message(err: &anyhow::Error, folder: &str, log: Option<&Path>) -> String {
    let mut message = format!("Plumb Search could not start.\n\n{err:#}");
    let mut details = Vec::new();
    if !message.contains(folder) {
        details.push(format!("Data folder: {folder}"));
    }
    if let Some(log) = log {
        details.push(format!("Log file: {}", log.display()));
    }
    if !details.is_empty() {
        message.push_str("\n\n");
        message.push_str(&details.join("\n"));
    }
    message
}

/// Stops the node before the app exits.
///
/// Quitting from the menu bar icon, closing the window when there is no
/// icon, a stop signal or a failed start asks the app to exit:
/// that first request is held back while the node stops in the background,
/// then the app exits with the same code. Quitting from the macOS menu skips
/// the request and only ends the event loop, so the node is stopped there.
fn on_run_event(app: &AppHandle, event: RunEvent) {
    match event {
        RunEvent::ExitRequested { code, api, .. } => {
            let node = app.state::<Node>();
            let mut phase = node.phase();
            match std::mem::replace(&mut *phase, Phase::Stopped) {
                Phase::Started(task) => {
                    api.prevent_exit();
                    let quitting = app.clone();
                    *phase = Phase::Stopping(async_runtime::spawn(async move {
                        stop_node(task).await;
                        *quitting.state::<Node>().phase() = Phase::Stopped;
                        quitting.exit(code.unwrap_or(0));
                    }));
                    drop(phase);
                    // The window is still open after a signal; the app should
                    // look closed while the node stops.
                    hide_main_window(app);
                }
                Phase::Stopping(stopping) => {
                    api.prevent_exit();
                    *phase = Phase::Stopping(stopping);
                }
                other @ (Phase::NotStarted | Phase::Stopped) => *phase = other,
            }
        }
        // Clicking the Dock icon while the window is closed.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => {
            if matches!(*app.state::<Node>().phase(), Phase::Started(_)) {
                show_main_window(app);
            }
        }
        RunEvent::Exit => {
            let pending = std::mem::replace(&mut *app.state::<Node>().phase(), Phase::Stopped);
            let wait = match pending {
                Phase::Started(task) => async_runtime::spawn(stop_node(task)),
                Phase::Stopping(stopping) => stopping,
                Phase::NotStarted | Phase::Stopped => return,
            };
            hide_main_window(app);
            // The process ends when this returns, so wait here.
            async_runtime::block_on(async move {
                if tokio::time::timeout(STOP_TIMEOUT, wait).await.is_err() {
                    warn!("quitting without waiting any longer for the node");
                }
            });
        }
        _ => {}
    }
}

/// Waits for the node to finish starting, if it still is, then stops it,
/// for at most [`STOP_TIMEOUT`].
async fn stop_node(task: JoinHandle<Option<NodeHandle>>) {
    // In a task of its own, so that a panic still lets the app exit.
    let stopping = async_runtime::spawn(async move {
        if let Ok(Some(node)) = task.await {
            if let Err(err) = node.shutdown().await {
                error!("the node did not stop cleanly: {err:#}");
            }
        }
    });
    match tokio::time::timeout(STOP_TIMEOUT, stopping).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => error!("stopping the node failed: {err}"),
        Err(_) => warn!(
            "the node did not stop within {} seconds; quitting anyway",
            STOP_TIMEOUT.as_secs()
        ),
    }
}

/// Quits cleanly on Ctrl-C and, on Unix, SIGTERM. A second signal exits at
/// once, without waiting for the node.
fn quit_on_stop_signals(app: AppHandle) {
    async_runtime::spawn(async move {
        if let Err(err) = stop_signal().await {
            warn!("cannot listen for stop signals: {err}");
            return;
        }
        info!("stop signal received; quitting");
        app.exit(0);
        if stop_signal().await.is_ok() {
            warn!("second stop signal; exiting without waiting for the node");
            std::process::exit(1);
        }
    });
}

async fn stop_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Menu item ids of the menu bar or notification area icon.
const MENU_OPEN: &str = "open";
const MENU_AT_LOGIN: &str = "at-login";
const MENU_QUIT: &str = "quit";

/// Adds the icon in the menu bar (macOS) or notification area (Windows,
/// Linux), whose menu opens the window or quits the app. Double-clicking it
/// opens the window too, where the system reports clicks (not on Linux).
fn add_tray_icon(app: &AppHandle) -> Result<()> {
    let at_login = app.autolaunch().is_enabled().unwrap_or_else(|err| {
        warn!("cannot tell whether the app starts at login: {err}");
        false
    });
    let at_login = CheckMenuItem::with_id(
        app,
        MENU_AT_LOGIN,
        "Start at login",
        true,
        at_login,
        None::<&str>,
    )?;
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, MENU_OPEN, "Open Plumb Search", true, None::<&str>)?,
            &at_login,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, MENU_QUIT, "Quit Plumb Search", true, None::<&str>)?,
        ],
    )?;
    let icon = app
        .default_window_icon()
        .context("the app has no icon")?
        .clone();
    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("Plumb Search")
        .menu(&menu)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            MENU_OPEN => show_main_window(app),
            MENU_AT_LOGIN => set_start_at_login(app, &at_login),
            MENU_QUIT => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Turns starting at login on or off, as the menu item now shows; if that
/// fails, the item goes back to what is true.
fn set_start_at_login(app: &AppHandle, item: &CheckMenuItem<tauri::Wry>) {
    let wanted = item.is_checked().unwrap_or(false);
    let autolaunch = app.autolaunch();
    let changed = if wanted {
        autolaunch.enable()
    } else {
        autolaunch.disable()
    };
    match changed {
        Ok(()) => info!(
            "start at login turned {}",
            if wanted { "on" } else { "off" }
        ),
        Err(err) => {
            warn!(
                "cannot turn start at login {}: {err}",
                if wanted { "on" } else { "off" }
            );
            let _ = item.set_checked(autolaunch.is_enabled().unwrap_or(!wanted));
        }
    }
}

/// Whether the system started the app at login.
fn started_at_login() -> bool {
    std::env::args().skip(1).any(|arg| arg == AT_LOGIN)
}

/// Closing the window hides it while the app stays in the menu bar or
/// notification area, so the node keeps answering the browser's searches.
/// The first time, a message says where the app went.
fn on_window_event(window: &Window, event: &WindowEvent) {
    let WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    let app = window.app_handle();
    if window.label() != MAIN_WINDOW || !app.state::<Node>().in_tray.load(Ordering::Relaxed) {
        return;
    }
    api.prevent_close();
    let _ = window.hide();
    if first_time_closed(app) {
        app.dialog()
            .message(KEEPS_RUNNING)
            .title("Plumb Search")
            .kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::Ok)
            .show(|_| {});
    }
}

/// What the first close of the window says.
const KEEPS_RUNNING: &str = if cfg!(target_os = "macos") {
    "Plumb Search is still running in the menu bar, so searches from your browser keep \
     working.\n\nTo open it again or quit, click its icon in the menu bar."
} else {
    "Plumb Search is still running in the notification area, so searches from your \
     browser keep working.\n\nTo open it again or quit, click its icon there."
};

/// Whether the window is being closed for the first time, which is noted in
/// the app's config folder.
fn first_time_closed(app: &AppHandle) -> bool {
    let Ok(dir) = app.path().app_config_dir() else {
        return false;
    };
    let marker = dir.join("closed-once");
    if marker.exists() {
        return false;
    }
    if let Err(err) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&marker, b"")) {
        // Saying it again next time beats never saying it.
        warn!("cannot note that the window was closed: {err}");
    }
    true
}

/// Brings the window to the front, when the app is launched again.
fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn hide_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.hide();
    }
}

/// The name the system knows Firefox by, to open a page in it.
const FIREFOX: &str = if cfg!(target_os = "macos") {
    "Firefox"
} else {
    "firefox"
};

/// Whether `url` is the node's page with the steps to add Plumb to Firefox,
/// which only Firefox can follow: the panel's "Add to Firefox" button.
fn is_add_to_firefox(url: &Url, node: Option<&Url>) -> bool {
    node.is_some_and(|node| node.origin() == url.origin())
        && url.path() == plumb_node::web::ADD_TO_FIREFOX_PATH
}

/// Opens `url` in Firefox, or in the default browser when Firefox cannot be
/// started (most likely, it is not installed).
fn open_in_firefox(app: &AppHandle, url: &Url) {
    debug!("opening a page in Firefox");
    // URLs and opener errors can contain search terms, including private
    // searches in fragments. Keep them out of the app's persisted log.
    if app.opener().open_url(url.as_str(), Some(FIREFOX)).is_err() {
        warn!("could not open a page in Firefox; trying the default browser");
        open_in_browser(app, url);
    }
}

fn open_in_browser(app: &AppHandle, url: &Url) {
    debug!("opening a page in the default browser");
    if app.opener().open_url(url.as_str(), None::<&str>).is_err() {
        warn!("could not open a page in the default browser");
    }
}

/// Where a link, form or redirect may lead.
#[derive(Debug, PartialEq, Eq)]
enum Destination {
    /// The app's own pages, shown in the window.
    Window,
    /// Any other web page, handed to the default browser.
    Browser,
    /// Anything else (`file:`, `data:`, `javascript:`...), dropped.
    Nowhere,
}

/// Decides where `url` goes, given the node's page once it listens and the
/// dev server's page in `cargo tauri dev`.
fn destination(url: &Url, node: Option<&Url>, dev_server: Option<&Url>) -> Destination {
    let same_origin = |page: &Url| page.origin() == url.origin();
    if node.is_some_and(same_origin) {
        // Only the panel: the search pages and the JSON API belong in the
        // browser, and the window has no back button.
        let panel = format!("/{PANEL_PATH}");
        let path = url.path();
        if path == panel || path.starts_with(&format!("{panel}/")) {
            Destination::Window
        } else {
            Destination::Browser
        }
    } else if is_bundled_page(url) || dev_server.is_some_and(same_origin) {
        Destination::Window
    } else if is_web_page(url) {
        Destination::Browser
    } else {
        Destination::Nowhere
    }
}

/// Whether `url` is one of the pages bundled into the app, which Tauri
/// serves at `tauri://localhost/`, or at `http://tauri.localhost/` on Windows.
fn is_bundled_page(url: &Url) -> bool {
    if cfg!(windows) {
        is_web_page(url) && url.host_str() == Some("tauri.localhost")
    } else {
        url.scheme() == "tauri" && url.host_str() == Some("localhost")
    }
}

fn is_web_page(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        s.parse().unwrap()
    }

    const NODE: &str = "http://127.0.0.1:41234/";

    #[test]
    fn the_nodes_panel_stays_in_the_window() {
        let node = url(NODE);
        for page in [
            "http://127.0.0.1:41234/app",
            "http://127.0.0.1:41234/app/settings",
            "http://127.0.0.1:41234/app?section=network",
            "http://127.0.0.1:41234/app/features",
            "http://127.0.0.1:41234/app/refresh",
            // Other nodes, controlled through this one.
            "http://127.0.0.1:41234/app/nodes/new",
            "http://127.0.0.1:41234/app/nodes/3f2a9c01b7de?section=resources",
        ] {
            assert_eq!(
                destination(&url(page), Some(&node), None),
                Destination::Window,
                "{page}"
            );
        }
        assert_eq!(
            url(NODE).join(PANEL_PATH).unwrap().as_str(),
            "http://127.0.0.1:41234/app"
        );
    }

    #[test]
    fn the_nodes_search_pages_and_json_api_open_in_the_browser() {
        let node = url(NODE);
        for page in [
            "http://127.0.0.1:41234/",
            "http://127.0.0.1:41234/search?q=us+bank",
            "http://127.0.0.1:41234/apps",
            "http://127.0.0.1:41234/api/search?q=us+bank",
            "http://127.0.0.1:41234/api/status",
        ] {
            assert_eq!(
                destination(&url(page), Some(&node), None),
                Destination::Browser,
                "{page}"
            );
        }
    }

    #[test]
    fn other_web_pages_open_in_the_browser() {
        let node = url(NODE);
        for page in [
            "https://www.usbank.com/",
            "http://usbank.com/",
            // Same host, but another port, scheme or name is another origin.
            "http://127.0.0.1:8080/",
            "https://127.0.0.1:41234/",
            "http://localhost:41234/",
        ] {
            assert_eq!(
                destination(&url(page), Some(&node), None),
                Destination::Browser,
                "{page}"
            );
        }
        // Until the node listens, no local page is its own.
        assert_eq!(destination(&url(NODE), None, None), Destination::Browser);
    }

    #[test]
    fn the_add_to_firefox_page_is_the_nodes_own() {
        let node = url(NODE);
        assert!(is_add_to_firefox(
            &url("http://127.0.0.1:41234/add-to-firefox"),
            Some(&node)
        ));
        assert!(!is_add_to_firefox(
            &url("http://127.0.0.1:41234/add-to-firefox"),
            None
        ));
        for other in [
            "http://127.0.0.1:41234/",
            "http://127.0.0.1:8080/add-to-firefox",
            "https://evil.example/add-to-firefox",
        ] {
            assert!(!is_add_to_firefox(&url(other), Some(&node)), "{other}");
        }
    }

    #[test]
    fn other_schemes_go_nowhere() {
        let node = url(NODE);
        for page in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,hello",
            "about:blank",
            "ftp://example.com/",
        ] {
            assert_eq!(
                destination(&url(page), Some(&node), None),
                Destination::Nowhere,
                "{page}"
            );
        }
    }

    #[test]
    fn the_bundled_page_stays_in_the_window() {
        let (bundled, other_platforms) = if cfg!(windows) {
            (
                "http://tauri.localhost/index.html",
                "tauri://localhost/index.html",
            )
        } else {
            (
                "tauri://localhost/index.html",
                "http://tauri.localhost/index.html",
            )
        };
        assert_eq!(destination(&url(bundled), None, None), Destination::Window);
        assert_ne!(
            destination(&url(other_platforms), None, None),
            Destination::Window
        );
    }

    #[test]
    fn the_start_failure_message_names_the_data_folder_once_and_the_log() {
        let folder = "/home/me/.local/share/io.github.sueheir.plumbsearch";
        let log = format!("{folder}/logs/plumb.log");
        let locked = anyhow!("{folder} is in use by another Plumb node");
        let message = start_failure_message(&locked, folder, Some(Path::new(&log)));
        assert_eq!(
            message,
            format!(
                "Plumb Search could not start.\n\n\
                 {folder} is in use by another Plumb node\n\n\
                 Log file: {folder}/logs/plumb.log"
            )
        );

        let unbound = anyhow!("address in use").context("listening on 127.0.0.1:0");
        let message = start_failure_message(&unbound, folder, None);
        assert_eq!(
            message,
            format!(
                "Plumb Search could not start.\n\n\
                 listening on 127.0.0.1:0: address in use\n\n\
                 Data folder: {folder}"
            )
        );
    }

    #[test]
    fn the_app_joins_the_network_through_plumbsearch_org() {
        let net = network_config();
        assert_eq!(net.bootstrap.len(), 2);
        assert!(net.bootstrap[0]
            .to_string()
            .starts_with("/dns4/plumbsearch.org/tcp/4001/p2p/"));
        assert!(!net.relay_server);
        assert_eq!(net.listen.len(), 4);
    }

    #[test]
    fn a_port_in_use_cannot_be_listened_on() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = taken.local_addr().unwrap();
        assert!(!can_listen_on(addr));
        drop(taken);
        assert!(can_listen_on(addr));
    }

    #[test]
    fn the_dev_servers_pages_stay_in_the_window() {
        let dev = url("http://127.0.0.1:1430/");
        let page = url("http://127.0.0.1:1430/index.html");
        assert_eq!(destination(&page, None, Some(&dev)), Destination::Window);
        assert_eq!(destination(&page, None, None), Destination::Browser);
    }
}
