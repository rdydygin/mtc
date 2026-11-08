//! mtc: a fast, keyboard-first Mattermost desktop client.

// Release builds on Windows are GUI apps: no console window behind them.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod actions;
mod app_root;
mod appearance;
mod assets;
mod composer;
#[cfg(not(target_os = "macos"))]
mod deep_link;
mod dither;
mod extensions;
mod http;
mod lang_icon;
mod login;
mod palette;
mod persist;
mod post_row;
mod sidebar;
mod skeleton;
mod sso;
mod status;
mod switcher;
mod time;
mod timeline_view;
mod update;
mod workspace;

use gpui_kit::component::TitleBar;
use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};

use crate::app_root::AppRoot;

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,mtc=info,session=info,mattermost=info"),
    )
    .init();

    // Linux and Windows open an SSO callback link by starting mtc with it.
    // Pass it to the instance that's already running, if there is one.
    #[cfg(not(target_os = "macos"))]
    let launch_urls = {
        let urls = deep_link::urls_from_args();
        if !urls.is_empty() && deep_link::forward_to_running(&urls) {
            return;
        }
        urls
    };

    // SSO callback URLs arrive before or after launch (from macOS, or from
    // processes the browser starts elsewhere); forward them into the app
    // once it is running.
    let (urls_tx, mut urls_rx) = futures::channel::mpsc::unbounded::<Vec<String>>();
    #[cfg(not(target_os = "macos"))]
    {
        deep_link::listen(urls_tx.clone());
        if !launch_urls.is_empty() {
            let _ = urls_tx.unbounded_send(launch_urls);
        }
    }
    let app = gpui_kit::application().with_assets(assets::AppAssets);
    app.on_open_urls(move |urls| {
        let _ = urls_tx.unbounded_send(urls);
    });
    app.run(move |cx| {
        gpui_kit::init(cx);
        appearance::restore(cx);
        update::init(cx);
        actions::init(cx);
        extensions::init(cx);
        sso::register_callback_schemes(cx);
        // State is saved in the background; finish before the process ends.
        cx.on_app_quit(|_| {
            persist::flush();
            async {}
        })
        .detach();
        cx.spawn(async move |cx| {
            use futures::StreamExt as _;
            while let Some(urls) = urls_rx.next().await {
                cx.update(|cx| sso::handle_open_urls(urls, cx));
            }
        })
        .detach();

        // Window geometry is a platform boundary, so it is in pixels.
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1200.), px(800.)),
                cx,
            ))),
            window_min_size: Some(size(px(720.), px(480.))),
            ..TitleBar::window_options()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| AppRoot::new(window, cx))
        })
        .expect("failed to open the main window");
        cx.activate(true);
    });
}
