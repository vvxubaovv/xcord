//! AI copilot plugin.
//!
//! Uses an OpenAI-compatible Chat Completions endpoint (OpenAI, DeepSeek,
//! Ollama, vLLM, ...) with streaming and function calling. Every tool call goes
//! through the same MCP registry exposed to external clients.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{
    services, Ai, AiAssistant, AiMessageView, AiSnapshot, Command, Commands, I18n, McpTools,
    PostOffice, Runtime, RuntimeHandle, Sessions, TerminalId, ToolSpec, Ui, UiCommand, Workspace,
    AUTOMATION_SYNTAX,
};

const INTERNAL_AI: &str = "ai/internal";
const MAX_TOOL_ROUNDS: usize = 64;
const MAX_TOOL_RESULT: usize = 14_000;
/// Keep long conversations inside the provider context window. Older user
/// turns are dropped only when the history exceeds this rough character budget.
const MAX_CONTEXT_CHARS: usize = 400_000;
const COMPACT_TOOL_RESULT: usize = 4_000;
/// Never discard the most recent 50 user turns, even when compacting tools.
const MIN_HISTORY_TURNS: usize = 50;

const AI_ZH: &[(&str, &str)] = &[
    ("Thinking…", "思考中…"),
    ("Running {tool}…", "正在运行 {tool}…"),
    (
        "No API key configured. Open AI settings and store a key.",
        "未配置 API Key。请打开 AI 设置并保存 Key。",
    ),
    ("The copilot is still responding.", "AI 仍在回复中。"),
    ("Prompt is empty.", "提示词为空。"),
    ("AI base URL is empty.", "AI 基础 URL 为空。"),
    ("AI model is empty.", "AI 模型为空。"),
    ("No API key — open settings", "未配置 API Key — 打开设置"),
];

#[derive(Clone)]
pub struct AiPlugin;

impl Plugin for AiPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("ai", "AI Copilot")
            .description("Streaming chat + MCP tool calling via OpenAI-compatible APIs")
            .author("xcord")
            .capabilities(&["ai", "mcp", "assistant"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[
            services::RUNTIME,
            services::SESSIONS,
            services::WORKSPACE,
            services::MCP_TOOLS,
            services::COMMANDS,
            services::UI_HOST,
            services::I18N,
        ]
    }

    fn apply(&self, ctx: Context, _config: Value) -> Result<()> {
        let runtime = ctx.require::<Runtime>(services::RUNTIME)?;
        let sessions = (*ctx.require::<Sessions>(services::SESSIONS)?).clone();
        let workspace = (*ctx.require::<Workspace>(services::WORKSPACE)?).clone();
        let tools = (*ctx.require::<McpTools>(services::MCP_TOOLS)?).clone();
        let commands = (*ctx.require::<Commands>(services::COMMANDS)?).clone();
        let ui = (*ctx.require::<Ui>(services::UI_HOST)?).clone();
        let i18n = (*ctx.require::<I18n>(services::I18N)?).clone();

        let translation_handle = i18n.register_pairs("ai", "zh-CN", AI_ZH);
        let translations = i18n.clone();
        ctx.effect("unregister ai translations", move || {
            translations.unregister(translation_handle);
        });

        let history_path = xcord_store::config_dir().join("ai-conversations.json");
        let history = load_history(&history_path);
        let service = Rc::new(AiService {
            sessions,
            workspace,
            tools,
            i18n,
            runtime: runtime.handle(),
            messages: RefCell::new(history.current),
            streaming: Cell::new(false),
            stream_text: RefCell::new(String::new()),
            status: RefCell::new(String::new()),
            error: RefCell::new(None),
            has_key: Cell::new(false),
            context_terminal: Cell::new(None),
            cancel: RefCell::new(None),
            history_path,
            archives: RefCell::new(history.conversations),
            context_key: RefCell::new(if history.current_key.is_empty() {
                "default".to_string()
            } else {
                history.current_key
            }),
            currents: RefCell::new(history.currents),
            pending_context: Cell::new(None),
        });
        service.refresh_key();

        let facade: Rc<dyn AiAssistant> = service.clone();
        ctx.provide(services::AI, Ai(facade))?;
        ctx.provide(INTERNAL_AI, service.clone())?;

        let mut handles = Vec::new();
        handles.push(
            commands.register(
                Command::new("ai.toggle", "Toggle AI copilot", "AI", {
                    let ui = ui.clone();
                    move |_| ui.send(UiCommand::ToggleAi)
                })
                .shortcut("Ctrl+Shift+I"),
            ),
        );
        handles.push(commands.register(Command::new(
            "ai.settings",
            "Configure AI copilot…",
            "AI",
            {
                let ui = ui.clone();
                move |_| ui.send(UiCommand::OpenAiSettings)
            },
        )));
        handles.push(
            commands.register(Command::new("ai.clear", "Clear AI conversation", "AI", {
                let service = service.clone();
                move |_| service.clear()
            })),
        );

        ctx.effect("unregister ai commands", move || {
            for handle in handles {
                commands.unregister(handle);
            }
        });

        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ApiToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl ApiMessage {
    fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    fn assistant(content: Option<String>, tool_calls: Vec<ApiToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content,
            tool_calls,
            tool_call_id: None,
        }
    }

    fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApiToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: FunctionCall,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FunctionCall {
    name: String,
    arguments: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ConversationRecord {
    id: String,
    title: String,
    updated: String,
    #[serde(default)]
    key: String,
    messages: Vec<ApiMessage>,
}

#[derive(Default, Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default)]
    current: Vec<ApiMessage>,
    #[serde(default)]
    current_key: String,
    #[serde(default)]
    currents: HashMap<String, Vec<ApiMessage>>,
    #[serde(default)]
    conversations: Vec<ConversationRecord>,
}

fn load_history(path: &PathBuf) -> HistoryFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn conversation_title(messages: &[ApiMessage]) -> String {
    messages
        .iter()
        .find(|message| message.role == "user")
        .and_then(|message| message.content.as_deref())
        .map(|text| {
            let line = text.lines().next().unwrap_or("").trim();
            let line = if line.is_empty() { text.trim() } else { line };
            line.chars().take(60).collect::<String>()
        })
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "Conversation".to_string())
}

enum AiEvent {
    Delta(String),
    Status(String),
    Progress(Vec<ApiMessage>),
    Finish(Vec<ApiMessage>),
    Cancelled(Vec<ApiMessage>),
    Error(String),
}

#[derive(Clone)]
struct CancelHandle {
    sender: Arc<tokio::sync::watch::Sender<bool>>,
}

impl CancelHandle {
    fn new() -> Self {
        let (sender, _) = tokio::sync::watch::channel(false);
        Self {
            sender: Arc::new(sender),
        }
    }

    fn cancel(&self) {
        let _ = self.sender.send(true);
    }

    fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }

    fn receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.sender.subscribe()
    }
}

