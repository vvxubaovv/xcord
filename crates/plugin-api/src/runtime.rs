use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use xcord_cordis::Context;

/// Work that must run on the UI thread, optionally capturing `Send` data from
/// an I/O task. The closure receives the UI [`Context`] when executed.
pub type Job = Box<dyn FnOnce(&Context) + Send + 'static>;

/// Thread-safe handle used by I/O tasks to schedule work on the UI thread.
#[derive(Clone)]
pub struct PostOffice {
    sender: Sender<Job>,
}

impl PostOffice {
    pub(crate) fn new(sender: Sender<Job>) -> Self {
        Self { sender }
    }

    /// Queue a closure. Returns `false` if the UI is gone.
    pub fn post(&self, job: impl FnOnce(&Context) + Send + 'static) -> bool {
        self.sender.try_send(Box::new(job)).is_ok()
    }

    pub fn is_open(&self) -> bool {
        !self.sender.is_closed()
    }
}

impl std::fmt::Debug for PostOffice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostOffice")
            .field("open", &self.is_open())
            .finish()
    }
}

/// A cloneable reference to the I/O runtime and its post office. This is what
/// transport providers keep; they never touch the kernel directly.
#[derive(Clone)]
pub struct RuntimeHandle {
    pub handle: tokio::runtime::Handle,
    pub office: PostOffice,
}

impl RuntimeHandle {
    pub fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.handle.spawn(future);
    }

    pub fn sleep(&self, duration: Duration) -> tokio::time::Sleep {
        tokio::time::sleep(duration)
    }

    pub fn post(&self, job: impl FnOnce(&Context) + Send + 'static) -> bool {
        self.office.post(job)
    }
}

/// The I/O runtime owned by the `runtime` plugin.
///
/// The Tokio runtime is intentionally separate from GPUI's executor: blocking
/// socket work never stalls rendering, and results are posted back explicitly.
pub struct RuntimeService {
    runtime: RefCell<Option<tokio::runtime::Runtime>>,
    handle: RuntimeHandle,
    receiver: Receiver<Job>,
    context: Context,
}

impl RuntimeService {
    pub fn new(context: Context, worker_threads: usize) -> std::io::Result<Rc<Self>> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads.max(1))
            .thread_name("xcord-io")
            .enable_all()
            .build()?;
        let (sender, receiver) = async_channel::unbounded();
        let handle = RuntimeHandle {
            handle: runtime.handle().clone(),
            office: PostOffice::new(sender),
        };
        Ok(Rc::new(Self {
            runtime: RefCell::new(Some(runtime)),
            handle,
            receiver,
            context,
        }))
    }

    pub fn handle(&self) -> RuntimeHandle {
        self.handle.clone()
    }

    pub fn context(&self) -> &Context {
        &self.context
    }

    /// The UI's job receiver. Clone it into a GPUI task.
    pub fn receiver(&self) -> Receiver<Job> {
        self.receiver.clone()
    }

    pub fn try_next_job(&self) -> Option<Job> {
        self.receiver.try_recv().ok()
    }

    pub fn execute(&self, job: Job) {
        job(&self.context);
    }

    /// Run a closure on a blocking worker thread.
    pub fn spawn_blocking<F>(&self, task: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.handle.handle.spawn_blocking(task);
    }

    /// Stop accepting new jobs and tear down the I/O runtime. Called when the
    /// runtime plugin unloads; outstanding futures are dropped.
    pub fn shutdown(&self) {
        if let Some(runtime) = self.runtime.borrow_mut().take() {
            runtime.shutdown_background();
        }
    }

    pub fn is_running(&self) -> bool {
        self.runtime.borrow().is_some()
    }
}

impl Drop for RuntimeService {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.get_mut().take() {
            runtime.shutdown_background();
        }
    }
}

impl std::fmt::Debug for RuntimeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeService")
            .field("running", &self.is_running())
            .field("context", &self.context.label())
            .finish()
    }
}

/// Service wrapper: `ctx.require::<Runtime>(services::RUNTIME)`.
#[derive(Clone)]
pub struct Runtime(pub Rc<RuntimeService>);

impl Deref for Runtime {
    type Target = RuntimeService;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
