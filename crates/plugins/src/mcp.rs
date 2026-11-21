//! MCP (Model Context Protocol) server plugin.
//!
//! Exposes Xcord's live workspace over a local HTTP JSON-RPC endpoint and
//! provides the same tools/resources to the built-in AI copilot. A separate
//! `xcord-mcp` binary bridges stdio MCP clients (Claude Desktop, Cursor, ...)
//! to this endpoint.

use std::cell::RefCell;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tiny_http::{Header, Method, Response, Server, StatusCode};
use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{
    services, Automation, Commands, McpResourceRegistry, McpResources, McpServerStatus, McpStatus,
    McpToolRegistry, McpTools, PostOffice, ResourceContent, ResourceSpec, Runtime, Sessions,
    ToolSpec, Workspace,
};
use xcord_store::{AuthKind, HighlightRule, SessionProfile, Snippet};

const PROTOCOL_VERSION: &str = "2024-11-05";
const INTERNAL_PROTOCOL: &str = "mcp/protocol";

/// MCP plugin entry point.
#[derive(Clone)]
pub struct McpPlugin;

impl Plugin for McpPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("mcp", "MCP Server")
            .description("MCP tools/resources over localhost JSON-RPC for AI clients")
            .author("xcord")
            .capabilities(&["mcp", "ai", "tools"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[
            services::RUNTIME,
            services::SESSIONS,
            services::WORKSPACE,
            services::COMMANDS,
            services::AUTOMATION,
        ]
    }

    fn apply(&self, ctx: Context, _config: Value) -> Result<()> {
        let runtime = ctx.require::<Runtime>(services::RUNTIME)?;
        let sessions = (*ctx.require::<Sessions>(services::SESSIONS)?).clone();
        let workspace = (*ctx.require::<Workspace>(services::WORKSPACE)?).clone();
        let commands = (*ctx.require::<Commands>(services::COMMANDS)?).clone();
        let automation = (*ctx.require::<Automation>(services::AUTOMATION)?).clone();

        let tools = Rc::new(McpToolRegistry::new());
        let resources = Rc::new(McpResourceRegistry::new());
        register_tools(
            &tools,
            sessions.clone(),
            workspace.clone(),
            commands.clone(),
            automation.clone(),
        );
        register_resources(&resources, sessions.clone(), workspace.clone());

        ctx.provide(services::MCP_TOOLS, McpTools(tools.clone()))?;
        ctx.provide(services::MCP_RESOURCES, McpResources(resources.clone()))?;

        let mut settings = sessions.settings();
        if settings.mcp_token.trim().is_empty() {
            settings.mcp_token = uuid::Uuid::new_v4().simple().to_string();
            if let Err(error) = sessions.save_settings(settings.clone()) {
                log::warn!("could not persist MCP token: {error}");
            }
        }
        let token = settings.mcp_token.clone();
        if let Err(error) = write_token_file(&token) {
            log::warn!("could not write MCP token file: {error}");
        }

        let protocol = Rc::new(McpProtocol {
            tools: tools.clone(),
            resources: resources.clone(),
        });
        ctx.provide(INTERNAL_PROTOCOL, protocol.clone())?;

        let mut status = McpServerStatus {
            enabled: settings.mcp_enabled,
            running: false,
            url: format!("http://127.0.0.1:{}/mcp", settings.mcp_port),
            token: token.clone(),
            port: settings.mcp_port,
            tools: tools.list().len(),
            error: None,
        };

        if settings.mcp_enabled {
            match start_http(
                runtime.handle().office.clone(),
                token.clone(),
                settings.mcp_port,
            ) {
                Ok(server) => {
                    status.running = true;
                    let stop = server.stop.clone();
                    let handle = server.handle;
                    ctx.effect("stop MCP server", move || {
                        stop.store(true, Ordering::Relaxed);
                        let _ = handle.join();
                    });
                }
                Err(error) => {
                    log::warn!("MCP server unavailable: {error}");
                    status.error = Some(error);
                }
            }
        }

        ctx.provide(
            services::MCP_STATUS,
            McpStatus(Rc::new(RefCell::new(status))),
        )?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HTTP transport

struct HttpServer {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

fn start_http(
    office: PostOffice,
    token: String,
    port: u16,
) -> std::result::Result<HttpServer, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = stop.clone();
    let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();
    let handle = std::thread::Builder::new()
        .name("xcord-mcp".into())
        .spawn(move || {
            let server = match Server::http(("127.0.0.1", port)) {
                Ok(server) => server,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            while !stop_for_thread.load(Ordering::Relaxed) {
                match server.recv_timeout(Duration::from_millis(200)) {
                    Ok(Some(request)) => handle_request(request, &office, &token),
                    Ok(None) => {}
                    Err(error) => {
                        log::warn!("MCP accept error: {error}");
                        break;
                    }
                }
            }
        })
        .map_err(|error| error.to_string())?;

    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => Ok(HttpServer { stop, handle }),
        Ok(Err(error)) => {
            let _ = handle.join();
            Err(error)
        }
        Err(_) => Err("MCP server did not start in time".into()),
    }
}

fn handle_request(mut request: tiny_http::Request, office: &PostOffice, token: &str) {
    let path = request.url().split('?').next().unwrap_or("/");
    if path == "/health" {
        respond_json(request, 200, json!({"ok": true, "name": "xcord-mcp"}));
        return;
    }
    if path != "/mcp" {
        respond_json(request, 404, json!({"error": "not found"}));
        return;
    }
    if request.method() != &Method::Post {
        respond_json(request, 405, json!({"error": "use POST /mcp"}));
        return;
    }
    if !authorized(&request, token) {
        respond_json(
            request,
            401,
            json!({"error": "invalid or missing bearer token"}),
        );
        return;
    }

    let mut body = String::new();
    {
        let reader = request.as_reader();
        if reader
            .take(4 * 1024 * 1024)
            .read_to_string(&mut body)
            .is_err()
        {
            respond_json(request, 400, json!({"error": "invalid body"}));
            return;
        }
    }
    let value: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(error) => {
            respond_json(
                request,
                400,
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": error.to_string()}}),
            );
            return;
        }
    };

    let is_notification = value.get("id").is_none();
    if is_notification {
        // Notifications need no response; execute initialize/initialized for
        // symmetry but never block a client on the result.
        let _ = request.respond(Response::empty(StatusCode(202)));
        return;
    }

    let (tx, rx) = mpsc::channel::<Option<Value>>();
    let request_for_ui = value.clone();
    let posted = office.post(move |ctx| {
        let response = ctx
            .service::<Rc<McpProtocol>>(INTERNAL_PROTOCOL)
            .and_then(|protocol| protocol.handle(ctx, &request_for_ui));
        let _ = tx.send(response);
    });
    if !posted {
        respond_json(
            request,
            503,
            json!({"jsonrpc": "2.0", "id": value.get("id").cloned(), "error": {"code": -32603, "message": "UI is unavailable"}}),
        );
        return;
    }
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Some(response)) => respond_json(request, 200, response),
        Ok(None) => {
            let _ = request.respond(Response::empty(StatusCode(202)));
        }
        Err(_) => respond_json(
            request,
            504,
            json!({"jsonrpc": "2.0", "id": value.get("id").cloned(), "error": {"code": -32603, "message": "UI timed out"}}),
        ),
    }
}

