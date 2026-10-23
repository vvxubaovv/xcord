use std::cell::{Cell, RefCell};
use std::rc::Rc;

use serde_json::Value;
use xcord_cordis::{Context, Kernel, Plugin, PluginMeta, PluginState, Result};

#[derive(Clone)]
struct Provider {
    value: u32,
    applied: Rc<Cell<u32>>,
}

impl Plugin for Provider {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("provider", "Provider")
    }
    fn apply(&self, ctx: Context, _config: Value) -> Result<()> {
        self.applied.set(self.applied.get() + 1);
        ctx.provide("value", self.value)?;
        Ok(())
    }
}

#[derive(Clone)]
struct Consumer {
    seen: Rc<Cell<u32>>,
    effects: Rc<RefCell<Vec<&'static str>>>,
}

impl Plugin for Consumer {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("consumer", "Consumer")
    }
    fn inject(&self) -> &'static [&'static str] {
        &["value"]
    }
    fn apply(&self, ctx: Context, _config: Value) -> Result<()> {
        let value = ctx.require::<u32>("value")?;
        self.seen.set(*value);
        let effects = self.effects.clone();
        ctx.effect("consumer-a", move || effects.borrow_mut().push("a"));
        let effects = self.effects.clone();
        ctx.effect("consumer-b", move || effects.borrow_mut().push("b"));
        Ok(())
    }
}

#[test]
fn plugin_waits_for_injected_service_then_starts() {
    let kernel = Kernel::new();
    let ctx = kernel.context();
    let seen = Rc::new(Cell::new(0));
    let effects = Rc::new(RefCell::new(Vec::new()));

    let consumer = ctx.plugin(Consumer {
        seen: seen.clone(),
        effects: effects.clone(),
    });
    assert_eq!(consumer.state(), Some(PluginState::Pending));
    assert_eq!(seen.get(), 0);

    let provider = ctx.plugin(Provider {
        value: 42,
        applied: Rc::new(Cell::new(0)),
    });
    assert_eq!(provider.state(), Some(PluginState::Active));
    assert_eq!(consumer.state(), Some(PluginState::Active));
    assert_eq!(seen.get(), 42);

    kernel.shutdown();
    assert_eq!(*effects.borrow(), vec!["b", "a"]);
}

#[test]
fn unloading_provider_cascades_to_dependents() {
    let kernel = Kernel::new();
    let ctx = kernel.context();
    let seen = Rc::new(Cell::new(0));
    let effects = Rc::new(RefCell::new(Vec::new()));

    let provider = ctx.plugin(Provider {
        value: 7,
        applied: Rc::new(Cell::new(0)),
    });
    let consumer = ctx.plugin(Consumer {
        seen: seen.clone(),
        effects: effects.clone(),
    });
    assert_eq!(consumer.state(), Some(PluginState::Active));
    assert_eq!(*effects.borrow(), Vec::<&str>::new());

    provider.unload().unwrap();
    assert_eq!(provider.state(), Some(PluginState::Disposed));
    assert_eq!(consumer.state(), Some(PluginState::Disposed));
    assert_eq!(*effects.borrow(), vec!["b", "a"]);
    assert!(ctx.service::<u32>("value").is_none());
}

#[test]
fn plugin_failure_disposes_effects_and_services() {
    #[derive(Clone)]
    struct Broken {
        cleaned: Rc<Cell<bool>>,
    }
    impl Plugin for Broken {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("broken", "Broken")
        }
        fn apply(&self, ctx: Context, _config: Value) -> Result<()> {
            ctx.provide("half", 1u8)?;
            let cleaned = self.cleaned.clone();
            ctx.effect("cleanup", move || cleaned.set(true));
            Err(xcord_cordis::Error::PluginFailed {
                name: "broken",
                message: "boom".into(),
            })
        }
    }

    let kernel = Kernel::new();
    let cleaned = Rc::new(Cell::new(false));
    let handle = kernel.context().plugin(Broken {
        cleaned: cleaned.clone(),
    });

    assert_eq!(handle.state(), Some(PluginState::Failed));
    assert!(cleaned.get());
    assert!(kernel.context().service::<u8>("half").is_none());
    let info = handle.info().unwrap();
    assert!(info.error.unwrap().contains("boom"));
}

#[test]
fn events_are_scoped_to_the_plugin() {
    #[derive(Clone)]
    struct Emitter;
    impl Plugin for Emitter {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("emitter", "Emitter")
        }
        fn apply(&self, ctx: Context, _config: Value) -> Result<()> {
            ctx.provide("emit-ready", ()).ok();
            Ok(())
        }
    }

    #[derive(Clone, Debug)]
    struct Ping(u32);

    let kernel = Kernel::new();
    let ctx = kernel.context();
    let received = Rc::new(RefCell::new(Vec::new()));
    {
        let received = received.clone();
        ctx.on(move |event: &Ping, _ctx| received.borrow_mut().push(event.0));
    }
    ctx.emit(&Ping(1));
    ctx.emit(&Ping(2));
    assert_eq!(*received.borrow(), vec![1, 2]);

    kernel.shutdown();
    ctx.emit(&Ping(3));
    assert_eq!(*received.borrow(), vec![1, 2]);
}

#[test]
fn reload_restarts_plugin_and_dependents() {
    let kernel = Kernel::new();
    let ctx = kernel.context();
    let applied = Rc::new(Cell::new(0));
    let seen = Rc::new(Cell::new(0));
    let effects = Rc::new(RefCell::new(Vec::new()));

    let provider = ctx.plugin(Provider {
        value: 11,
        applied: applied.clone(),
    });
    let consumer = ctx.plugin(Consumer {
        seen: seen.clone(),
        effects: effects.clone(),
    });
    assert_eq!(applied.get(), 1);

    provider.reload().unwrap();
    assert_eq!(applied.get(), 2);
    assert_eq!(provider.state(), Some(PluginState::Active));
    assert_eq!(consumer.state(), Some(PluginState::Active));
    assert_eq!(seen.get(), 11);
    // Consumer was restarted, so its old effects ran once, then new ones were
    // registered and disposed at shutdown.
    assert_eq!(*effects.borrow(), vec!["b", "a"]);

    kernel.shutdown();
    assert_eq!(*effects.borrow(), vec!["b", "a", "b", "a"]);
}

#[test]
fn duplicate_service_is_rejected() {
    let kernel = Kernel::new();
    let ctx = kernel.context();
    ctx.plugin(Provider {
        value: 1,
        applied: Rc::new(Cell::new(0)),
    });
    let second = ctx.plugin(Provider {
        value: 2,
        applied: Rc::new(Cell::new(0)),
    });
    assert_eq!(second.state(), Some(PluginState::Failed));
    assert_eq!(
        *ctx.require::<u32>("value").unwrap(),
        1,
        "first provider keeps ownership"
    );
}

#[test]
fn hot_reload_all_preserves_services() {
    let kernel = Kernel::new();
    let ctx = kernel.context();
    ctx.plugin(Provider {
        value: 5,
        applied: Rc::new(Cell::new(0)),
    });
    ctx.plugin(Consumer {
        seen: Rc::new(Cell::new(0)),
        effects: Rc::new(RefCell::new(Vec::new())),
    });
    kernel.reload_all().unwrap();
    assert!(kernel.is_ready());
    assert_eq!(kernel.active_plugins().len(), 2);
    assert_eq!(*ctx.require::<u32>("value").unwrap(), 5);
}
