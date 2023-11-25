//! # Async Executor
//!
//! This crate is what you might build if you were to add a thread pool to [smol](https://github.com/smol-rs/smol), and wanted paused tasks.
//!
//! I take somewhat the opposite approach to Tokio.
//! (I assume, I've never actually used Tokio.)
//! I believe that Tokio assumes that you want all of your code to be async, whereas I do not.
//!
//! I take the approach that most of the main code is synchronous, with async code as needed.
//! This is exactly the situation I found myself in when I wanted to add async support to my language, [dwarf](https://github.com/uberFoo/dwarf).
//!
//! ## Hello Worild
//!
//! ```
//! // Create an executor with four threads.
//! # use puteketeke::{AsyncTask, Executor};
//! # use futures_lite::future;
//! let executor = Executor::new(4);
//!
//! let task = executor
//!     .create_task(async { println!("Hello world") })
//!     .unwrap();
//!
//! // Note that we need to start the task, otherwise it will never run.
//! executor.start_task(&task);
//! future::block_on(task);
//! ````
//!
//! ## Motivation
//!
//! Primarily this crate is intended to be used by those that wish to have some async in their code, and don't want to commit fully.
//! This crate does not require wrapping main in 'async fn`, and it's really pretty simple.
//! The features of note are:
//!
//! - an RAII executor, backed by a threadpool, that cleans up after itself when dropped
//! - a task that starts paused, and is freely passed around as a value
//! - the ability to create isolated workers upon which tasks are executed
//! - an ergonomic and simple API
//! - parallel timers
//!
//! Practically speaking, the greatest need that I had in dwarf, besides an executor, was the ability to enqueue a paused task, and pass it around.
//! It's not really too difficult to explain why either.
//! Consider this dwarf code:
//!
//! ```ignore
//! async fn main() -> Future<()> {
//!     let future = async {
//!         print(42);
//!     };
//!     print("The answer to life the universe and everything is: ");
//!     future.await;
//! }
//! ```
//!
//! As you might guess, this prints "The answer to life the universe and everything is: 42".
//!
//! The interpreter is processing statements and expressions.
//! When the block expression:
//!
//! ```ignore
//! async {
//!     print(42);
//! }
//! ```
//!
//! is processed, we don't want the future to start executing yet, otherwise the sentence would be wrong.
//!
//! What we need is to hold on to that future and only start it when it is awaited, after the print expression.
//! Furthermore, we need to be able to pass that future around the interpreter as we process the subsequent statements and expressions.
//!
//! ## Notes
//!
//! ### Timers
//! For reasons that we have yet to ascertain specifically, timers only work properly on one executor.
//! What is apparently happening is that `Timer::after(n)` is remembering the maximal `n` across workers.
//! This causes all timers to fire at the max of the current value or the previous maximum.
//!
//! For this reason we've included an [`Executor::timer`] method that will create a timer on the root worker.
//! Unfortunately this requires capturing a clone of the executor to pass into your future.
//!
//! Additionally concurrent timer accuracy is a function of the number of threads.
//! Based on empirical testing, there is a non-linear relationship between the timer / thread ratio and the timer error.
//! When timers == threads the error is 0.
//! As the number of concurrent timers increases the error in the duration increases.
//! I plan on working out exactly what is happening, and what I can do about it.
//!
//! ### The Future
//!
//! ~~I fully intend to add priorities to tasks.
//! I have in fact begun work, but it's stalled on some sticky ownership issues.
//! I may in fact end up taking another approach to the one that I've begun.
//! In any case, felt that it was better to publish what I have now, and then iterate on it.~~
//!
//! Been there.
//!
//! Done that.
//!
//! Got the t-shirt.
//!
//! It was sort of neat to make happen, but the tasks are dispatched too quickly to make a difference.
//!
//! I plan to see what I can do about sending `&Worker`s around, rather than owned copies.
//!
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]
use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

use async_executor::{Executor as SmolExecutor, Task as SmolTask};
use async_io::Timer;
use crossbeam::channel::{unbounded, Receiver, Sender};
use futures_lite::future;
use once_cell::sync::OnceCell;
use parking_lot::{Mutex, MutexGuard};
use slab::Slab;
use threadpool::ThreadPool;