pub struct AiService {
    sessions: Sessions,
    workspace: Workspace,
    tools: McpTools,
    i18n: I18n,
    runtime: RuntimeHandle,
    messages: RefCell<Vec<ApiMessage>>,
    streaming: Cell<bool>,
    stream_text: RefCell<String>,
    status: RefCell<String>,
    error: RefCell<Option<String>>,
    has_key: Cell<bool>,
    context_terminal: Cell<Option<TerminalId>>,
    cancel: RefCell<Option<CancelHandle>>,
    history_path: PathBuf,
    archives: RefCell<Vec<ConversationRecord>>,
    context_key: RefCell<String>,
    currents: RefCell<HashMap<String, Vec<ApiMessage>>>,
    pending_context: Cell<Option<TerminalId>>,
}

impl AiService {
    fn api_key(&self) -> Option<String> {
        self.sessions.ai_api_key().or_else(|| {
            for name in ["XCORD_AI_API_KEY", "DEEPSEEK_API_KEY", "OPENAI_API_KEY"] {
                if let Ok(value) = std::env::var(name) {
                    if !value.trim().is_empty() {
                        return Some(value);
                    }
                }
            }
            None
        })
    }

    fn push_delta(&self, text: &str) {
        let mut stream = self.stream_text.borrow_mut();
        stream.push_str(text);
        if normalize_dsml_markup(&stream).contains("<|DSML|") {
            // Do not flash raw DSML tool-call markup in the chat bubble.
            stream.clear();
        }
    }

    fn apply_event(&self, event: AiEvent) {
        match event {
            AiEvent::Delta(text) => self.push_delta(&text),
            AiEvent::Status(status) => *self.status.borrow_mut() = status,
            AiEvent::Progress(messages) => {
                *self.messages.borrow_mut() = messages;
                self.stream_text.borrow_mut().clear();
                self.persist_history();
            }
            AiEvent::Finish(messages) => {
                *self.messages.borrow_mut() = messages;
                self.stream_text.borrow_mut().clear();
                self.streaming.set(false);
                self.status.borrow_mut().clear();
                *self.error.borrow_mut() = None;
                self.cancel.borrow_mut().take();
                self.persist_history();
                self.apply_pending_context();
            }
            AiEvent::Cancelled(messages) => {
                *self.messages.borrow_mut() = messages;
                let partial = std::mem::take(&mut *self.stream_text.borrow_mut());
                if !partial.trim().is_empty() {
                    self.messages
                        .borrow_mut()
                        .push(ApiMessage::assistant(Some(partial), Vec::new()));
                }
                self.streaming.set(false);
                self.status.borrow_mut().clear();
                *self.error.borrow_mut() = None;
                self.cancel.borrow_mut().take();
                self.persist_history();
                self.apply_pending_context();
            }
            AiEvent::Error(error) => {
                let partial = std::mem::take(&mut *self.stream_text.borrow_mut());
                if !partial.is_empty() {
                    self.messages
                        .borrow_mut()
                        .push(ApiMessage::assistant(Some(partial), Vec::new()));
                }
                *self.error.borrow_mut() = Some(error);
                self.streaming.set(false);
                self.status.borrow_mut().clear();
                self.persist_history();
                self.apply_pending_context();
            }
        }
    }

    fn context_key_for(&self, id: Option<TerminalId>) -> String {
        let Some(id) = id else {
            return "default".to_string();
        };
        self.workspace
            .tabs()
            .into_iter()
            .find(|tab| tab.id == id)
            .map(|tab| {
                if tab.profile_id.trim().is_empty() {
                    format!("tab:{}", id.raw())
                } else {
                    format!("profile:{}", tab.profile_id)
                }
            })
            .unwrap_or_else(|| format!("tab:{}", id.raw()))
    }