fn authorized(request: &tiny_http::Request, token: &str) -> bool {
    request.headers().iter().any(|header| {
        header.field.equiv("Authorization")
            && header
                .value
                .as_str()
                .strip_prefix("Bearer ")
                .is_some_and(|value| value == token)
    })
}

fn respond_json(request: tiny_http::Request, status: u16, value: Value) {
    let body = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
    let content_type = Header::from_bytes("Content-Type", "application/json").unwrap();
    let response = Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(content_type);
    let _ = request.respond(response);
}

// ---------------------------------------------------------------------------
// Protocol core

struct McpProtocol {
    tools: Rc<McpToolRegistry>,
    resources: Rc<McpResourceRegistry>,
}

impl McpProtocol {
    fn handle(&self, ctx: &Context, request: &Value) -> Option<Value> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {"listChanged": false},
                    "resources": {"subscribe": false, "listChanged": false}
                },
                "serverInfo": {"name": "xcord", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Xcord is a plugin-first SSH workspace. Use list_tabs and terminal_read before terminal_send. Mutating tools are marked in annotations."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": self.tools.list().into_iter().map(|tool| json!({
                    "name": tool.name,
                    "title": tool.title,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                    "annotations": {"readOnlyHint": tool.read_only}
                })).collect::<Vec<_>>()
            })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.tools.call(ctx, name, args) {
                    Ok(value) => Ok(tool_content(&value, false)),
                    Err(error) => Ok(tool_content(&json!({"error": error}), true)),
                }
            }
            "resources/list" => Ok(json!({
                "resources": self.resources.list().into_iter()
                    .filter(|resource| !resource.uri.contains('{'))
                    .map(|resource| json!({
                        "uri": resource.uri,
                        "name": resource.name,
                        "description": resource.description,
                        "mimeType": resource.mime_type
                    })).collect::<Vec<_>>()
            })),
            "resources/templates/list" => Ok(json!({
                "resourceTemplates": [{
                    "uriTemplate": "xcord://terminal/{id}",
                    "name": "Terminal output",
                    "description": "Recent output of a tab (plain text)",
                    "mimeType": "text/plain"
                }]
            })),
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
                match self.resources.read(ctx, uri) {
                    Ok(contents) => Ok(json!({
                        "contents": contents.into_iter().map(|content| json!({
                            "uri": content.uri,
                            "mimeType": content.mime_type,
                            "text": content.text
                        })).collect::<Vec<_>>()
                    })),
                    Err(error) => Err(((-32002), error)),
                }
            }
            "notifications/initialized" => return None,
            _ => Err((-32601, format!("method not found: {method}"))),
        };

        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        })
    }
}

fn tool_content(value: &Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error
    })
}

// ---------------------------------------------------------------------------
// Capability registration