static mut GLOBAL_LOCK: OnceCell<Mutex<()>> = OnceCell::new();
static mut EXECUTOR: OnceCell<Slab<GlobalExecutor>> = OnceCell::new();
static mut TASK_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Global lock for the executor.
///
/// We need a mutex to protect the global executor list. This probably isn't normally
/// a problem, as I expect most users to require a single executor, but for `cargo test`,
/// that's another story.
///
/// We need this because we can't wrap the `Slab<GlobalExecutor>` in a `Mutex` because
/// it would hold the mutex across the `timer` function. This would really suck, if we
/// were allowed to do it, which we are not.
struct ExecutorLock<'a> {
    spectre: PhantomData<&'a ()>,
}

impl<'a> ExecutorLock<'a> {
    fn lock() -> MutexGuard<'a, ()> {
        if let Some(lock) = unsafe { GLOBAL_LOCK.get() } {
            let guard = lock.lock();
            guard
        } else {
            unsafe {
                // During `cargo test` this can already be set, even though it
                // shouldn't be. So we just don't do anything with the result.
                // Either way there is a mutex in there and we attempt to lock it.
                let _ = GLOBAL_LOCK.set(Mutex::new(()));
            }
            let lock = unsafe { GLOBAL_LOCK.get().unwrap() };
            let guard = lock.lock();
            guard
        }
    }
}

/// Worker Abstraction
///
/// A Worker is what you are given in response to [`Executor::new_worker`] and
/// [`Executor::root_worker`] functions. It's small, it Clones, and it's all
/// you need to spawn tasks, and destroy Workers.
#[derive(Clone, Debug)]
pub enum Worker {
    /// A lone executor
    ///
    /// An executor is able to perform all of the tasks expected by a worker,
    /// so it's a valid worker.
    Executor(Executor),
    /// A special made worker
    ///
    /// This worker combines an executor, along with an index to a worker.
    Worker(Executor, usize),
}

impl Worker {
    /// Spawn a timer on the worker
    ///
    /// This is a convenience function that creates a timer on the worker.
    pub async fn timer(&self, duration: Duration) -> Instant {
        match self {
            Self::Executor(executor) => executor.timer(duration).await,
            Self::Worker(executor, _) => executor.timer(duration).await,
        }
    }

    /// Destroy the worker
    ///
    /// This invalidates the worker, as well as removing the underlying [`AsyncWorker`]
    /// from the executor.
    pub fn destroy(self) {
        match self {
            Self::Executor(_) => {}
            Self::Worker(executor, key) => executor.remove_worker(key),
        }
    }

    /// Spawn a task on the worker
    ///
    /// This is a convenience function that creates a task on the worker, and
    /// then starts the task running.
    pub fn spawn_task<T>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        match self {
            Self::Executor(executor) => executor.spawn_task(future),
            Self::Worker(executor, key) => executor.spawn_task_on_worker(*key, future),
        }
    }

    /// Create a task on the worker
    ///
    /// This is a convenience function that creates a task on the worker.
    pub fn create_task<T>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        match self {
            Self::Executor(executor) => executor.create_task(future),
            Self::Worker(executor, key) => executor.create_task_on_worker(*key, future),
        }
    }

    /// Start a task on the worker
    ///
    /// This is a convenience function that starts a task on the worker.
    pub fn start_task<T>(&self, task: &AsyncTask<'_, T>) {
        match self {
            Self::Executor(executor) => executor.start_task(task),
            Self::Worker(executor, _) => executor.start_task(task),
        }
    }
}

/// Rust async executor
///
/// This is the main interface to the runtime, besides the [`Worker`] struct.
/// What isn't covered by that struct is covered here.
///
///
#[derive(Clone, Debug)]
pub struct Executor(Arc<usize>);

impl Executor {
    /// Create a new executor
    ///
    /// This creates a new executor with the specified number of threads.
    pub fn new(thread_count: usize) -> Self {
        Self(Arc::new(GlobalExecutor::new_executor(thread_count)))
    }

