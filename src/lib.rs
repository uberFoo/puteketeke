//! # uberFoo Executor
//!
//! This crate is what you might build if you were to add a thread pool to smol, and wanted paused tasks.
//!
//! uberFoo Executor is a simple executor built on top of [smol](https://github.com/smol-rs/smol).
//! It's main purpose is to provide an ergonomic API for adding async support to interpreters and virtual machines.
//! Here's a hello world:
//!
//! ```
//! // Create an executor with four threads.
//! # use uberfoo_async::{AsyncTask, Executor};
//! # use futures_lite::future;
//! let executor = Executor::new(4);
//!
//! let task = executor
//!     .create_task(async { println!("Hello world") })
//!     .unwrap();
//!
//! executor.start_task(&task);
//! future::block_on(task);
//! ````
//!
//! ## Random
//!
//! This crate takes something like the opposite approach to Tokio.
//! Tokio assumes that you want all of your code to be async, whereas we do not.
//! We take the approach that most of the main code is synchronous, with async code as needed.
//!
//! ## Timers
//! For reasons that I have yet to ascertain, timers only work properly on one executor.
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
//!
//! Given a bit of thought, this is not surprising.
//! In the `stress_test_timer_workers` unit test, we start a number of concurrent timers.
//! When the first timer is polled it will be Pending.
//!
//!
//!
//! ## The Future
//!
//! I fully intend to add priorities to tasks.
//! I have in fact begun work, but it's stalled on some sticky ownership issues.
//! I may in fact end up taking another approach to the one that I''ve begun.
//! In any case, felt that it was better to publish what I have now, and then iterate on it.
//!
//! I'd like to sort out the issue that is requiring the use of a static global.
//!
//! I also plan to see what I can do about sending `&Worker`s around, rather than owned copies.
//!
//! ```ignore
//! // Create an executor with four threads.
//! # use uberfoo_async::{AsyncTask, Executor};
//! # use futures_lite::future;
//! let executor = Executor::new(4);
//! let future = async {
//!    println!("Hello, world!");
//! };
//!
//! let worker_id = executor.get_root_worker_id();
//! let worker = executor.get_worker(worker_id).unwrap();
//! let task = AsyncTask::new(worker, future);
//!
//! // Equivalently
//! // let task = executor.create_task(future).unwrap();
//!
//! executor.start_task(&task);
//! future::block_on(task);
//! ````

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
use tracing;

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

pub enum Worker {
    Executor(Executor),
    Worker(Executor, usize),
}

impl Worker {
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
}

#[derive(Clone, Debug)]
pub struct Executor(Arc<usize>);

impl Executor {
    pub fn new(thread_count: usize) -> Self {
        Self(Arc::new(GlobalExecutor::new_executor(thread_count)))
    }

