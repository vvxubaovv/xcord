//! AI copilot plugin.
//!
//! Uses an OpenAI-compatible Chat Completions endpoint (OpenAI, DeepSeek,
//! Ollama, vLLM, ...) with streaming and function calling. Every tool call goes
//! through the same MCP registry exposed to external clients.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
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
const MAX_TOOL_ROUNDS: usize = 6;
const MAX_TOOL_RESULT: usize = 14_000;

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

        let service = Rc::new(AiService {
            sessions,
            workspace,
            tools,
            i18n,
            runtime: runtime.handle(),
            messages: RefCell::new(Vec::new()),
            streaming: Cell::new(false),
            stream_text: RefCell::new(String::new()),
            status: RefCell::new(String::new()),
            error: RefCell::new(None),
            has_key: Cell::new(false),
            context_terminal: Cell::new(None),
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

#[derive(Clone, Debug, Serialize)]
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

enum AiEvent {
    Delta(String),
    Status(String),
    Progress(Vec<ApiMessage>),
    Finish(Vec<ApiMessage>),
    Error(String),
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
        self.stream_text.borrow_mut().push_str(text);
    }

    fn apply_event(&self, event: AiEvent) {
        match event {
            AiEvent::Delta(text) => self.push_delta(&text),
            AiEvent::Status(status) => *self.status.borrow_mut() = status,
            AiEvent::Progress(messages) => {
                *self.messages.borrow_mut() = messages;
                self.stream_text.borrow_mut().clear();
            }
            AiEvent::Finish(messages) => {
                *self.messages.borrow_mut() = messages;
                self.stream_text.borrow_mut().clear();
                self.streaming.set(false);
                self.status.borrow_mut().clear();
                *self.error.borrow_mut() = None;
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
            }
        }
    }

    fn terminal_context(&self) -> Option<String> {
        let settings = self.sessions.settings();
        if !settings.ai_include_terminal {
            return None;
        }
        let tabs = self.workspace.tabs();
        let explicit = self
            .context_terminal
            .get()
            .filter(|id| tabs.iter().any(|tab| tab.id == *id));
        if self.context_terminal.get().is_some() && explicit.is_none() {
            self.context_terminal.set(None);
        }
        let id = explicit.or_else(|| self.workspace.active())?;
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
            if explicit.is_some() {
                "User-pinned conversation tab"
            } else {
                "Current active tab"
            },
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
        let context = explicit.or_else(|| self.workspace.active());
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
            context_follows_active: explicit.is_none(),
        }
    }

    fn set_context_terminal(&self, id: Option<TerminalId>) {
        self.context_terminal.set(id);
    }

    fn context_terminal(&self) -> Option<TerminalId> {
        self.context_terminal.get()
    }

    fn send(&self, prompt: &str) -> std::result::Result<(), String> {
        self.spawn_request(prompt.to_string())
    }

    fn clear(&self) {
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

async fn run_chat(
    office: PostOffice,
    api_key: String,
    url: String,
    model: String,
    system: String,
    mut messages: Vec<ApiMessage>,
    tools: Vec<ToolSpec>,
    allow_writes: bool,
) -> std::result::Result<(), String> {
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
        let response = client
            .post(&url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("request failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(format!(
                "provider returned {status}: {}",
                truncate(&text, 800)
            ));
        }

        let result = read_response(response, &office).await?;
        let content = if result.content.trim().is_empty() {
            None
        } else {
            Some(result.content)
        };
        let tool_calls = result.tool_calls;

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
                call_tool(office.clone(), tool_name.clone(), arguments).await
            };
            let text = match output {
                Ok(value) => value_to_text(value),
                Err(error) => format!("ERROR: {error}"),
            };
            messages.push(ApiMessage::tool(call.id, truncate(&text, MAX_TOOL_RESULT)));
            post_progress(&office, &messages);
        }
    }

    Err(format!(
        "stopped after {MAX_TOOL_ROUNDS} tool rounds without a final answer"
    ))
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
    fn truncation_respects_utf8_boundaries() {
        let text = "你好世界".repeat(10);
        let truncated = truncate(&text, 10);
        assert!(truncated.ends_with("[truncated]"));
    }
}
