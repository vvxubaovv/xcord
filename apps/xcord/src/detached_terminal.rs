//! A standalone OS window that mirrors one terminal tab.
//!
//! The window shares the UI-thread workspace with the main shell, so output,
//! input and resize events stay live. It intentionally has no shell chrome:
//! the native title bar is the drag target for moving the window to another
//! monitor.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{div, px, rgb, Context, FocusHandle, MouseButton, Render, Rgba, Window};
use tokio::sync::broadcast::error::RecvError;
use xcord_plugin_api::{
    Highlights, I18n, TerminalId, Themes, Ui, UiCommand, UiHost, Workspace, WorkspaceEvent,
};
use xcord_store::Settings;

use crate::terminal_view::TerminalView;

struct DetachedUiHost;

impl UiHost for DetachedUiHost {
    fn send(&self, _command: UiCommand) {
        // A detached terminal has no shell panels. Ctrl+Shift shortcuts that
        // would open the palette/settings simply do nothing here.
    }
}

pub struct DetachedTerminal {
    terminal: gpui::Entity<TerminalView>,
    focus_handle: FocusHandle,
    themes: Themes,
    title: String,
    i18n: I18n,
    closed: bool,
}

impl DetachedTerminal {
    pub fn new(
        workspace: Workspace,
        themes: Themes,
        highlights: Highlights,
        settings: Settings,
        i18n: I18n,
        tab: TerminalId,
        title: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let themes_for_header = themes.clone();
        let terminal = cx.new(|cx| {
            let mut view = TerminalView::new(
                workspace.clone(),
                themes,
                highlights,
                Ui(Rc::new(DetachedUiHost)),
                settings,
                cx,
            );
            view.set_active(Some(tab), cx);
            view
        });
        let focus_handle = terminal.read(cx).focus_handle();
        let workspace_for_events = workspace.clone();
        let terminal_for_events = terminal.clone();
        let mut events = workspace.subscribe();
        cx.spawn(async move |this, cx| loop {
            match events.recv().await {
                Ok(WorkspaceEvent::Dirty(id)) if id == tab => {
                    let _ = this.update(cx, |this, cx| {
                        this.terminal.update(cx, |_, cx| cx.notify());
                        cx.notify();
                    });
                }
                Ok(WorkspaceEvent::Closed(id)) if id == tab => {
                    let _ = this.update(cx, |this, cx| {
                        this.closed = true;
                        cx.notify();
                    });
                }
                Ok(WorkspaceEvent::TabsChanged) => {
                    let tabs = workspace_for_events.tabs();
                    let found = tabs.iter().find(|info| info.id == tab).cloned();
                    let _ = this.update(cx, |this, cx| {
                        if let Some(info) = found {
                            this.title = info.title;
                            this.closed =
                                matches!(info.status, xcord_plugin_api::TabStatus::Closed);
                        }
                        cx.notify();
                    });
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {
                    let _ = terminal_for_events.update(cx, |_, cx| cx.notify());
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
                Err(RecvError::Closed) => break,
            }
        })
        .detach();

        Self {
            terminal,
            focus_handle,
            themes: themes_for_header,
            title,
            i18n,
            closed: false,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn color(value: [u8; 3]) -> Rgba {
        rgb(((value[0] as u32) << 16) | ((value[1] as u32) << 8) | value[2] as u32)
    }
}

impl Render for DetachedTerminal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle);
        }
        let theme = self.themes.active();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Self::color(theme.background))
            .child(
                div()
                    .h(px(30.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(Self::color(theme.panel))
                    .border_b_1()
                    .border_color(Self::color(theme.border))
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(Self::color(theme.foreground))
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(if self.closed {
                                Self::color(theme.danger)
                            } else {
                                Self::color(theme.muted)
                            })
                            .child(if self.closed {
                                self.i18n.t("app", "Closed")
                            } else {
                                self.i18n.t("app", "Detached")
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("detached-close")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_size(px(10.))
                            .text_color(Self::color(theme.muted))
                            .cursor_pointer()
                            .hover(|style| style.text_color(Self::color(theme.danger)))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|_, _, window: &mut Window, _| {
                                    window.remove_window();
                                }),
                            )
                            .child(format!("× {}", self.i18n.t("app", "Close window"))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(self.terminal.clone()),
            )
    }
}
