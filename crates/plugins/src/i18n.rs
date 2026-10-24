//! Localization plugin.
//!
//! Owns the active locale and all translation catalogs. Built-in catalogs cover
//! the shell, command palette, plugin metadata and the AI panel. Plugins can add
//! their own domain at runtime:
//!
//! ```ignore
//! let i18n = ctx.require::<I18n>(services::I18N)?;
//! let handle = i18n.register_pairs("my-plugin", "zh-CN", &[("Ready", "就绪")]);
//! ctx.effect("my-plugin translations", move || i18n.unregister(handle));
//! ```

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use xcord_cordis::{Context, Plugin, PluginMeta, Result};
use xcord_plugin_api::{services, I18n, LocaleInfo, LocalizationService, Sessions};

const ZH_CN: &str = "zh-CN";
const EN_US: &str = "en-US";

#[derive(Clone)]
pub struct I18nPlugin;

impl Plugin for I18nPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("i18n", "Localization")
            .description("Locale catalogs shared by the shell and plugins")
            .author("xcord")
            .capabilities(&["localization", "i18n"])
    }

    fn inject(&self) -> &'static [&'static str] {
        &[services::SESSIONS]
    }

    fn apply(&self, ctx: Context, _config: serde_json::Value) -> Result<()> {
        let sessions = (*ctx.require::<Sessions>(services::SESSIONS)?).clone();
        let configured = sessions.settings().language;
        let locale = normalize_locale(&configured);
        let state = Rc::new(LocalizationState::new(locale));
        state.load_builtins();
        ctx.provide(services::I18N, I18n(state))?;
        Ok(())
    }
}

fn normalize_locale(locale: &str) -> String {
    let lower = locale.trim().to_ascii_lowercase();
    if lower.starts_with("en") {
        EN_US.into()
    } else {
        ZH_CN.into()
    }
}

struct LocalizationState {
    locale: RefCell<String>,
    catalogs: RefCell<HashMap<(String, String), BTreeMap<String, String>>>,
    registrations: RefCell<HashMap<u64, (String, String, Vec<String>)>>,
    next_handle: Cell<u64>,
}

impl LocalizationState {
    fn new(locale: String) -> Self {
        Self {
            locale: RefCell::new(normalize_locale(&locale)),
            catalogs: RefCell::new(HashMap::new()),
            registrations: RefCell::new(HashMap::new()),
            next_handle: Cell::new(0),
        }
    }

    fn load_builtins(&self) {
        self.register_pairs("app", ZH_CN, APP_ZH);
        self.register_pairs("commands", ZH_CN, COMMANDS_ZH);
        self.register_pairs("categories", ZH_CN, CATEGORIES_ZH);
        self.register_pairs("plugin", ZH_CN, PLUGINS_ZH);
        self.register_pairs("status", ZH_CN, STATUS_ZH);
    }
}

impl LocalizationService for LocalizationState {
    fn current_locale(&self) -> String {
        self.locale.borrow().clone()
    }

    fn set_locale(&self, locale: &str) -> std::result::Result<(), String> {
        let normalized = normalize_locale(locale);
        if !matches!(normalized.as_str(), ZH_CN | EN_US) {
            return Err(format!("unsupported locale `{locale}`"));
        }
        *self.locale.borrow_mut() = normalized;
        Ok(())
    }

    fn available_locales(&self) -> Vec<LocaleInfo> {
        vec![
            LocaleInfo {
                id: ZH_CN.into(),
                name: "中文（简体）".into(),
            },
            LocaleInfo {
                id: EN_US.into(),
                name: "English".into(),
            },
        ]
    }

    fn register(&self, domain: &str, locale: &str, entries: BTreeMap<String, String>) -> u64 {
        let domain = domain.to_string();
        let locale = normalize_locale(locale);
        let keys = entries.keys().cloned().collect::<Vec<_>>();
        let mut catalogs = self.catalogs.borrow_mut();
        let catalog = catalogs
            .entry((domain.clone(), locale.clone()))
            .or_default();
        for (key, value) in entries {
            catalog.insert(key, value);
        }
        let mut registrations = self.registrations.borrow_mut();
        let handle = self.next_handle.get() + 1;
        self.next_handle.set(handle);
        registrations.insert(handle, (domain, locale, keys));
        handle
    }

    fn unregister(&self, handle: u64) {
        let Some((domain, locale, keys)) = self.registrations.borrow_mut().remove(&handle) else {
            return;
        };
        let mut catalogs = self.catalogs.borrow_mut();
        if let Some(catalog) = catalogs.get_mut(&(domain, locale)) {
            for key in keys {
                catalog.remove(&key);
            }
        }
    }