    fn switch_context(&self, id: Option<TerminalId>) {
        let next_key = self.context_key_for(id);
        let current_key = self.context_key.borrow().clone();
        if next_key == current_key {
            self.context_terminal.set(id);
            return;
        }
        let mut currents = self.currents.borrow_mut();
        let current_messages = self.messages.borrow().clone();
        // Migrate the pre-per-connection history to the first connection the
        // user opens instead of leaving it stranded under "default".
        let migrate_default = current_key == "default"
            && !currents.contains_key(&next_key)
            && !current_messages.is_empty();
        if !current_key.is_empty() && !migrate_default {
            currents.insert(current_key, current_messages);
        }
        let next_messages = if migrate_default {
            self.messages.borrow().clone()
        } else {
            currents.remove(&next_key).unwrap_or_default()
        };
        drop(currents);
        *self.messages.borrow_mut() = next_messages;
        *self.context_key.borrow_mut() = next_key;
        self.context_terminal.set(id);
        self.stream_text.borrow_mut().clear();
        *self.error.borrow_mut() = None;
        self.status.borrow_mut().clear();
        self.persist_history();
    }

    fn apply_pending_context(&self) {
        if let Some(id) = self.pending_context.take() {
            self.switch_context(Some(id));
        }
    }

    fn persist_history(&self) {
        let history = HistoryFile {
            current: self.messages.borrow().clone(),
            current_key: self.context_key.borrow().clone(),
            currents: self.currents.borrow().clone(),
            conversations: self.archives.borrow().clone(),
        };
        let Ok(text) = serde_json::to_string_pretty(&history) else {
            return;
        };
        if let Some(parent) = self.history_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.history_path, text);
    }

    fn archive_current(&self) -> bool {
        let messages = self.messages.borrow().clone();
        if messages.is_empty() {
            return false;
        }
        let record = ConversationRecord {
            id: uuid::Uuid::new_v4().to_string(),
            title: conversation_title(&messages),
            updated: chrono::Utc::now().to_rfc3339(),
            key: self.context_key.borrow().clone(),
            messages,
        };
        let mut archives = self.archives.borrow_mut();
        archives.insert(0, record);
        archives.truncate(50);
        true
    }

    fn terminal_context(&self) -> Option<String> {
        let settings = self.sessions.settings();
        if !settings.ai_include_terminal {
            return None;
        }
        let tabs = self.workspace.tabs();
        let id = self
            .context_terminal
            .get()
            .filter(|id| tabs.iter().any(|tab| tab.id == *id))
            .or_else(|| self.workspace.active())
            .filter(|id| tabs.iter().any(|tab| tab.id == *id))?;
        let tab = tabs.into_iter().find(|tab| tab.id == id)?;
        let text = self.workspace.terminal_text(id, 220)?;
        let text = if text.len() > 9000 {
            let mut start = text.len() - 9000;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            &text[start..]
        } else {
            &text
        };
        Some(format!(
            "{}: id={} title={:?} {}@{}:{} status={}.\nRecent terminal output (may include stale text):\n```\n{}\n```",
            "Current active tab",
            tab.id.raw(),
            tab.title,
            tab.user,
            tab.host,
            tab.port,
            tab.status.label(),
            text
        ))
    }

    fn spawn_request(&self, prompt: String) -> std::result::Result<(), String> {
        if self.streaming.get() {
            return Err(self.i18n.t("ai", "The copilot is still responding."));
        }
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return Err(self.i18n.t("ai", "Prompt is empty."));
        }
        let settings = self.sessions.settings();
        let Some(key) = self.api_key() else {
            let message = self.i18n.t(
                "ai",
                "No API key configured. Open AI settings and store a key.",
            );
            *self.error.borrow_mut() = Some(message.clone());
            return Err(message);
        };
        let base_url = settings
            .ai_base_url
            .trim()
            .trim_end_matches('/')
            .to_string();
        if base_url.is_empty() {
            let message = self.i18n.t("ai", "AI base URL is empty.");
            *self.error.borrow_mut() = Some(message.clone());
            return Err(message);
        }
        let url = if base_url.ends_with("/chat/completions") {
            base_url
        } else {
            format!("{base_url}/chat/completions")
        };
        let model = settings.ai_model.trim().to_string();
        if model.is_empty() {
            return Err(self.i18n.t("ai", "AI model is empty."));
        }

        {
            let mut messages = self.messages.borrow_mut();
            messages.push(ApiMessage::user(prompt));
        }
        self.persist_history();
        let cancel_handle = CancelHandle::new();
        *self.cancel.borrow_mut() = Some(cancel_handle.clone());
        self.streaming.set(true);
        self.stream_text.borrow_mut().clear();
        *self.status.borrow_mut() = self.i18n.t("ai", "Thinking…");
        *self.error.borrow_mut() = None;

        let mut system = settings.ai_system_prompt.clone();
        system.push_str("\n\nAutomation script context (Rhai):\n");
        system.push_str(AUTOMATION_SYNTAX);
        system.push_str(
            "\nAutomation tools are available through MCP. Use automation_list_scripts and              automation_read_script before editing. Use automation_write_script or              automation_create_script only when writes are allowed, and preserve the user's intent.              After changing a script, the editor reloads automatically.",
        );
        if let Some(context) = self.terminal_context() {
            system.push_str("\n\n");
            system.push_str(&context);
        }
        let tools = self.tools.list();
        let allow_writes = settings.ai_allow_writes;
        let history = self.messages.borrow().clone();
        let office = self.runtime.office.clone();
        let api_key = key;
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let result = run_chat(
                office.clone(),
                api_key,
                url,
                model,
                system,
                history,
                tools,
                allow_writes,
                cancel_handle,
            )
            .await;
            if let Err(error) = result {
                let office = office.clone();
                let _ = office.post(move |ctx| {
                    if let Some(ai) = ctx.service::<Rc<AiService>>(INTERNAL_AI) {
                        ai.apply_event(AiEvent::Error(error));
                    }
                });
            }
        });
        Ok(())
    }
}

