pub mod child;
pub mod env_capture;
pub mod serve_url;
pub mod state;
pub mod token;
pub mod workspaces_client;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Mutex;
use std::time::Duration;

use tauri::menu::{Menu, MenuBuilder, MenuEvent, MenuItemBuilder, SubmenuBuilder};
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

/// How long login-shell environment capture may take before falling back to
/// the app's own environment.
const ENV_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for `otto serve`'s `otto serve: <url>` announcement.
const SERVE_URL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `otto serve` gets to exit after `SIGTERM` before `SIGKILL`.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// How many trailing lines of `serve.log` an early-exit error shows.
const LOG_TAIL_LINES: usize = 50;
const OPEN_FOLDER_MENU_ID: &str = "open_folder";
const MAIN_WINDOW_LABEL: &str = "main";

/// The started `otto serve` child and what `open_folder` needs to talk to it
/// and to trust further directories the same way startup did. `token` and
/// `addr` never change once known, so they live alongside `child` rather
/// than in their own mutexes.
struct Running {
    child: Child,
    token: Option<String>,
    addr: Option<SocketAddr>,
    otto_binary: PathBuf,
    environment: Vec<(String, String)>,
}

/// `None` until the background startup sequence spawned in `setup` finishes
/// (or forever, if it fails before spawning a child). Managed before that
/// sequence starts so a quit during startup still finds a child to shut
/// down once one exists.
struct AppState {
    running: Mutex<Option<Running>>,
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let app_handle = app.handle().clone();
            app.manage(AppState {
                running: Mutex::new(None),
            });

            let menu = build_menu(&app_handle)?;
            app.set_menu(menu)?;

            // `blocking_pick_folder`/`blocking_show` need the main run loop,
            // which this `setup` call runs on, so the whole startup sequence
            // (which uses both) runs on its own thread instead.
            std::thread::spawn(move || start(app_handle));

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![pick_directory])
        .on_menu_event(on_menu_event)
        .on_window_event(|window, event| {
            if matches!(event, WindowEvent::CloseRequested { .. }) {
                window.app_handle().exit(0);
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building the Otto application")
        .run(|app_handle, event| {
            if let RunEvent::ExitRequested { .. } = event {
                shut_down_child(app_handle);
            }
        });
}

/// Captures the environment, picks and trusts a workspace, spawns
/// `otto serve`, and opens the main window on its announced URL. Runs off
/// the main thread (see `run`'s `setup`); errors it can't already show a
/// dialog for are shown here before exiting.
fn start(app_handle: AppHandle) {
    if let Err(error) = try_start(&app_handle) {
        show_message(
            &app_handle,
            "Otto",
            &error.to_string(),
            MessageDialogKind::Error,
        );
        app_handle.exit(1);
    }
}

fn try_start(app_handle: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = app_handle.path().app_data_dir()?;
    let log_dir = app_handle.path().app_log_dir()?;
    std::fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join("serve.log");

    let environment = match child::capture_login_shell_env(ENV_TIMEOUT) {
        child::EnvCapture::Captured(pairs) => pairs,
        child::EnvCapture::Fallback {
            environment,
            reason,
        } => {
            show_message(
                app_handle,
                "Environment capture",
                &format!(
                    "Otto could not capture your login shell's environment ({reason}). Using the app's own environment instead."
                ),
                MessageDialogKind::Warning,
            );
            environment
        }
    };

    let otto_binary = sidecar_path()?;
    let state_path = state::state_path(&data_dir);
    let mut saved = state::load_state(&state_path);

    let workspace = loop {
        if let Some(dir) = saved
            .workspace
            .as_deref()
            .filter(|dir| Path::new(dir).is_dir())
        {
            break PathBuf::from(dir);
        }
        let Some(picked) = app_handle
            .dialog()
            .file()
            .set_title("Choose a folder for Otto to work in")
            .blocking_pick_folder()
        else {
            app_handle.exit(1);
            return Ok(());
        };
        if let Ok(dir) = picked.into_path() {
            saved.workspace = Some(dir.to_string_lossy().into_owned());
        }
    };

    let trust = child::run_trust(&otto_binary, &workspace, &environment)?;
    if !trust.success {
        show_message(
            app_handle,
            "Otto",
            &format!("otto trust failed:\n\n{}", trust.stderr),
            MessageDialogKind::Error,
        );
        app_handle.exit(1);
        return Ok(());
    }
    state::save_state(&state_path, &saved)?;

    let mut process = child::spawn_serve(&otto_binary, &workspace, &environment, &log_path)?;
    let stdout = process.stdout.take().expect("otto serve's stdout is piped");
    let lines = child::spawn_line_reader(stdout);
    let url = match serve_url::wait_for_serve_url(&lines, SERVE_URL_TIMEOUT) {
        Ok(url) => url,
        Err(_) => {
            let status = process.try_wait().ok().flatten();
            let message = child::describe_early_exit(status, &log_path, LOG_TAIL_LINES);
            show_message(
                app_handle,
                "Otto did not start",
                &message,
                MessageDialogKind::Error,
            );
            let _ = process.kill();
            app_handle.exit(1);
            return Ok(());
        }
    };

    let parsed_url: tauri::Url = url.parse()?;
    let token = token::extract_token(&url);
    let addr = serve_url::parse_addr(&url);

    let state = app_handle.state::<AppState>();
    *state.running.lock().expect("running mutex is not poisoned") = Some(Running {
        child: process,
        token,
        addr,
        otto_binary,
        environment,
    });

    WebviewWindowBuilder::new(
        app_handle,
        MAIN_WINDOW_LABEL,
        WebviewUrl::External(parsed_url),
    )
    .title("Otto")
    .inner_size(1200.0, 800.0)
    .build()?;

    Ok(())
}

/// The sidecar binary's path at runtime: next to this executable, under the
/// base name Tauri's `externalBin` config uses (the target-triple suffix is
/// stripped when the sidecar is bundled alongside the app executable).
fn sidecar_path() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| std::io::Error::other("the running executable has no parent directory"))?;
    Ok(dir.join("otto"))
}