    fn translate(&self, domain: &str, key: &str) -> Option<String> {
        let current = self.locale.borrow().clone();
        let catalogs = self.catalogs.borrow();
        for locale in [current.as_str(), EN_US, ZH_CN] {
            if let Some(value) = catalogs
                .get(&(domain.to_string(), locale.to_string()))
                .and_then(|catalog| catalog.get(key))
            {
                return Some(value.clone());
            }
            if locale == current.as_str() && current == EN_US {
                // English uses the source key first; don't fall through to
                // Chinese for a missing English string.
                return None;
            }
        }
        None
    }
}

const APP_ZH: &[(&str, &str)] = &[
    ("AI", "AI"),
    ("Palette", "命令面板"),
    ("Plugins", "插件"),
    ("Settings", "设置"),
    ("Search sessions", "搜索会话"),
    ("Send a command to the active session…", "向当前会话发送命令…"),
    ("Ask about this session…", "询问当前会话…"),
    ("New session", "新建会话"),
    ("New group", "新建分组"),
    ("Group name", "分组名称"),
    ("Create", "创建"),
    ("Move to group", "移动到分组"),
    ("Group name cannot be empty.", "分组名称不能为空。"),
    ("Group created: {name}", "已创建分组：{name}"),
    ("Edit session", "编辑会话"),
    ("SESSIONS", "会话"),
    (
        "No sessions match. Create one to get started.",
        "没有匹配的会话。新建一个开始使用。",
    ),
    ("edit", "编辑"),
    ("Duplicate", "复制会话"),
    ("{name} copy", "{name} 副本"),
    ("{name} copy {index}", "{name} 副本 {index}"),
    ("Copied session to {name}", "已复制为 {name}"),
    ("Ctrl+Shift+P palette", "Ctrl+Shift+P 命令面板"),
    ("ready", "就绪"),
    (
        "An SSH workspace where everything is a plugin.",
        "一切皆插件的 SSH 工作台。",
    ),
    (
        "SSH • terminal • compose • snippets • highlight • logging • themes",
        "SSH • 终端 • 编辑栏 • 片段 • 高亮 • 日志 • 主题",
    ),
    ("connect", "连接"),
    ("Command palette", "命令面板"),
    ("Manage plugins", "管理插件"),
    ("Type a command…", "输入命令…"),
    ("No matching commands.", "没有匹配的命令。"),
    (
        "↑↓ navigate · Enter run · Esc close",
        "↑↓ 选择 · Enter 执行 · Esc 关闭",
    ),
    (
        "Credentials are stored in the OS keychain, never in config.toml.",
        "凭据保存在系统钥匙串中，不会写入 config.toml。",
    ),
    ("Name", "名称"),
    ("Group", "分组"),
    ("Host", "主机"),
    ("Port", "端口"),
    ("User", "用户名"),
    ("Password", "密码"),
    ("Private key", "私钥"),
    ("Private key file", "私钥文件"),
    ("Browse…", "浏览…"),
    ("Key passphrase (optional)", "私钥口令（可选）"),
    ("Save password in keychain", "将密码保存到钥匙串"),
    ("Save passphrase in keychain", "将口令保存到钥匙串"),
    ("Log this session", "记录此会话日志"),
    (
        "Host keys are verified with trust-on-first-use.",
        "主机密钥采用首次使用信任（TOFU）校验。",
    ),
    ("Test connection", "测试连接"),
    ("Testing connection…", "正在测试连接…"),
    ("Cancel", "取消"),
    ("Save & connect", "保存并连接"),
    ("Save", "保存"),
    ("Reload", "重载"),
    ("core", "核心"),
    ("Language", "语言"),
    ("Theme", "主题"),
    ("Font size", "字体大小"),
    ("Copy on select", "选中即复制"),
    ("Compose bar", "编辑栏"),
    ("Strict host keys", "严格主机密钥"),
    ("Session logging", "会话日志"),
    ("Output highlighting", "输出高亮"),
    (
        "Changes apply live. Some plugin options need a plugin reload.",
        "更改会即时生效；部分插件选项需要重新加载插件。",
    ),
    (
        "Base URL must start with http:// or https://",
        "基础 URL 必须以 http:// 或 https:// 开头",
    ),
    ("Model is required.", "模型不能为空。"),
    (
        "Everything is a plugin. Unloading a service cascades to its dependents; effects run in reverse order.",
        "一切皆插件。卸载服务会级联卸载依赖它的插件，副作用按相反顺序执行。",
    ),
    ("Kernel diagnostics", "内核诊断"),
    (
        "Highlighter reload is live. Core plugins restart with the app.",
        "高亮器支持热重载；核心插件需要重启应用。",
    ),
    ("Reinitialize highlighter", "重新初始化高亮器"),
    ("Host is required.", "主机不能为空。"),
    (
        "Port must be a number between 1 and 65535.",
        "端口必须是 1-65535 之间的数字。",
    ),
    ("Choose a private key file.", "请选择私钥文件。"),
    ("This session needs a password.", "此会话需要密码。"),
    (
        "Enter a password to test this session.",
        "请输入密码后再测试连接。",
    ),
    ("No API key — open settings", "未配置 API Key — 打开设置"),
    ("AI Copilot", "AI 助手"),
    ("AI & MCP", "AI 与 MCP"),
    (
        "Ask the copilot to inspect a session, explain output, or run a fix with tools.",
        "让 AI 助手检查会话、解释输出，或使用工具执行修复。",
    ),
    ("Explain this terminal", "解释当前终端"),
    ("Why did the last command fail?", "上一条命令为什么失败？"),
    ("Summarize open tabs", "汇总当前标签页"),
    ("Terminal context", "终端上下文"),
    ("Copy", "复制"),
    ("Copy chat", "复制对话"),
    ("Selected message", "已选择消息"),
    ("Whole conversation", "整个对话"),
    ("Select files to send with rz", "选择要通过 rz 发送的文件"),
    ("List files", "列出文件"),
    ("Disk usage", "磁盘使用"),
    ("Tail syslog", "跟踪系统日志"),
    ("Sending files to remote rz…", "正在向远端 rz 发送文件…"),
    ("Conversation terminal", "对话终端"),
    (
        "Conversation terminal · {title}",
        "对话终端 · {title}",
    ),
    ("Follow active tab", "跟随活动标签"),
    ("No open terminals", "暂无打开的终端"),
    ("Writes allowed", "允许写入"),
    ("Writes blocked", "禁止写入"),
    ("Send", "发送"),
    ("Thinking…", "思考中…"),
    (
        "MCP server running · 12 tools",
        "MCP 服务运行中 · 12 个工具",
    ),
    (
        "Copy Claude/Cursor config",
        "复制 Claude/Cursor 配置",
    ),
    ("Base URL", "基础 URL"),
    ("Model", "模型"),
    ("API key", "API Key"),
    (
        "API key (a key is already stored)",
        "API Key（已保存）",
    ),
    ("Send terminal context", "发送终端上下文"),
    ("Allow terminal writes", "允许终端写入"),
    ("Clear stored key", "清除已保存的 Key"),
    (
        "Any OpenAI-compatible endpoint works. The key is stored in Windows Credential Manager.",
        "支持任意 OpenAI 兼容端点。Key 保存在系统凭据管理器中。",
    ),
    (
        "MCP speaks JSON-RPC on 127.0.0.1; the bridge reads mcp-token from the config directory.",
        "MCP 在 127.0.0.1 上使用 JSON-RPC；桥接程序从配置目录读取 mcp-token。",
    ),
    ("connected", "已连接"),
    ("connecting", "连接中"),
    ("closed", "已关闭"),
    ("failed", "连接失败"),
    ("No sessions yet.", "还没有会话。"),
    ("Sessions", "会话"),
    ("plugin-first SSH", "插件化 SSH"),
    ("Xcord — plugin-first SSH", "Xcord — 插件化 SSH"),
    ("Production web", "生产环境 Web"),
    ("Leave blank to use the keychain", "留空则使用钥匙串中的凭据"),
    ("Leave blank to keep the stored key", "留空则保留已保存的 Key"),
    ("no host key", "无主机密钥"),
    ("{count} plugins active", "{count} 个插件已启用"),
    ("v{version} · {state}", "v{version} · {state}"),
    (
        "{active} active / {total} registered",
        "{active} 个活动 / {total} 个已注册",
    ),
    ("{model} · MCP tools", "{model} · MCP 工具"),
    ("Endpoint: {url}", "端点：{url}"),
    (
        "Endpoint: {url} · token {token}",
        "端点：{url} · 令牌 {token}",
    ),
    (
        "MCP server running · {tools} tools",
        "MCP 服务运行中 · {tools} 个工具",
    ),
    (
        "MCP server unavailable: {error}",
        "MCP 服务不可用：{error}",
    ),
    (
        "Secret was not saved: {error}",
        "凭据未保存：{error}",
    ),
    ("Connecting to {endpoint}…", "正在连接 {endpoint}…"),
    ("Connecting…", "正在连接…"),
    ("Session no longer exists.", "会话已不存在。"),
    ("AI settings saved.", "AI 设置已保存。"),
    ("Stored AI API key removed.", "已清除保存的 AI API Key。"),
    (
        "Settings saved. Reload plugins to apply logging/highlighter changes.",
        "设置已保存。日志/高亮相关更改可能需要重新加载插件。",
    ),
    (
        "Enter the password for this session.",
        "请输入此会话的密码。",
    ),
    ("MCP server unavailable", "MCP 服务不可用"),
];

