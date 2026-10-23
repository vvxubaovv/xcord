use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;

use serde_json::Value as Config;

use crate::context::{Context, Scope};
use crate::error::{
    next_diagnostic_sequence, Diagnostic, DiagnosticLevel, Error, PluginStatusEvent, Result,
    ServiceEvent,
};
use crate::event::EventBus;
use crate::plugin::{Plugin, PluginFactory, PluginMeta};
use crate::service::ServiceSlot;

/// Identity of a plugin instance inside one kernel.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PluginId(pub(crate) u64);

impl PluginId {
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PluginId({})", self.0)
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Lifecycle state of a plugin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginState {
    /// Waiting for injected services.
    Pending,
    /// `apply` is currently running.
    Loading,
    /// Started successfully.
    Active,
    /// `apply` returned an error or panicked.
    Failed,
    /// Effects are being disposed.
    Unloading,
    /// Fully stopped; can be restarted with the same config.
    Disposed,
}

impl PluginState {
    pub fn label(self) -> &'static str {
        match self {
            PluginState::Pending => "pending",
            PluginState::Loading => "loading",
            PluginState::Active => "active",
            PluginState::Failed => "failed",
            PluginState::Unloading => "unloading",
            PluginState::Disposed => "disposed",
        }
    }

    pub fn is_settled(self) -> bool {
        matches!(
            self,
            PluginState::Active | PluginState::Failed | PluginState::Disposed
        )
    }
}

impl fmt::Display for PluginState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

pub(crate) struct PluginSlot {
    pub(crate) id: PluginId,
    pub(crate) meta: PluginMeta,
    pub(crate) inject: Vec<&'static str>,
    pub(crate) factory: PluginFactory,
    pub(crate) config: Config,
    pub(crate) state: PluginState,
    pub(crate) error: Option<String>,
    pub(crate) plugin: Option<Box<dyn Plugin>>,
    pub(crate) context: Option<Context>,
}

/// A read-only description of a plugin, suitable for a plugin-manager UI.
#[derive(Clone, Debug)]
pub struct PluginInfo {
    pub id: PluginId,
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
    pub author: &'static str,
    pub capabilities: Vec<&'static str>,
    pub inject: Vec<&'static str>,
    pub state: PluginState,
    pub error: Option<String>,
}

pub(crate) struct KernelInner {
    pub(crate) services: RefCell<HashMap<&'static str, ServiceSlot>>,
    pub(crate) events: EventBus,
    plugins: RefCell<Vec<Rc<RefCell<PluginSlot>>>>,
    root_scope: Rc<Scope>,
    next_plugin_id: Cell<u64>,
    starting: Cell<bool>,
    pending_wake: Cell<bool>,
    diagnostics: RefCell<Vec<Diagnostic>>,
    disposed: Cell<bool>,
}

