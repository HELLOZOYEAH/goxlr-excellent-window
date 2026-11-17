// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
mod macos;

mod ipc;

use directories::ProjectDirs;
use interprocess::local_socket::tokio::prelude::LocalSocketStream;
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ToFsName, ToNsName};
use serde_json::{json, Value};
use std::env;
use std::fs::{create_dir_all, File};
use std::io::ErrorKind;

use crate::ipc::Socket;
use interprocess::local_socket::traits::tokio::Stream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Listener, Manager};
use tungstenite::{connect, Message};

static WINDOW_NAME: &str = "main";
static POPOUT_WINDOW_NAME: &str = "link-popout";
static POPOUT_SIZE: (f64, f64) = (700.0, 230.0);
static READY_EVENT_NAME: &str = "READY";
static SHOW_EVENT_NAME: &str = "si-event";
static HIDE_EVENT_NAME: &str = "HIDE-UI";
static STOP_EVENT_NAME: &str = "seppuku";

static SOCKET_PATH: &str = "/tmp/goxlr.socket";
static NAMED_PIPE: &str = "@goxlr.socket";

#[tokio::main]
async fn main() -> Result<(), String> {
    // Under Wayland on Linux gtkwebkit has a bug which prevents it from launching correctly, the
    // following sets an ENV flag prior to startup that disables the DMABUF renderer to fix it.
    #[cfg(target_os = "linux")]
    env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");

    // If running the utility has an error, make sure log level is debug, and propagate the
    // error up to the user on Windows.
    if let Err(e) = run_application().await {
        show_error("GoXLR Utility UI".into(), e.to_string());
        return Err(e.to_string());
    }
    Ok(())
}