const COMMANDS_ZH: &[(&str, &str)] = &[
    ("ai.toggle", "切换 AI 助手"),
    ("ai.settings", "配置 AI 助手…"),
    ("ai.clear", "清空 AI 对话"),
    ("app.quit", "退出 Xcord"),
    ("tab.close", "关闭当前标签页"),
    ("tab.next", "下一个标签页"),
    ("tab.prev", "上一个标签页"),
    ("terminal.reconnect", "重新连接当前标签页"),
    ("terminal.clear", "清空当前终端"),
    ("session.new", "新建会话…"),
    ("session.edit", "编辑当前会话…"),
    ("palette.open", "打开命令面板"),
    ("plugins.open", "管理插件…"),
    ("settings.open", "打开设置…"),
    ("compose.focus", "聚焦编辑栏"),
];

const CATEGORIES_ZH: &[(&str, &str)] = &[
    ("AI", "AI"),
    ("Application", "应用"),
    ("Tabs", "标签页"),
    ("Terminal", "终端"),
    ("Sessions", "会话"),
    ("View", "视图"),
];

const PLUGINS_ZH: &[(&str, &str)] = &[
    ("ai.name", "AI 助手"),
    (
        "ai.description",
        "通过 OpenAI 兼容接口进行流式对话并调用 MCP 工具",
    ),
    ("commands.name", "命令注册表"),
    ("commands.description", "收集所有插件贡献的命令面板项"),
    ("highlights.name", "输出高亮器"),
    ("highlights.description", "使用正则规则为终端输出着色"),
    ("i18n.name", "本地化"),
    ("i18n.description", "界面与插件共享的语言目录"),
    ("logging.name", "会话日志"),
    (
        "logging.description",
        "将会话输出追加到每个标签页的日志文件",
    ),
    ("mcp.name", "MCP 服务"),
    (
        "mcp.description",
        "通过本地 JSON-RPC 向 AI 客户端提供 MCP 工具与资源",
    ),
    ("runtime.name", "I/O 运行时"),
    (
        "runtime.description",
        "Tokio 工作线程池与 UI PostOffice 桥接",
    ),
    ("sessions.name", "会话存储"),
    (
        "sessions.description",
        "TOML 工作区、配置、片段与系统钥匙串凭据",
    ),
    ("ssh.name", "SSH 传输"),
    (
        "ssh.description",
        "基于 russh 的 SSH-2 客户端，支持 TOFU 主机密钥校验",
    ),
    ("themes.name", "主题"),
    ("themes.description", "终端与界面共享的配色方案"),
    ("transports.name", "传输注册表"),
    ("transports.description", "连接提供者注册表"),
    ("workspace.name", "终端工作区"),
    ("workspace.description", "标签页、终端模拟与连接生命周期"),
];