impl AiAssistant for AiService {
    fn snapshot(&self) -> AiSnapshot {
        let settings = self.sessions.settings();
        let mut messages = Vec::new();
        for message in self.messages.borrow().iter() {
            match message.role.as_str() {
                "user" => {
                    if let Some(content) = &message.content {
                        messages.push(AiMessageView {
                            role: "You".into(),
                            text: content.clone(),
                            tool: None,
                        });
                    }
                }
                "assistant" => {
                    if let Some(content) = &message.content {
                        if !content.is_empty() {
                            messages.push(AiMessageView {
                                role: "AI".into(),
                                text: content.clone(),
                                tool: None,
                            });
                        }
                    }
                    for call in &message.tool_calls {
                        messages.push(AiMessageView {
                            role: "Tool call".into(),
                            text: format!(
                                "{}\n{}",
                                call.function.name,
                                pretty_arguments(&call.function.arguments)
                            ),
                            tool: Some(call.function.name.clone()),
                        });
                    }
                }
                "tool" => {
                    if let Some(content) = &message.content {
                        messages.push(AiMessageView {
                            role: "Tool result".into(),
                            text: truncate(content, 2400),
                            tool: None,
                        });
                    }
                }
                _ => {}
            }
        }
        if self.streaming.get() {
            let partial = self.stream_text.borrow().clone();
            if !partial.is_empty() {
                messages.push(AiMessageView {
                    role: "AI".into(),
                    text: partial,
                    tool: None,
                });
            }
        }
        let tabs = self.workspace.tabs();
        let explicit = self
            .context_terminal
            .get()
            .filter(|id| tabs.iter().any(|tab| tab.id == *id));
        if self.context_terminal.get().is_some() && explicit.is_none() {
            self.context_terminal.set(None);
        }
        let context = self
            .context_terminal
            .get()
            .filter(|id| tabs.iter().any(|tab| tab.id == *id))
            .or_else(|| {
                self.workspace
                    .active()
                    .filter(|id| tabs.iter().any(|tab| tab.id == *id))
            });
        let context_title = context
            .and_then(|id| tabs.into_iter().find(|tab| tab.id == id))
            .map(|tab| tab.title);
        AiSnapshot {
            messages,
            streaming: self.streaming.get(),
            status: self.status.borrow().clone(),
            error: self.error.borrow().clone(),
            model: settings.ai_model,
            base_url: settings.ai_base_url,
            has_key: self.has_key.get(),
            allow_writes: settings.ai_allow_writes,
            include_terminal: settings.ai_include_terminal,
            context_terminal: context,
            context_title,
            context_follows_active: true,
            conversations: self
                .archives
                .borrow()
                .iter()
                .filter(|conversation| {
                    conversation.key.is_empty() || conversation.key == *self.context_key.borrow()
                })
                .map(|conversation| xcord_plugin_api::AiConversationView {
                    id: conversation.id.clone(),
                    title: conversation.title.clone(),
                    updated: conversation.updated.clone(),
                    message_count: conversation.messages.len(),
                })
                .collect(),
        }
    }

    fn set_context_terminal(&self, id: Option<TerminalId>) {
        if self.streaming.get() {
            if self.context_terminal.get() != id {
                self.pending_context.set(id);
                self.cancel();
            }
            return;
        }
        self.switch_context(id);
    }

    fn context_terminal(&self) -> Option<TerminalId> {
        self.context_terminal.get()
    }

    fn send(&self, prompt: &str) -> std::result::Result<(), String> {
        self.spawn_request(prompt.to_string())
    }

    fn cancel(&self) {
        if !self.streaming.get() {
            return;
        }
        if let Some(handle) = self.cancel.borrow().as_ref() {
            handle.cancel();
        }
        *self.status.borrow_mut() = self.i18n.t("ai", "Stopping…");
    }

    fn new_conversation(&self) -> std::result::Result<(), String> {
        if self.streaming.get() {
            return Err(self.i18n.t("ai", "Stop the current response first."));
        }
        self.archive_current();
        self.messages.borrow_mut().clear();
        self.stream_text.borrow_mut().clear();
        *self.error.borrow_mut() = None;
        self.status.borrow_mut().clear();
        self.persist_history();
        Ok(())
    }

    fn open_conversation(&self, id: &str) -> std::result::Result<(), String> {
        if self.streaming.get() {
            return Err(self.i18n.t("ai", "Stop the current response first."));
        }
        let key = self.context_key.borrow().clone();
        if !self.archives.borrow().iter().any(|conversation| {
            conversation.id == id && (conversation.key.is_empty() || conversation.key == key)
        }) {
            return Err(self.i18n.t("ai", "Conversation not found."));
        }
        self.archive_current();
        let index = self
            .archives
            .borrow()
            .iter()
            .position(|conversation| conversation.id == id)
            .ok_or_else(|| self.i18n.t("ai", "Conversation not found."))?;
        let record = self.archives.borrow_mut().remove(index);
        *self.messages.borrow_mut() = record.messages;
        self.stream_text.borrow_mut().clear();
        *self.error.borrow_mut() = None;
        self.status.borrow_mut().clear();
        self.persist_history();
        Ok(())
    }

