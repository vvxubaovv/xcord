use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;

use xcord_cordis::Context;

/// A command palette entry. Plugins contribute these from `apply`.
#[derive(Clone)]
pub struct Command {
    pub id: &'static str,
    pub title: String,
    pub category: &'static str,
    pub shortcut: Option<&'static str>,
    pub handler: Rc<dyn Fn(&Context)>,
}

impl Command {
    pub fn new(
        id: &'static str,
        title: impl Into<String>,
        category: &'static str,
        handler: impl Fn(&Context) + 'static,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            category,
            shortcut: None,
            handler: Rc::new(handler),
        }
    }

    pub fn shortcut(mut self, shortcut: &'static str) -> Self {
        self.shortcut = Some(shortcut);
        self
    }
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("category", &self.category)
            .finish()
    }
}

/// A plugin-owned command table. Registration handles are scope effects so a
/// hot-reloaded plugin does not leave stale commands behind.
#[derive(Default)]
pub struct CommandRegistry {
    commands: RefCell<Vec<(u64, Command)>>,
    next: RefCell<u64>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            commands: RefCell::new(Vec::new()),
            next: RefCell::new(1),
        }
    }

    pub fn register(&self, command: Command) -> u64 {
        let mut next = self.next.borrow_mut();
        let id = *next;
        *next += 1;
        self.commands.borrow_mut().push((id, command));
        id
    }

    pub fn unregister(&self, handle: u64) {
        self.commands.borrow_mut().retain(|(id, _)| *id != handle);
    }

    pub fn list(&self) -> Vec<Command> {
        self.commands
            .borrow()
            .iter()
            .map(|(_, command)| command.clone())
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<Command> {
        self.commands
            .borrow()
            .iter()
            .find(|(_, command)| command.id == id)
            .map(|(_, command)| command.clone())
    }

    pub fn run(&self, id: &str, ctx: &Context) -> bool {
        if let Some(command) = self.get(id) {
            (command.handler)(ctx);
            true
        } else {
            false
        }
    }

    pub fn len(&self) -> usize {
        self.commands.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Service wrapper: `ctx.require::<Commands>(services::COMMANDS)`.
#[derive(Clone)]
pub struct Commands(pub Rc<CommandRegistry>);

impl Deref for Commands {
    type Target = CommandRegistry;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