    pub fn create_task<T>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        GlobalExecutor::create_task(*self.0, future)
    }

    pub fn create_task_on_worker<T>(
        &self,
        key: usize,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Option<AsyncTask<'static, T>>
    where
        T: Send + std::fmt::Debug + 'static,
    {
        GlobalExecutor::create_task_on_worker(*self.0, key, future)
    }

    pub fn timer(&self, duration: Duration) -> impl Future<Output = Instant> {
        GlobalExecutor::timer(*self.0, duration)
    }

    pub fn start_task<T>(&self, task: &AsyncTask<T>) {
        GlobalExecutor::start_task(*self.0, task);
    }

    pub fn remove_worker(&self, index: usize) {
        GlobalExecutor::remove_worker(*self.0, index);
    }

    pub fn new_worker(&self) -> Worker {
        let key = GlobalExecutor::new_worker(*self.0);
        Worker::Worker(self.clone(), key)
    }

    pub fn root_worker(&self) -> Worker {
        let key = GlobalExecutor::root_worker(*self.0);
        Worker::Worker(self.clone(), key)
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

    fn start_task<T>(key: usize, task: &AsyncTask<T>) {
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

    pub fn root_worker(key: usize) -> usize {
        let this = unsafe { EXECUTOR.get().unwrap() };
        if let Some(executor) = this.get(key) {
            executor.0.root_worker()
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

    fn worker_at_index(&'a self, index: usize) -> Option<AsyncWorker<'a>> {
        let guard = self.workers.lock();
        if let Some(worker) = guard.get(index) {
            Some(worker.clone())
        } else {
            None
        }
    }

    fn root_worker(&'a self) -> usize {
        self.root
    }

    fn new_worker(&mut self) -> usize {
        let mut guard = self.workers.lock();
        let entry = guard.vacant_entry();
        let idx = entry.key();
        entry.insert(AsyncWorker::new(idx));
        // dbg!(idx);
        idx
    }

    fn start_task<T>(&self, task: &AsyncTask<T>) {
        task.start();
        // We get the worker in this roundabout manner to avoid capturing a
        // reference to the task which causes "data escapes the function" errors.
        let worker_id = task.worker.id();
        let guard = self.workers.lock();
        let worker = guard.get(worker_id).unwrap();
        if let Some(sender) = &self.sender {
            let _ = sender.send(worker.clone());
        }
    }

    async fn timer(&'a self, duration: Duration) -> Instant {
        // We need to put all of the timers on the same worker so that they
        // interact properly.
        // dbg!(self.root_worker());
        // dbg!(self.worker_at_index(self.root_worker()).unwrap());
        // let timer = Timer::after(duration);
        // dbg!(timer);
        let task = AsyncTask::new(
            self.worker_at_index(self.root_worker()).unwrap(),
            Timer::after(duration),
        );
        self.start_task(&task);
        task.await
    }

    fn create_task<T>(
        &'a mut self,
        future: impl Future<Output = T> + Send + 'a,
    ) -> Option<AsyncTask<'a, T>>
    where
        T: Send + std::fmt::Debug + 'a,
    {
        self.create_task_on_worker(self.root, future)
    }

    fn create_task_on_worker<T>(
        &'a mut self,
        key: usize,
        future: impl Future<Output = T> + Send + 'a,
    ) -> Option<AsyncTask<'a, T>>
    where
        T: Send + std::fmt::Debug + 'a,
    {
        if let Some(worker) = self.worker_at_index(key) {
            Some(AsyncTask::new(worker, future))
        } else {
            None
        }
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
                loop {
                    match receiver.recv().ok() {
                        Some(worker) => {
                            tracing::trace!("Executor::run: worker found");
                            tracing::trace!("Executor::run: worker: {worker:?}");
                            match worker.try_tick() {
                                true => {
                                    tracing::trace!("Executor::run: worker ticked");
                                    tracing::trace!("Executor::run: worker: {worker:?}");
                                }
                                false => {
                                    tracing::trace!("Executor::run: tick failed");
                                    tracing::trace!("Executor::run: worker: {worker:?}");
                                }
                            }
                            tracing::debug!("Executor::run: worker finished");
                            tracing::trace!("Executor::run: thread: {:?}", thread::current().id());
                        }
                        None => break,
                    };
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

#[derive(Clone, Debug)]
pub struct AsyncWorker<'a> {
    id: usize,
    pub ex: Arc<SmolExecutor<'a>>,
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
    pub fn new(
        worker: AsyncWorker<'a>,
        future: impl Future<Output = T> + Send + 'a,
    ) -> AsyncTask<'a, T>
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

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn start(&self) {
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
    fn race() {
        let executor = Executor::new(1);

        let inner_executor = executor.clone();
        let task_1 = executor
            .create_task(async move {
                for _ in 0..96 {
                    inner_executor.timer(Duration::from_millis(1)).await;
                }
                96
            })
            .unwrap();

        let inner_executor = executor.clone();
        let task_0 = executor
            .create_task(async move {
                for _ in 0..42 {
                    inner_executor.timer(Duration::from_millis(1)).await;
                }
                42
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

        let executor = Executor::new(50);

        let mut tasks = Vec::new();
        let mut sum = 0;
        for _ in 0..100 {
            let worker = executor.new_worker();
            let sleep_millis = rand::random::<u64>() % 100;
            sum += sleep_millis;
            let inner_executor = executor.clone();
            let task = worker
                .create_task(async move {
                    let now = Instant::now();
                    inner_executor
                        .timer(Duration::from_millis(sleep_millis))
                        .await;

                    let elapsed = now.elapsed();
                    let delta = elapsed - Duration::from_millis(sleep_millis);
                    println!("sleep: {sleep_millis:?}");
                    println!("elapsed: {elapsed:?}");
                    println!("delta: {delta:?}");
                    // println!("duration: {duration:?}");
                    // assert!(delta.subsec_millis() < 5);
                })
                .unwrap();
            executor.start_task(&task);
            tasks.push(task);
        }

        let now = Instant::now();
        for task in &tasks {
            executor.start_task(&task);
        }

        future::block_on(futures::future::join_all(tasks));
        let duration = now.elapsed();
        println!("duration: {duration:?}, sum: {sum}");
    }
}