fn register_tools(
    tools: &Rc<McpToolRegistry>,
    sessions: Sessions,
    workspace: Workspace,
    commands: Commands,
    automation: Automation,
) {
    let no_args = json!({"type": "object", "properties": {}});
    tools.register(
        ToolSpec {
            name: "list_sessions".into(),
            title: "List sessions".into(),
            description: "List saved Xcord connection profiles (no secrets).".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, _args| {
                let list: Vec<Value> = sessions
                    .list()
                    .into_iter()
                    .map(|profile| {
                        json!({
                            "id": profile.id,
                            "name": profile.name,
                            "group": profile.group,
                            "host": profile.host,
                            "port": profile.port,
                            "user": profile.user,
                            "auth": profile.auth.label(),
                            "has_saved_password": sessions.password(&profile).is_some(),
                            "has_saved_passphrase": sessions.passphrase(&profile).is_some(),
                            "logging": profile.logging,
                            "tags": profile.tags,
                        })
                    })
                    .collect();
                Ok(Value::Array(list))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "list_tabs".into(),
            title: "List open tabs".into(),
            description: "List open terminal tabs with connection status and dimensions.".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        {
            let workspace = workspace.clone();
            move |_ctx, _args| {
                let list: Vec<Value> = workspace
                    .tabs()
                    .into_iter()
                    .map(|tab| {
                        let dims = workspace.dimensions(tab.id);
                        json!({
                            "id": tab.id.raw(),
                            "title": tab.title,
                            "profile_id": tab.profile_id,
                            "host": tab.host,
                            "user": tab.user,
                            "port": tab.port,
                            "status": tab.status.label(),
                            "error": match &tab.status {
                                xcord_plugin_api::TabStatus::Failed(message) => Some(message.clone()),
                                _ => None,
                            },
                            "dims": dims.map(|(cols, rows)| json!({"cols": cols, "rows": rows})),
                            "active": workspace.active() == Some(tab.id),
                            "host_key": tab.host_key.map(|key| key.fingerprint),
                            "bell": tab.bell,
                        })
                    })
                    .collect();
                Ok(Value::Array(list))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "terminal_read".into(),
            title: "Read terminal output".into(),
            description: "Read recent scrollback and screen text from a tab.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer", "description": "Tab id from list_tabs"},
                    "max_lines": {"type": "integer", "default": 200, "minimum": 20, "maximum": 2000}
                },
                "required": ["tab_id"]
            }),
            read_only: true,
        },
        {
            let workspace = workspace.clone();
            move |_ctx, args| {
                let id = tab_id(&args)?;
                let max_lines =
                    args.get("max_lines").and_then(Value::as_u64).unwrap_or(200) as usize;
                workspace
                    .terminal_text(id, max_lines)
                    .map(Value::String)
                    .ok_or_else(|| format!("tab {} is not open", id.raw()))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "terminal_send".into(),
            title: "Send terminal input".into(),
            description: "Write text to a tab's PTY, optionally followed by Enter.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "text": {"type": "string"},
                    "append_newline": {"type": "boolean", "default": true}
                },
                "required": ["tab_id", "text"]
            }),
            read_only: false,
        },
        {
            let workspace = workspace.clone();
            move |_ctx, args| {
                let id = tab_id(&args)?;
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "text is required".to_string())?;
                if workspace.tabs().iter().all(|tab| tab.id != id) {
                    return Err(format!("tab {} is not open", id.raw()));
                }
                workspace.write(id, text.as_bytes());
                if args
                    .get("append_newline")
                    .and_then(Value::as_bool)
                    .unwrap_or(true)
                {
                    workspace.write(id, b"\r");
                }
                Ok(json!({"ok": true, "tab_id": id.raw()}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "open_session".into(),
            title: "Open a saved session".into(),
            description: "Open a saved profile in a new tab using stored credentials.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            let workspace = workspace.clone();
            move |_ctx, args| {
                let session_id = args
                    .get("session_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "session_id is required".to_string())?;
                let profile = sessions
                    .get(session_id)
                    .ok_or_else(|| format!("unknown session `{session_id}`"))?;
                let credential = match &profile.auth {
                    xcord_store::AuthKind::Password { .. } => sessions.password(&profile),
                    xcord_store::AuthKind::PublicKey { .. } => sessions.passphrase(&profile),
                    _ => None,
                };
                if matches!(profile.auth, xcord_store::AuthKind::Password { .. })
                    && credential.is_none()
                {
                    return Err("this profile needs a password and has none in the vault".into());
                }
                let id = workspace
                    .open(xcord_plugin_api::OpenRequest {
                        profile,
                        credential,
                        cols: 100,
                        rows: 30,
                    })
                    .map_err(|error| error)?;
                Ok(json!({"tab_id": id.raw()}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "close_tab".into(),
            title: "Close a tab".into(),
            description: "Close an open terminal tab.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"tab_id": {"type": "integer"}},
                "required": ["tab_id"]
            }),
            read_only: false,
        },
        {
            let workspace = workspace.clone();
            move |_ctx, args| {
                let id = tab_id(&args)?;
                workspace.close(id);
                Ok(json!({"ok": true}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "select_tab".into(),
            title: "Select a tab".into(),
            description: "Activate a tab in the UI.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"tab_id": {"type": "integer"}},
                "required": ["tab_id"]
            }),
            read_only: true,
        },
        {
            let workspace = workspace.clone();
            move |_ctx, args| {
                let id = tab_id(&args)?;
                workspace.select(id);
                Ok(json!({"ok": true}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "list_plugins".into(),
            title: "List plugins".into(),
            description: "Inspect the plugin graph: state, injected services and errors.".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        |ctx, _args| {
            let list: Vec<Value> = ctx
                .kernel()
                .plugins()
                .into_iter()
                .map(|plugin| {
                    json!({
                        "id": plugin.id.raw(),
                        "name": plugin.name,
                        "version": plugin.version,
                        "state": plugin.state.label(),
                        "capabilities": plugin.capabilities,
                        "inject": plugin.inject,
                        "error": plugin.error,
                    })
                })
                .collect();
            Ok(Value::Array(list))
        },
    );

    tools.register(
        ToolSpec {
            name: "plugin_diagnostics".into(),
            title: "Plugin diagnostics".into(),
            description: "Recent kernel diagnostics, useful for debugging plugins.".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        |ctx, _args| {
            let list: Vec<Value> = ctx
                .kernel()
                .diagnostics()
                .into_iter()
                .rev()
                .take(80)
                .map(|diagnostic| {
                    json!({
                        "level": diagnostic.level.to_string(),
                        "plugin": diagnostic.plugin,
                        "message": diagnostic.message,
                    })
                })
                .collect();
            Ok(Value::Array(list))
        },
    );

    tools.register(
        ToolSpec {
            name: "list_commands".into(),
            title: "List commands".into(),
            description: "List command-palette entries contributed by plugins.".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        {
            let commands = commands.clone();
            move |_ctx, _args| {
                let list: Vec<Value> = commands
                    .list()
                    .into_iter()
                    .map(|command| {
                        json!({
                            "id": command.id,
                            "title": command.title,
                            "category": command.category,
                            "shortcut": command.shortcut,
                        })
                    })
                    .collect();
                Ok(Value::Array(list))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "run_command".into(),
            title: "Run a UI command".into(),
            description: "Execute a plugin command by id (see list_commands).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"command_id": {"type": "string"}},
                "required": ["command_id"]
            }),
            read_only: false,
        },
        {
            let commands = commands.clone();
            move |ctx, args| {
                let id = args
                    .get("command_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "command_id is required".to_string())?;
                if commands.run(id, ctx) {
                    Ok(json!({"ok": true}))
                } else {
                    Err(format!("unknown command `{id}`"))
                }
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "automation_syntax".into(),
            title: "Automation syntax".into(),
            description: "Return the Rhai API reference for Xcord automation scripts.".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        |_ctx, _args| Ok(Value::String(xcord_plugin_api::AUTOMATION_SYNTAX.into())),
    );

    tools.register(
        ToolSpec {
            name: "automation_list_scripts".into(),
            title: "List automation scripts".into(),
            description: "List managed Rhai automation scripts with id, name and path.".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        {
            let automation = automation.clone();
            move |_ctx, _args| {
                let list: Vec<Value> = automation
                    .scripts()
                    .into_iter()
                    .map(|script| {
                        json!({
                            "id": script.id,
                            "name": script.name,
                            "path": script.path,
                        })
                    })
                    .collect();
                Ok(Value::Array(list))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "automation_read_script".into(),
            title: "Read automation script".into(),
            description: "Read a managed automation script by id.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"script_id": {"type": "string"}},
                "required": ["script_id"]
            }),
            read_only: true,
        },
        {
            let automation = automation.clone();
            move |_ctx, args| {
                let id = args
                    .get("script_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "script_id is required".to_string())?;
                let source = automation
                    .script_source(id)
                    .ok_or_else(|| format!("unknown automation script `{id}`"))?;
                Ok(json!({"id": id, "source": source}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "automation_write_script".into(),
            title: "Write automation script".into(),
            description: "Replace the full source of an existing managed automation script. This is a mutating tool.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "script_id": {"type": "string"},
                    "source": {"type": "string"}
                },
                "required": ["script_id", "source"]
            }),
            read_only: false,
        },
        {
            let automation = automation.clone();
            move |_ctx, args| {
                let id = args
                    .get("script_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "script_id is required".to_string())?;
                let source = args
                    .get("source")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "source is required".to_string())?;
                automation.write_script(id, source)?;
                Ok(json!({"ok": true, "script_id": id}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "automation_create_script".into(),
            title: "Create automation script".into(),
            description: "Create a new managed Rhai script and optionally write its source. This is a mutating tool.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "source": {"type": "string"}
                },
                "required": ["name", "source"]
            }),
            read_only: false,
        },
        {
            let automation = automation.clone();
            move |_ctx, args| {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "name is required".to_string())?;
                let source = args
                    .get("source")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "source is required".to_string())?;
                let script = automation.create_script(name)?;
                automation.write_script(&script.id, source)?;
                Ok(json!({
                    "ok": true,
                    "id": script.id,
                    "name": script.name,
                    "path": script.path,
                }))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "get_settings".into(),
            title: "Get settings".into(),
            description: "Read sanitized Xcord settings (secrets are removed).".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, _args| {
                let mut value =
                    serde_json::to_value(sessions.settings()).map_err(|e| e.to_string())?;
                if let Some(map) = value.as_object_mut() {
                    map.remove("mcp_token");
                }
                Ok(value)
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "list_groups".into(),
            title: "List session groups".into(),
            description: "List all session group paths.".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, _args| {
                Ok(Value::Array(
                    sessions.groups().into_iter().map(Value::String).collect(),
                ))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "create_group".into(),
            title: "Create session group".into(),
            description: "Create a nested session group. Parent groups are created automatically.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string", "description": "Group path, e.g. Servers/Prod"}},
                "required": ["name"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                let name = required_str(&args, "name")?;
                sessions.create_group(&name)?;
                Ok(json!({"created": name}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "rename_group".into(),
            title: "Rename session group".into(),
            description: "Rename a group and all descendant groups.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "old": {"type": "string"},
                    "new": {"type": "string"}
                },
                "required": ["old", "new"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                sessions
                    .rename_group(&required_str(&args, "old")?, &required_str(&args, "new")?)?;
                Ok(json!({"renamed": true}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "delete_group".into(),
            title: "Delete session group".into(),
            description: "Delete a group and its child groups; contained sessions move to the parent.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                sessions.delete_group(&required_str(&args, "name")?)?;
                Ok(json!({"deleted": true}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "create_session".into(),
            title: "Create session".into(),
            description:
                "Create a saved SSH connection profile. Passwords are stored in the OS vault."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "group": {"type": "string", "default": "Sessions"},
                    "host": {"type": "string"},
                    "port": {"type": "integer", "default": 22},
                    "user": {"type": "string"},
                    "auth": {"type": "string", "enum": ["password", "public_key", "agent", "none"]},
                    "key_path": {"type": "string"},
                    "password": {"type": "string"},
                    "passphrase": {"type": "string"},
                    "term": {"type": "string"},
                    "login_script": {"type": "string"},
                    "startup_command": {"type": "string"},
                    "logging": {"type": "boolean"},
                    "tags": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["name", "host"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                let name = required_str(&args, "name")?;
                let host = required_str(&args, "host")?;
                let mut profile = SessionProfile::new(name);
                profile.host = host;
                profile.port = args
                    .get("port")
                    .and_then(Value::as_u64)
                    .unwrap_or(22)
                    .clamp(1, 65535) as u16;
                profile.user = args
                    .get("user")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                profile.group = args
                    .get("group")
                    .and_then(Value::as_str)
                    .unwrap_or("Sessions")
                    .to_string();
                apply_profile_fields(&mut profile, &args);
                sessions.create_group(&profile.group)?;
                sessions.save(profile.clone())?;
                apply_profile_secrets(&sessions, &profile, &args)?;
                Ok(profile_json(&sessions, &profile))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "update_session".into(),
            title: "Update session".into(),
            description: "Update fields on an existing session profile. Omitted fields are preserved.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "group": {"type": "string"},
                    "host": {"type": "string"},
                    "port": {"type": "integer"},
                    "user": {"type": "string"},
                    "auth": {"type": "string", "enum": ["password", "public_key", "agent", "none"]},
                    "key_path": {"type": "string"},
                    "password": {"type": "string", "description": "Empty string clears the saved password"},
                    "passphrase": {"type": "string"},
                    "term": {"type": "string"},
                    "login_script": {"type": "string"},
                    "startup_command": {"type": "string"},
                    "logging": {"type": "boolean"},
                    "tags": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["id"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                let id = required_str(&args, "id")?;
                let mut profile = sessions.get(&id).ok_or_else(|| format!("session {id} not found"))?;
                if let Some(value) = args.get("name").and_then(Value::as_str) {
                    profile.name = value.to_string();
                }
                if let Some(value) = args.get("host").and_then(Value::as_str) {
                    profile.host = value.to_string();
                }
                if let Some(value) = args.get("port").and_then(Value::as_u64) {
                    profile.port = value.clamp(1, 65535) as u16;
                }
                if let Some(value) = args.get("user").and_then(Value::as_str) {
                    profile.user = value.to_string();
                }
                if let Some(value) = args.get("group").and_then(Value::as_str) {
                    profile.group = value.to_string();
                }
                apply_profile_fields(&mut profile, &args);
                sessions.create_group(&profile.group)?;
                sessions.save(profile.clone())?;
                apply_profile_secrets(&sessions, &profile, &args)?;
                Ok(profile_json(&sessions, &profile))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "delete_session".into(),
            title: "Delete session".into(),
            description: "Delete a saved session profile.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                sessions.delete(&required_str(&args, "id")?)?;
                Ok(json!({"deleted": true}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "move_session".into(),
            title: "Move session".into(),
            description: "Move a session profile into another group.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "group": {"type": "string"}
                },
                "required": ["id", "group"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                sessions.move_session(
                    &required_str(&args, "id")?,
                    &required_str(&args, "group")?,
                    None,
                    true,
                )?;
                Ok(json!({"moved": true}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "list_snippets".into(),
            title: "List snippets".into(),
            description: "List saved quick command snippets.".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, _args| {
                Ok(Value::Array(
                    sessions.snippets().into_iter().map(snippet_json).collect(),
                ))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "create_snippet".into(),
            title: "Create snippet".into(),
            description: "Create a quick command snippet.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "command": {"type": "string"},
                    "send_enter": {"type": "boolean", "default": true}
                },
                "required": ["name", "command"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                let mut snippet = Snippet::new(
                    required_str(&args, "name")?,
                    required_str(&args, "command")?,
                );
                snippet.send_enter = args
                    .get("send_enter")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                sessions.save_snippet(snippet.clone())?;
                Ok(snippet_json(snippet))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "delete_snippet".into(),
            title: "Delete snippet".into(),
            description: "Delete a quick command snippet.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                sessions.delete_snippet(&required_str(&args, "id")?)?;
                Ok(json!({"deleted": true}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "list_highlights".into(),
            title: "List highlight rules".into(),
            description: "List terminal highlight rules.".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, _args| {
                Ok(Value::Array(
                    sessions
                        .highlights()
                        .into_iter()
                        .map(highlight_json)
                        .collect(),
                ))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "save_highlight".into(),
            title: "Save highlight rule".into(),
            description: "Create a terminal highlight rule.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "pattern": {"type": "string"},
                    "foreground": {"type": "string"},
                    "background": {"type": "string"},
                    "enabled": {"type": "boolean", "default": true}
                },
                "required": ["name", "pattern"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                let mut rule = HighlightRule::new(
                    required_str(&args, "name")?,
                    required_str(&args, "pattern")?,
                );
                if let Some(value) = args.get("foreground").and_then(Value::as_str) {
                    rule.foreground = Some(value.to_string());
                }
                if let Some(value) = args.get("background").and_then(Value::as_str) {
                    rule.background = Some(value.to_string());
                }
                rule.enabled = args.get("enabled").and_then(Value::as_bool).unwrap_or(true);
                sessions.save_highlight(rule.clone())?;
                Ok(highlight_json(rule))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "delete_highlight".into(),
            title: "Delete highlight rule".into(),
            description: "Delete a terminal highlight rule.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                sessions.delete_highlight(&required_str(&args, "id")?)?;
                Ok(json!({"deleted": true}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "automation_run_script".into(),
            title: "Run automation script".into(),
            description: "Run an automation script against a terminal tab.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "script_id": {"type": "string"},
                    "tab_id": {"type": "integer"}
                },
                "required": ["script_id", "tab_id"]
            }),
            read_only: false,
        },
        {
            let automation = automation.clone();
            move |_ctx, args| {
                let script_id = required_str(&args, "script_id")?;
                let tab = tab_id(&args)?;
                let run = automation.run(&script_id, tab)?;
                Ok(script_run_json(&run))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "automation_stop_script".into(),
            title: "Stop automation script".into(),
            description: "Stop a running automation script by run id.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"run_id": {"type": "integer"}},
                "required": ["run_id"]
            }),
            read_only: false,
        },
        {
            let automation = automation.clone();
            move |_ctx, args| {
                let run_id = args
                    .get("run_id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "run_id is required".to_string())?;
                automation.stop(run_id);
                Ok(json!({"stopping": run_id}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "automation_run_status".into(),
            title: "Automation run status".into(),
            description: "Read the active automation run, or a specific run by id.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"run_id": {"type": "integer"}}
            }),
            read_only: true,
        },
        {
            let automation = automation.clone();
            move |_ctx, args| {
                let run = match args.get("run_id").and_then(Value::as_u64) {
                    Some(run_id) => automation.run_snapshot(run_id),
                    None => automation.active_run(),
                };
                Ok(run.map(|run| script_run_json(&run)).unwrap_or(Value::Null))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "next_tab".into(),
            title: "Select next tab".into(),
            description: "Select the next open terminal tab.".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        {
            let workspace = workspace.clone();
            move |_ctx, _args| {
                workspace.select_next();
                Ok(json!({"active": workspace.active().map(|id| id.raw())}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "previous_tab".into(),
            title: "Select previous tab".into(),
            description: "Select the previous open terminal tab.".into(),
            input_schema: no_args.clone(),
            read_only: true,
        },
        {
            let workspace = workspace.clone();
            move |_ctx, _args| {
                workspace.select_previous();
                Ok(json!({"active": workspace.active().map(|id| id.raw())}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "move_tab".into(),
            title: "Move tab".into(),
            description: "Reorder a tab relative to another tab.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "target_id": {"type": "integer"},
                    "after": {"type": "boolean", "default": true}
                },
                "required": ["tab_id", "target_id"]
            }),
            read_only: false,
        },
        {
            let workspace = workspace.clone();
            move |_ctx, args| {
                let id = tab_id(&args)?;
                let target = args
                    .get("target_id")
                    .and_then(Value::as_u64)
                    .map(xcord_plugin_api::TerminalId::new)
                    .ok_or_else(|| "target_id is required".to_string())?;
                let after = args.get("after").and_then(Value::as_bool).unwrap_or(true);
                workspace.move_tab(id, target, after);
                Ok(json!({"moved": true}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "reconnect_tab".into(),
            title: "Reconnect tab".into(),
            description: "Reconnect a terminal tab and return the new tab id.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"tab_id": {"type": "integer"}},
                "required": ["tab_id"]
            }),
            read_only: false,
        },
        {
            let workspace = workspace.clone();
            move |_ctx, args| {
                let id = workspace.reconnect(tab_id(&args)?)?;
                Ok(json!({"tab_id": id.raw()}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "resize_terminal".into(),
            title: "Resize terminal".into(),
            description: "Resize a terminal tab PTY.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "cols": {"type": "integer", "minimum": 20},
                    "rows": {"type": "integer", "minimum": 5}
                },
                "required": ["tab_id", "cols", "rows"]
            }),
            read_only: false,
        },
        {
            let workspace = workspace.clone();
            move |_ctx, args| {
                let id = tab_id(&args)?;
                let cols =
                    args.get("cols")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| "cols is required".to_string())? as u16;
                let rows =
                    args.get("rows")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| "rows is required".to_string())? as u16;
                workspace.resize(id, cols, rows);
                Ok(json!({"cols": cols, "rows": rows}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "scroll_terminal".into(),
            title: "Scroll terminal".into(),
            description:
                "Scroll a terminal by delta lines, jump to an offset, or scroll to bottom.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "delta": {"type": "integer"},
                    "offset": {"type": "integer"},
                    "bottom": {"type": "boolean"}
                },
                "required": ["tab_id"]
            }),
            read_only: true,
        },
        {
            let workspace = workspace.clone();
            move |_ctx, args| {
                let id = tab_id(&args)?;
                if args.get("bottom").and_then(Value::as_bool).unwrap_or(false) {
                    workspace.scroll_to_bottom(id);
                } else if let Some(offset) = args.get("offset").and_then(Value::as_u64) {
                    workspace.scroll_to(id, offset as usize);
                } else {
                    let delta = args.get("delta").and_then(Value::as_i64).unwrap_or(0) as i32;
                    workspace.scroll_by(id, delta);
                }
                Ok(json!({"scrolled": true}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "export_sessions".into(),
            title: "Export sessions".into(),
            description: "Export all saved sessions to a TOML file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                sessions.export(&required_str(&args, "path")?)?;
                Ok(json!({"exported": true}))
            }
        },
    );

    tools.register(
        ToolSpec {
            name: "import_sessions".into(),
            title: "Import sessions".into(),
            description: "Import sessions from a TOML export file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            read_only: false,
        },
        {
            let sessions = sessions.clone();
            move |_ctx, args| {
                sessions.import(&required_str(&args, "path")?)?;
                Ok(json!({"imported": true}))
            }
        },
    );
}

fn required_str(args: &Value, key: &str) -> std::result::Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{key} is required"))
}

fn apply_profile_fields(profile: &mut SessionProfile, args: &Value) {
    if let Some(value) = args.get("term").and_then(Value::as_str) {
        profile.term = value.to_string();
    }
    if let Some(value) = args.get("login_script").and_then(Value::as_str) {
        profile.login_script = value.to_string();
    }
    if let Some(value) = args.get("startup_command").and_then(Value::as_str) {
        profile.startup_command = Some(value.to_string());
    }
    if let Some(value) = args.get("logging").and_then(Value::as_bool) {
        profile.logging = value;
    }
    if let Some(values) = args.get("tags").and_then(Value::as_array) {
        profile.tags = values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    if let Some(auth) = args.get("auth").and_then(Value::as_str) {
        let key_path = args
            .get("key_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        profile.auth = match auth {
            "public_key" => AuthKind::PublicKey {
                path: key_path,
                save_passphrase: false,
                secret_id: None,
            },
            "agent" => AuthKind::Agent,
            "none" => AuthKind::None,
            _ => AuthKind::Password {
                save: false,
                secret_id: None,
            },
        };
    } else if let Some(key_path) = args.get("key_path").and_then(Value::as_str) {
        if let AuthKind::PublicKey { path, .. } = &mut profile.auth {
            *path = key_path.to_string();
        }
    }
}

fn apply_profile_secrets(
    sessions: &Sessions,
    profile: &SessionProfile,
    args: &Value,
) -> std::result::Result<(), String> {
    if let Some(password) = args.get("password").and_then(Value::as_str) {
        if password.is_empty() {
            sessions.clear_password(profile)?;
        } else {
            sessions.store_password(profile, password)?;
        }
    }
    if let Some(passphrase) = args.get("passphrase").and_then(Value::as_str) {
        if passphrase.is_empty() {
            sessions.clear_passphrase(profile)?;
        } else {
            sessions.store_passphrase(profile, passphrase)?;
        }
    }
    Ok(())
}

fn profile_json(sessions: &Sessions, profile: &SessionProfile) -> Value {
    json!({
        "id": profile.id,
        "name": profile.name,
        "group": profile.group,
        "host": profile.host,
        "port": profile.port,
        "user": profile.user,
        "auth": profile.auth.label(),
        "has_saved_password": sessions.password(profile).is_some(),
        "has_saved_passphrase": sessions.passphrase(profile).is_some(),
        "logging": profile.logging,
        "tags": profile.tags,
    })
}

fn snippet_json(snippet: Snippet) -> Value {
    json!({
        "id": snippet.id,
        "name": snippet.name,
        "command": snippet.command,
        "send_enter": snippet.send_enter,
        "action": format!("{:?}", snippet.action).to_lowercase(),
        "script_id": snippet.script_id,
        "script_file": snippet.script_file,
    })
}

fn highlight_json(rule: HighlightRule) -> Value {
    json!({
        "id": rule.id,
        "name": rule.name,
        "pattern": rule.pattern,
        "foreground": rule.foreground,
        "background": rule.background,
        "enabled": rule.enabled,
    })
}

fn script_run_json(run: &xcord_plugin_api::ScriptRunSnapshot) -> Value {
    json!({
        "run_id": run.run_id,
        "script_id": run.script_id,
        "script_name": run.script_name,
        "tab_id": run.terminal.raw(),
        "status": format!("{:?}", run.status).to_lowercase(),
        "current_line": run.current_line,
        "error": run.error,
        "logs": run.logs.iter().map(|log| json!({
            "level": format!("{:?}", log.level).to_lowercase(),
            "message": log.message,
        })).collect::<Vec<_>>(),
    })
}

fn tab_id(args: &Value) -> std::result::Result<xcord_plugin_api::TerminalId, String> {
    args.get("tab_id")
        .and_then(Value::as_u64)
        .map(xcord_plugin_api::TerminalId::new)
        .ok_or_else(|| "tab_id is required".to_string())
}

fn register_resources(
    resources: &Rc<McpResourceRegistry>,
    sessions: Sessions,
    workspace: Workspace,
) {
    let specs = [
        (
            ResourceSpec {
                uri: "xcord://sessions".into(),
                name: "Saved sessions".into(),
                description: "Connection profiles as JSON".into(),
                mime_type: "application/json".into(),
            },
            "sessions",
        ),
        (
            ResourceSpec {
                uri: "xcord://tabs".into(),
                name: "Open tabs".into(),
                description: "Live tab and connection state as JSON".into(),
                mime_type: "application/json".into(),
            },
            "tabs",
        ),
        (
            ResourceSpec {
                uri: "xcord://plugins".into(),
                name: "Plugin graph".into(),
                description: "Plugin states and dependencies as JSON".into(),
                mime_type: "application/json".into(),
            },
            "plugins",
        ),
        (
            ResourceSpec {
                uri: "xcord://terminal/{id}".into(),
                name: "Terminal output".into(),
                description: "Recent output of a tab (text/plain)".into(),
                mime_type: "text/plain".into(),
            },
            "terminal",
        ),
    ];

    for (spec, kind) in specs {
        let uri = spec.uri.clone();
        let mime = spec.mime_type.clone();
        let sessions = sessions.clone();
        let workspace = workspace.clone();
        resources.register(spec, move |ctx, requested| {
            let kind = kind;
            if kind == "terminal" {
                let id_text = requested
                    .strip_prefix("xcord://terminal/")
                    .ok_or_else(|| "not a terminal URI".to_string())?;
                let id = id_text
                    .parse::<u64>()
                    .map_err(|_| "invalid terminal id".to_string())?;
                let text = workspace
                    .terminal_text(xcord_plugin_api::TerminalId::new(id), 800)
                    .ok_or_else(|| format!("tab {id} is not open"))?;
                return Ok(vec![ResourceContent {
                    uri: requested.to_string(),
                    mime_type: mime.clone(),
                    text,
                }]);
            }
            if requested != uri {
                return Err("not this resource".into());
            }
            let text = match kind {
                "sessions" => serde_json::to_string_pretty(
                    &sessions
                        .list()
                        .into_iter()
                        .map(|profile| {
                            json!({
                                "id": profile.id,
                                "name": profile.name,
                                "group": profile.group,
                                "host": profile.host,
                                "port": profile.port,
                                "user": profile.user,
                                "auth": profile.auth.label(),
                            })
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_default(),
                "tabs" => serde_json::to_string_pretty(
                    &workspace
                        .tabs()
                        .into_iter()
                        .map(|tab| {
                            json!({
                                "id": tab.id.raw(),
                                "title": tab.title,
                                "status": tab.status.label(),
                                "active": workspace.active() == Some(tab.id),
                                "dims": workspace.dimensions(tab.id),
                            })
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_default(),
                "plugins" => serde_json::to_string_pretty(
                    &ctx.kernel()
                        .plugins()
                        .into_iter()
                        .map(|plugin| {
                            json!({
                                "name": plugin.name,
                                "state": plugin.state.label(),
                                "inject": plugin.inject,
                                "error": plugin.error,
                            })
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_default(),
                _ => String::new(),
            };
            Ok(vec![ResourceContent {
                uri: requested.to_string(),
                mime_type: mime.clone(),
                text,
            }])
        });
    }
}

fn write_token_file(token: &str) -> std::io::Result<()> {
    let dir = xcord_store::config_dir();
    std::fs::create_dir_all(&dir)?;
    let path: PathBuf = dir.join("mcp-token");
    std::fs::write(&path, token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}