    /// Spawn a task on the executor
    ///
    /// Create a new task on the main worker and start it running.
    pub fn spawn_task<T>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        GlobalExecutor::spawn_task(*self.0, future)
    }

    fn spawn_task_on_worker<T>(
        &self,
        key: usize,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        GlobalExecutor::spawn_task_on_worker(*self.0, key, future)
    }

    /// Create a task on the executor
    ///
    /// Create a task on the main worker
    pub fn create_task<T>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        GlobalExecutor::create_task(*self.0, future)
    }

    fn create_task_on_worker<T>(
        &self,
        key: usize,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        GlobalExecutor::create_task_on_worker(*self.0, key, future)
    }

    /// Pause execution for the specified duration
    ///
    /// This starts a timer on the root worker.
    pub fn timer(&self, duration: Duration) -> impl Future<Output = Instant> {
        GlobalExecutor::timer(*self.0, duration)
    }

    /// Start a paused task
    ///
    /// This starts a paused task on the executor.
    pub fn start_task<T>(&self, task: &AsyncTask<'_, T>) {
        GlobalExecutor::start_task(*self.0, task);
    }

    /// Worker cardinality
    ///
    /// This returns the number of workers on the executor.
    pub fn worker_count(&self) -> usize {
        GlobalExecutor::worker_count(*self.0)
    }

    fn remove_worker(&self, index: usize) {
        GlobalExecutor::remove_worker(*self.0, index);
    }

    /// Create a new worker
    ///
    /// This creates a new worker on the executor.
    pub fn new_worker(&self) -> Worker {
        let key = GlobalExecutor::new_worker(*self.0);
        Worker::Worker(self.clone(), key)
    }

    /// Root worker
    ///
    /// This returns the root worker on the executor.
    pub fn root_worker(&self) -> Worker {
        Worker::Executor(self.clone())
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        if Arc::strong_count(&self.0) > 1 {
            return;
        }
        GlobalExecutor::shutdown(*self.0);
    }
}

#[derive(Debug)]
struct GlobalExecutor(UberExecutor<'static>);

impl GlobalExecutor {
    fn new_executor(thread_count: usize) -> usize {
        let executor = UberExecutor::new(thread_count);
        executor.start(thread_count);

        let _guard = ExecutorLock::lock();

        match unsafe { EXECUTOR.get_mut() } {
            Some(global) => global.insert(GlobalExecutor(executor)),
            None => {
                // Outside of tests running, I think that the most common use case
                // will be one executor.
                let mut slab = Slab::with_capacity(1);
                let key = slab.insert(GlobalExecutor(executor));
                unsafe {
                    EXECUTOR.set(slab).unwrap();
                }

                key
            }
        }
    }

    fn shutdown(key: usize) {
        let _guard = ExecutorLock::lock();
        if let Some(global) = unsafe { EXECUTOR.get_mut() } {
            global.try_remove(key);
        } else {
            panic!("global executor not initialized");
        }
    }

    fn spawn_task<T>(
        executor_key: usize,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        if let Some(this) = unsafe { EXECUTOR.get_mut() } {
            if let Some(executor) = this.get_mut(executor_key) {
                let executor = &mut executor.0;
                executor.spawn_task(future)
            } else {
                None
            }
        } else {
            panic!("global executor not initialized");
        }
    }

    fn spawn_task_on_worker<T>(
        executor_key: usize,
        key: usize,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        if let Some(this) = unsafe { EXECUTOR.get_mut() } {
            if let Some(executor) = this.get_mut(executor_key) {
                let executor = &mut executor.0;
                executor.spawn_task_on_worker(key, future)
            } else {
                None
            }
        } else {
            panic!("global executor not initialized");
        }
    }

    fn create_task<T>(
        executor_key: usize,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        if let Some(this) = unsafe { EXECUTOR.get_mut() } {
            if let Some(executor) = this.get_mut(executor_key) {
                let executor = &mut executor.0;
                executor.create_task(future)
            } else {
                None
            }
        } else {
            panic!("global executor not initialized");
        }
    }

    fn create_task_on_worker<T>(
        executor_key: usize,
        key: usize,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        if let Some(this) = unsafe { EXECUTOR.get_mut() } {
            if let Some(executor) = this.get_mut(executor_key) {
                let executor = &mut executor.0;
                executor.create_task_on_worker(key, future)
            } else {
                None
            }
        } else {
            panic!("global executor not initialized");
        }
    }

    async fn timer(key: usize, duration: Duration) -> Instant {
        if let Some(this) = unsafe { EXECUTOR.get_mut() } {
            if let Some(executor) = this.get(key) {
                executor.0.timer(duration).await
            } else {
                panic!("executor {key} not initialized");
            }
        } else {
            panic!("global executor not initialized");
        }
    }

    fn start_task<T>(key: usize, task: &AsyncTask<'_, T>) {
        let this = unsafe { EXECUTOR.get().unwrap() };
        if let Some(executor) = this.get(key) {
            executor.0.start_task(task);
        } else {
            panic!("global executor not initialized");
        }
    }

    fn remove_worker(key: usize, index: usize) {
        let this = unsafe { EXECUTOR.get_mut().unwrap() };
        if let Some(executor) = this.get_mut(key) {
            executor.0.remove_worker(index);
        } else {
            panic!("global executor not initialized");
        }
    }

    fn new_worker(key: usize) -> usize {
        let this = unsafe { EXECUTOR.get_mut().unwrap() };
        if let Some(executor) = this.get_mut(key) {
            executor.0.new_worker()
        } else {
            panic!("global executor not initialized");
        }
    }

    fn worker_count(key: usize) -> usize {
        let this = unsafe { EXECUTOR.get().unwrap() };
        if let Some(executor) = this.get(key) {
            executor.0.worker_count()
        } else {
            panic!("global executor not initialized");
        }
    }
}

#[derive(Debug)]
struct UberExecutor<'a> {
    root: usize,
    workers: Mutex<Slab<AsyncWorker<'a>>>,
    pool: ThreadPool,
    sender: Option<Sender<AsyncWorker<'a>>>,
    receiver: Receiver<AsyncWorker<'a>>,
}

