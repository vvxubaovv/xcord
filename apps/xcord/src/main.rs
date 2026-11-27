#![cfg_attr(windows, windows_subsystem = "windows")]

mod detached_terminal;
mod ime;
mod markdown;
mod mermaid;
pub(crate) mod root;
mod script_editor;
mod selectable_text;
mod terminal_view;
mod text_input;
mod transfer;

use gpui::{
    px, size, App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions,
};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    Application::new().run(|cx: &mut App| {
        // First launch: 75% of the actual display width, keeping the same
        // 5:3 window aspect (height = width * 0.6).
        let initial_size = cx
            .primary_display()
            .map(|display| {
                let display_size = display.bounds().size;
                let width = f32::from(display_size.width) * 0.75;
                let height = (width * 0.6).min(f32::from(display_size.height) * 0.85);
                size(px(width), px(height))
            })
            .unwrap_or_else(|| size(px(1440.0), px(864.0)));
        let bounds = Bounds::centered(None, initial_size, cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Xcord — plugin-first SSH".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| root::XcordApp::new(window, cx)),
        )
        .expect("could not open the Xcord window");
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
