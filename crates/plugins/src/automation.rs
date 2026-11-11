//! Rhai-based terminal automation.
//!
//! Scripts run on their own OS thread. UI-thread pumps execute queued writes
//! and feed the latest terminal text into a shared buffer. The script blocks
//! only on its own thread while waiting for output.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rhai::{Array, Dynamic, Engine, EvalAltResult, Map, NativeCallContext, Position};
use tokio::sync::broadcast;
use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{
    services, Automation, AutomationEvent, AutomationService, Command, Commands, I18n, RunStatus,
    ScriptInfo, ScriptLog, ScriptLogLevel, ScriptRunSnapshot, TerminalId, Ui, UiCommand, Workspace,
};

const SAMPLE: &str = r#"// Xcord Rhai automation example
// send_line("echo hello");
// let result = expect("hello", 5000);
// log("matched: " + result.pattern);
"#;

#[derive(Clone)]
pub struct AutomationPlugin;

impl Plugin for AutomationPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("automation", "Terminal Automation")
            .description("Rhai scripts that send input and react to terminal output")
            .author("xcord")
            .capabilities(&["automation", "rhai", "terminal"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[
            services::WORKSPACE,
            services::COMMANDS,
            services::UI_HOST,
            services::I18N,
        ]
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let workspace = (*ctx.require::<Workspace>(services::WORKSPACE)?).clone();
        let commands = (*ctx.require::<Commands>(services::COMMANDS)?).clone();
        let ui = (*ctx.require::<Ui>(services::UI_HOST)?).clone();
        let i18n = (*ctx.require::<I18n>(services::I18N)?).clone();

        let scripts_dir = scripts_dir();
        let _ = std::fs::create_dir_all(&scripts_dir);
        if std::fs::read_dir(&scripts_dir)
            .map(|entries| {
                !entries
                    .filter_map(|entry| entry.ok())
                    .any(|entry| entry.path().extension().is_some_and(|ext| ext == "rhai"))
            })
            .unwrap_or(true)
        {
            let _ = std::fs::write(scripts_dir.join("example.rhai"), SAMPLE);
        }

        let state = Arc::new(AutomationState::new(workspace, scripts_dir));
        ctx.provide(
            services::AUTOMATION,
            Automation(Rc::new(RcAutomation(state.clone()))),
        )?;
        let handle = commands.register(Command::new(
            "automation.open",
            i18n.t("app", "Automation"),
            "Automation",
            {
                let ui = ui.clone();
                move |_| {
                    ui.send(UiCommand::ToggleAutomation);
                }
            },
        ));
        ctx.effect("unregister automation command", move || {
            commands.unregister(handle);
        });
        let _ = state;
        Ok(())
    }
}

struct RcAutomation(Arc<AutomationState>);

impl std::ops::Deref for RcAutomation {
    type Target = AutomationState;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AutomationService for RcAutomation {
    fn scripts(&self) -> Vec<ScriptInfo> {
        self.0.scripts()
    }
    fn script_source(&self, id: &str) -> Option<String> {
        self.0.script_source(id)
    }
    fn create_script(&self, name: &str) -> std::result::Result<ScriptInfo, String> {
        self.0.create_script(name)
    }
    fn scripts_dir(&self) -> String {
        self.0.scripts_dir.to_string_lossy().into_owned()
    }
    fn run(
        &self,
        script_id: &str,
        terminal: TerminalId,
    ) -> std::result::Result<ScriptRunSnapshot, String> {
        self.0.run(script_id, terminal)
    }
    fn stop(&self, run_id: u64) {
        self.0.stop(run_id)
    }
    fn pump(&self) {
        self.0.pump()
    }
    fn update_terminal(&self, terminal: TerminalId, text: String) {
        self.0.update_terminal(terminal, text)
    }
    fn active_run(&self) -> Option<ScriptRunSnapshot> {
        self.0.active_run()
    }
    fn run_snapshot(&self, run_id: u64) -> Option<ScriptRunSnapshot> {
        self.0.run_snapshot(run_id)
    }
    fn subscribe(&self) -> broadcast::Receiver<AutomationEvent> {
        self.0.events.subscribe()
    }
}

struct HostCommand {
    terminal: TerminalId,
    bytes: Vec<u8>,
}

struct SharedRun {
    text: String,
    baseline: usize,
    last_output: Instant,
    has_output: bool,
    cancelled: bool,
    status: RunStatus,
    current_line: usize,
    logs: Vec<ScriptLog>,
    error: Option<String>,
}

struct Run {
    id: u64,
    script_id: String,
    name: String,
    terminal: TerminalId,
    shared: Arc<(Mutex<SharedRun>, Condvar)>,
    rx: Mutex<Receiver<HostCommand>>,
}

impl Run {
    fn snapshot(&self) -> ScriptRunSnapshot {
        let (lock, _) = &*self.shared;
        let shared = lock.lock().unwrap();
        ScriptRunSnapshot {
            run_id: self.id,
            script_id: self.script_id.clone(),
            script_name: self.name.clone(),
            terminal: self.terminal,
            status: shared.status,
            current_line: shared.current_line,
            logs: shared.logs.clone(),
            error: shared.error.clone(),
        }
    }
}

struct AutomationState {
    workspace: Workspace,
    scripts_dir: PathBuf,
    events: broadcast::Sender<AutomationEvent>,
    runs: Mutex<HashMap<u64, Arc<Run>>>,
    next_run: AtomicU64,
}

impl AutomationState {
    fn new(workspace: Workspace, scripts_dir: PathBuf) -> Self {
        Self {
            workspace,
            scripts_dir,
            events: broadcast::channel(256).0,
            runs: Mutex::new(HashMap::new()),
            next_run: AtomicU64::new(1),
        }
    }

