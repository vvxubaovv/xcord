use std::cell::{Cell, RefCell};
use std::panic::AssertUnwindSafe;
use std::rc::Rc;

use serde_json::Value as Config;

use crate::kernel::KernelInner;
use crate::service::ServiceSlot;
use crate::{
    Error, Event, EventBus, Kernel, Plugin, PluginFactory, PluginHandle, PluginId, Result,
};

/// An undo action owned by a plugin scope.
///
/// Effects are executed in reverse registration order when the scope is
/// disposed, mirroring Cordis' `ctx.effect()`:
///
/// ```
/// # use xcord_cordis::Kernel;
/// let kernel = Kernel::new();
/// let ctx = kernel.context();
/// let order = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
/// let o1 = order.clone();
/// ctx.effect("first", move || o1.borrow_mut().push(1));
/// let o2 = order.clone();
/// ctx.effect("second", move || o2.borrow_mut().push(2));
/// kernel.shutdown();
/// assert_eq!(*order.borrow(), vec![2, 1]);
/// ```
pub struct Effect {
    label: String,
    dispose: Option<Box<dyn FnOnce()>>,
}

impl Effect {
    pub(crate) fn new(label: impl Into<String>, dispose: impl FnOnce() + 'static) -> Self {
        Self {
            label: label.into(),
            dispose: Some(Box::new(dispose)),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn run(mut self) {
        let Some(dispose) = self.dispose.take() else {
            return;
        };
        if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(dispose)) {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            log::error!(
                "effect `{}` panicked during disposal: {}",
                self.label,
                message
            );
        }
    }
}

impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Effect")
            .field("label", &self.label)
            .field("spent", &self.dispose.is_none())
            .finish()
    }
}

/// A live event listener.
///
/// Subscriptions created through [`Context::on`] are also registered as
/// effects, so manual cancellation is only needed for early opt-out.
pub struct Subscription {
    pub(crate) bus: EventBus,
    pub(crate) id: u64,
    pub(crate) active: bool,
}

impl Subscription {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn off(&mut self) {
        if self.active {
            self.active = false;
            self.bus.unsubscribe(self.id);
        }
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("id", &self.id)
            .field("active", &self.active)
            .finish()
    }
}

/// A resource scope. Each plugin owns exactly one; the root context owns the
/// kernel's root scope.
pub(crate) struct Scope {
    pub(crate) label: String,
    pub(crate) effects: RefCell<Vec<Effect>>,
    pub(crate) disposed: Cell<bool>,
}

impl Scope {
    pub(crate) fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            effects: RefCell::new(Vec::new()),
            disposed: Cell::new(false),
        }
    }

    pub(crate) fn dispose(&self) {
        if self.disposed.replace(true) {
            return;
        }
        loop {
            let effect = self.effects.borrow_mut().pop();
            match effect {
                Some(effect) => effect.run(),
                None => break,
            }
        }
    }

    pub(crate) fn push(&self, effect: Effect) {
        if self.disposed.get() {
            effect.run();
        } else {
            self.effects.borrow_mut().push(effect);
        }
    }
}

/// The handle every plugin receives.
///
/// `Context` is cheap to clone. Clones share the same scope, so effects and
/// listeners registered through a clone still belong to the originating
/// plugin and are disposed with it.
#[derive(Clone)]
pub struct Context {
    pub(crate) inner: Rc<KernelInner>,
    pub(crate) owner: Option<PluginId>,
    pub(crate) scope: Rc<Scope>,
}

impl Context {
    pub(crate) fn new(inner: Rc<KernelInner>, owner: Option<PluginId>, scope: Rc<Scope>) -> Self {
        Self {
            inner,
            owner,
            scope,
        }
    }

    /// The plugin that owns this context, or `None` for the root context.
    pub fn owner(&self) -> Option<PluginId> {
        self.owner
    }

    /// The kernel this context belongs to.
    pub fn kernel(&self) -> Kernel {
        Kernel {
            inner: self.inner.clone(),
        }
    }