impl<'a> UberExecutor<'a> {
    fn new(thread_count: usize) -> Self {
        let mut workers = Slab::with_capacity(thread_count);
        let entry = workers.vacant_entry();
        let root = entry.key();
        entry.insert(AsyncWorker::new(root));

        let (worker_send, worker_recv) = unbounded();

        let pool = ThreadPool::new(thread_count);

        UberExecutor {
            root,
            workers: parking_lot::Mutex::new(workers),
            pool,
            sender: Some(worker_send),
            receiver: worker_recv,
        }
    }

    fn worker_count(&self) -> usize {
        self.workers.lock().len()
    }

    fn worker_at_index(&'a self, index: usize) -> Option<AsyncWorker<'a>> {
        let guard = self.workers.lock();
        guard.get(index).cloned()
    }

    fn root_worker(&'a self) -> usize {
        self.root
    }

    fn new_worker(&mut self) -> usize {
        let mut guard = self.workers.lock();
        let entry = guard.vacant_entry();
        let idx = entry.key();
        entry.insert(AsyncWorker::new(idx));
        idx
    }

    fn spawn_task<T>(
        &'a self,
        future: impl Future<Output = T> + Send + 'a,
    ) -> Option<AsyncTask<'a, T>>
    where
        T: Send + std::fmt::Debug + 'a,
    {
        self.spawn_task_on_worker(self.root, future)
    }

    fn spawn_task_on_worker<T>(
        &'a self,
        key: usize,
        future: impl Future<Output = T> + Send + 'a,
    ) -> Option<AsyncTask<'a, T>>
    where
        T: Send + std::fmt::Debug + 'a,
    {
        let task = self.create_task_on_worker(key, future)?;
        self.start_task(&task);
        Some(task)
    }

    fn start_task<T>(&self, task: &AsyncTask<'_, T>) {
        task.start();
        // We get the worker in this roundabout manner to avoid capturing a
        // reference to the task which causes "data escapes the function" errors.
        let worker_id = task.worker.id();
        let guard = self.workers.lock();
        if let Some(worker) = guard.get(worker_id) {
            if let Some(sender) = &self.sender {
                let _ = sender.send(worker.clone());
            }
        }
    }

    async fn timer(&'a self, duration: Duration) -> Instant {
        // We need to put all of the timers on the same worker so that they
        // interact properly.
        let task = AsyncTask::new(
            self.worker_at_index(self.root_worker()).unwrap(),
            Timer::after(duration),
        );
        self.start_task(&task);
        task.await
    }

    fn create_task<T>(
        &'a self,
        future: impl Future<Output = T> + Send + 'a,
    ) -> Option<AsyncTask<'a, T>>
    where
        T: Send + std::fmt::Debug + 'a,
    {
        self.create_task_on_worker(self.root, future)
    }

    fn create_task_on_worker<T>(
        &'a self,
        key: usize,
        future: impl Future<Output = T> + Send + 'a,
    ) -> Option<AsyncTask<'a, T>>
    where
        T: Send + std::fmt::Debug + 'a,
    {
        self.worker_at_index(key).map(|worker| {
            let task = AsyncTask::new(worker, future);
            tracing::debug!(target: "async", "create_task: {task:?}");
            task
        })
    }

    fn start(&self, thread_count: usize) {
        let receiver: Receiver<AsyncWorker<'static>> =
            unsafe { std::mem::transmute(self.receiver.clone()) };
        for _ in 0..thread_count {
            let receiver = receiver.clone();
            let span = tracing::trace_span!(
                "Executor Thread",
                id = format!("{:?}", thread::current().id())
            );

            self.pool.execute(move || {
                let _enter = span.enter();
                while let Ok(worker) = receiver.recv() {
                    tracing::trace!("Executor::run: worker found");
                    tracing::trace!("Executor::run: worker: {worker:?}");
                    match worker.try_tick() {
                        true => {
                            tracing::trace!("Executor::run: worker ticked");
                        }
                        false => {
                            tracing::trace!("Executor::run: tick failed");
                        }
                    }
                    tracing::debug!("Executor::run: worker finished");
                }
            });
        }
    }

    fn remove_worker(&'a mut self, index: usize) -> AsyncWorker<'a> {
        tracing::debug!(target: "async", "Executor::remove_worker: {index}");
        self.workers.lock().remove(index)
    }
}

impl Drop for UberExecutor<'_> {
    fn drop(&mut self) {
        drop(self.sender.take());
        self.pool.join();
    }
}

/// Asynchronous task worker
///
/// This struct is just a wrapper around a [`SmolExecutor`] and it's methods.
///
#[derive(Clone, Debug)]
pub struct AsyncWorker<'a> {
    id: usize,
    ex: Arc<SmolExecutor<'a>>,
}

