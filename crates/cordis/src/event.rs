use std::any::{Any, TypeId};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::context::Subscription;
use crate::Event;

struct Entry {
    id: u64,
    handler: Rc<dyn Fn(&dyn Any)>,
}

struct EventBusInner {
    next_id: Cell<u64>,
    handlers: RefCell<HashMap<TypeId, Vec<Entry>>>,
    removed: RefCell<HashSet<u64>>,
}

/// A typed publish/subscribe bus shared by all plugins.
///
/// Events are keyed by their Rust type, which gives compile-time checked
/// payloads without a global string registry. Listeners are removed
/// automatically when the plugin scope that created them is disposed.
#[derive(Clone)]
pub struct EventBus {
    inner: Rc<EventBusInner>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(EventBusInner {
                next_id: Cell::new(1),
                handlers: RefCell::new(HashMap::new()),
                removed: RefCell::new(HashSet::new()),
            }),
        }
    }

    /// Subscribe to `E`. The returned [`Subscription`] can be dropped; the
    /// listener lives until it is explicitly cancelled or its owning plugin
    /// scope is disposed.
    pub fn subscribe<E: Event>(&self, handler: impl Fn(&E) + 'static) -> Subscription {
        let id = self.inner.next_id.get();
        self.inner.next_id.set(id + 1);

        let entry = Entry {
            id,
            handler: Rc::new(move |any: &dyn Any| {
                if let Some(event) = any.downcast_ref::<E>() {
                    handler(event);
                }
            }),
        };

        self.inner
            .handlers
            .borrow_mut()
            .entry(TypeId::of::<E>())
            .or_default()
            .push(entry);

        Subscription {
            bus: self.clone(),
            id,
            active: true,
        }
    }

    pub fn unsubscribe(&self, id: u64) {
        self.inner.removed.borrow_mut().insert(id);
        // Compact lazily: remove from every type bucket only when buckets grow
        // large, keeping dispatch cheap for the common case.
        if self.inner.removed.borrow().len() > 64 {
            let removed = std::mem::take(&mut *self.inner.removed.borrow_mut());
            for entries in self.inner.handlers.borrow_mut().values_mut() {
                entries.retain(|entry| !removed.contains(&entry.id));
            }
        }
    }

    /// Publish an event to all current listeners.
    pub fn emit<E: Event>(&self, event: &E) {
        let entries: Vec<Entry> = {
            let handlers = self.inner.handlers.borrow();
            match handlers.get(&TypeId::of::<E>()) {
                Some(entries) => entries
                    .iter()
                    .map(|entry| Entry {
                        id: entry.id,
                        handler: entry.handler.clone(),
                    })
                    .collect(),
                None => return,
            }
        };

        let any: &dyn Any = event;
        for entry in entries {
            if self.inner.removed.borrow().contains(&entry.id) {
                continue;
            }
            (entry.handler)(any);
        }
    }

    /// Number of live listeners (for diagnostics/tests).
    pub fn listener_count(&self) -> usize {
        let removed = self.inner.removed.borrow();
        self.inner
            .handlers
            .borrow()
            .values()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| !removed.contains(&entry.id))
                    .count()
            })
            .sum()
    }
}
