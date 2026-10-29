// The Syphon app for Windows and Linux. See README.md.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod discover;
mod proxy;
mod store;
mod vpn;
mod wake;

use proxy::{AppState, UiCmd};
use std::sync::Arc;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

/// Pages served by the node look for "SyphonApp/" to know they are inside the app (sign-in happens on the app's
/// own "Who's watching?" page, and the menus offer "Switch profile" and "App settings").
fn user_agent() -> String {
    let v = proxy::VERSION;
    if cfg!(windows) {
        format!("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 Edg/130.0.0.0 SyphonApp/{v}")
    } else {
        format!("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15 SyphonApp/{v}")
    }
}

fn main() {
    // Linux: WebKitGTK's newer way of drawing (DMA-BUF) leaves a blank or white window with some graphics drivers and
    // desktops (seen on Cinnamon). The older way works everywhere. Anyone who sets the variable themselves keeps theirs.
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
        // still blank? `syphon-app --safe-graphics` also turns off compositing (slower video, but it always draws)
        if std::env::args().any(|a| a == "--safe-graphics") {
            std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        }
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--autostart"])))
        .setup(|app| {
            let data = app.path().app_data_dir()?;
            let resources = app.path().resource_dir().ok();
            let handle = app.handle().clone();
            let ui: proxy::UiFn = Arc::new(move |cmd| match cmd {
                UiCmd::Tv(on) => {
                    if let Some(w) = handle.get_webview_window("main") {
                        let _ = w.set_decorations(!on);
                        let _ = w.set_fullscreen(on);
                    }
                }
                UiCmd::Autostart(on) => {
                    let m = handle.autolaunch();
                    let _ = if on { m.enable() } else { m.disable() };
                }
                UiCmd::Quit => handle.exit(0),
            });
            let st = AppState::new(data, resources, ui);
            let tv = st.store.lock().unwrap().settings.tv_mode;
            let port = tauri::async_runtime::block_on(proxy::serve(st.clone()))?;
            let url = format!("http://127.0.0.1:{port}/__app/start?k={}", st.key);
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse()?))
                .title("Syphon")
                .inner_size(1280.0, 800.0)
                .min_inner_size(720.0, 480.0)
                .fullscreen(tv)
                .decorations(!tv)
                .user_agent(&user_agent())
                // the window is dark from the first moment, before the first page has drawn
                .background_color(tauri::window::Color(27, 26, 32, 255))
                .build()?;
            app.manage(st);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("the Syphon app could not start");
}