async fn run_application() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() == 2 {
        if args[1] == "--install" {
            manage(true).await?;
            return Ok(());
        }
        if args[1] == "--remove" {
            manage(false).await?;
            return Ok(());
        }
    }

    let url = goxlr_preflight().await?;

    let builder = tauri::Builder::default();
    builder
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            // Register the Single Instance plugin on Windows, Linux and MacOs
            // Trigger a global event if something (eg, the util) attempts to open this again.
            let _ = app.emit(SHOW_EVENT_NAME, None::<String>);
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            device_view,
            link_popout,
            window_close,
            window_minimize,
            window_toggle_maximize,
            window_hide,
            window_drag
        ])
        .setup(|app| {
            let global_window = app.handle().clone();
            app.listen_any(SHOW_EVENT_NAME, move |_| {
                // Do anything and everything to make sure this Window is visible and focused!
                let window = global_window.get_webview_window(WINDOW_NAME).unwrap();
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();

                #[cfg(target_os = "macos")]
                {
                    match macos::MainThreadMarker::new() {
                        Some(mtm) => macos::show_dock(mtm),
                        None => {
                            macos::Queue::main().exec_async(|| {
                                macos::show_dock(macos::MainThreadMarker::new().unwrap())
                            });
                        }
                    };
                }
            });

            let hide_handle = app.handle().clone();
            app.listen_any(HIDE_EVENT_NAME, move |_| {
                let window = hide_handle.get_webview_window(WINDOW_NAME).unwrap();
                window.hide().unwrap();
                #[cfg(target_os = "macos")]
                {
                    match macos::MainThreadMarker::new() {
                        Some(mtm) => macos::hide_dock(mtm),
                        None => {
                            macos::Queue::main().exec_async(|| {
                                macos::hide_dock(macos::MainThreadMarker::new().unwrap())
                            });
                        }
                    };
                }
            });

            let ready_handle = app.handle().clone();
            app.listen_any(READY_EVENT_NAME, move |data| {
                let address = data.payload();
                let window = ready_handle.get_webview_window(WINDOW_NAME).unwrap();
                let _ = window.eval(format!("window.location.replace({})", address).as_str());
            });

            let shutdown_handle = app.handle().clone();
            app.listen_any(STOP_EVENT_NAME, move |_| {
                // Terminate the App..
                shutdown_handle.exit(0);
            });
            tokio::task::spawn(goxlr_utility_monitor(app.handle().clone(), url));

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Only the main window goes to the tray
                if window.label() != WINDOW_NAME {
                    return;
                }
                window.hide().unwrap();
                api.prevent_close();
                #[cfg(target_os = "macos")]
                {
                    match macos::MainThreadMarker::new() {
                        Some(mtm) => macos::hide_dock(mtm),
                        None => {
                            macos::Queue::main().exec_async(|| {
                                macos::hide_dock(macos::MainThreadMarker::new().unwrap())
                            });
                        }
                    };
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error running tauri app");

    Ok(())
}

// Device view (goxlr-ui util/appWindow.js): the window shrinks to just the device, with a
// see-through background (the window is created transparent in tauri.conf.json), and grows back
// into the full app when the page asks. The window never has the Windows frame: the page draws its
// own minimise / maximise / close buttons and drags the window by its header.
//
// Growing centres the app on the middle of the device (`anchor`, CSS pixels inside the window),
// fitted to the screen it is on. Shrinking back returns to where the device was, unless the app
// has been moved since; then the device appears in the middle of the app.
#[tauri::command]
fn device_view(
    window: tauri::WebviewWindow,
    on: bool,
    width: f64,
    height: f64,
    anchor: Option<(f64, f64)>,
) -> Result<(), String> {
    let err = |e: tauri::Error| e.to_string();
    window.set_decorations(false).map_err(err)?;
    if on && window.is_maximized().unwrap_or(false) {
        window.unmaximize().map_err(err)?;
    }
    window.set_shadow(!on).map_err(err)?;
    window.set_resizable(!on).map_err(err)?;

    let at = window.outer_position().map_err(err)?;
    let now = window.outer_size().map_err(err)?;
    let scale = window.scale_factor().map_err(err)?;
    let (centre_x, centre_y) = match anchor {
        Some((x, y)) => (at.x + (x * scale) as i32, at.y + (y * scale) as i32),
        None => (at.x + now.width as i32 / 2, at.y + now.height as i32 / 2),
    };
    let area = work_area(&window, centre_x, centre_y);
    let (w, h) = ((width * scale) as i32, (height * scale) as i32);
    let mut target = fit_on_screen(centre_x, centre_y, w, h, area);

    let mut device_spot = DEVICE_SPOT.lock().unwrap();
    if on {
        if let Some((device, app)) = *device_spot {
            if app == (at.x, at.y) {
                target = (device.0, device.1, w, h);
            }
        }
    } else {
        *device_spot = Some(((at.x, at.y), (target.0, target.1)));
    }
    place_window(&window, target).map_err(err)
}

// Where the device was, and where the app opened from there: (device x, y), (app x, y)
static DEVICE_SPOT: Mutex<Option<((i32, i32), (i32, i32))>> = Mutex::new(None);

// A window this big, as close to centred on (x, y) as `area` allows: (x, y, width, height).
// Everything is in physical pixels; `area` is left, top, right, bottom.
fn fit_on_screen(x: i32, y: i32, width: i32, height: i32, area: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let (left, top, right, bottom) = area;
    let width = width.min(right - left);
    let height = height.min(bottom - top);
    (
        (x - width / 2).clamp(left, right - width),
        (y - height / 2).clamp(top, bottom - height),
        width,
        height,
    )
}

// The usable part of the screen under this point (no taskbar)
#[cfg(target_os = "windows")]
fn work_area(_window: &tauri::WebviewWindow, x: i32, y: i32) -> (i32, i32, i32, i32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(monitor, &mut info);
    }
    let r = info.rcWork;
    (r.left, r.top, r.right, r.bottom)
}

#[cfg(not(target_os = "windows"))]
fn work_area(window: &tauri::WebviewWindow, x: i32, y: i32) -> (i32, i32, i32, i32) {
    let monitor = window
        .monitor_from_point(x as f64, y as f64)
        .ok()
        .flatten()
        .or_else(|| window.current_monitor().ok().flatten());
    match monitor {
        Some(m) => (
            m.position().x,
            m.position().y,
            m.position().x + m.size().width as i32,
            m.position().y + m.size().height as i32,
        ),
        None => (i32::MIN / 2, i32::MIN / 2, i32::MAX / 2, i32::MAX / 2),
    }
}

// Moves and resizes in one step on Windows, so the page doesn't jump between the two
#[cfg(target_os = "windows")]
fn place_window(window: &tauri::WebviewWindow, (x, y, w, h): (i32, i32, i32, i32)) -> tauri::Result<()> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};

    let hwnd = HWND(window.hwnd()?.0);
    unsafe {
        let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn place_window(window: &tauri::WebviewWindow, (x, y, w, h): (i32, i32, i32, i32)) -> tauri::Result<()> {
    window.set_position(tauri::PhysicalPosition::new(x, y))?;
    window.set_size(tauri::PhysicalSize::new(w as u32, h as u32))
}

// The GoXLR Link pop-out: its own window with no frame, like the device view. Async because
// Windows can deadlock when a window is created from a command on the main thread.
#[tauri::command]
async fn link_popout(app: AppHandle, window: tauri::WebviewWindow, url: String) -> Result<(), String> {
    if let Some(open) = app.get_webview_window(POPOUT_WINDOW_NAME) {
        let _ = open.show();
        let _ = open.unminimize();
        return open.set_focus().map_err(|e| e.to_string());
    }
    let page = window.url().map_err(|e| e.to_string())?;
    let url = page.join(&url).map_err(|e| e.to_string())?;
    tauri::WebviewWindowBuilder::new(&app, POPOUT_WINDOW_NAME, tauri::WebviewUrl::External(url))
        .title("GoXLR Link")
        .inner_size(POPOUT_SIZE.0, POPOUT_SIZE.1)
        .center()
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// Really closes the window (the main window's close button only hides it)
#[tauri::command]
fn window_close(window: tauri::WebviewWindow) -> Result<(), String> {
    window.close().map_err(|e| e.to_string())
}

#[tauri::command]
fn window_minimize(window: tauri::WebviewWindow) -> Result<(), String> {
    window.minimize().map_err(|e| e.to_string())
}

#[tauri::command]
fn window_toggle_maximize(window: tauri::WebviewWindow) -> Result<(), String> {
    let maximized = window.is_maximized().map_err(|e| e.to_string())?;
    if maximized { window.unmaximize() } else { window.maximize() }.map_err(|e| e.to_string())
}

// Like the close button: the app keeps running in the tray
#[tauri::command]
fn window_hide(window: tauri::WebviewWindow) -> Result<(), String> {
    window.hide().map_err(|e| e.to_string())
}

#[tauri::command]
fn window_drag(window: tauri::WebviewWindow) -> Result<(), String> {
    window.start_dragging().map_err(|e| e.to_string())
}

async fn goxlr_preflight() -> Result<String, String> {
    // Because Windows also supports unix sockets, we need to maintain legacy behaviour..
    let path = if cfg!(windows) {
        NAMED_PIPE.to_ns_name::<GenericNamespaced>()
    } else {
        SOCKET_PATH.to_fs_name::<GenericFilePath>()
    }
    .map_err(|e| format!("Unable to get path to socket: {}", e))?;

    let connection = LocalSocketStream::connect(path).await;

    if connection.is_err() {
        let message = "The GoXLR Utility must be running before launching this app.";
        return Err(message.into());
    }
    let mut socket: Socket<Value, Value> = Socket::new(connection.unwrap());
    if socket.send(json!("GetStatus")).await.is_ok() {
        if let Ok(Some(result)) = socket.try_read().await {
            // Firstly, preform the general config check, and see if we need to run a command..
            if let Some(command) = check_app_runtime(&result)? {
                // We need to send a command to the GoXLR..
                if socket.send(command).await.is_ok() {
                    // We don't actually care about the result, just clear the response..
                    let _ = socket.try_read().await;
                }
            }

            // Now, grab the HTTP Address / Port for the utility..
            return get_goxlr_host(&result);
        }
    }
    Err("Unable to locate the Utility's URL".into())
}

fn check_app_runtime(status: &Value) -> Result<Option<Value>, String> {
    if let Some(status) = status.get("Status") {
        if let Some(config) = status.get("config") {
            if let Some(activation) = config.get("activation") {
                if let Some(path) = activation.get("active_path") {
                    let exe = get_current_path();

                    let path = {
                        let mut found = None;
                        if let Some(path) = path.as_str() {
                            #[cfg(not(unix))]
                            {
                                let mut command = windows_args::Args::parse_cmd(path);
                                if let Some(command) = command.next() {
                                    found.replace(command);
                                }
                            }
                            #[cfg(unix)]
                            {
                                let command = shell_words::split(path);
                                if let Ok(params) = command {
                                    found.replace(params[0].clone());
                                }
                            }
                        }
                        found
                    };
                    println!("{:#?}", path);

                    return if path.is_none() || PathBuf::from(path.unwrap()) != exe {
                        let title = String::from("GoXLR Utility UI");
                        let message = String::from("Use this app to control your GoXLR?");
                        if show_option(title, message).is_ok() {
                            let command = get_activator_command(Some(exe));
                            let json = serde_json::from_str::<Value>(&command).unwrap();
                            Ok(Some(json))
                        } else {
                            Err("Unable to obtain User Consent".into())
                        }
                    } else {
                        // We're already setup, nothing more to do here..
                        Ok(None)
                    };
                }
            } else {
                // The 'Activation' Settings aren't present, pre-1.0.6 utility..
                return Ok(None);
            }
        }
    }
    Err("Unable to locate Activation Path".into())
}

fn get_goxlr_host(status: &Value) -> Result<String, String> {
    if let Some(status) = status.get("Status") {
        if let Some(config) = status.get("config") {
            if let Some(http_settings) = config.get("http_settings") {
                if let Some(address) = http_settings.get("bind_address") {
                    if let Some(address) = address.as_str() {
                        if let Some(port) = http_settings.get("port") {
                            if let Some(port) = port.as_u64() {
                                let address = if address != "localhost" && address == "0.0.0.0" {
                                    "localhost"
                                } else {
                                    address
                                };
                                Ok(format!("{}:{}", address, port))
                            } else {
                                Err("Unable to Parse Port".into())
                            }
                        } else {
                            Err("Port Missing from http_status".into())
                        }
                    } else {
                        Err("Unable to parse bind_address as String".into())
                    }
                } else {
                    Err("bind_address Missing from http_status".into())
                }
            } else {
                Err("http_settings missing from Config response".into())
            }
        } else {
            Err("config missing from Status response".into())
        }
    } else {
        Err("Status missing from GetStatus response!".into())
    }
}

async fn supports_activation(socket: &mut Socket<Value, Value>) -> bool {
    if socket.send(json!("GetStatus")).await.is_ok() {
        if let Ok(Some(result)) = socket.try_read().await {
            if let Some(status) = result.get("Status") {
                if let Some(config) = status.get("config") {
                    if config.get("activation").is_some() {
                        return true;
                    }
                }
            }
        }
    }
    false
}

async fn goxlr_utility_monitor(handle: AppHandle, host: String) {
    println!("Spawning the Monitor.. {}", host);

    // Grab and Parse the URL..
    let ws_address = format!("ws://{}/api/websocket", host);
    let http_address = format!("http://{}/", host);

    // Attempt to connect to the websocket..
    let result = connect(ws_address.as_str());
    if result.is_err() {
        // Hide the UI itself before showing the error..
        let _ = handle.emit(HIDE_EVENT_NAME, None::<String>);

        // We only support windows for these currently..
        show_error(
            "Unable to Launch UI".to_string(),
            "Unable to connect to the GoXLR Utility".to_string(),
        );

        // Now drop and quit.
        let _ = handle.emit(STOP_EVENT_NAME, None::<String>);
        return;
    }

    // Got a good connection, grab the socket..
    let (mut socket, _) = result.unwrap();
    // Trigger the event that lets the window know we're ready..
    let _ = handle.emit(READY_EVENT_NAME, &http_address);

    // Anything that's not a valid message, or is a 'Close' message breaks the loop.
    while let Ok(message) = socket.read() {
        if let Message::Close(..) = message {
            // Break the loop so we can shutdown the app
            break;
        }
    }
    // Loop Ended, this happens when socket is closed.
    let _ = handle.emit(STOP_EVENT_NAME, None::<String>);
}

// Installs this app into the util..
async fn manage(install: bool) -> Result<(), String> {
    println!("Checking if Utility is Running..");
    let path = if cfg!(windows) {
        NAMED_PIPE.to_ns_name::<GenericNamespaced>()
    } else {
        SOCKET_PATH.to_fs_name::<GenericFilePath>()
    }
    .map_err(|e| format!("Unable to get path to socket: {}", e))?;

    let connection = LocalSocketStream::connect(path).await;

    if connection.is_err() {
        println!("Utility Not Running, changing config directly..");
        println!("Locating Settings File..");
        let path = get_settings_file();
        let json = if !&path.exists() {
            if !install {
                // If we're removing, and the path is missing, do nothing.
                return Ok(());
            }
            create_settings_path(&path);
            json!({ "activate": Value::Null })
        } else {
            load_settings(&path)
        };
        write_settings(&path, json, install);
    } else {
        println!("Utility Running, attempting via IPC");
        let method = if install { "Install" } else { "Remove" };

        let mut socket: Socket<Value, Value> = Socket::new(connection.unwrap());
        if supports_activation(&mut socket).await {
            let path = if install {
                Some(get_current_path())
            } else {
                None
            };

            let command = get_activator_command(path);

            // Attempt to Register ourselves as the UI App..
            println!("Executing: {}", command);

            let json = serde_json::from_str::<Value>(&command).unwrap();
            let _ = socket.send(json).await;
        } else {
            return Err(format!(
                "Unable to {}, Please stop the GoXLR Utility first.",
                method
            ));
        }
    }
    Ok(())
}

fn get_activator_command(exe: Option<PathBuf>) -> String {
    let exe = if let Some(exe) = exe {
        Value::String(get_platform_path(exe))
    } else {
        Value::Null
    };

    // Attempt to Register ourselves as the UI App..
    format!("{{ \"Daemon\": {{ \"SetActivatorPath\": {}  }} }}", exe)
}

fn get_platform_path(exe: PathBuf) -> String {
    let quoter = if cfg!(windows) { "\"" } else { "'" };
    format!("{}{}{}", quoter, exe.to_string_lossy(), quoter)
}

fn get_current_path() -> PathBuf {
    if let Ok(app_image) = env::var("APPIMAGE") {
        println!("Using AppImage at {}", &app_image);
        PathBuf::from(app_image)
    } else {
        env::current_exe().unwrap()
    }
}

fn create_settings_path(path: &Path) {
    println!("Creating path if needed..");
    if let Some(parent) = path.parent() {
        if let Err(e) = create_dir_all(parent) {
            if e.kind() != ErrorKind::AlreadyExists {
                panic!("Unable to Create Project Directory");
            }
        }
    }
}

fn load_settings(path: &PathBuf) -> Value {
    println!("Loading Existing Settings..");
    let path_str = String::from(path.to_string_lossy());
    match File::open(path) {
        Ok(reader) => serde_json::from_reader(reader)
            .unwrap_or_else(|_| panic!("Could not parse daemon settings file at {}", path_str)),
        Err(_) => panic!(
            "Could not open daemon settings file for reading at {}",
            path_str
        ),
    }
}

fn write_settings(path: &PathBuf, mut value: Value, install: bool) {
    let exe = get_current_path();

    value["activate"] = if install {
        Value::String(get_platform_path(exe))
    } else {
        Value::Null
    };

    let path_str = String::from(path.to_string_lossy());
    let writer = File::create(path).unwrap_or_else(|_| {
        panic!(
            "Could not open daemon settings file for writing at {}",
            path_str
        )
    });
    serde_json::to_writer_pretty(writer, &value)
        .unwrap_or_else(|_| panic!("Could not write to daemon settings file at {}", path_str));
}

fn get_settings_file() -> PathBuf {
    let proj_dirs = ProjectDirs::from("org", "GoXLR-on-Linux", "GoXLR-Utility")
        .expect("Couldn't find project directories");
    proj_dirs.config_dir().join("settings.json")
}

#[cfg(target_os = "linux")]
fn show_error(title: String, message: String) {
    use std::process::Command;
    // We have two choices here, kdialog, or zenity. We'll try both.
    if let Err(e) = Command::new("kdialog")
        .arg("--title")
        .arg(title.clone())
        .arg("--error")
        .arg(message.clone())
        .output()
    {
        println!("Error Running kdialog: {}, falling back to zenity..", e);
        let _ = Command::new("zenity")
            .arg("--title")
            .arg(title)
            .arg("--error")
            .arg("--text")
            .arg(message)
            .output();
    }
}

#[cfg(target_os = "linux")]
fn show_option(title: String, message: String) -> Result<(), ()> {
    use std::process::Command;
    // We need to grab the return status..
    if let Ok(status) = Command::new("kdialog")
        .arg("--title")
        .arg(title.clone())
        .arg("--yesno")
        .arg(message.clone())
        .status()
    {
        if status.success() {
            Ok(())
        } else {
            Err(())
        }
    } else if let Ok(status) = Command::new("zenity")
        .arg("--title")
        .arg(title)
        .arg("--question")
        .arg("--text")
        .arg(message)
        .status()
    {
        if status.success() {
            Ok(())
        } else {
            Err(())
        }
    } else {
        // We weren't able to trigger kdialog, or zenity, this is a failure.
        Err(())
    }
}

#[cfg(target_os = "windows")]
fn show_error(title: String, message: String) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let title = HSTRING::from(title);
    let message = HSTRING::from(message);

    unsafe {
        MessageBoxW(None, &message, &title, MB_ICONERROR | MB_OK);
    }
}

#[cfg(target_os = "windows")]
fn show_option(title: String, message: String) -> Result<(), ()> {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONQUESTION, MB_YESNO, MESSAGEBOX_RESULT,
    };

    let title = HSTRING::from(title);
    let message = HSTRING::from(message);

    unsafe {
        let result = MessageBoxW(None, &message, &title, MB_ICONQUESTION | MB_YESNO);

        // Either the windows crate doesn't have the enum, or I can't find it.. Either way..
        // https://learn.microsoft.com/en-us/dotnet/api/system.windows.messageboxresult
        if result == MESSAGEBOX_RESULT(6) {
            return Ok(());
        }
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::fit_on_screen;

    const SCREEN: (i32, i32, i32, i32) = (0, 0, 1920, 1040);

    #[test]
    fn centres_on_the_device_when_there_is_room() {
        assert_eq!(fit_on_screen(960, 520, 1300, 910, SCREEN), (310, 65, 1300, 910));
    }

    #[test]
    fn stays_on_screen_next_to_an_edge() {
        assert_eq!(fit_on_screen(1800, 900, 1300, 910, SCREEN), (620, 130, 1300, 910));
        assert_eq!(fit_on_screen(40, 20, 1300, 910, SCREEN), (0, 0, 1300, 910));
    }

    #[test]
    fn shrinks_to_fit_a_small_screen() {
        assert_eq!(fit_on_screen(400, 300, 1300, 910, (0, 0, 1280, 680)), (0, 0, 1280, 680));
    }

    #[test]
    fn works_on_a_screen_left_of_the_main_one() {
        assert_eq!(fit_on_screen(-100, 500, 1300, 910, (-1920, 0, 0, 1040)), (-1300, 45, 1300, 910));
    }
}

#[cfg(target_os = "macos")]
fn show_error(title: String, message: String) {
    match macos::MainThreadMarker::new() {
        Some(mtm) => macos::show_messagebox(mtm, title, message),
        None => {
            macos::Queue::main().exec_async(|| {
                macos::show_messagebox(macos::MainThreadMarker::new().unwrap(), title, message);
            });
        }
    };
}

#[cfg(target_os = "macos")]
fn show_option(title: String, message: String) -> Result<(), ()> {
    let mtm = macos::MainThreadMarker::new().unwrap();
    macos::show_question(mtm, title, message)
}
