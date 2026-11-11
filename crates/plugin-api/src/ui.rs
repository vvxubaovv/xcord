use std::ops::Deref;
use std::rc::Rc;

/// A request from a plugin to the GPUI shell.
#[derive(Clone, Debug)]
pub enum UiCommand {
    NewSession,
    EditSession(String),
    OpenPalette,
    ShowPlugins,
    ShowSettings,
    FocusCompose,
    Notify { level: NoticeLevel, message: String },
    CloseActiveTab,
    ReconnectActive,
    SelectNextTab,
    SelectPrevTab,
    CopySelection,
    Paste,
    ToggleAi,
    OpenAiSettings,
    ToggleFtp,
    ToggleAutomation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
    Success,
}

/// Implemented by the GPUI shell and provided as the `ui-host` service.
///
/// This is the only direction in which plugins may touch UI state: they send a
/// semantic command, never a widget. Alternative shells (CLI, web) can provide
/// their own implementation.
pub trait UiHost: 'static {
    fn send(&self, command: UiCommand);
}

/// Service wrapper: `ctx.require::<Ui>(services::UI_HOST)`.
#[derive(Clone)]
pub struct Ui(pub Rc<dyn UiHost>);

impl Deref for Ui {
    type Target = dyn UiHost;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
