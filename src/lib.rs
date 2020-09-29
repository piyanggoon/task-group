pub use crate::metrics::Metrics;
use crate::metrics::*;
use futures_util::future::{abortable, AbortHandle};
use parking_lot::Mutex;
use std::{
    collections::{hash_map::Entry, HashMap},
    fmt::{Debug, Display},
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

mod metrics;

pub type TaskId = u128;

#[derive(Clone, Debug)]
pub enum Shutdown {
    /// Runtime shutdown.
    Runtime,
    /// Task group was dropped.
    TaskGroup,
}

/// This can be awaited on to get task output.
pub struct TaskHandle<T> {
    id: TaskId,
    name: String,
    inner: Pin<Box<dyn Future<Output = Result<T, Shutdown>> + Send + 'static>>,
}

impl<T> TaskHandle<T> {
    pub fn id(&self) -> TaskId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl<T> Future for TaskHandle<T>
where
    T: Send + 'static,
{
    type Output = Result<T, Shutdown>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.inner).poll(cx)
    }
}

#[derive(Debug)]
struct InnerTask {
    abort_handle: AbortHandle,
    name: String,
}

#[derive(Debug)]
struct Inner {
    tasks: Mutex<HashMap<TaskId, InnerTask>>,
    metrics: Arc<dyn Metrics>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            tasks: Default::default(),
            metrics: Arc::new(DummyMetrics),
        }
    }
}

impl Inner {
    fn insert(&self, item: InnerTask) -> TaskId {
        let mut tasks = self.tasks.lock();
        loop {
            let id = rand::random();
            if let Entry::Vacant(vacant) = tasks.entry(id) {
                let name = item.name.clone();
                vacant.insert(item);
                self.metrics.task_started(id, name);
                return id;
            }
        }
    }

    fn remove(&self, id: TaskId) {
        let mut tasks = self.tasks.lock();
        if let Some(task) = tasks.remove(&id) {
            task.abort_handle.abort();
            self.metrics.task_stopped(id, task.name);
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        for (id, child) in self.tasks.lock().drain() {
            child.abort_handle.abort();
            self.metrics.task_stopped(id, child.name);
        }
    }
}

/// This is a holding structure that "owns" tasks. That is, when this struct is dropped, tasks are cancelled and eventually dropped from the Tokio runtime.
#[derive(Debug)]
pub struct TaskGroup {
    inner: Arc<Inner>,
}

impl Default for TaskGroup {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskGroup {
    pub fn new() -> Self {
        Self {
            inner: Default::default(),
        }
    }

    pub fn new_with_metrics<M: Metrics>(metrics: M) -> Self {
        Self {
            inner: Arc::new(Inner {
                tasks: Default::default(),
                metrics: Arc::new(metrics),
            }),
        }
    }

    /// Spawn task on this task group.
    pub fn spawn<Fut, T>(&self, future: Fut) -> TaskHandle<T>
    where
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_with_name("(unnamed)", future)
    }

    /// Spawn task on this task group.
    pub fn spawn_with_name<N, Fut, T>(&self, name: N, future: Fut) -> TaskHandle<T>
    where
        N: Display,
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let name = name.to_string();
        let (t, abort_handle) = abortable(future);
        let id = self.inner.insert(InnerTask {
            abort_handle,
            name: name.clone(),
        });
        let spawned_handle = tokio::spawn({
            let inner = Arc::downgrade(&self.inner);
            Box::pin(async move {
                let res = t.await;
                if let Some(inner) = inner.upgrade() {
                    inner.remove(id);
                }
                res
            })
        });
        TaskHandle {
            id,
            name,
            inner: Box::pin(async move {
                spawned_handle
                    .await
                    .map_err(|_| Shutdown::Runtime)?
                    .map_err(|_| Shutdown::TaskGroup)
            }),
        }
    }

    /// Abort an existing task.
    pub fn abort(&self, id: TaskId) {
        self.inner.remove(id)
    }

    /// Create a new task group that will be child to this one.
    ///
    /// Currently just inherits the metrics.
    pub fn subgroup(&self) -> Self {
        Self::new_with_metrics(self.inner.metrics.clone())
    }
}