impl<'a> AsyncWorker<'a> {
    /// Creates a new executor.
    fn new(id: usize) -> AsyncWorker<'a> {
        AsyncWorker {
            id,
            ex: Arc::new(SmolExecutor::new()),
        }
    }

    fn id(&self) -> usize {
        self.id
    }

    fn spawn<T>(&self, future: impl Future<Output = T> + Send + 'a) -> SmolTask<T>
    where
        T: Send + 'a,
    {
        tracing::debug!(target: "async", "spawn executor: {:?}", self);
        self.ex.spawn(future)
    }

    async fn resolve_task<T>(&self, task: SmolTask<T>) -> T
    where
        T: std::fmt::Debug,
    {
        tracing::debug!(target: "async", "ChaChaExecutor: resolve_task: {self:?}");
        tracing::debug!(target: "async", "ChaChaExecutor: resolve_task: task: {task:?}");
        let result = self.ex.run(task).await;
        tracing::debug!(target: "async", "ChaChaExecutor: resolve_task: {self:?}, RESOLVED: {result:?}");
        result
    }

    fn try_tick(&self) -> bool {
        tracing::debug!(target: "async", "try_tick: {:?}", self);
        self.ex.try_tick()
    }
}

/// An asynchronous task that starts in a quiescent state
///
/// This is a wrapper around a [`SmolTask`], which is a wrapper around a [`Future`].
/// The innovation is that when we create this task it actually wraps the user's
/// future in another. There is an atomic bool trigger to set it all in motion.
/// Once the trigger is popped, the wrapping future runs and spawns the user's
/// future onto the executor.
///
/// ```
/// # use puteketeke::{AsyncTask, Executor};
/// # let executor = Executor::new(1);
/// let task = executor.create_task(async { 42 }).unwrap();
/// println!("task id: {:?}", task.id());
/// ````
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct AsyncTask<'a, T> {
    inner: Option<SmolTask<T>>,
    worker: AsyncWorker<'a>,
    started: AtomicBool,
    waker: Option<Waker>,
    id: usize,
}

