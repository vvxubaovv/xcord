use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;

use bytes::Bytes;
use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{services, NoticeLevel, Sessions, TerminalId, Ui, UiCommand};
use xcord_store::config_dir;

/// Raw output event emitted by the workspace plugin. Logging (and future
/// consumers such as a session recorder or a trigger system) subscribe to this
/// rather than reaching into terminal buffers.
#[derive(Clone, Debug)]
pub struct TerminalOutputEvent {
    pub terminal: TerminalId,
    pub profile_id: String,
    pub profile_name: String,
    pub logging: bool,
    pub data: Bytes,
}

/// Writes per-session logs to the workspace's `logs/` directory.
#[derive(Clone)]
pub struct LoggingPlugin;

impl Plugin for LoggingPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("logging", "Session Logging")
            .description("Appends session output to per-tab log files")
            .author("xcord")
            .capabilities(&["logging", "terminal"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[services::SESSIONS]
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let sessions = ctx.require::<Sessions>(services::SESSIONS)?;
        let settings = sessions.settings();
        let log_all = settings.logging;

        let root = if settings.log_dir.trim().is_empty() {
            config_dir().join("logs")
        } else {
            PathBuf::from(&settings.log_dir)
        };
        if let Err(error) = std::fs::create_dir_all(&root) {
            log::warn!("could not create log directory {}: {error}", root.display());
            return Ok(());
        }

        let writers: Rc<RefCell<HashMap<TerminalId, File>>> = Rc::new(RefCell::new(HashMap::new()));
        let root = Rc::new(root);
        let ui = ctx.service::<Ui>(services::UI_HOST);
        let writers_for_event = writers.clone();
        let root_for_event = root.clone();

        ctx.on(move |event: &TerminalOutputEvent, _ctx| {
            if !event.logging && !log_all {
                return;
            }
            let mut writers = writers_for_event.borrow_mut();
            let file = match writers.get_mut(&event.terminal) {
                Some(file) => file,
                None => {
                    let name = sanitize(&event.profile_name);
                    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
                    let path = root_for_event.join(format!(
                        "{}-{}-{}.log",
                        name,
                        event.terminal.raw(),
                        stamp
                    ));
                    match OpenOptions::new().create(true).append(true).open(&path) {
                        Ok(mut file) => {
                            let _ = writeln!(
                                file,
                                "== xcord session {} ({}) started {} ==",
                                event.profile_name,
                                event.profile_id,
                                chrono::Local::now().to_rfc3339()
                            );
                            writers.insert(event.terminal, file);
                            writers.get_mut(&event.terminal).unwrap()
                        }
                        Err(error) => {
                            log::warn!("could not open log {}: {error}", path.display());
                            return;
                        }
                    }
                }
            };
            if let Err(error) = file.write_all(&event.data) {
                log::warn!("could not write session log: {error}");
            }
            let _ = file.flush();
        });

        let writers_for_cleanup = writers.clone();
        let ui_for_cleanup = ui.clone();
        ctx.effect("flush logs", move || {
            let mut writers = writers_for_cleanup.borrow_mut();
            for (id, mut file) in writers.drain() {
                let _ = file.flush();
                if let Some(ui) = &ui_for_cleanup {
                    ui.send(UiCommand::Notify {
                        level: NoticeLevel::Info,
                        message: format!("session log closed for {id}"),
                    });
                }
            }
        });

        Ok(())
    }
}

fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "session".into()
    } else {
        cleaned
    }
}
