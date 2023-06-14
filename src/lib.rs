//! This library provides a timer driver for scheduling and executing timed operations. The driver
//! allows you to create handles for sleeping for a specific duration or until a specified timeout.
//! It operates in a concurrent environment and uses a binary heap for efficient scheduling of
//! events.
//!
//! # Example
//!
//! ```rust
//! use async_spin_sleep::Builder;
//! use std::time::Duration;
//!
//! // Create a handle for sleeping for 1 second
//! let (handle, driver) = Builder::default().build();
//!
//! // Spawn the driver on a separate thread.
//! // The timer will be dropped when all handles are dropped.
//! std::thread::spawn(driver);
//!
//! let sleep_future = handle.sleep_for(Duration::from_secs(1));
//!
//! // Wait for the sleep future to complete
//! let result = futures::executor::block_on(sleep_future);
//! if let Ok(overly) = result {
//!     println!("Slept {overly:?} more than requested");
//! } else {
//!     println!("Sleep error: {:?}", result.err());
//! }
//! ```
use std::{
    mem::replace,
    pin::Pin,
    sync::{atomic::{AtomicIsize, Ordering}, Arc},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use crossbeam::channel;
use driver::{NodeDesc, WakerCell};

/* -------------------------------------------- Init -------------------------------------------- */
#[cfg(windows)]
const DEFAULT_SCHEDULE_RESOLUTION: Duration = Duration::from_millis(33);
#[cfg(unix)]
const DEFAULT_SCHEDULE_RESOLUTION: Duration = Duration::from_millis(3);

#[derive(Debug)]
pub struct Builder {
    /// Default scheduling resolution for this driver. Setting this to a lower value may decrease
    /// CPU usage of the driver, but may also dangerously increase the chance of missing a wakeup
    /// event due to the OS scheduler.
    pub schedule_resolution: Duration,

    /// Aborted nodes that are too far from execution may remain in the driver's memory for a long
    /// time. This value specifies the maximum number of aborted nodes that can be stored in the
    /// driver's memory. If this value is exceeded, the driver will collect garbage.
    pub collect_garbage_at: usize,

    /// Set channel capacity. This value is used to initialize the channel that connects the driver
    /// and its handles. If the channel is full, the driver will block until the channel is
    /// available.
    ///
    /// When [`None`] is specified, an unbounded channel will be used.
    pub channel_capacity: Option<usize>,

    /// A shared handle to represent naive number of garbage nodes to collect
    ///
    /// # How it works
    ///
    /// [future]
    /// 1. node is aborted
    /// 2. downgrade handle
    ///     - if node is alive -> worker already locked the weak handle, thus we don't touch
    ///       the `gc_count`, as the server is going to call waker
    ///     - if node dead -> worker didn't lock the weak handle, it may persist in the queue. thus
    ///       we should increment `gc_count`
    ///
    /// [worker]
    /// - on every new node insertion, check if `gc_count` is greater than `collect_garbage_at`
    /// - on node execution, try to lock the handle, and if it fails, decrement `gc_count` and
    ///   consume the node.
    gc_counter: Arc<AtomicIsize>,

    // Force 'default' only
    _0: (),
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            schedule_resolution: DEFAULT_SCHEDULE_RESOLUTION,
            collect_garbage_at: 1000,
            channel_capacity: None,
            gc_counter: Default::default(),
            _0: (),
        }
    }
}

impl Builder {
    pub fn build(self) -> (Handle, impl FnOnce()) {
        self.build_d_ary::<2>()
    }

    pub fn build_d_ary<const D: usize>(self) -> (Handle, impl FnOnce()) {
        let (tx, rx) = if let Some(cap) = self.channel_capacity {
            channel::bounded(cap)
        } else {
            channel::unbounded()
        };

        let handle = Handle { tx: tx.clone(), gc_counter: self.gc_counter.clone() };
        let driver = move || driver::execute::<D>(self, rx);

        (handle, driver)
    }
}

pub fn create() -> (Handle, impl FnOnce()) {
    Builder::default().build()
}