impl<'a, T> AsyncTask<'a, T> {
    fn new(worker: AsyncWorker<'a>, future: impl Future<Output = T> + Send + 'a) -> AsyncTask<'a, T>
    where
        T: Send + std::fmt::Debug + 'a,
    {
        tracing::trace_span!("AsyncTask::new");
        // 🚧 Replace this with a task id from the worker. We can use the worker id,
        // shifted left 32 bits, and then have a 32 bit task id. Rather, shift half
        // a usize, whatever it may be.I think maybe less. How many tasks might a
        // worker actually have? I guess there is the root worker. Actually, the question
        // is how many simultaneous tasks are we likely to have, because that's the size
        // of the upper word.
        let id = unsafe { TASK_COUNT.fetch_add(1, Ordering::SeqCst) };
        tracing::trace!("AsyncTask::new: {id}");

        // spawn a task that spawns a task 🌈
        let inner = worker.clone();
        let future = async move {
            tracing::trace!("AsyncTask::future: spawn inner task: {id}");

            inner.spawn(future).await
        };

        Self {
            inner: Some(worker.spawn(future)),
            worker: worker.clone(),
            started: AtomicBool::new(false),
            waker: None,
            id,
        }
    }

    /// Return the task id
    ///
    pub fn id(&self) -> usize {
        self.id
    }

    fn start(&self) {
        if !self.started.load(Ordering::SeqCst) {
            tracing::trace!(target: "async", "AsyncTask::start: {}", self.id);
            self.started.store(true, Ordering::SeqCst);
            if let Some(waker) = self.waker.as_ref() {
                waker.clone().wake();
            }
        }
    }
}