fn build_menu(app_handle: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let open_folder = MenuItemBuilder::with_id(OPEN_FOLDER_MENU_ID, "Open Folder…")
        .accelerator("CmdOrCtrl+O")
        .build(app_handle)?;
    let app_menu = SubmenuBuilder::new(app_handle, "Otto")
        .about(None)
        .separator()
        .hide()
        .separator()
        .quit()
        .build()?;
    let file_menu = SubmenuBuilder::new(app_handle, "File")
        .item(&open_folder)
        .separator()
        .close_window()
        .build()?;
    MenuBuilder::new(app_handle)
        .items(&[&app_menu, &file_menu])
        .build()
}

fn on_menu_event(app_handle: &AppHandle, event: MenuEvent) {
    if event.id() == OPEN_FOLDER_MENU_ID {
        let app_handle = app_handle.clone();
        // `blocking_pick_folder` needs the main run loop, which this handler
        // runs on, so open_folder runs on its own thread instead.
        std::thread::spawn(move || open_folder(&app_handle));
    }
}

/// **File > Open Folder…**: picks a directory, trusts it, registers it with
/// the running `otto serve` over the app's own `POST /v1/workspaces` call,
/// remembers it, and reloads the webview so it reflects the new workspace.
fn open_folder(app_handle: &AppHandle) {
    let Some(picked) = app_handle
        .dialog()
        .file()
        .set_title("Choose a folder for Otto to work in")
        .blocking_pick_folder()
    else {
        return;
    };
    let Ok(dir) = picked.into_path() else {
        return;
    };

    let state = app_handle.state::<AppState>();
    let running = state.running.lock().expect("running mutex is not poisoned");
    let Some((otto_binary, environment, addr, token)) = running.as_ref().map(|running| {
        (
            running.otto_binary.clone(),
            running.environment.clone(),
            running.addr,
            running.token.clone(),
        )
    }) else {
        drop(running);
        show_message(
            app_handle,
            "Otto",
            "Otto is still starting",
            MessageDialogKind::Warning,
        );
        return;
    };
    drop(running);

    match child::run_trust(&otto_binary, &dir, &environment) {
        Ok(trust) if !trust.success => {
            show_message(
                app_handle,
                "Otto",
                &format!("otto trust failed:\n\n{}", trust.stderr),
                MessageDialogKind::Error,
            );
            return;
        }
        Err(error) => {
            show_message(
                app_handle,
                "Otto",
                &format!("running otto trust: {error}"),
                MessageDialogKind::Error,
            );
            return;
        }
        Ok(_) => {}
    }

    let Some(addr) = addr else {
        show_message(
            app_handle,
            "Otto",
            "otto serve's address is not known",
            MessageDialogKind::Error,
        );
        return;
    };
    let token = token.unwrap_or_default();
    let path = dir.to_string_lossy().into_owned();

    match workspaces_client::post_workspace(addr, &token, &path) {
        Ok((status, _)) if (200..300).contains(&status) => {
            if let Ok(data_dir) = app_handle.path().app_data_dir() {
                let state_path = state::state_path(&data_dir);
                let _ = state::save_state(
                    &state_path,
                    &state::State {
                        workspace: Some(path),
                    },
                );
            }
            if let Some(window) = app_handle.get_webview_window(MAIN_WINDOW_LABEL) {
                if let Ok(url) = window.url() {
                    let _ = window.navigate(url);
                }
            }
        }
        Ok((status, body)) => show_message(
            app_handle,
            "Otto",
            &format!("registering the workspace failed: HTTP {status}\n\n{body}"),
            MessageDialogKind::Error,
        ),
        Err(error) => show_message(
            app_handle,
            "Otto",
            &format!("registering the workspace failed: {error}"),
            MessageDialogKind::Error,
        ),
    }
}

/// Called by the web UI's "Add workspace…" button as
/// `window.__TAURI__.core.invoke('pick_directory')`: shows the native folder
/// picker attached to the calling window and returns the picked absolute
/// path, or `None` on cancel. It does not trust, register, or remember the
/// folder; the web UI does that over the HTTP API. `capabilities/main.json`
/// allows it from the main window's `http://127.0.0.1:*` page only.
///
/// Async commands run off the main thread, which `blocking_pick_folder`
/// requires.
#[tauri::command]
async fn pick_directory(window: tauri::WebviewWindow) -> Option<String> {
    window
        .dialog()
        .file()
        .set_parent(&window)
        .set_title("Choose a folder for Otto to work in")
        .blocking_pick_folder()?
        .into_path()
        .ok()
        .map(|dir| dir.to_string_lossy().into_owned())
}

fn show_message(app_handle: &AppHandle, title: &str, message: &str, kind: MessageDialogKind) {
    app_handle
        .dialog()
        .message(message)
        .title(title)
        .kind(kind)
        .blocking_show();
}

fn shut_down_child(app_handle: &AppHandle) {
    let Some(state) = app_handle.try_state::<AppState>() else {
        return;
    };
    let mut running = state.running.lock().expect("running mutex is not poisoned");
    if let Some(running) = running.as_mut() {
        child::shutdown_child(&mut running.child, SHUTDOWN_GRACE);
    }
}
