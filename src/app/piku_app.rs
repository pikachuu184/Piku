//! Application bootstrap: asset source, component init, theme, state, window.

use gpui::{AppContext as _, Bounds, WindowBounds, WindowKind, WindowOptions, px, size};
use gpui_component::{Root, TitleBar};

use crate::app::assets::PikuAssets;
use crate::state::PikuState;
use crate::ui::shell::Workspace;

pub fn run() {
    let app = gpui_platform::application().with_assets(PikuAssets);

    app.run(move |cx| {
        gpui_component::init(cx);
        crate::theme::apply(cx);
        crate::app::actions::init(cx);
        PikuState::init(cx);
        crate::ui::register_panels(cx);
        cx.activate(true);

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

            let window = cx
                .open_window(options, |window, cx| {
                    let workspace = cx.new(|cx| Workspace::new(window, cx));
                    cx.new(|cx| Root::new(workspace, window, cx))
                })
                .expect("failed to open the PIKU window");

            window
                .update(cx, |_, window, cx| {
                    window.activate_window();
                    window.set_window_title("PIKU");
                    cx.on_release(|_, cx| {
                        cx.quit();
                    })
                    .detach();
                })
                .expect("failed to update window");
        })
        .detach();
    });
}
