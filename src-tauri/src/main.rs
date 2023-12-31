// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod webview;

use log::{error, info};
use memospot::*;
use native_dialog::MessageType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Mutex;
use tauri::api::process::{Command, CommandEvent};
use tauri::State;

use config::Config;

#[cfg(target_os = "macos")]
use {tauri::Manager, window_shadows::set_shadow};

struct MemosPort(Mutex<u16>);

#[tauri::command]
fn js_get_memos_port(memos_port: State<MemosPort>) -> u16 {
    *memos_port.0.lock().unwrap()
}

#[derive(Default, Serialize, Deserialize)]
struct MemosLog {
    time: String,
    latency: String,
    method: String,
    uri: String,
    status: u16,
    error: String,
}

static LOGGING_CONFIG_YAML: &str = r#"
# https://github.com/estk/log4rs#quick-start
appenders:
    file:
        encoder:
            pattern: "{d(%Y-%m-%d %H:%M:%S)} - {h({l})}: {m}{n}"
        path: $ENV{MEMOSPOT_DATA}/memos.log
        kind: rolling_file
        policy:
            trigger:
                kind: size
                limit: 10 mb
            roller:
                kind: fixed_window
                pattern: $ENV{MEMOSPOT_DATA}/memos.log.{}.gz
                count: 5
                base: 1
root:
    # debug | info | warn | error | off
    level: error
    appenders:
        - file
"#;

/// Enable logging if logging_config.yaml exists
///
/// Return true if logging is enabled
async fn setup_logger(data_path: &Path) -> bool {
    let log_config: PathBuf = data_path.join("logging_config.yaml");
    std::env::set_var("MEMOSPOT_DATA", data_path.to_string_lossy().to_string());

    if !std::path::Path::new(&log_config).exists() {
        // logging is disabled
        return false;
    }

    if log4rs::init_file(&log_config, Default::default()).is_ok() {
        // logging is enabled and config is ok
        return true;
    }

    // Logging is enabled, but config is bad
    if let Ok(mut file) = File::create(&log_config) {
        let config_template = LOGGING_CONFIG_YAML.replace("    ", "  ");

        if let Err(e) = file.write_all(config_template.as_bytes()) {
            panic_dialog!(
                "Failed to write to `{}`:\n{}",
                log_config.to_string_lossy(),
                e.to_string()
            );
        }
        if let Err(e) = file.flush() {
            panic_dialog!(
                "Failed to flush `{}` to disk:\n{}",
                log_config.to_string_lossy(),
                &e
            );
        }
    } else {
        panic_dialog!(
            "Failed to truncate `{}`. Please delete it and restart the application.",
            log_config.to_string_lossy()
        );
    }

    if log4rs::init_file(&log_config, Default::default()).is_ok() {
        // logging is enabled and config was reset
        return true;
    }

    panic_dialog!(
        "Failed to setup logging!\nPlease delete `{}` and restart the application.",
        log_config.to_string_lossy()
    );
}

