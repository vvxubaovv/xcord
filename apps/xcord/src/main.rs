#![cfg_attr(windows, windows_subsystem = "windows")]

mod root;
mod selectable_text;
mod terminal_view;
mod text_input;

use gpui::{
    px, size, App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions,
};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
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
        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
}