pub fn create_d_ary<const D: usize>() -> (Handle, impl FnOnce()) {
    Builder::default().build_d_ary::<D>()
}

/* ------------------------------------------- Driver ------------------------------------------- */
mod driver {
    use std::{
        cell::UnsafeCell,
        sync::{atomic::Ordering::{Relaxed, self}, Weak},
        task::Waker,
        time::{Duration, Instant},
    };

    use crossbeam::channel::{self, TryRecvError};
    use dary_heap::DaryHeap;
    use educe::Educe;

    use crate::Builder;

    #[derive(Debug)]
    pub(crate) enum Event {
        SleepUntil(NodeDesc),
    }

    /// ```plain
    /// if exists foremost-node
    ///     if node is far from execution
    ///         condvar-sleep until safety limit
    ///         continue
    ///     else
    ///         while until foremost-node is executed
    /// else
    ///     wait condvar
    /// ```
    pub(crate) fn execute<const D: usize>(this: Builder, rx: channel::Receiver<Event>) {
        let mut nodes = DaryHeap::<Node, D>::new();
        let pivot = Instant::now();
        let to_usec = |x: Instant| x.duration_since(pivot).as_micros() as u64;
        let resolution_usec = this.schedule_resolution.as_micros() as u64;
        let gc_counter = this.gc_counter;

        'outer: loop {
            let now = to_usec(Instant::now());

            let mut event = if let Some(node) = nodes.peek() {
                let remain = node.timeout_usec.saturating_sub(now);
                if remain > resolution_usec {
                    let system_sleep_for = remain - resolution_usec;
                    let Ok(x) = rx.recv_timeout(Duration::from_micros( system_sleep_for)) else { continue };
                    x
                } else {
                    loop {
                        let now = to_usec(Instant::now());
                        if now >= node.timeout_usec {
                            let node = nodes.pop().unwrap();

                            if let Some(waker) = node.weak_waker.upgrade() {
                                // SAFETY: No other thread access value of this pointer.
                                unsafe { (*waker.get()).take().unwrap().wake() }
                            } else {
                                gc_counter.fetch_sub(1, Ordering::AcqRel);
                            }

                            continue 'outer;
                        } else {
                            match rx.try_recv() {
                                Ok(x) => break x,
                                Err(TryRecvError::Disconnected) if nodes.is_empty() => break 'outer,
                                Err(TryRecvError::Disconnected) | Err(TryRecvError::Empty) => {
                                    std::thread::yield_now()
                                }
                            }
                        }
                    }
                }
            } else {
                let Ok(x) = rx.recv() else { 
                    break 
                };
                x
            };

            if gc_counter.load(Ordering::Acquire) as usize > this.collect_garbage_at {
                let fn_retain = |x: &Node| x.weak_waker.strong_count() > 0;

                let mut vec = nodes.into_vec();
                let num_total = vec.len();
                vec.retain(fn_retain);
                nodes = DaryHeap::from(vec);

                let n_collected = num_total - nodes.len();
                gc_counter.fetch_sub(n_collected as _, Ordering::AcqRel);
            }

            loop {
                match event {
                    Event::SleepUntil(desc) => {
                        nodes.push(Node { timeout_usec: to_usec(desc.timeout), weak_waker: desc.waker })
                    }
                };

                // Consume all events in the channel.
                match rx.try_recv() {
                    Ok(x) => event = x,
                    Err(TryRecvError::Disconnected) if nodes.is_empty() => break 'outer,
                    Err(TryRecvError::Disconnected) | Err(TryRecvError::Empty) => break,
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    pub(crate) struct NodeDesc {
        pub timeout: Instant,
        pub waker: Weak<WakerCell>,
    }

    #[derive(Debug, Clone, Educe)]
    #[educe(Eq, PartialEq, PartialOrd, Ord)]
    pub(crate) struct Node {
        #[educe(PartialOrd(method = "cmp_rev_partial"), Ord(method = "cmp_rev"))]
        pub timeout_usec: u64,

        #[educe(Eq(ignore), PartialEq(ignore), PartialOrd(ignore), Ord(ignore))]
        pub weak_waker: Weak<WakerCell>,
    }

    fn cmp_rev(a: &u64, b: &u64) -> std::cmp::Ordering {
        b.cmp(a)
    }

    fn cmp_rev_partial(a: &u64, b: &u64) -> Option<std::cmp::Ordering> {
        b.partial_cmp(a)
    }

    #[derive(Debug)]
    pub(crate) struct WakerCell {
        // SAFETY: This UnsafeCell is only used to take the value of the waker.
        //  dsa sads
        waker: UnsafeCell<Option<Waker>>,
    }

    unsafe impl Sync for WakerCell {}
    unsafe impl Send for WakerCell {}

    impl std::ops::Deref for WakerCell {
        type Target = UnsafeCell<Option<Waker>>;

        fn deref(&self) -> &Self::Target {
            &self.waker
        }
    }

    impl WakerCell {
        pub fn new(waker: Waker) -> Self {
            Self { waker: UnsafeCell::new(Some(waker)) }
        }
    }
}

/* ------------------------------------------- Handle ------------------------------------------- */
#[derive(Debug, Clone)]
pub struct Handle {
    tx: channel::Sender<driver::Event>,
    gc_counter: Arc<AtomicIsize>,
}

impl Handle {
    /// Returns a future that sleeps for the specified duration.
    ///
    /// [`SleepFuture`] returns the duration that overly passed the specified duration.
    pub fn sleep_for(&self, duration: Duration) -> SleepFuture {
        self.sleep_until(Instant::now() + duration)
    }

    /// Returns a future that sleeps until the specified instant.
    ///
    /// [`SleepFuture`] returns the duration that overly passed the specified instant.
    pub fn sleep_until(&self, timeout: Instant) -> SleepFuture {
        SleepFuture {
            tx: self.tx.clone(),
            state: SleepState::Pending,
            timeout,
            gc_counter: self.gc_counter.clone(),
        }
    }
}

/* ------------------------------------------- Future ------------------------------------------- */
#[derive(Debug)]
pub struct SleepFuture {
    tx: channel::Sender<driver::Event>,
    gc_counter: Arc<AtomicIsize>,
    timeout: Instant,
    state: SleepState,
}

#[cfg(test)]
static_assertions::assert_impl_all!(SleepFuture: Send, Sync, Unpin);

#[derive(Debug, thiserror::Error)]
pub enum SleepError {
    #[error("driver shutdown")]
    Shutdown,
}

#[derive(Debug)]
enum SleepState {
    Pending,

    /// SAFETY: No other thread but worker thread can access this.
    Sleeping(Arc<WakerCell>),
    Woken,
}

unsafe impl Sync for SleepState {}
unsafe impl Send for SleepState {}

impl std::future::Future for SleepFuture {
    type Output = Result<Duration, SleepError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let now = Instant::now();

        if let Some(over) = now.checked_duration_since(self.timeout) {
            self.state = SleepState::Woken;
            return Poll::Ready(Ok(over));
        }

        if matches!(self.state, SleepState::Pending) {
            let waker = Arc::new(WakerCell::new(cx.waker().clone()));
            let event = driver::Event::SleepUntil(NodeDesc {
                timeout: self.timeout,
                waker: Arc::downgrade(&waker),
            });

            if let Err(_) = self.tx.send(event) {
                return Poll::Ready(Err(SleepError::Shutdown));
            }

            self.state = SleepState::Sleeping(waker);
        }

        Poll::Pending
    }
}

impl Drop for SleepFuture {
    fn drop(&mut self) {
        if let SleepState::Sleeping(n) = replace(&mut self.state, SleepState::Woken) {
            if let Some(_) = Arc::into_inner(n) {
                // okay, we got the last reference, server is guaranteed to decrement the gc_counter.
                self.gc_counter.fetch_add(1, Ordering::AcqRel);
            } else {
                // otherwise, server got the last reference, thus gc_counter won't be decremented.
                // so we don't touch this here either.
            }
        }
    }
}