    fn clear(&self) {
        self.cancel.borrow_mut().take();
        self.messages.borrow_mut().clear();
        self.stream_text.borrow_mut().clear();
        *self.error.borrow_mut() = None;
        self.status.borrow_mut().clear();
        self.streaming.set(false);
    }

    fn refresh_key(&self) {
        self.has_key.set(self.api_key().is_some());
    }
}

fn normalize_dsml_markup(input: &str) -> String {
    let chars = input.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(input.len());
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '<' {
            let mut cursor = index + 1;
            while cursor < chars.len() && chars[cursor].is_whitespace() {
                cursor += 1;
            }
            let closing = chars.get(cursor) == Some(&'/');
            if closing {
                cursor += 1;
                while cursor < chars.len() && chars[cursor].is_whitespace() {
                    cursor += 1;
                }
            }
            if chars
                .get(cursor)
                .is_some_and(|ch| *ch == '|' || *ch == '｜')
            {
                cursor += 1;
                while cursor < chars.len() && chars[cursor].is_whitespace() {
                    cursor += 1;
                }
                let start = cursor;
                while cursor < chars.len() && chars[cursor].is_ascii_alphabetic() {
                    cursor += 1;
                }
                let token = chars[start..cursor].iter().collect::<String>();
                while cursor < chars.len() && chars[cursor].is_whitespace() {
                    cursor += 1;
                }
                if token.eq_ignore_ascii_case("DSML")
                    && chars
                        .get(cursor)
                        .is_some_and(|ch| *ch == '|' || *ch == '｜')
                {
                    output.push_str(if closing { "</|DSML|" } else { "<|DSML|" });
                    index = cursor + 1;
                    continue;
                }
            }
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

fn dsml_attribute(attrs: &str, key: &str) -> Option<String> {
    let pattern = format!("{key}=\"");
    let start = attrs.find(&pattern)? + pattern.len();
    let end = attrs[start..].find('"')? + start;
    Some(attrs[start..end].to_string())
}

fn dsml_parameter_value(value: &str, string: bool) -> Value {
    let trimmed = value.trim();
    if string {
        return Value::String(value.to_string());
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| Value::String(trimmed.to_string()))
}

/// DeepSeek V4 may emit tool calls as DSML markup in the text stream instead of
/// structured OpenAI `tool_calls`. Parse that markup so those calls still run.
fn parse_dsml_tool_calls(raw: &str) -> Option<(String, Vec<ApiToolCall>)> {
    let normalized = normalize_dsml_markup(raw);
    let marker = "<|DSML|";
    if !normalized.contains(marker) {
        return None;
    }
    let (start_tag, end_tag) = if normalized.contains("<|DSML| calls>") {
        ("<|DSML| calls>", "</|DSML| calls>")
    } else if normalized.contains("<|DSML|calls>") {
        ("<|DSML|calls>", "</|DSML|calls>")
    } else if normalized.contains("<|DSML| tool_calls>") {
        ("<|DSML| tool_calls>", "</|DSML| tool_calls>")
    } else if normalized.contains("<|DSML|tool_calls>") {
        ("<|DSML|tool_calls>", "</|DSML|tool_calls>")
    } else {
        return None;
    };
    let block_start = normalized.find(start_tag)?;
    let after_start = block_start + start_tag.len();
    let block_end = normalized[after_start..].find(end_tag)? + after_start + end_tag.len();
    let block = &normalized[after_start..block_end - end_tag.len()];

    let mut calls = Vec::new();
    let invoke_marker = format!("{marker} invoke");
    let invoke_marker_compact = format!("{marker}invoke");
    let mut cursor = 0usize;
    while let Some((relative, marker_len)) = block[cursor..]
        .find(&invoke_marker)
        .map(|relative| (relative, invoke_marker.len()))
        .or_else(|| {
            block[cursor..]
                .find(&invoke_marker_compact)
                .map(|relative| (relative, invoke_marker_compact.len()))
        })
    {
        let invoke_start = cursor + relative;
        let open_end = block[invoke_start..].find('>')? + invoke_start + 1;
        let attrs = &block[invoke_start + marker_len..open_end - 1];
        let name = dsml_attribute(attrs, "name")?;
        let close = block[open_end..].find("</|DSML| invoke>")? + open_end;
        let body = &block[open_end..close];
        let mut arguments = serde_json::Map::new();
        let param_marker = format!("{marker} parameter");
        let param_marker_compact = format!("{marker}parameter");
        let mut param_cursor = 0usize;
        while let Some((relative_param, marker_len)) = body[param_cursor..]
            .find(&param_marker)
            .map(|relative| (relative, param_marker.len()))
            .or_else(|| {
                body[param_cursor..]
                    .find(&param_marker_compact)
                    .map(|relative| (relative, param_marker_compact.len()))
            })
        {
            let param_start = param_cursor + relative_param;
            let Some(param_open_end_rel) = body[param_start..].find('>') else {
                break;
            };
            let param_open_end = param_start + param_open_end_rel + 1;
            let param_attrs = &body[param_start + marker_len..param_open_end - 1];
            let Some(param_name) = dsml_attribute(param_attrs, "name") else {
                break;
            };
            let Some(param_close_rel) = body[param_open_end..].find("</|DSML| parameter>") else {
                break;
            };
            let param_close = param_open_end + param_close_rel;
            let string = dsml_attribute(param_attrs, "string")
                .map(|value| value == "true")
                .unwrap_or(true);
            arguments.insert(
                param_name,
                dsml_parameter_value(&body[param_open_end..param_close], string),
            );
            param_cursor = param_close + "</|DSML| parameter>".len();
        }
        let id = format!("dsml-call-{}", calls.len() + 1);
        calls.push(ApiToolCall {
            id: id.clone(),
            kind: "function".into(),
            function: FunctionCall {
                name,
                arguments: Value::Object(arguments).to_string(),
            },
        });
        cursor = close + "</|DSML| invoke>".len();
    }
    let mut cleaned = normalized;
    cleaned.replace_range(block_start..block_end, "");
    Some((cleaned.trim().to_string(), calls))
}

fn api_message_chars(message: &ApiMessage) -> usize {
    let mut total = message.content.as_ref().map(String::len).unwrap_or(0);
    for call in &message.tool_calls {
        total += call.function.name.len() + call.function.arguments.len();
    }
    total
}

/// Drop the oldest complete user turns until the request fits the context
/// budget. A user turn is a `user` message plus the assistant/tool messages
/// that follow it, so the provider never sees orphaned tool results.
fn compact_history(mut messages: Vec<ApiMessage>) -> Vec<ApiMessage> {
    let user_starts = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == "user")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    // Drop only turns older than the retention window. Keeping 50 user turns
    // is a hard product requirement; the remaining content is compacted below.
    let excess = user_starts.len().saturating_sub(MIN_HISTORY_TURNS);
    if excess > 0 && messages.iter().map(api_message_chars).sum::<usize>() > MAX_CONTEXT_CHARS {
        let cut = user_starts.get(excess).copied().unwrap_or(messages.len());
        messages.drain(0..cut);
    }

    if messages.iter().map(api_message_chars).sum::<usize>() > MAX_CONTEXT_CHARS {
        for message in &mut messages {
            match message.role.as_str() {
                "tool" | "assistant" | "user" => {
                    if let Some(content) = &mut message.content {
                        *content = truncate(content, COMPACT_TOOL_RESULT);
                    }
                }
                _ => {}
            }
        }
    }
    messages
}

async fn run_chat(
    office: PostOffice,
    api_key: String,
    url: String,
    model: String,
    system: String,
    mut messages: Vec<ApiMessage>,
    tools: Vec<ToolSpec>,
    allow_writes: bool,
    cancel: CancelHandle,
) -> std::result::Result<(), String> {
    let mut cancel_rx = cancel.receiver();
    if messages.len() > 1 {
        messages = compact_history(messages);
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|error| error.to_string())?;

    let tool_payload: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema
                }
            })
        })
        .collect();

    for _round in 0..MAX_TOOL_ROUNDS {
        if cancel.is_cancelled() {
            post_event(&office, AiEvent::Cancelled(messages));
            return Ok(());
        }
        if messages.len() > 1 {
            messages = compact_history(messages);
        }
        let mut request_messages = Vec::with_capacity(messages.len() + 1);
        request_messages.push(json!({"role": "system", "content": system}));
        for message in &messages {
            request_messages.push(serde_json::to_value(message).map_err(|e| e.to_string())?);
        }
        let mut body = json!({
            "model": model,
            "messages": request_messages,
            "stream": true,
            "temperature": 0.2
        });
        if !tool_payload.is_empty() {
            body["tools"] = Value::Array(tool_payload.clone());
            body["tool_choice"] = Value::String("auto".into());
        }

        post_status(&office, "Thinking…");
        let request = client.post(&url).bearer_auth(&api_key).json(&body).send();
        let response = tokio::select! {
            result = request => result.map_err(|error| format!("request failed: {error}"))?,
            _ = cancel_rx.changed() => {
                post_event(&office, AiEvent::Cancelled(messages));
                return Ok(());
            }
        };
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(format!(
                "provider returned {status}: {}",
                truncate(&text, 800)
            ));
        }

        let result = tokio::select! {
            result = read_response(response, &office) => result?,
            _ = cancel_rx.changed() => {
                post_event(&office, AiEvent::Cancelled(messages));
                return Ok(());
            }
        };
        let mut content_text = result.content;
        let mut tool_calls = result.tool_calls;
        if tool_calls.is_empty() {
            if let Some((cleaned, parsed)) = parse_dsml_tool_calls(&content_text) {
                content_text = cleaned;
                tool_calls = parsed;
            } else {
                let normalized = normalize_dsml_markup(&content_text);
                if let Some(position) = normalized.find("<|DSML|") {
                    content_text = normalized[..position].trim().to_string();
                }
            }
        }
        let content = if content_text.trim().is_empty() {
            None
        } else {
            Some(content_text)
        };

        if tool_calls.is_empty() {
            messages.push(ApiMessage::assistant(content, Vec::new()));
            post_finish(&office, messages);
            return Ok(());
        }

        messages.push(ApiMessage::assistant(content, tool_calls.clone()));
        post_progress(&office, &messages);

        for call in tool_calls {
            let tool_name = call.function.name.clone();
            post_status(&office, format!("Running {tool_name}…"));
            let read_only = tools
                .iter()
                .find(|tool| tool.name == tool_name)
                .map(|tool| tool.read_only)
                .unwrap_or(false);
            let arguments: Value = if call.function.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.function.arguments)
                    .unwrap_or_else(|_| json!({ "__raw": call.function.arguments }))
            };
            let output = if !read_only && !allow_writes {
                Err(format!(
                    "tool `{tool_name}` changes state and terminal writes are disabled; ask the user to enable them"
                ))
            } else {
                tokio::select! {
                    result = call_tool(office.clone(), tool_name.clone(), arguments) => result,
                    _ = cancel_rx.changed() => {
                        post_event(&office, AiEvent::Cancelled(messages));
                        return Ok(());
                    }
                }
            };
            let text = match output {
                Ok(value) => value_to_text(value),
                Err(error) => format!("ERROR: {error}"),
            };
            messages.push(ApiMessage::tool(call.id, truncate(&text, MAX_TOOL_RESULT)));
            post_progress(&office, &messages);
        }
    }

    // If the model kept requesting tools, make one final request without
    // tools so the user still gets an answer grounded in the collected results.
    let final_system = format!(
        "{system}\n\nTool budget is exhausted. Do not call any tools now.          Give the user the best final answer using the tool results above.          If information is missing, say so explicitly and suggest the next step."
    );
    let mut final_messages = Vec::with_capacity(messages.len() + 1);
    final_messages.push(json!({"role": "system", "content": final_system}));
    for message in &messages {
        final_messages.push(serde_json::to_value(message).map_err(|e| e.to_string())?);
    }
    let body = json!({
        "model": model,
        "messages": final_messages,
        "stream": true,
        "temperature": 0.2
    });
    post_status(&office, "Finalizing answer…");
    let request = client.post(&url).bearer_auth(&api_key).json(&body).send();
    let response = tokio::select! {
        result = request => result.map_err(|error| format!("final answer request failed: {error}"))?,
        _ = cancel_rx.changed() => {
            post_event(&office, AiEvent::Cancelled(messages));
            return Ok(());
        }
    };
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!(
            "provider returned {status} while finalizing: {}",
            truncate(&text, 800)
        ));
    }
    let result = tokio::select! {
        result = read_response(response, &office) => result?,
        _ = cancel_rx.changed() => {
            post_event(&office, AiEvent::Cancelled(messages));
            return Ok(());
        }
    };
    let final_content = parse_dsml_tool_calls(&result.content)
        .map(|(cleaned, _)| cleaned)
        .unwrap_or(result.content)
        .trim()
        .to_string();
    if final_content.is_empty() {
        return Err("The model did not produce a final answer.".to_string());
    }
    messages.push(ApiMessage::assistant(Some(final_content), Vec::new()));
    post_finish(&office, messages);
    Ok(())
}