impl<'a, T> Future for AsyncTask<'a, T>
where
    T: std::fmt::Debug,
{
    type Output = T;

    #[tracing::instrument(level = "trace", target = "async")]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        tracing::trace!("AsyncTask::poll {:?}", self);
        let this = std::pin::Pin::into_inner(self);

        if this.started.load(Ordering::SeqCst) {
            tracing::trace!("AsyncTask::poll: ready: {}", this.id,);
            let task = this.inner.take().unwrap();
            Poll::Ready(future::block_on(this.worker.resolve_task(task)))
        } else {
            tracing::trace!("AsyncTask::poll: pending: {}", this.id,);
            this.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_compat::Compat;
    use futures_lite::future;
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    static INIT_LOGGER: AtomicBool = AtomicBool::new(false);

    fn start_logger() {
        if INIT_LOGGER.load(Ordering::SeqCst) {
            return;
        }
        INIT_LOGGER.store(true, Ordering::SeqCst);
        let format_layer = fmt::layer().with_thread_ids(true).pretty();
        let filter_layer =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error"));

        tracing_subscriber::registry()
            .with(filter_layer)
            .with(format_layer)
            .init();
    }

    // With the exception of the mutable/immutable borrow issue, the lifetime
    // issue is what concerns me the most. `task` is fully consumed by the
    // `block_on` call. I don't understand why executor needs to live longer.
    // #[test]
    // fn lifetime_problem() {
    //     let mut executor = UberExecutor::new(4);

    //     let task = executor
    //         .create_task(async {
    //             println!("Hello, world!");
    //         })
    //         .unwrap();
    //     executor.start_task(&task);
    //     future::block_on(task);
    // }

    #[test]
    fn test_executor() {
        color_backtrace::install();
        let executor = Executor::new(1);

        let task = executor
            .create_task(async {
                println!("Hello, world!");
                42
            })
            .unwrap();

        executor.start_task(&task);
        let foo = future::block_on(task);

        assert_eq!(42, foo);
    }

    #[test]
    fn test_timer() {
        color_backtrace::install();
        let executor = Executor::new(1);

        let inner_executor = executor.clone();
        let future = async move {
            inner_executor.timer(Duration::from_millis(0)).await;
            println!("Hello, world!");
            inner_executor.timer(Duration::from_millis(100)).await;
        };

        let task: AsyncTask<'_, ()> = executor.root_worker().create_task(future).unwrap();

        let start = Instant::now();
        executor.start_task(&task);
        let _: () = future::block_on(task);
        let duration = start.elapsed();

        assert!(duration.subsec_millis() >= 100);
    }

    #[test]
    fn test_two_timer() {
        color_backtrace::install();
        let executor = Executor::new(1);

        let inner_executor = executor.clone();
        let task_0 = executor
            .create_task(async move {
                let now = Instant::now();
                inner_executor.timer(Duration::from_millis(500)).await;
                now.elapsed()
            })
            .unwrap();

        let inner_executor = executor.clone();
        let task_1 = executor
            .create_task(async move {
                let now = Instant::now();
                inner_executor.timer(Duration::from_millis(100)).await;
                now.elapsed()
            })
            .unwrap();

        executor.start_task(&task_0);
        executor.start_task(&task_1);

        let (a, b) = future::block_on(future::zip(task_0, task_1));
        assert!(b < a);
    }

    #[test]
    fn race() {
        color_backtrace::install();
        let executor = Executor::new(1);

        let inner_executor = executor.clone();
        let task_0 = executor
            .create_task(async move {
                for _ in 0..42 {
                    inner_executor.timer(Duration::from_millis(1)).await;
                }
                42
            })
            .unwrap();

        let inner_executor = executor.clone();
        let task_1 = executor
            .create_task(async move {
                for _ in 0..96 {
                    inner_executor.timer(Duration::from_millis(1)).await;
                }
                96
            })
            .unwrap();

        executor.start_task(&task_0);
        executor.start_task(&task_1);

        let result = future::block_on(future::or(task_0, task_1));

        assert_eq!(result, 42);
    }

    #[test]
    fn stress_test_timer_workers() {
        start_logger();
        color_backtrace::install();

        // You need a significant fraction of the workers as threads.
        // This may be a bug.
        let executor = Executor::new(25);

        let mut tasks = Vec::new();
        let mut sum = 0;
        for _ in 0..100 {
            let worker = executor.new_worker();
            let sleep_millis = rand::random::<u64>() % 100;
            sum += sleep_millis;
            let inner_worker = worker.clone();
            let task = worker
                .create_task(async move {
                    let now = Instant::now();
                    inner_worker
                        .timer(Duration::from_millis(sleep_millis))
                        .await;
                    println!("sleep: {sleep_millis:?}");

                    let elapsed = now.elapsed();
                    println!("elapsed: {elapsed:?}");

                    let delta = elapsed - Duration::from_millis(sleep_millis);
                    println!("delta: {delta:?}");

                    assert!(delta.subsec_millis() < 3);
                    inner_worker.destroy();
                })
                .unwrap();
            // 🚧 Why do we need this second start here, in addition to the one
            // below? I mean, without it the timers are serial, but why?
            executor.start_task(&task);
            tasks.push(task);
        }

        let now = Instant::now();
        for task in &tasks {
            executor.start_task(task);
        }

        future::block_on(futures::future::join_all(tasks));
        let duration = now.elapsed();
        println!("duration: {duration:?}, sum: {sum}");

        // Account for the root worker.
        assert_eq!(1, executor.worker_count());
    }

    #[test]
    fn reqwest() {
        let executor = Executor::new(1);

        let future = Compat::new(async {
            let body = reqwest::get("https://www.rust-lang.org")
                .await?
                .text()
                .await?;

            println!("body = {:?}", body);
            Ok::<(), reqwest::Error>(())
        });

        let task = executor.spawn_task(future).unwrap();
        let _ = future::block_on(task);
    }

    #[test]
    fn remove_root_worker() {
        let executor = Executor::new(1);
        let worker = executor.root_worker();
        worker.destroy();
        assert_eq!(1, executor.worker_count());
    }

    #[test]
    fn worker() {
        let executor = Executor::new(1);
        let worker = executor.new_worker();

        let task0 = worker
            .spawn_task(async {
                println!("Hello, world!");
            })
            .unwrap();

        let task1 = worker.create_task(async { 42 }).unwrap();

        worker.start_task(&task1);

        let _ = future::block_on(task0);
        let _ = future::block_on(task1);

        worker.destroy();
        assert_eq!(1, executor.worker_count());
    }
}
