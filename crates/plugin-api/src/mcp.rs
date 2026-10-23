//! MCP (Model Context Protocol) capability registry.
//!
//! Xcord exposes its live workspace to AI clients as MCP tools and resources.
//! The registry lives on the UI thread; transport threads post a job to the UI
//! and wait for the result. The in-app copilot calls the same registry, so
//! external clients and the built-in assistant share one capability surface.

use std::cell::{Cell, RefCell};
use std::ops::Deref;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use xcord_cordis::Context;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub title: String,
    pub description: String,
    /// JSON Schema for the tool arguments.
    pub input_schema: Value,
    /// Read-only tools may be invoked by the copilot without enabling writes.
    #[serde(default)]
    pub read_only: bool,
}

type ToolHandler = Rc<dyn Fn(&Context, Value) -> Result<Value, String>>;

struct ToolEntry {
    id: u64,
    spec: ToolSpec,
    handler: ToolHandler,
}

/// Plugin-contributed MCP tools.
#[derive(Default)]
pub struct McpToolRegistry {
    entries: RefCell<Vec<ToolEntry>>,
    next: Cell<u64>,
}

impl McpToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        spec: ToolSpec,
        handler: impl Fn(&Context, Value) -> Result<Value, String> + 'static,
    ) -> u64 {
        let id = self.next.get() + 1;
        self.next.set(id);
        self.entries.borrow_mut().push(ToolEntry {
            id,
            spec,
            handler: Rc::new(handler),
        });
        id
    }

    pub fn unregister(&self, id: u64) {
        self.entries.borrow_mut().retain(|entry| entry.id != id);
    }

    pub fn list(&self) -> Vec<ToolSpec> {
        self.entries
            .borrow()
            .iter()
            .map(|entry| entry.spec.clone())
            .collect()
    }

    pub fn is_read_only(&self, name: &str) -> bool {
        self.entries
            .borrow()
            .iter()
            .find(|entry| entry.spec.name == name)
            .map(|entry| entry.spec.read_only)
            .unwrap_or(false)
    }

    pub fn call(&self, ctx: &Context, name: &str, args: Value) -> Result<Value, String> {
        let handler = {
            let entries = self.entries.borrow();
            let entry = entries
                .iter()
                .find(|entry| entry.spec.name == name)
                .ok_or_else(|| format!("unknown tool `{name}`"))?;
            entry.handler.clone()
        };
        handler(ctx, args)
    }
}

/// Service wrapper: `ctx.require::<McpTools>("mcp-tools")`.
#[derive(Clone)]
pub struct McpTools(pub Rc<McpToolRegistry>);

impl Deref for McpTools {
    type Target = McpToolRegistry;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceSpec {
    pub uri: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResourceContent {
    pub uri: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub text: String,
}

type ResourceReader = Rc<dyn Fn(&Context, &str) -> Result<Vec<ResourceContent>, String>>;

struct ResourceEntry {
    id: u64,
    spec: ResourceSpec,
    reader: ResourceReader,
}

#[derive(Default)]
pub struct McpResourceRegistry {
    entries: RefCell<Vec<ResourceEntry>>,
    next: Cell<u64>,
}

impl McpResourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        spec: ResourceSpec,
        reader: impl Fn(&Context, &str) -> Result<Vec<ResourceContent>, String> + 'static,
    ) -> u64 {
        let id = self.next.get() + 1;
        self.next.set(id);
        self.entries.borrow_mut().push(ResourceEntry {
            id,
            spec,
            reader: Rc::new(reader),
        });
        id
    }

    pub fn unregister(&self, id: u64) {
        self.entries.borrow_mut().retain(|entry| entry.id != id);
    }

    pub fn list(&self) -> Vec<ResourceSpec> {
        self.entries
            .borrow()
            .iter()
            .map(|entry| entry.spec.clone())
            .collect()
    }

    pub fn read(&self, ctx: &Context, uri: &str) -> Result<Vec<ResourceContent>, String> {
        let readers: Vec<ResourceReader> = self
            .entries
            .borrow()
            .iter()
            .map(|entry| entry.reader.clone())
            .collect();
        let mut last_error = None;
        for reader in readers {
            match reader(ctx, uri) {
                Ok(content) => return Ok(content),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| format!("unknown resource `{uri}`")))
    }
}

/// Service wrapper: `ctx.require::<McpResources>("mcp-resources")`.
#[derive(Clone)]
pub struct McpResources(pub Rc<McpResourceRegistry>);

impl Deref for McpResources {
    type Target = McpResourceRegistry;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Live status of the local MCP endpoint, shown in the UI.
#[derive(Clone, Debug, Default, Serialize)]
pub struct McpServerStatus {
    pub enabled: bool,
    pub running: bool,
    pub url: String,
    pub token: String,
    pub port: u16,
    pub tools: usize,
    pub error: Option<String>,
}

#[derive(Clone, Default)]
pub struct McpStatus(pub Rc<RefCell<McpServerStatus>>);

impl McpStatus {
    pub fn snapshot(&self) -> McpServerStatus {
        self.0.borrow().clone()
    }

    pub fn set(&self, status: McpServerStatus) {
        *self.0.borrow_mut() = status;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use xcord_cordis::Kernel;

    #[test]
    fn tools_are_registered_called_and_permissioned() {
        let kernel = Kernel::new();
        let ctx = kernel.context();
        let registry = McpToolRegistry::new();
        let spec = ToolSpec {
            name: "echo".into(),
            title: "Echo".into(),
            description: "Echo arguments".into(),
            input_schema: json!({"type": "object"}),
            read_only: true,
        };
        let handle = registry.register(spec, |_ctx, args| {
            Ok(json!({"echo": args.get("value").cloned().unwrap_or(Value::Null)}))
        });
        assert_eq!(registry.list().len(), 1);
        assert!(registry.is_read_only("echo"));
        let result = registry
            .call(&ctx, "echo", json!({"value": 7}))
            .expect("tool call");
        assert_eq!(result["echo"], 7);
        assert!(registry.call(&ctx, "missing", json!({})).is_err());
        registry.unregister(handle);
        assert!(registry.list().is_empty());
    }

    #[test]
    fn resources_dispatch_to_the_matching_reader() {
        let kernel = Kernel::new();
        let ctx = kernel.context();
        let registry = McpResourceRegistry::new();
        registry.register(
            ResourceSpec {
                uri: "xcord://test".into(),
                name: "Test".into(),
                description: "Test resource".into(),
                mime_type: "application/json".into(),
            },
            |_ctx, uri| {
                if uri != "xcord://test" {
                    return Err("not this resource".into());
                }
                Ok(vec![ResourceContent {
                    uri: uri.into(),
                    mime_type: "application/json".into(),
                    text: "{}".into(),
                }])
            },
        );
        let content = registry.read(&ctx, "xcord://test").unwrap();
        assert_eq!(content[0].text, "{}");
        assert!(registry.read(&ctx, "xcord://other").is_err());
    }
}