const STATUS_ZH: &[(&str, &str)] = &[
    ("active", "活动"),
    ("pending", "等待中"),
    ("loading", "加载中"),
    ("failed", "失败"),
    ("unloaded", "已卸载"),
    ("unloading", "卸载中"),
    ("connected", "已连接"),
    ("connecting", "连接中"),
    ("closed", "已关闭"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_default_to_chinese_and_fall_back_to_english() {
        let state = Rc::new(LocalizationState::new(ZH_CN.into()));
        state.load_builtins();
        let i18n = I18n(state);
        assert_eq!(i18n.t("app", "New session"), "新建会话");
        assert_eq!(i18n.t("commands", "settings.open"), "打开设置…");
        assert_eq!(i18n.t("plugin", "ssh.name"), "SSH 传输");

        i18n.set_locale(EN_US).unwrap();
        assert_eq!(i18n.t("app", "New session"), "New session");
        assert_eq!(i18n.t("commands", "settings.open"), "settings.open");
    }

    #[test]
    fn plugin_domains_register_and_unregister() {
        let state = Rc::new(LocalizationState::new(ZH_CN.into()));
        state.load_builtins();
        let i18n = I18n(state);
        let handle = i18n.register_pairs("demo", ZH_CN, &[("Hello", "你好")]);
        assert_eq!(i18n.t("demo", "Hello"), "你好");
        i18n.unregister(handle);
        assert_eq!(i18n.t("demo", "Hello"), "Hello");
    }
}