struct StreamResult {
    content: String,
    tool_calls: Vec<ApiToolCall>,
}

#[derive(Default)]
struct ToolCallAccum {
    id: String,
    name: String,
    arguments: String,
}

async fn read_response(
    response: reqwest::Response,
    office: &PostOffice,
) -> std::result::Result<StreamResult, String> {
    let is_sse = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.contains("text/event-stream"))
        .unwrap_or(false);

    if !is_sse {
        let value: Value = response
            .json()
            .await
            .map_err(|error| format!("invalid JSON response: {error}"))?;
        return parse_full_response(&value);
    }

    let mut content = String::new();
    let mut calls: Vec<ToolCallAccum> = Vec::new();
    let mut buffer = String::new();
    let mut stream = response.bytes_stream();
    let mut last_emit = Instant::now();
    let mut pending = String::new();
    let mut done = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("stream error: {error}"))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(position) = buffer.find("\n\n") {
            let event = buffer[..position].replace('\r', "");
            buffer = buffer[position + 2..].to_string();
            for line in event.lines() {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    done = true;
                    break;
                }
                let Value::Object(map) = serde_json::from_str::<Value>(data).unwrap_or(Value::Null)
                else {
                    continue;
                };
                let Some(choice) = map
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|choices| choices.first())
                else {
                    continue;
                };
                let Some(delta) = choice.get("delta") else {
                    continue;
                };
                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                    content.push_str(text);
                    pending.push_str(text);
                }
                if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for call in tool_calls {
                        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        while calls.len() <= index {
                            calls.push(ToolCallAccum::default());
                        }
                        let slot = &mut calls[index];
                        if let Some(id) = call.get("id").and_then(Value::as_str) {
                            slot.id = id.to_string();
                        }
                        if let Some(function) = call.get("function") {
                            if let Some(name) = function.get("name").and_then(Value::as_str) {
                                slot.name.push_str(name);
                            }
                            if let Some(arguments) =
                                function.get("arguments").and_then(Value::as_str)
                            {
                                slot.arguments.push_str(arguments);
                            }
                        }
                    }
                }
            }
            if !pending.is_empty() && last_emit.elapsed() >= Duration::from_millis(40) {
                post_delta(office, std::mem::take(&mut pending));
                last_emit = Instant::now();
            }
            if done {
                break;
            }
        }
        if done {
            break;
        }
    }
    if !pending.is_empty() {
        post_delta(office, pending);
    }

    let tool_calls = calls
        .into_iter()
        .filter(|call| !call.name.is_empty())
        .map(|call| ApiToolCall {
            id: if call.id.is_empty() {
                format!("call_{}", uuid::Uuid::new_v4().simple())
            } else {
                call.id
            },
            kind: "function".into(),
            function: FunctionCall {
                name: call.name,
                arguments: call.arguments,
            },
        })
        .collect();

    Ok(StreamResult {
        content,
        tool_calls,
    })
}