#[tokio::main]
async fn main() {
    let data_path = get_app_data_path("memospot");
    if !data_path.exists() {
        if let Err(e) = std::fs::create_dir_all(&data_path) {
            panic_dialog!(
                "Failed to create data directory `{}`:\n{}",
                data_path.to_string_lossy(),
                e.to_string()
            );
        }
    }

    if !writable(&data_path) {
        panic_dialog!(
            "Data directory is not writable:\n{}",
            data_path.to_string_lossy()
        );
    }

    let cfg_file = data_path.join("memospot.yaml");
    if !cfg_file.exists() {
        if let Err(e) = config::Config::reset_file(&cfg_file) {
            panic_dialog!(
                "Failed to create config file `{}`:\n{}",
                cfg_file.to_string_lossy(),
                e.to_string()
            );
        }
    }
    if !writable(&cfg_file) {
        panic_dialog!(
            "Config file is not writable:\n{}",
            cfg_file.to_string_lossy()
        );
    }

    let mut cfg_reader = config::Config::init(&cfg_file);
    if let Err(e) = cfg_reader {
        if !confirm_dialog(
            "Configuration Error",
            &format!(
                "Failed to parse configuration file:\n\n{}\n\n\
                Do you want to reset the configuration file and start the application with default settings?",
                e
            ),
            MessageType::Warning
        ) {
            panic_dialog!("You must fix the config file manually and restart the application.");
        }

        if let Err(e) = config::Config::reset_file(&cfg_file) {
            panic_dialog!(
                "Failed to reset config file `{}`:\n{}",
                cfg_file.to_string_lossy(),
                e.to_string()
            );
        }
        cfg_reader = Ok(config::Config::default());
    }

    let config = cfg_reader.unwrap().clone();
    let mut last_port = config.memos.port;

    if cfg!(dev) && last_port != 0 {
        last_port += 1;
    }

    let db = data_path.join("memos_prod.db");
    if db.exists() && !writable(&db) {
        panic_dialog!("Database is not writable:\n{}", db.to_string_lossy());
    }

    let debug_memos = setup_logger(&data_path).await;
    info!("Starting Memospot");
    info!("Data path: {}", data_path.to_string_lossy());

    if !webview::is_available() {
        if !confirm_dialog(
            "WebView Error",
            "WebView is required for this application to work and it's not available on this system!\
            \n\nDo you want to install it?",
            MessageType::Error,
        ) {
            exit(1);
        }

        let _ = webview::install().await;
        if !webview::is_available() {
            panic_dialog!(
                "WebView is still not available!\n\n\
            Please install it manually and relaunch the application."
            );
        }
    }

    let mut memos_bin = "memos".to_owned();
    if std::env::consts::OS == "windows" {
        memos_bin.push_str(".exe");
    }
    let current_exe = std::env::current_exe().unwrap();
    let cwd = current_exe.parent().unwrap();
    let memos_server_bin = cwd.join(memos_bin).clone();
    let memos_path = std::path::Path::new(&memos_server_bin);
    if !memos_path.exists() {
        panic_dialog!(
            "Unable to find Memos server at\n{}",
            memos_server_bin.display()
        );
    }

    let mut memos_mode: &str = "prod";
    if cfg!(dev) {
        memos_mode = "demo";
    }

    let Some(memos_port) = portpicker::find_open_port(last_port) else {
        panic_dialog!("Failed to find an open port!");
    };

    if memos_port != last_port {
        let mut new_config = config.clone();
        new_config.memos.port = memos_port;
        if let Err(e) = Config::save_file(&cfg_file, &new_config) {
            panic_dialog!(
                "Failed to save config file:\n`{}`\n\n{}",
                cfg_file.to_string_lossy(),
                e.to_string()
            );
        }
    }

    let telemetry = config.memos.telemetry;

    let memos_env_vars: HashMap<String, String> = HashMap::from_iter(vec![
        ("MEMOS_MODE".to_owned(), memos_mode.to_owned()),
        ("MEMOS_PORT".to_owned(), memos_port.to_string()),
        ("MEMOS_ADDR".to_owned(), "127.0.0.1".to_owned()),
        (
            "MEMOS_DATA".to_owned(),
            data_path.to_string_lossy().to_string(),
        ),
        ("MEMOS_METRIC".to_owned(), telemetry.to_string()),
    ]);

    tauri::async_runtime::spawn(async move {
        let cmd = Command::new(memos_server_bin.clone().to_str().unwrap())
            .envs(memos_env_vars)
            .spawn();
        if cmd.is_err() {
            panic_dialog!("Failed to spawn Memos server!");
        }

        if !debug_memos {
            return;
        }

        let (mut rx, _) = cmd.unwrap();
        while let Some(event) = rx.recv().await {
            if let CommandEvent::Stdout(line) = event {
                let json: MemosLog = serde_json::from_str(&line).unwrap_or_default();
                if json.time.is_empty() {
                    continue;
                }

                if !json.error.is_empty() {
                    error!(
                        "latency={}, method={}, uri={}, {}",
                        json.latency, json.method, json.uri, json.error
                    );
                    continue;
                }

                info!(
                    "latency={}, method={}, uri={}, status={}",
                    json.latency, json.method, json.uri, json.status
                );
            }
        }
    });

    let tauri_run = tauri::Builder::default()
        .setup(move |_app| {
            // Shadows looks bad on Windows 10 and doesn't work on Linux
            #[cfg(target_os = "macos")]
            {
                if let Some(window) = _app.get_window("main") {
                    let _ = set_shadow(&window, true);
                }
            }

            Ok(())
        })
        .manage(MemosPort(Mutex::new(memos_port)))
        .invoke_handler(tauri::generate_handler![js_get_memos_port])
        .run(tauri::generate_context!());

    if tauri_run.is_err() {
        panic_dialog!("Failed to run Tauri application");
    }
}
