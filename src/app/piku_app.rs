//! Application bootstrap: asset source, component init, theme, state, window.

use gpui::{AppContext as _, Bounds, WindowBounds, WindowKind, WindowOptions, px, size};
use gpui_component::{Root, TitleBar};

use crate::app::assets::PikuAssets;
use crate::state::PikuState;
use crate::ui::shell::Workspace;

pub fn run() {
    let app = gpui_platform::application().with_assets(PikuAssets);

    app.run(move |cx| {
        // Everything inside this closure, and every gpui callback it installs,
        // runs on this thread. Recording it lets the diagnostics layer tell
        // "blocking on the UI thread" from "blocking on a worker".
        crate::app::diagnostics::mark_ui_thread();

        // Before anything can render. PIKU opens no sockets, and gpui's image
        // loader is the one path that could try — a markdown image becomes a
        // `Resource::Uri` unconditionally, so previewing a file would otherwise
        // be enough to fetch an attacker-chosen URL. gpui's default client
        // already refuses, but by accident rather than by decision; installing
        // this makes it a decision, and makes the next person to want an HTTP
        // client confront it.
        cx.set_http_client(std::sync::Arc::new(crate::app::http::DenyAllHttpClient));

        gpui_component::init(cx);
        crate::theme::apply(cx);
        crate::app::actions::init(cx);
        if !PikuState::init(cx) {
            // Without a backend there is no way to reach the filesystem, and a
            // file manager that cannot do that should say so and stop rather
            // than open an empty window.
            tracing::error!("backend failed to start; exiting");
            cx.quit();
            return;
        }
        crate::ui::register_panels(cx);
        cx.activate(true);

        // Drain outstanding backend work before the process goes away, so an
        // in-flight copy is cancelled rather than torn down mid-write.
        //
        // gpui polls every quit future on the foreground executor under a
        // 200 ms timeout, so this must `.await` (not block) and must finish
        // well inside that budget — see `SHUTDOWN_GRACE`.
        cx.on_app_quit(|cx| {
            let backend = PikuState::global(cx).backend().clone();
            async move {
                backend.shutdown().await;
            }
        })
        .detach();

        let mut window_size = size(px(1440.), px(900.));
        if let Some(display) = cx.primary_display() {
            let display_size = display.bounds().size;
            window_size.width = window_size.width.min(display_size.width * 0.9);
            window_size.height = window_size.height.min(display_size.height * 0.9);
        }
        let window_bounds = Bounds::centered(None, window_size, cx);

        cx.spawn(async move |cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(window_bounds)),
                titlebar: Some(TitleBar::title_bar_options()),
                window_min_size: Some(size(px(960.), px(600.))),
                kind: WindowKind::Normal,
                ..Default::default()
            };

            // A window we cannot open (or cannot talk to) is unrecoverable, but
            // it is not a bug to panic over: log it and shut down cleanly so
            // the platform layer gets to run its teardown.
            let window = match cx.open_window(options, |window, cx| {
                let workspace = cx.new(|cx| Workspace::new(window, cx));
                cx.new(|cx| Root::new(workspace, window, cx))
            }) {
                Ok(window) => window,
                Err(error) => {
                    tracing::error!(%error, "failed to open the PIKU window");
                    cx.update(|cx| cx.quit());
                    return;
                }
            };

            let updated = window.update(cx, |_, window, cx| {
                window.activate_window();
                window.set_window_title("PIKU");
                cx.on_release(|_, cx| {
                    cx.quit();
                })
                .detach();
            });
            if let Err(error) = updated {
                tracing::error!(%error, "failed to initialize the PIKU window");
                cx.update(|cx| cx.quit());
            }
        })
        .detach();
    });
}