    /// Register an effect. It runs (LIFO) when this context's scope is disposed.
    pub fn effect(&self, label: impl Into<String>, dispose: impl FnOnce() + 'static) {
        self.scope.push(Effect::new(label, dispose));
    }

    /// Alias for [`Context::effect`], familiar from Cordis.
    pub fn on_cleanup(&self, label: impl Into<String>, dispose: impl FnOnce() + 'static) {
        self.effect(label, dispose);
    }

    /// Provide a typed service under a stable name.
    pub fn provide<T: 'static>(&self, name: &'static str, value: T) -> Result<()> {
        self.provide_rc(name, Rc::new(value))
    }

    /// Provide an already-reference-counted service (useful for sharing a
    /// struct with several consumers without another wrapper).
    pub fn provide_rc<T: 'static>(&self, name: &'static str, value: Rc<T>) -> Result<()> {
        self.kernel()
            .provide(name, ServiceSlot::new(value, self.owner))
    }

    /// Withdraw a service by name. Only the owning plugin may do this.
    pub fn withdraw(&self, name: &'static str) -> Result<()> {
        self.kernel().withdraw(name, self.owner)
    }

    /// Fetch a service. Returns `None` if it is absent or has another type.
    pub fn service<T: 'static>(&self, name: &'static str) -> Option<Rc<T>> {
        self.kernel().get_service::<T>(name)
    }

    /// Fetch a service or produce a descriptive error.
    pub fn require<T: 'static>(&self, name: &'static str) -> Result<Rc<T>> {
        let plugin = self.owner.and_then(|id| self.kernel().plugin_name(id));
        self.kernel()
            .get_service::<T>(name)
            .ok_or(Error::MissingService { name, plugin })
    }

    pub fn has_service(&self, name: &'static str) -> bool {
        self.inner.services.borrow().contains_key(name)
    }

    pub fn service_names(&self) -> Vec<&'static str> {
        self.inner.services.borrow().keys().copied().collect()
    }

    /// Publish a typed event.
    pub fn emit<E: Event>(&self, event: &E) {
        self.inner.events.emit(event);
    }

    /// Subscribe to a typed event. The listener is scoped to this plugin and is
    /// removed automatically on unload.
    pub fn on<E: Event>(&self, handler: impl Fn(&E, &Context) + 'static) -> Subscription {
        let ctx = self.clone();
        let subscription = self
            .inner
            .events
            .subscribe(move |event| handler(event, &ctx));
        let bus = self.inner.events.clone();
        let id = subscription.id();
        self.effect("event listener", move || bus.unsubscribe(id));
        subscription
    }

    /// Register a plugin from a clonable prototype. The kernel calls `clone()`
    /// each time the plugin (re)starts, so hot reload produces a fresh state.
    pub fn plugin<P: Plugin + Clone + 'static>(&self, plugin: P) -> PluginHandle {
        self.plugin_with(plugin, Config::Null)
    }

    /// Register a plugin with configuration.
    pub fn plugin_with<P: Plugin + Clone + 'static>(
        &self,
        plugin: P,
        config: impl Into<Config>,
    ) -> PluginHandle {
        self.plugin_factory(move || plugin.clone(), config)
    }

    /// Register a plugin from a factory function, allowing each (re)start to
    /// build a fresh instance.
    pub fn plugin_factory<P, F>(&self, make: F, config: impl Into<Config>) -> PluginHandle
    where
        P: Plugin,
        F: Fn() -> P + 'static,
    {
        let factory: PluginFactory = Rc::new(move || Box::new(make()));
        self.kernel().register_plugin(factory, config.into())
    }

    /// A label for diagnostics.
    pub fn label(&self) -> &str {
        &self.scope.label
    }

    pub(crate) fn dispose_scope(&self) {
        self.scope.dispose();
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("owner", &self.owner)
            .field("scope", &self.scope.label)
            .finish()
    }
}
