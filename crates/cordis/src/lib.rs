//! # xcord-cordis
//!
//! A small, single-threaded re-imagining of [Cordis](https://cordis.js.org/) for Rust.
//!
//! Cordis describes itself as *"a meta-framework for plugin-based applications"*.
//! Everything in an application is a plugin: plugins provide services, consume
//! services, listen to events, and spawn effects. A plugin is never called
//! directly — it is loaded because its dependencies exist, and unloaded when
//! they disappear.
//!
//! This crate implements the same ideas with Rust's type system:
//!
//! * [`Context`] — the handle every plugin receives. Provides typed services,
//!   event subscription, effects, and child plugin management.
//! * [`Plugin`] — the unit of behaviour. Declares injected service names and is
//!   started only when all of them are available.
//! * **Effects** — undo actions scoped to a plugin. They run LIFO when the
//!   plugin is unloaded, which is how resources never leak.
//! * **[`EventBus`]** — typed, decoupled communication between plugins.
//! * **Dependency graph** — unloading a service unloads everything that depends
//!   on it; adding one starts everything that was waiting for it.
//! * **Hot reload** — a plugin can be unloaded and re-applied with the same
//!   config, together with its dependents.
//!
//! The kernel is deliberately single-threaded (`Rc`/`RefCell`, no `Send`
//! bounds). GUIs are single-threaded too; asynchronous I/O should live in a
//! separate runtime and communicate with the UI thread through a post-office
//! channel. See `xcord-plugin-api` for that bridge.
//!
//! ## Example
//!
//! ```
//! use xcord_cordis::{Context, Plugin, PluginMeta, Result};
//! use serde_json::Value;
//!
//! #[derive(Clone)]
//! struct Clock;
//! impl Plugin for Clock {
//!     fn meta(&self) -> PluginMeta { PluginMeta::new("clock", "Clock") }
//!     fn apply(&self, ctx: Context, _config: Value) -> Result<()> {
//!         ctx.provide("clock", "tick".to_string())?;
//!         Ok(())
//!     }
//! }
//!
//! #[derive(Clone)]
//! struct Greeter;
//! impl Plugin for Greeter {
//!     fn meta(&self) -> PluginMeta { PluginMeta::new("greeter", "Greeter") }
//!     fn inject(&self) -> &'static [&'static str] { &["clock"] }
//!     fn apply(&self, ctx: Context, _config: Value) -> Result<()> {
//!         let clock = ctx.require::<String>("clock")?;
//!         assert_eq!(&*clock, "tick");
//!         Ok(())
//!     }
//! }
//!
//! let kernel = xcord_cordis::Kernel::new();
//! let ctx = kernel.context();
//! ctx.plugin(Greeter);       // waits for "clock"
//! ctx.plugin(Clock);         // starts Greeter too
//! assert_eq!(kernel.active_plugins().len(), 2);
//! ```

mod context;
mod error;
mod event;
mod kernel;
mod plugin;
mod service;

pub use context::{Context, Effect, Subscription};
pub use error::{
    Diagnostic, DiagnosticLevel, Error, Event, PluginStatusEvent, Result, ServiceEvent,
};
pub use event::EventBus;
pub use kernel::{Kernel, PluginHandle, PluginId, PluginInfo, PluginState};
pub use plugin::{factory, Plugin, PluginFactory, PluginMeta};

pub use serde_json::Value as Config;