    fn scripts(&self) -> Vec<ScriptInfo> {
        let mut scripts = std::fs::read_dir(&self.scripts_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rhai"))
            .filter_map(|path| {
                let id = path.file_stem()?.to_str()?.to_string();
                Some(ScriptInfo {
                    name: id.clone(),
                    id,
                    path: path.to_string_lossy().into_owned(),
                })
            })
            .collect::<Vec<_>>();
        scripts.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        scripts
    }

    fn script_path(&self, id: &str) -> Option<PathBuf> {
        self.scripts()
            .into_iter()
            .find(|script| script.id == id)
            .map(|script| PathBuf::from(script.path))
    }

    fn script_source(&self, id: &str) -> Option<String> {
        let path = self.script_path(id)?;
        std::fs::read_to_string(path).ok()
    }

    fn create_script(&self, name: &str) -> std::result::Result<ScriptInfo, String> {
        let stem = sanitize_script_name(name);
        let mut index = 0;
        let path = loop {
            let candidate = if index == 0 {
                self.scripts_dir.join(format!("{stem}.rhai"))
            } else {
                self.scripts_dir.join(format!("{stem}-{index}.rhai"))
            };
            if !candidate.exists() {
                break candidate;
            }
            index += 1;
        };
        std::fs::write(&path, SAMPLE).map_err(|error| error.to_string())?;
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(&stem)
            .to_string();
        Ok(ScriptInfo {
            name: id.clone(),
            id,
            path: path.to_string_lossy().into_owned(),
        })
    }

    fn run(
        &self,
        script_id: &str,
        terminal: TerminalId,
    ) -> std::result::Result<ScriptRunSnapshot, String> {
        if self
            .runs
            .lock()
            .unwrap()
            .values()
            .any(|run| run.snapshot().status == RunStatus::Running)
        {
            return Err("A script is already running.".to_string());
        }
        let path = self
            .script_path(script_id)
            .ok_or_else(|| "Script not found.".to_string())?;
        let source = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let source = source
            .strip_prefix('\u{feff}')
            .unwrap_or(&source)
            .to_string();
        if source.len() > 1_000_000 {
            return Err("Script is larger than 1 MB.".to_string());
        }
        let run_id = self.next_run.fetch_add(1, Ordering::Relaxed);
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(script_id)
            .to_string();
        let shared = Arc::new((
            Mutex::new(SharedRun {
                text: String::new(),
                baseline: 0,
                last_output: Instant::now(),
                has_output: false,
                cancelled: false,
                status: RunStatus::Running,
                current_line: 0,
                logs: Vec::new(),
                error: None,
            }),
            Condvar::new(),
        ));
        let (command_tx, command_rx) = mpsc::channel();
        let run = Arc::new(Run {
            id: run_id,
            script_id: script_id.to_string(),
            name,
            terminal,
            shared: shared.clone(),
            rx: Mutex::new(command_rx),
        });
        self.runs.lock().unwrap().insert(run_id, run.clone());

        let host = Arc::new(HostApi {
            terminal,
            shared: shared.clone(),
            commands: command_tx,
            events: self.events.clone(),
            run_id,
        });
        let state_for_thread = shared.clone();
        let events = self.events.clone();
        thread::Builder::new()
            .name(format!("xcord-automation-{run_id}"))
            .spawn(move || {
                let mut engine = Engine::new_raw();
                let _ = engine.set_max_operations(20_000_000);
                let _ = engine.set_max_call_levels(64);
                register_host_functions(&mut engine, host.clone());
                let result = engine
                    .compile(&source)
                    .map_err(|error| error.to_string())
                    .and_then(|ast| engine.run_ast(&ast).map_err(|error| error.to_string()));
                let (lock, condvar) = &*state_for_thread;
                let mut shared = lock.lock().unwrap();
                if shared.cancelled {
                    shared.status = RunStatus::Stopped;
                } else {
                    match result {
                        Ok(()) => shared.status = RunStatus::Success,
                        Err(error) => {
                            shared.status = RunStatus::Failed;
                            shared.error = Some(error);
                        }
                    }
                }
                if let Some(error) = shared.error.clone() {
                    shared.logs.push(ScriptLog {
                        level: ScriptLogLevel::Error,
                        message: error,
                    });
                }
                condvar.notify_all();
                drop(shared);
                let _ = events.send(AutomationEvent::Changed { run_id });
            })
            .map_err(|error| error.to_string())?;

        self.emit(run_id);
        Ok(run.snapshot())
    }

    fn stop(&self, run_id: u64) {
        let Some(run) = self.runs.lock().unwrap().get(&run_id).cloned() else {
            return;
        };
        let (lock, condvar) = &*run.shared;
        let mut shared = lock.lock().unwrap();
        if shared.status == RunStatus::Running {
            shared.cancelled = true;
            shared.status = RunStatus::Stopped;
            shared.logs.push(ScriptLog {
                level: ScriptLogLevel::Warning,
                message: "Script stopped by user.".to_string(),
            });
        }
        condvar.notify_all();
        drop(shared);
        self.emit(run_id);
    }

    fn pump(&self) {
        let runs = self
            .runs
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for run in runs {
            let rx = run.rx.lock().unwrap();
            loop {
                match rx.try_recv() {
                    Ok(command) => {
                        let (lock, _) = &*run.shared;
                        let cancelled = lock.lock().unwrap().cancelled;
                        if !cancelled {
                            self.workspace.write(command.terminal, &command.bytes);
                        }
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
        }
    }

    fn update_terminal(&self, terminal: TerminalId, text: String) {
        let runs = self
            .runs
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for run in runs {
            if run.terminal != terminal {
                continue;
            }
            let (lock, condvar) = &*run.shared;
            let mut shared = lock.lock().unwrap();
            if shared.status != RunStatus::Running {
                continue;
            }
            shared.has_output = text.len() > shared.baseline;
            shared.text = text.clone();
            shared.last_output = Instant::now();
            condvar.notify_all();
        }
    }

    fn active_run(&self) -> Option<ScriptRunSnapshot> {
        self.runs
            .lock()
            .unwrap()
            .values()
            .map(|run| run.snapshot())
            .filter(|run| run.status == RunStatus::Running)
            .max_by_key(|run| run.run_id)
    }

    fn run_snapshot(&self, run_id: u64) -> Option<ScriptRunSnapshot> {
        self.runs
            .lock()
            .unwrap()
            .get(&run_id)
            .map(|run| run.snapshot())
    }

    fn emit(&self, run_id: u64) {
        let _ = self.events.send(AutomationEvent::Changed { run_id });
    }
}

struct HostApi {
    run_id: u64,
    terminal: TerminalId,
    shared: Arc<(Mutex<SharedRun>, Condvar)>,
    commands: Sender<HostCommand>,
    events: broadcast::Sender<AutomationEvent>,
}

impl HostApi {
    fn mark(&self, position: Position) {
        let Some(line) = position.line() else {
            return;
        };
        let (lock, _) = &*self.shared;
        let mut shared = lock.lock().unwrap();
        if shared.current_line != line {
            shared.current_line = line;
            drop(shared);
            let _ = self.events.send(AutomationEvent::Changed {
                run_id: self.run_id,
            });
        }
    }

    fn log(&self, level: ScriptLogLevel, message: impl Into<String>) {
        let (lock, _) = &*self.shared;
        let mut shared = lock.lock().unwrap();
        shared.logs.push(ScriptLog {
            level,
            message: message.into(),
        });
        drop(shared);
        let _ = self.events.send(AutomationEvent::Changed {
            run_id: self.run_id,
        });
    }

    fn begin_expect(&self) -> std::result::Result<(), Box<EvalAltResult>> {
        let (lock, _) = &*self.shared;
        let shared = lock.lock().unwrap();
        if shared.cancelled || shared.status != RunStatus::Running {
            return Err(runtime_error("script stopped"));
        }
        Ok(())
    }

    fn send(&self, bytes: Vec<u8>) -> std::result::Result<(), Box<EvalAltResult>> {
        self.begin_expect()?;
        let (lock, _) = &*self.shared;
        let mut shared = lock.lock().unwrap();
        shared.baseline = shared.text.len();
        shared.has_output = false;
        drop(shared);
        self.commands
            .send(HostCommand {
                terminal: self.terminal,
                bytes,
            })
            .map_err(|_| runtime_error("terminal is closed"))
    }

    fn send_line(&self, text: &str) -> std::result::Result<(), Box<EvalAltResult>> {
        self.begin_expect()?;
        {
            let (lock, _) = &*self.shared;
            let mut shared = lock.lock().unwrap();
            shared.baseline = shared.text.len();
            shared.has_output = false;
        }
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\r');
        self.commands
            .send(HostCommand {
                terminal: self.terminal,
                bytes,
            })
            .map_err(|_| runtime_error("terminal is closed"))
    }

    fn expect(
        &self,
        patterns: Vec<String>,
        timeout_ms: i64,
        regex: bool,
    ) -> std::result::Result<Map, Box<EvalAltResult>> {
        self.begin_expect()?;
        let timeout = Duration::from_millis(timeout_ms.max(0) as u64);
        let deadline = Instant::now() + timeout;
        let (lock, condvar) = &*self.shared;
        let mut shared = lock.lock().unwrap();
        loop {
            if shared.cancelled || shared.status != RunStatus::Running {
                return Err(runtime_error("script stopped"));
            }
            let mut baseline = shared.baseline.min(shared.text.len());
            if shared.text.len() < shared.baseline {
                baseline = 0;
            }
            let haystack = shared.text[baseline..].to_string();
            if let Some((pattern, start, end, text)) = find_match(&haystack, &patterns, regex) {
                let absolute_start = baseline + start;
                let absolute_end = baseline + end;
                shared.baseline = absolute_end;
                let before = tail(&shared.text[..absolute_start], 2000);
                let mut map = Map::new();
                map.insert("pattern".into(), Dynamic::from(pattern));
                map.insert("text".into(), Dynamic::from(text));
                map.insert("before".into(), Dynamic::from(before));
                return Ok(map);
            }
            let now = Instant::now();
            if now >= deadline {
                let tail_text = tail(&shared.text, 2000);
                return Err(runtime_error(format!(
                    "timeout waiting for {}; last output:\n{}",
                    patterns.join(" | "),
                    tail_text
                )));
            }
            let wait = (deadline - now).min(Duration::from_millis(100));
            let (next, _) = condvar.wait_timeout(shared, wait).unwrap();
            shared = next;
        }
    }

    fn wait_idle(
        &self,
        idle_ms: i64,
        timeout_ms: i64,
    ) -> std::result::Result<String, Box<EvalAltResult>> {
        self.begin_expect()?;
        let idle = Duration::from_millis(idle_ms.max(0) as u64);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);
        let (lock, condvar) = &*self.shared;
        let mut shared = lock.lock().unwrap();
        loop {
            if shared.cancelled || shared.status != RunStatus::Running {
                return Err(runtime_error("script stopped"));
            }
            if shared.has_output && shared.last_output.elapsed() >= idle {
                let text = shared.text[shared.baseline.min(shared.text.len())..].to_string();
                shared.baseline = shared.text.len();
                return Ok(text);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(runtime_error("timeout waiting for idle output"));
            }
            let wait = (deadline - now).min(Duration::from_millis(50));
            let (next, _) = condvar.wait_timeout(shared, wait).unwrap();
            shared = next;
        }
    }

    fn text_tail(&self, lines: usize) -> String {
        let (lock, _) = &*self.shared;
        let shared = lock.lock().unwrap();
        let all = shared.text.lines().collect::<Vec<_>>();
        all[all.len().saturating_sub(lines.max(1))..].join("\n")
    }

    fn sleep(&self, ms: i64) -> std::result::Result<(), Box<EvalAltResult>> {
        let until = Instant::now() + Duration::from_millis(ms.max(0) as u64);
        while Instant::now() < until {
            self.begin_expect()?;
            thread::sleep(
                Duration::from_millis(25).min(until.saturating_duration_since(Instant::now())),
            );
        }
        Ok(())
    }
}

fn register_host_functions(engine: &mut Engine, host: Arc<HostApi>) {
    let h = host.clone();
    engine.register_fn("log", move |ctx: NativeCallContext, message: String| {
        h.mark(ctx.call_position());
        h.log(ScriptLogLevel::Info, message);
    });
    let h = host.clone();
    engine.register_fn(
        "fail",
        move |ctx: NativeCallContext,
              message: String|
              -> std::result::Result<(), Box<EvalAltResult>> {
            h.mark(ctx.call_position());
            h.log(ScriptLogLevel::Error, message.clone());
            Err(runtime_error(message))
        },
    );
    let h = host.clone();
    engine.register_fn(
        "sleep",
        move |ctx: NativeCallContext, ms: i64| -> std::result::Result<(), Box<EvalAltResult>> {
            h.mark(ctx.call_position());
            h.sleep(ms)
        },
    );
    let h = host.clone();
    engine.register_fn(
        "send",
        move |ctx: NativeCallContext,
              text: String|
              -> std::result::Result<(), Box<EvalAltResult>> {
            h.mark(ctx.call_position());
            h.send(text.into_bytes())
        },
    );
    let h = host.clone();
    engine.register_fn(
        "send_line",
        move |ctx: NativeCallContext,
              text: String|
              -> std::result::Result<(), Box<EvalAltResult>> {
            h.mark(ctx.call_position());
            h.send_line(&text)
        },
    );
    let h = host.clone();
    engine.register_fn(
        "send_key",
        move |ctx: NativeCallContext, key: String| -> std::result::Result<(), Box<EvalAltResult>> {
            h.mark(ctx.call_position());
            let bytes = match key.as_str() {
                "enter" => b"\r".to_vec(),
                "tab" => b"\t".to_vec(),
                "escape" => b"\x1b".to_vec(),
                "ctrl+c" => vec![0x03],
                "ctrl+d" => vec![0x04],
                "up" => b"\x1b[A".to_vec(),
                "down" => b"\x1b[B".to_vec(),
                "right" => b"\x1b[C".to_vec(),
                "left" => b"\x1b[D".to_vec(),
                _ => return Err(runtime_error(format!("unsupported key: {key}"))),
            };
            h.send(bytes)
        },
    );
    let h = host.clone();
    engine.register_fn(
        "expect",
        move |ctx: NativeCallContext,
              pattern: String,
              timeout_ms: i64|
              -> std::result::Result<Map, Box<EvalAltResult>> {
            h.mark(ctx.call_position());
            h.expect(vec![pattern], timeout_ms, false)
        },
    );
    let h = host.clone();
    engine.register_fn(
        "expect_regex",
        move |ctx: NativeCallContext,
              pattern: String,
              timeout_ms: i64|
              -> std::result::Result<Map, Box<EvalAltResult>> {
            h.mark(ctx.call_position());
            h.expect(vec![pattern], timeout_ms, true)
        },
    );
    let h = host.clone();
    engine.register_fn(
        "expect_any",
        move |ctx: NativeCallContext,
              patterns: Array,
              timeout_ms: i64|
              -> std::result::Result<Map, Box<EvalAltResult>> {
            h.mark(ctx.call_position());
            let patterns = patterns
                .into_iter()
                .filter_map(|value| value.clone().try_cast::<String>())
                .collect::<Vec<_>>();
            h.expect(patterns, timeout_ms, false)
        },
    );
    let h = host.clone();
    engine.register_fn(
        "wait_idle",
        move |ctx: NativeCallContext,
              idle_ms: i64,
              timeout_ms: i64|
              -> std::result::Result<String, Box<EvalAltResult>> {
            h.mark(ctx.call_position());
            h.wait_idle(idle_ms, timeout_ms)
        },
    );
    let h = host.clone();
    engine.register_fn("read_screen", move |ctx: NativeCallContext| -> String {
        h.mark(ctx.call_position());
        h.text_tail(40)
    });
    let h = host.clone();
    engine.register_fn(
        "read_scrollback",
        move |ctx: NativeCallContext, lines: i64| -> String {
            h.mark(ctx.call_position());
            h.text_tail(lines.max(1) as usize)
        },
    );
}

fn find_match(
    haystack: &str,
    patterns: &[String],
    regex: bool,
) -> Option<(String, usize, usize, String)> {
    let mut best: Option<(String, usize, usize, String)> = None;
    for pattern in patterns {
        if regex {
            let Ok(regex) = regex::Regex::new(pattern) else {
                continue;
            };
            if let Some(found) = regex.find(haystack) {
                let candidate = (
                    pattern.clone(),
                    found.start(),
                    found.end(),
                    found.as_str().to_string(),
                );
                if best.as_ref().is_none_or(|best| candidate.1 < best.1) {
                    best = Some(candidate);
                }
            }
        } else if let Some(start) = haystack.find(pattern) {
            let end = start + pattern.len();
            let candidate = (pattern.clone(), start, end, pattern.clone());
            if best.as_ref().is_none_or(|best| candidate.1 < best.1) {
                best = Some(candidate);
            }
        }
    }
    best
}

fn tail(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

fn runtime_error(message: impl Into<String>) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(
        Dynamic::from(message.into()),
        Position::NONE,
    ))
}

fn sanitize_script_name(name: &str) -> String {
    let name = name.trim().trim_end_matches(".rhai");
    let mut result = String::new();
    for ch in name.chars() {
        if ch.is_alphanumeric() || matches!(ch, '_' | '-' | ' ' | '.') {
            result.push(ch);
        } else {
            result.push('_');
        }
    }
    let result = result.trim().to_string();
    if result.is_empty() {
        "script".to_string()
    } else {
        result
    }
}

fn scripts_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("xcord").join("scripts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rhai_send_and_expect_roundtrip() {
        let shared = Arc::new((
            Mutex::new(SharedRun {
                text: String::new(),
                baseline: 0,
                last_output: Instant::now(),
                has_output: false,
                cancelled: false,
                status: RunStatus::Running,
                current_line: 0,
                logs: Vec::new(),
                error: None,
            }),
            Condvar::new(),
        ));
        let (command_tx, command_rx) = mpsc::channel();
        let (events, _) = broadcast::channel(16);
        let host = Arc::new(HostApi {
            run_id: 1,
            terminal: TerminalId::new(1),
            shared: shared.clone(),
            commands: command_tx,
            events,
        });
        let host_for_thread = host.clone();
        let script = r#"
            send_line("echo hi");
            let m = expect("hi", 2000);
            log(m.pattern);
            if m.pattern == "hi" {
                log("branch");
            }
        "#;
        let handle = thread::spawn(move || -> std::result::Result<(), String> {
            let mut engine = Engine::new_raw();
            register_host_functions(&mut engine, host_for_thread);
            let ast = engine.compile(script).map_err(|error| error.to_string())?;
            engine.run_ast(&ast).map_err(|error| error.to_string())
        });
        let command = command_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("send_line command");
        assert_eq!(command.bytes, b"echo hi\r");
        {
            let (lock, condvar) = &*shared;
            let mut state = lock.lock().unwrap();
            state.text = "echo hi\r\nhi\r\n".into();
            state.has_output = true;
            state.last_output = Instant::now();
            condvar.notify_all();
        }
        assert!(handle.join().unwrap().is_ok());
        let (lock, _) = &*shared;
        let state = lock.lock().unwrap();
        assert!(state.logs.iter().any(|log| log.message == "hi"));
        assert!(state.logs.iter().any(|log| log.message == "branch"));
        assert!(state.current_line > 0);
    }
}