fn parse_full_response(value: &Value) -> std::result::Result<StreamResult, String> {
    let message = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .ok_or_else(|| {
            format!(
                "unexpected provider response: {}",
                truncate(&value.to_string(), 600)
            )
        })?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    let function = call.get("function")?;
                    Some(ApiToolCall {
                        id: call
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("call")
                            .to_string(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: function.get("name")?.as_str()?.to_string(),
                            arguments: function
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}")
                                .to_string(),
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(StreamResult {
        content,
        tool_calls,
    })
}

async fn call_tool(
    office: PostOffice,
    name: String,
    args: Value,
) -> std::result::Result<Value, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let posted = office.post(move |ctx| {
        let result = ctx
            .service::<McpTools>(services::MCP_TOOLS)
            .ok_or_else(|| "MCP tool registry is unavailable".to_string())
            .and_then(|tools| tools.call(ctx, &name, args));
        let _ = tx.send(result);
    });
    if !posted {
        return Err("UI is unavailable".into());
    }
    rx.await
        .map_err(|_| "tool call was cancelled".to_string())?
}

fn post_event(office: &PostOffice, event: AiEvent) {
    let _ = office.post(move |ctx| {
        if let Some(ai) = ctx.service::<Rc<AiService>>(INTERNAL_AI) {
            ai.apply_event(event);
        }
    });
}

fn post_delta(office: &PostOffice, text: String) {
    if !text.is_empty() {
        post_event(office, AiEvent::Delta(text));
    }
}

fn post_status(office: &PostOffice, text: impl Into<String>) {
    post_event(office, AiEvent::Status(text.into()));
}

fn post_progress(office: &PostOffice, messages: &[ApiMessage]) {
    post_event(office, AiEvent::Progress(messages.to_vec()));
}

fn post_finish(office: &PostOffice, messages: Vec<ApiMessage>) {
    post_event(office, AiEvent::Finish(messages));
}

fn value_to_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string()),
    }
}