/// The plugin kernel.
///
/// # Thread affinity
///
/// The kernel is intentionally `!Send`: services, plugins and contexts live on
/// the UI thread. Asynchronous workers should post closures back to the UI
/// thread; see `xcord-plugin-api`.
#[derive(Clone)]
pub struct Kernel {
    pub(crate) inner: Rc<KernelInner>,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(KernelInner {
                services: RefCell::new(HashMap::new()),
                events: EventBus::new(),
                plugins: RefCell::new(Vec::new()),
                root_scope: Rc::new(Scope::new("root")),
                next_plugin_id: Cell::new(0),
                starting: Cell::new(false),
                pending_wake: Cell::new(false),
                diagnostics: RefCell::new(Vec::new()),
                disposed: Cell::new(false),
            }),
        }
    }

    /// The root context. Application-level resources can be provided here.
    pub fn context(&self) -> Context {
        Context::new(self.inner.clone(), None, self.inner.root_scope.clone())
    }

    /// Register and (if possible) start a plugin.
    pub fn register_plugin(&self, factory: PluginFactory, config: Config) -> PluginHandle {
        let probe = factory();
        let meta = probe.meta();
        let inject = probe.inject().to_vec();

        let id = PluginId(self.inner.next_plugin_id.get() + 1);
        self.inner.next_plugin_id.set(id.0);

        let slot = Rc::new(RefCell::new(PluginSlot {
            id,
            meta: meta.clone(),
            inject,
            factory,
            config,
            state: PluginState::Pending,
            error: None,
            plugin: Some(probe),
            context: None,
        }));

        self.inner.plugins.borrow_mut().push(slot.clone());
        self.emit_status(&slot);
        self.log(
            DiagnosticLevel::Info,
            Some(id),
            format!("registered plugin `{}`", meta.name),
        );

        self.start_slot(&slot);

        PluginHandle {
            kernel: self.clone(),
            id,
        }
    }

    pub fn plugins(&self) -> Vec<PluginInfo> {
        self.inner
            .plugins
            .borrow()
            .iter()
            .map(|slot| {
                let slot = slot.borrow();
                PluginInfo {
                    id: slot.id,
                    name: slot.meta.name,
                    version: slot.meta.version,
                    description: slot.meta.description,
                    author: slot.meta.author,
                    capabilities: slot.meta.capabilities.to_vec(),
                    inject: slot.inject.clone(),
                    state: slot.state,
                    error: slot.error.clone(),
                }
            })
            .collect()
    }

    pub fn active_plugins(&self) -> Vec<PluginInfo> {
        self.plugins()
            .into_iter()
            .filter(|plugin| plugin.state == PluginState::Active)
            .collect()
    }

    pub fn plugin(&self, id: PluginId) -> Option<PluginInfo> {
        self.plugins().into_iter().find(|plugin| plugin.id == id)
    }

    pub fn plugin_id_by_name(&self, name: &str) -> Option<PluginId> {
        self.plugins()
            .into_iter()
            .find(|plugin| plugin.name == name)
            .map(|plugin| plugin.id)
    }

    pub fn state(&self, id: PluginId) -> Option<PluginState> {
        self.find(id).map(|slot| slot.borrow().state)
    }

    /// All plugin states have settled (nothing pending or loading).
    pub fn is_ready(&self) -> bool {
        self.plugins()
            .iter()
            .all(|plugin| !matches!(plugin.state, PluginState::Pending | PluginState::Loading))
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.inner.diagnostics.borrow().clone()
    }

    pub fn clear_diagnostics(&self) {
        self.inner.diagnostics.borrow_mut().clear();
    }

    /// Unload a plugin together with everything that depends on its services.
    pub fn unload(&self, id: PluginId) -> Result<()> {
        let mut seen = HashSet::new();
        self.unload_inner(id, &mut seen)
    }

    /// Hot-reload a plugin and every plugin that depends on it, preserving
    /// registration order and configuration.
    pub fn reload(&self, id: PluginId) -> Result<()> {
        if self.find(id).is_none() {
            return Err(Error::UnknownPlugin(id.0));
        }
        let order: Vec<PluginId> = self
            .inner
            .plugins
            .borrow()
            .iter()
            .map(|slot| slot.borrow().id)
            .collect();
        let reload_set = self.dependent_closure(id);

        // Dependents first (reverse registration order approximates a
        // topological order), then the target.
        for pid in order.iter().rev() {
            if reload_set.contains(pid) {
                self.unload(*pid)?;
            }
        }
        // Restart in registration order. Plugins whose dependencies are not
        // active yet remain pending and wake up automatically.
        for pid in &order {
            if reload_set.contains(pid) {
                if let Some(slot) = self.find(*pid) {
                    self.restart_slot(&slot)?;
                }
            }
        }
        self.log(
            DiagnosticLevel::Info,
            Some(id),
            "plugin hot-reloaded with dependents",
        );
        Ok(())
    }

    /// Tear down and re-apply every plugin. Used by the "Reinitialize plugins"
    /// command after configuration edits.
    pub fn reload_all(&self) -> Result<()> {
        let ids: Vec<PluginId> = self.plugins().iter().map(|plugin| plugin.id).collect();
        for id in ids.iter().rev() {
            self.unload(*id)?;
        }
        for id in &ids {
            if let Some(slot) = self.find(*id) {
                self.restart_slot(&slot)?;
            }
        }
        self.log(DiagnosticLevel::Info, None, "all plugins reinitialized");
        Ok(())
    }

    /// Unload everything and dispose root effects. The kernel cannot be reused
    /// afterwards.
    pub fn shutdown(&self) {
        if self.inner.disposed.replace(true) {
            return;
        }
        let ids: Vec<PluginId> = self.plugins().iter().map(|plugin| plugin.id).collect();
        for id in ids.iter().rev() {
            let _ = self.unload(*id);
        }
        self.inner.root_scope.dispose();
        self.inner.plugins.borrow_mut().clear();
        self.log(DiagnosticLevel::Info, None, "kernel shut down");
    }

    pub fn is_disposed(&self) -> bool {
        self.inner.disposed.get()
    }

    // ---------------------------------------------------------------- service

    pub(crate) fn provide(&self, name: &'static str, slot: ServiceSlot) -> Result<()> {
        if self.inner.disposed.get() {
            return Err(Error::Disposed);
        }
        let owner = slot.owner;
        {
            let mut services = self.inner.services.borrow_mut();
            if let Some(existing) = services.get(name) {
                let owner_name = existing
                    .owner
                    .and_then(|id| self.plugin_name(id))
                    .unwrap_or("root context")
                    .to_string();
                return Err(Error::DuplicateService {
                    name,
                    owner: owner_name,
                });
            }
            services.insert(name, slot);
        }

        let owner_id = owner.unwrap_or(PluginId(0));
        self.inner.events.emit(&ServiceEvent {
            name,
            registered: true,
            owner: owner_id,
        });
        self.log(
            DiagnosticLevel::Info,
            owner,
            format!("service `{name}` provided"),
        );

        self.wake_pending();
        Ok(())
    }

    pub(crate) fn withdraw(&self, name: &'static str, owner: Option<PluginId>) -> Result<()> {
        let removed = {
            let mut services = self.inner.services.borrow_mut();
            match services.get(name) {
                Some(existing) if existing.owner == owner => services.remove(name),
                Some(_) => return Err(Error::NotServiceOwner { name }),
                None => return Ok(()),
            }
        };
        if removed.is_some() {
            self.inner.events.emit(&ServiceEvent {
                name,
                registered: false,
                owner: owner.unwrap_or(PluginId(0)),
            });
            self.log(
                DiagnosticLevel::Info,
                owner,
                format!("service `{name}` withdrawn"),
            );
        }
        Ok(())
    }

    pub(crate) fn get_service<T: 'static>(&self, name: &'static str) -> Option<Rc<T>> {
        let (type_id, value, type_name) = {
            let services = self.inner.services.borrow();
            let slot = services.get(name)?;
            (slot.type_id, slot.value.clone(), slot.type_name)
        };
        if type_id != std::any::TypeId::of::<T>() {
            self.log(
                DiagnosticLevel::Warn,
                None,
                format!(
                    "service `{name}` is `{type_name}`, requested as `{}`",
                    std::any::type_name::<T>()
                ),
            );
            return None;
        }
        value.downcast::<T>().ok()
    }

    // ---------------------------------------------------------------- startup

    fn start_slot(&self, slot: &Rc<RefCell<PluginSlot>>) {
        if self.inner.disposed.get() {
            return;
        }

        let (id, name, config, plugin) = {
            let state = slot.borrow().state;
            if !matches!(
                state,
                PluginState::Pending | PluginState::Disposed | PluginState::Failed
            ) {
                return;
            }
            let inject = slot.borrow().inject.clone();
            if !self.deps_ready(&inject) {
                slot.borrow_mut().state = PluginState::Pending;
                return;
            }
            let mut slot = slot.borrow_mut();
            let Some(plugin) = slot.plugin.take() else {
                return;
            };
            slot.state = PluginState::Loading;
            slot.error = None;
            (slot.id, slot.meta.name, slot.config.clone(), plugin)
        };

        let scope = Rc::new(Scope::new(format!("plugin:{name}")));
        let ctx = Context::new(self.inner.clone(), Some(id), scope);
        slot.borrow_mut().context = Some(ctx.clone());
        self.emit_status(slot);

        let previous_starting = self.inner.starting.replace(true);
        let outcome =
            std::panic::catch_unwind(AssertUnwindSafe(|| plugin.apply(ctx.clone(), config)));
        self.inner.starting.set(previous_starting);

        match outcome {
            Ok(Ok(())) => {
                {
                    let mut slot = slot.borrow_mut();
                    slot.state = PluginState::Active;
                    slot.plugin = Some(plugin);
                }
                self.emit_status(slot);
                self.log(
                    DiagnosticLevel::Info,
                    Some(id),
                    format!("plugin `{name}` active"),
                );
            }
            Ok(Err(error)) => {
                let message = error.to_string();
                {
                    let mut slot = slot.borrow_mut();
                    slot.state = PluginState::Failed;
                    slot.error = Some(message.clone());
                    slot.plugin = None;
                }
                ctx.dispose_scope();
                self.remove_services_of(id);
                slot.borrow_mut().context = None;
                self.emit_status(slot);
                self.log(
                    DiagnosticLevel::Error,
                    Some(id),
                    format!("plugin `{name}` failed: {message}"),
                );
            }
            Err(payload) => {
                let panic = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("unknown panic");
                let message = format!("panicked: {panic}");
                {
                    let mut slot = slot.borrow_mut();
                    slot.state = PluginState::Failed;
                    slot.error = Some(message.clone());
                    slot.plugin = None;
                }
                ctx.dispose_scope();
                self.remove_services_of(id);
                slot.borrow_mut().context = None;
                self.emit_status(slot);
                self.log(
                    DiagnosticLevel::Error,
                    Some(id),
                    format!("plugin `{name}` {message}"),
                );
            }
        }

        if !previous_starting {
            self.wake_pending();
        }
    }

    fn wake_pending(&self) {
        if self.inner.disposed.get() {
            return;
        }
        if self.inner.starting.get() {
            self.inner.pending_wake.set(true);
            return;
        }

        self.inner.starting.set(true);
        loop {
            self.inner.pending_wake.set(false);
            let slots: Vec<Rc<RefCell<PluginSlot>>> =
                self.inner.plugins.borrow().iter().cloned().collect();
            let mut started_any = false;
            for slot in &slots {
                let ready = {
                    let slot = slot.borrow();
                    slot.state == PluginState::Pending && self.deps_ready(&slot.inject)
                };
                if ready {
                    self.start_slot(slot);
                    started_any = true;
                }
            }
            if !started_any && !self.inner.pending_wake.get() {
                break;
            }
        }
        self.inner.starting.set(false);
    }

    fn deps_ready(&self, deps: &[&'static str]) -> bool {
        let services = self.inner.services.borrow();
        deps.iter().all(|name| services.contains_key(name))
    }

    // ---------------------------------------------------------------- unload

    fn unload_inner(&self, id: PluginId, seen: &mut HashSet<PluginId>) -> Result<()> {
        if !seen.insert(id) {
            return Ok(());
        }
        let Some(slot) = self.find(id) else {
            return Err(Error::UnknownPlugin(id.0));
        };

        let state = slot.borrow().state;
        if matches!(state, PluginState::Disposed | PluginState::Unloading) {
            return Ok(());
        }
        if state == PluginState::Loading {
            return Err(Error::PluginFailed {
                name: slot.borrow().meta.name,
                message: "cannot unload a plugin while it is loading".into(),
            });
        }

        let provided: HashSet<&'static str> = self
            .inner
            .services
            .borrow()
            .iter()
            .filter(|(_, service)| service.owner == Some(id))
            .map(|(name, _)| *name)
            .collect();

        let dependents: Vec<PluginId> = if provided.is_empty() {
            Vec::new()
        } else {
            self.inner
                .plugins
                .borrow()
                .iter()
                .filter_map(|other| {
                    let other = other.borrow();
                    if other.id == id
                        || matches!(other.state, PluginState::Disposed | PluginState::Unloading)
                    {
                        return None;
                    }
                    other
                        .inject
                        .iter()
                        .any(|name| provided.contains(name))
                        .then_some(other.id)
                })
                .collect()
        };

        for dependent in dependents {
            self.unload_inner(dependent, seen)?;
        }

        {
            let mut slot = slot.borrow_mut();
            slot.state = PluginState::Unloading;
        }
        self.emit_status(&slot);

        let context = slot.borrow_mut().context.take();
        if let Some(context) = context {
            context.dispose_scope();
        }
        self.remove_services_of(id);

        {
            let mut slot = slot.borrow_mut();
            slot.state = PluginState::Disposed;
            slot.plugin = None;
        }
        self.emit_status(&slot);
        self.log(DiagnosticLevel::Info, Some(id), "plugin unloaded");
        Ok(())
    }

    fn restart_slot(&self, slot: &Rc<RefCell<PluginSlot>>) -> Result<()> {
        {
            let mut slot = slot.borrow_mut();
            if !matches!(
                slot.state,
                PluginState::Disposed | PluginState::Pending | PluginState::Failed
            ) {
                return Ok(());
            }
            let plugin = (slot.factory)();
            slot.meta = plugin.meta();
            slot.inject = plugin.inject().to_vec();
            slot.plugin = Some(plugin);
            slot.state = PluginState::Pending;
            slot.error = None;
        }
        self.start_slot(slot);
        Ok(())
    }

    fn remove_services_of(&self, id: PluginId) {
        let names: Vec<&'static str> = self
            .inner
            .services
            .borrow()
            .iter()
            .filter(|(_, service)| service.owner == Some(id))
            .map(|(name, _)| *name)
            .collect();
        for name in names {
            if self.inner.services.borrow_mut().remove(name).is_some() {
                self.inner.events.emit(&ServiceEvent {
                    name,
                    registered: false,
                    owner: id,
                });
                self.log(
                    DiagnosticLevel::Info,
                    Some(id),
                    format!("service `{name}` removed with plugin"),
                );
            }
        }
    }

    /// Transitive set of plugins that depend on `id` through its services.
    fn dependent_closure(&self, id: PluginId) -> HashSet<PluginId> {
        let mut set = HashSet::new();
        set.insert(id);

        loop {
            let mut changed = false;
            let provided: HashSet<&'static str> = self
                .inner
                .services
                .borrow()
                .iter()
                .filter(|(_, service)| {
                    service
                        .owner
                        .map(|owner| set.contains(&owner))
                        .unwrap_or(false)
                })
                .map(|(name, _)| *name)
                .collect();

            let candidates: Vec<PluginId> = self
                .inner
                .plugins
                .borrow()
                .iter()
                .filter_map(|other| {
                    let other = other.borrow();
                    if set.contains(&other.id)
                        || matches!(other.state, PluginState::Disposed | PluginState::Unloading)
                    {
                        return None;
                    }
                    other
                        .inject
                        .iter()
                        .any(|name| provided.contains(name))
                        .then_some(other.id)
                })
                .collect();

            for candidate in candidates {
                if set.insert(candidate) {
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        set
    }

    // ---------------------------------------------------------------- helpers

    fn find(&self, id: PluginId) -> Option<Rc<RefCell<PluginSlot>>> {
        self.inner
            .plugins
            .borrow()
            .iter()
            .find(|slot| slot.borrow().id == id)
            .cloned()
    }

    pub(crate) fn plugin_name(&self, id: PluginId) -> Option<&'static str> {
        self.find(id).map(|slot| slot.borrow().meta.name)
    }

    fn emit_status(&self, slot: &Rc<RefCell<PluginSlot>>) {
        let (id, name, state, error) = {
            let slot = slot.borrow();
            (slot.id, slot.meta.name, slot.state, slot.error.clone())
        };
        self.inner.events.emit(&PluginStatusEvent {
            id,
            name,
            state,
            error,
        });
    }

    fn log(&self, level: DiagnosticLevel, plugin: Option<PluginId>, message: impl Into<String>) {
        let message = message.into();
        let plugin = plugin
            .and_then(|id| self.plugin_name(id))
            .map(str::to_owned);
        match level {
            DiagnosticLevel::Info => log::info!("{}", message),
            DiagnosticLevel::Warn => log::warn!("{}", message),
            DiagnosticLevel::Error => log::error!("{}", message),
        }
        let mut diagnostics = self.inner.diagnostics.borrow_mut();
        diagnostics.push(Diagnostic {
            level,
            plugin,
            message,
            sequence: next_diagnostic_sequence(),
        });
        if diagnostics.len() > 1000 {
            diagnostics.remove(0);
        }
    }
}

/// A handle to a registered plugin instance.
#[derive(Clone)]
pub struct PluginHandle {
    kernel: Kernel,
    id: PluginId,
}

impl PluginHandle {
    pub fn id(&self) -> PluginId {
        self.id
    }

    pub fn state(&self) -> Option<PluginState> {
        self.kernel.state(self.id)
    }

    pub fn info(&self) -> Option<PluginInfo> {
        self.kernel.plugin(self.id)
    }

    pub fn unload(&self) -> Result<()> {
        self.kernel.unload(self.id)
    }

    pub fn reload(&self) -> Result<()> {
        self.kernel.reload(self.id)
    }
}

impl fmt::Debug for PluginHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginHandle")
            .field("id", &self.id)
            .field("state", &self.state())
            .finish()
    }
}