fn pretty_arguments(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| arguments.to_string())
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…\n[truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_non_streaming_tool_calls() {
        let value = json!({
            "choices": [{
                "message": {
                    "content": "checking",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "list_tabs", "arguments": "{}"}
                    }]
                }
            }]
        });
        let result = parse_full_response(&value).unwrap();
        assert_eq!(result.content, "checking");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].function.name, "list_tabs");
    }

    #[test]
    fn parses_dsml_tool_calls_from_content() {
        let text = r#"<|DSML| calls>
<|DSML| invoke name="terminal_read">
<|DSML| parameter name="max_lines" string="false">40</|DSML| parameter>
<|DSML| parameter name="tab_id" string="false">1</|DSML| parameter>
</|DSML| invoke>
</|DSML| calls>"#;
        let (content, calls) = parse_dsml_tool_calls(text).expect("DSML should parse");
        assert!(content.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "terminal_read");
        let arguments: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(arguments["max_lines"], 40);
        assert_eq!(arguments["tab_id"], 1);
    }

    #[test]
    fn parses_dsml_with_spaces_and_newlines() {
        let text = r#"< | DSML | calls>
< | DSML | invoke name="terminal_read">
< | DSML | parameter name="tab_id" string="false">1</
|DSML| parameter>
< | DSML | parameter name="max_lines" string="false"
>60</|DSML| parameter>
</|DSML| invoke>
</|DSML| calls>"#;
        let (_, calls) = parse_dsml_tool_calls(text).expect("spaced DSML should parse");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "terminal_read");
        let arguments: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(arguments["tab_id"], 1);
        assert_eq!(arguments["max_lines"], 60);
    }

    #[test]
    fn compact_history_keeps_at_least_fifty_turns() {
        let mut first = ApiMessage::user("old");
        first.content = Some("x".repeat(MAX_CONTEXT_CHARS + 10));
        let compacted = compact_history(vec![
            first,
            ApiMessage::assistant(Some("old answer".into()), Vec::new()),
            ApiMessage::user("latest"),
        ]);
        assert_eq!(compacted.first().map(|m| m.role.as_str()), Some("user"));
        assert!(compacted
            .first()
            .and_then(|m| m.content.as_ref())
            .is_some_and(|content| content.len() <= COMPACT_TOOL_RESULT + 32));

        let mut messages = Vec::new();
        for index in 0..51 {
            messages.push(ApiMessage::user(format!("user-{index}")));
            messages.push(ApiMessage::assistant(Some("y".repeat(10_000)), Vec::new()));
        }
        let compacted = compact_history(messages);
        let users = compacted
            .iter()
            .filter(|message| message.role == "user")
            .collect::<Vec<_>>();
        assert_eq!(users.len(), MIN_HISTORY_TURNS);
        assert_eq!(users[0].content.as_deref(), Some("user-1"));
    }

    #[test]
    fn history_file_roundtrips() {
        let history = HistoryFile {
            current: vec![ApiMessage::user("hello")],
            current_key: "profile:p1".into(),
            currents: HashMap::new(),
            conversations: vec![ConversationRecord {
                id: "c1".into(),
                title: conversation_title(&[ApiMessage::user("first line\nsecond")]),
                updated: "2026-01-01T00:00:00Z".into(),
                key: "profile:p1".into(),
                messages: vec![ApiMessage::assistant(Some("hi".into()), Vec::new())],
            }],
        };
        let text = serde_json::to_string(&history).unwrap();
        let loaded: HistoryFile = serde_json::from_str(&text).unwrap();
        assert_eq!(loaded.current.len(), 1);
        assert_eq!(loaded.conversations[0].title, "first line");
        assert_eq!(loaded.conversations[0].messages.len(), 1);
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        let text = "你好世界".repeat(10);
        let truncated = truncate(&text, 10);
        assert!(truncated.ends_with("[truncated]"));
    }
}
