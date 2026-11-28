//! XTaskQueue and XAsync: the runtime's callback queues and async-call
//! blocks, served as the task-queue provider's object from `QueryApiImpl`.
//!
//! As documented for the public API: a queue has a work port and a
//! completion port, each dispatched manually (the title calls dispatch),
//! on a thread pool, serialised on one thread, or immediately on the
//! submitting thread. A null queue handle means the process queue. An
//! async block's system area holds its status and result size, so both
//! stay readable after our state for the call is gone.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::thread::ThreadId;
#[cfg(test)]
use std::sync::Barrier;
use std::time::Duration;

use super::xgameruntime::{Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing};

/// The task-queue provider; it is also the interface id titles ask for.
pub const PROVIDER: Guid = Guid {
    data1: 0x073b_7dcb,
    data2: 0x1fcf,
    data3: 0x4030,
    data4: [0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28],
};

pub const E_PENDING: HRESULT = 0x8000_000A_u32 as i32;
pub const E_ABORT: HRESULT = 0x8000_4004_u32 as i32;
pub const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
pub const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
pub const E_NOT_SUFFICIENT_BUFFER: HRESULT = 0x8007_007A_u32 as i32;
pub const E_ILLEGAL_METHOD_CALL: HRESULT = 0x8000_000E_u32 as i32;

const WORK: u32 = 0;
const COMPLETION: u32 = 1;
const INFINITE: u32 = u32::MAX;

type Callback = extern "system" fn(*mut c_void, bool);
type MonitorCallback = extern "system" fn(*mut c_void, *const Queue, u32);
type TerminatedCallback = extern "system" fn(*mut c_void);
type CompletionRoutine = extern "system" fn(*mut AsyncBlock);
type Provider = extern "system" fn(u32, *const ProviderData) -> HRESULT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Manual,
    ThreadPool,
    Serialized,
    Immediate,
}

impl Mode {
    fn from_raw(mode: u32) -> Option<Mode> {
        [
            Mode::Manual,
            Mode::ThreadPool,
            Mode::Serialized,
            Mode::Immediate,
        ]
        .get(mode as usize)
        .copied()
    }
}

/// A title callback and its context (addresses, so tasks can cross threads).
#[derive(Clone, Copy)]
struct Task {
    context: usize,
    callback: usize,
}

impl Task {
    fn new(context: *mut c_void, callback: Callback) -> Task {
        Task {
            context: context as usize,
            callback: callback as usize,
        }
    }

    fn run(self, canceled: bool) {
        // SAFETY: built from a `Callback` in `new`.
        let callback: Callback = unsafe { std::mem::transmute(self.callback) };
        callback(self.context as *mut c_void, canceled);
    }
}

struct Port {
    mode: Mode,
    tasks: Mutex<VecDeque<Task>>,
    ready: Condvar,
}

impl Port {
    fn new(mode: Mode) -> Arc<Port> {
        let port = Arc::new(Port {
            mode,
            tasks: Mutex::default(),
            ready: Condvar::new(),
        });
        if mode == Mode::Serialized {
            // The worker ends once it holds the last reference (every queue
            // using the port is closed) and nothing is left to run.
            let worker = port.clone();
            std::thread::spawn(move || {
                loop {
                    match worker.take(1000, || false) {
                        Some(task) => task.run(false),
                        None if Arc::strong_count(&worker) == 1 => break,
                        None => {}
                    }
                }
            });
        }
        port
    }

    fn submit(&self, task: Task) {
        match self.mode {
            Mode::Immediate => task.run(false),
            Mode::ThreadPool => POOL.submit(task),
            Mode::Manual | Mode::Serialized => {
                lock(&self.tasks).push_back(task);
                self.ready.notify_one();
            }
        }
    }

    /// The next queued task, waiting up to `timeout_ms`, or until `ended`
    /// (checked whenever the port is woken) says no task will come.
    fn take(&self, timeout_ms: u32, ended: impl Fn() -> bool) -> Option<Task> {
        let waiting = |t: &mut VecDeque<Task>| t.is_empty() && !ended();
        let tasks = lock(&self.tasks);
        let mut tasks = if timeout_ms == INFINITE {
            self.ready
                .wait_while(tasks, waiting)
                .unwrap_or_else(|e| e.into_inner())
        } else {
            let timeout = Duration::from_millis(timeout_ms.into());
            self.ready
                .wait_timeout_while(tasks, timeout, waiting)
                .unwrap_or_else(|e| e.into_inner())
                .0
        };
        tasks.pop_front()
    }
}

/// The thread pool behind thread-pool ports: idle workers are reused, and a
/// new one starts whenever more tasks wait than workers idle, so a callback
/// that blocks on another never starves it. Workers stay for reuse.
struct Pool {
    /// Queued tasks and the number of idle workers.
    state: Mutex<(VecDeque<Task>, usize)>,
    ready: Condvar,
}

static POOL: Pool = Pool {
    state: Mutex::new((VecDeque::new(), 0)),
    ready: Condvar::new(),
};

impl Pool {
    fn submit(&'static self, task: Task) {
        let mut state = lock(&self.state);
        state.0.push_back(task);
        if state.0.len() > state.1 {
            std::thread::spawn(move || self.work());
        } else {
            self.ready.notify_one();
        }
    }

    fn work(&self) {
        let mut state = lock(&self.state);
        loop {
            while state.0.is_empty() {
                state.1 += 1;
                state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
                state.1 -= 1;
            }
            let task = state.0.pop_front();
            drop(state);
            if let Some(task) = task {
                task.run(false);
            }
            state = lock(&self.state);
        }
    }
}

/// A queue handle is `Arc::into_raw` of this; duplicating and closing a
/// handle add and drop a reference.
pub struct Queue {
    ports: [Arc<Port>; 2],
    monitors: Mutex<Vec<(u64, usize, usize)>>,
    terminated: AtomicBool,
    /// Delayed callbacks still waiting out their delay, in submission
    /// order: terminating the queue cancels them with the queued ones.
    delayed: Mutex<BTreeMap<u64, Task>>,
}

impl Queue {
    fn new(ports: [Arc<Port>; 2]) -> Arc<Queue> {
        Arc::new(Queue {
            ports,
            monitors: Mutex::default(),
            terminated: AtomicBool::new(false),
            delayed: Mutex::default(),
        })
    }

    fn submit(self: &Arc<Self>, port: u32, delay_ms: u32, task: Task) -> HRESULT {
        static NEXT_DELAYED: AtomicU64 = AtomicU64::new(0);
        if port > COMPLETION {
            return E_INVALIDARG;
        }
        if self.terminated.load(Ordering::Acquire) {
            return E_ABORT;
        }
        if delay_ms == 0 {
            self.deliver(port, task);
            return S_OK;
        }
        let id = NEXT_DELAYED.fetch_add(1, Ordering::Relaxed);
        {
            // Checked again under the lock termination drains: the callback
            // is either canceled with the others or refused.
            let mut delayed = lock(&self.delayed);
            if self.terminated.load(Ordering::Acquire) {
                return E_ABORT;
            }
            delayed.insert(id, task);
        }
        let queue = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay_ms.into()));
            // Gone if the queue was terminated meanwhile, which canceled it.
            let task = lock(&queue.delayed).remove(&id);
            if let Some(task) = task {
                queue.deliver(port, task);
            }
        });
        S_OK
    }

    fn deliver(self: &Arc<Self>, port: u32, task: Task) {
        self.ports[port as usize].submit(task);
        // Copied out: a monitor may unregister itself.
        let monitors = lock(&self.monitors).clone();
        for (_, context, callback) in monitors {
            // SAFETY: stored from a `MonitorCallback` in `register_monitor`.
            let callback: MonitorCallback = unsafe { std::mem::transmute(callback) };
            callback(context as *mut c_void, Arc::as_ptr(self), port);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The process queue: `None` until first used (then a thread-pool queue),
/// `Some(None)` once the title clears it.
static PROCESS_QUEUE: Mutex<Option<Option<Arc<Queue>>>> = Mutex::new(None);

fn process_queue() -> Option<Arc<Queue>> {
    lock(&PROCESS_QUEUE)
        .get_or_insert_with(|| {
            Some(Queue::new([
                Port::new(Mode::ThreadPool),
                Port::new(Mode::ThreadPool),
            ]))
        })
        .clone()
}

/// A new reference to the queue behind `handle` (null: the process queue).
fn queue(handle: *const Queue) -> Option<Arc<Queue>> {
    if handle.is_null() {
        return process_queue();
    }
    // SAFETY: a handle we returned, which the title has not closed.
    unsafe {
        Arc::increment_strong_count(handle);
        Some(Arc::from_raw(handle))
    }
}

pub fn XTaskQueueCreate(work: u32, completion: u32, out: *mut *const Queue) -> HRESULT {
    let (Some(work), Some(completion)) = (Mode::from_raw(work), Mode::from_raw(completion)) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let queue = Queue::new([Port::new(work), Port::new(completion)]);
    // SAFETY: checked non-null; the title passes room for a handle.
    unsafe { out.write(Arc::into_raw(queue)) };
    S_OK
}

/// A port handle points at the port, which its queue keeps alive.
pub fn XTaskQueueGetPort(handle: *const Queue, port: u32, out: *mut *const c_void) -> HRESULT {
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    if port > COMPLETION || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::as_ptr(&queue.ports[port as usize]).cast()) };
    S_OK
}

pub fn XTaskQueueCreateComposite(
    work: *const c_void,
    completion: *const c_void,
    out: *mut *const Queue,
) -> HRESULT {
    if work.is_null() || completion.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let port = |p: *const c_void| {
        // SAFETY: a port handle from `XTaskQueueGetPort`, whose queue is open.
        unsafe {
            Arc::increment_strong_count(p.cast::<Port>());
            Arc::from_raw(p.cast::<Port>())
        }
    };
    let queue = Queue::new([port(work), port(completion)]);
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::into_raw(queue)) };
    S_OK
}

pub fn XTaskQueueDuplicateHandle(handle: *const Queue, out: *mut *const Queue) -> HRESULT {
    match queue(handle) {
        Some(queue) if !out.is_null() => {
            // SAFETY: checked non-null.
            unsafe { out.write(Arc::into_raw(queue)) };
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

pub fn XTaskQueueCloseHandle(handle: *const Queue) {
    if !handle.is_null() {
        // SAFETY: a handle we returned; this drops its reference.
        unsafe { Arc::decrement_strong_count(handle) };
    }
}

/// Runs one queued callback of a manual port; false if none came in time,
/// or none can come: the queue was terminated (which ends a wait under way,
/// as a dispatcher thread's loop expects).
pub fn XTaskQueueDispatch(handle: *const Queue, port: u32, timeout_ms: u32) -> bool {
    let Some(queue) = queue(handle) else {
        return false;
    };
    let Some(port) = queue.ports.get(port as usize) else {
        return false;
    };
    if port.mode != Mode::Manual {
        return false;
    }
    match port.take(timeout_ms, || queue.terminated.load(Ordering::Acquire)) {
        Some(task) => {
            task.run(false);
            true
        }
        None => false,
    }
}

pub fn XTaskQueueSubmitDelayedCallback(
    handle: *const Queue,
    port: u32,
    delay_ms: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    match (queue(handle), callback) {
        (Some(queue), Some(callback)) => queue.submit(port, delay_ms, Task::new(context, callback)),
        _ => E_INVALIDARG,
    }
}

/// Cancels what is still queued or waiting out a delay (callbacks see
/// `canceled`), refuses new work, then calls `callback`.
pub fn XTaskQueueTerminate(
    handle: *const Queue,
    _wait: bool,
    context: *mut c_void,
    callback: Option<TerminatedCallback>,
) -> HRESULT {
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    queue.terminated.store(true, Ordering::Release);
    for port in &queue.ports {
        let pending: Vec<Task> = lock(&port.tasks).drain(..).collect();
        // Dispatches waiting on this queue end: nothing more will come.
        // (Those of other queues on the port see theirs is open and wait
        // on.) After the lock above, so none misses both flag and wake.
        port.ready.notify_all();
        for task in pending {
            task.run(true);
        }
    }
    let delayed = std::mem::take(&mut *lock(&queue.delayed));
    for task in delayed.into_values() {
        task.run(true);
    }
    if let Some(callback) = callback {
        callback(context);
    }
    S_OK
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

pub fn XTaskQueueRegisterMonitor(
    handle: *const Queue,
    context: *mut c_void,
    callback: Option<MonitorCallback>,
    token: *mut u64,
) -> HRESULT {
    let (Some(queue), Some(callback)) = (queue(handle), callback) else {
        return E_INVALIDARG;
    };
    if token.is_null() {
        return E_INVALIDARG;
    }
    let id = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    lock(&queue.monitors).push((id, context as usize, callback as usize));
    // SAFETY: checked non-null.
    unsafe { token.write(id) };
    S_OK
}

pub fn XTaskQueueUnregisterMonitor(handle: *const Queue, token: u64) {
    if let Some(queue) = queue(handle) {
        lock(&queue.monitors).retain(|m| m.0 != token);
    }
}

/// A new handle to the process queue, if there is one.
pub fn XTaskQueueGetCurrentProcessTaskQueue(out: *mut *const Queue) -> bool {
    let found = process_queue();
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(found.clone().map_or(std::ptr::null(), Arc::into_raw)) };
    }
    found.is_some()
}

pub fn XTaskQueueSetCurrentProcessTaskQueue(handle: *const Queue) {
    let new = if handle.is_null() {
        None
    } else {
        queue(handle)
    };
    *lock(&PROCESS_QUEUE) = Some(new);
}

/// An async block: what the title fills in, then the system area.
#[repr(C)]
pub struct AsyncBlock {
    pub queue: *const Queue,
    pub context: *mut c_void,
    pub callback: Option<CompletionRoutine>,
    /// Ours: [unused, status, result size, unused].
    pub internal: [usize; 4],
}

#[repr(C)]
pub struct ProviderData {
    pub block: *mut AsyncBlock,
    pub buffer_size: usize,
    pub buffer: *mut c_void,
    pub context: *mut c_void,
}

const OP_BEGIN: u32 = 0;
const OP_DO_WORK: u32 = 1;
const OP_GET_RESULT: u32 = 2;
const OP_CANCEL: u32 = 3;
const OP_CLEANUP: u32 = 4;

/// Tracks in-flight provider operations (`OP_BEGIN`, `OP_DO_WORK`,
/// `OP_CANCEL`) and completion claiming so `OP_CLEANUP` and completion
/// publication never race with an active provider callback on another thread.
#[derive(Default)]
struct Lifecycle {
    /// Threads currently executing a provider op on this call.
    active: Vec<ThreadId>,
    /// Set once `XAsyncCancel` starts dispatching `OP_CANCEL`.
    canceling: bool,
    /// First completion claimed for this call (`result`, `required_size`).
    pending: Option<(HRESULT, usize)>,
    /// Set once finalization (`OP_CLEANUP` + status publication + callback)
    /// has been handed to one thread.
    finalizing: bool,
    /// The thread running cleanup/publication; it must not wait on its own
    /// completion from inside the provider's cleanup callback.
    finalizer: Option<ThreadId>,
}

/// One call in flight, until its result is taken or it fails.
struct Call {
    provider: Provider,
    context: usize,
    identity: usize,
    block: usize,
    completion: Option<CompletionRoutine>,
    /// Coordinates provider ops with completion and cleanup without being held
    /// across any callback: cleanup may wait for a thread completing the same
    /// call.
    lifecycle: Mutex<Lifecycle>,
    /// Final status, kept separately so a waiter holding this call remains
    /// correct even if the title reuses the AsyncBlock after publication.
    result: AtomicI32,
    /// Provider cleanup is one-shot even if block reuse races final removal.
    cleaned: AtomicBool,
    /// The status is published.
    done: Mutex<bool>,
    finished: Condvar,
}

/// Keeps a thread recorded in [`Call::lifecycle`] while a provider op runs,
/// finalizing any deferred completion once the last active op exits.
struct OpGuard {
    call: Arc<Call>,
    block: *mut AsyncBlock,
    active: bool,
}

impl OpGuard {
    fn finish(mut self, result: HRESULT, required_size: usize) {
        self.active = false;
        if let Some((hr, size)) = self.call.leave_op(result, required_size) {
            finalize(&self.call, self.block, hr, size);
        }
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            if let Some((hr, size)) = self.call.leave_op(E_PENDING, 0) {
                finalize(&self.call, self.block, hr, size);
            }
        }
    }
}

impl Call {
    fn provider(
        &self,
        op: u32,
        block: *mut AsyncBlock,
        size: usize,
        buffer: *mut c_void,
    ) -> HRESULT {
        let data = ProviderData {
            block,
            buffer_size: size,
            buffer,
            context: self.context as *mut c_void,
        };
        (self.provider)(op, &data)
    }

    fn enter_begin(self: &Arc<Self>, block: *mut AsyncBlock) -> OpGuard {
        lock(&self.lifecycle)
            .active
            .push(std::thread::current().id());
        OpGuard {
            call: self.clone(),
            block,
            active: true,
        }
    }

    fn enter_work(self: &Arc<Self>, block: *mut AsyncBlock) -> Option<OpGuard> {
        let mut state = lock(&self.lifecycle);
        if state.canceling || state.pending.is_some() || state.finalizing {
            return None;
        }
        state.active.push(std::thread::current().id());
        Some(OpGuard {
            call: self.clone(),
            block,
            active: true,
        })
    }

    fn enter_cancel(self: &Arc<Self>, block: *mut AsyncBlock) -> Option<OpGuard> {
        let mut state = lock(&self.lifecycle);
        if state.canceling || state.pending.is_some() || state.finalizing {
            return None;
        }
        state.canceling = true;
        state.active.push(std::thread::current().id());
        Some(OpGuard {
            call: self.clone(),
            block,
            active: true,
        })
    }

    fn leave_op(&self, result: HRESULT, required_size: usize) -> Option<(HRESULT, usize)> {
        let me = std::thread::current().id();
        let mut state = lock(&self.lifecycle);
        if let Some(i) = state.active.iter().rposition(|&t| t == me) {
            state.active.swap_remove(i);
        }
        if result != E_PENDING && state.pending.is_none() {
            state.pending = Some((result, required_size));
        }
        if !state.finalizing
            && state.active.is_empty()
            && let Some(pending) = state.pending
        {
            state.finalizing = true;
            state.finalizer = Some(me);
            return Some(pending);
        }
        None
    }

    /// Waiting from inside an active provider callback or its own cleanup
    /// would prevent that callback from returning and publishing completion.
    fn cannot_wait_on_current_thread(&self) -> bool {
        let me = std::thread::current().id();
        let state = lock(&self.lifecycle);
        state.active.contains(&me) || (state.finalizing && state.finalizer == Some(me))
    }
}

/// Calls by block address.
static CALLS: LazyLock<Mutex<HashMap<usize, Arc<Call>>>> = LazyLock::new(Default::default);

#[cfg(test)]
struct FinalizeTestHook {
    block: usize,
    entered: Arc<Barrier>,
    resume: Arc<Barrier>,
}

#[cfg(test)]
static FINALIZE_TEST_HOOK: LazyLock<Mutex<Option<FinalizeTestHook>>> =
    LazyLock::new(Default::default);

#[cfg(test)]
struct CallLookupTestHook {
    block: usize,
    thread: std::thread::ThreadId,
    contended: std::sync::mpsc::Sender<bool>,
}

#[cfg(test)]
static CALL_LOOKUP_TEST_HOOK: LazyLock<Mutex<Option<CallLookupTestHook>>> =
    LazyLock::new(Default::default);

#[cfg(test)]
struct WaitForStatusTestHook {
    call: usize,
    thread: std::thread::ThreadId,
    entered: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
static WAIT_FOR_STATUS_TEST_HOOK: LazyLock<Mutex<Option<WaitForStatusTestHook>>> =
    LazyLock::new(Default::default);

#[cfg(test)]
fn notify_waiter_entered(call: &Arc<Call>) {
    let thread = std::thread::current().id();
    let address = Arc::as_ptr(call) as usize;
    let hook = {
        let mut hook = lock(&WAIT_FOR_STATUS_TEST_HOOK);
        if hook
            .as_ref()
            .is_some_and(|hook| hook.call == address && hook.thread == thread)
        {
            hook.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        let _sent = hook.entered.send(());
    }
}

#[cfg(test)]
fn pause_after_status_publication(block: *mut AsyncBlock) {
    let hook = {
        let mut hook = lock(&FINALIZE_TEST_HOOK);
        if hook
            .as_ref()
            .is_some_and(|hook| hook.block == block as usize)
        {
            hook.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        hook.entered.wait();
        hook.resume.wait();
    }
}

fn call(block: *mut AsyncBlock) -> Option<Arc<Call>> {
    #[cfg(test)]
    {
        let thread = std::thread::current().id();
        let hook = {
            let mut hook = lock(&CALL_LOOKUP_TEST_HOOK);
            if hook
                .as_ref()
                .is_some_and(|hook| hook.block == block as usize && hook.thread == thread)
            {
                hook.take()
            } else {
                None
            }
        };
        if let Some(hook) = hook {
            let contended = matches!(
                CALLS.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            );
            let _sent = hook.contended.send(contended);
        }
    }
    lock(&CALLS).get(&(block as usize)).cloned()
}

/// The block's status and result-size words.
fn words(block: *mut AsyncBlock) -> (&'static AtomicUsize, &'static AtomicUsize) {
    // SAFETY: the title's block outlives the call; its system area is ours.
    unsafe {
        let internal = std::ptr::addr_of_mut!((*block).internal).cast::<AtomicUsize>();
        (&*internal.add(1), &*internal.add(2))
    }
}

fn status(block: *mut AsyncBlock) -> HRESULT {
    words(block).0.load(Ordering::Acquire) as u32 as i32
}

/// Whether the block's system area is all zero, as in a block not yet used.
fn zeroed(block: *mut AsyncBlock) -> bool {
    // SAFETY: the title's block, whose system area is ours to read.
    unsafe { std::ptr::addr_of!((*block).internal).read_volatile() == [0; 4] }
}

fn cleanup_call(call: &Call, block: *mut AsyncBlock) {
    if !call.cleaned.swap(true, Ordering::AcqRel) {
        call.provider(OP_CLEANUP, block, 0, std::ptr::null_mut());
    }
}

fn release(block: *mut AsyncBlock) {
    // Unlocked before the provider runs: it may call back into us.
    let removed = lock(&CALLS).remove(&(block as usize));
    if let Some(call) = removed {
        cleanup_call(&call, block);
    }
}

pub fn XAsyncBegin(
    block: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    _identity_name: *const u8,
    provider: Option<Provider>,
) -> HRESULT {
    let Some(provider) = provider else {
        return E_INVALIDARG;
    };
    if block.is_null() {
        return E_INVALIDARG;
    }
    if let Some(left) = call(block) {
        // Calls are filed by block address, and one that succeeded stays
        // filed until its result is taken. A title that never takes it and
        // frees the block may get the same address for its next one, which
        // it zeroes as every new block must be; the block of a result left
        // behind never reads zero (it holds the result's size). So this is
        // a new block, and the call left behind ends here. A call in flight
        // keeps its address, and a block still holding a result is in use.
        let over = *lock(&left.done);
        if !(over && zeroed(block)) {
            return E_INVALIDARG;
        }
        release(block);
    }
    let (status, size) = words(block);
    status.store(E_PENDING as u32 as usize, Ordering::Release);
    size.store(0, Ordering::Release);
    // SAFETY: the non-null title block is valid for the duration of begin.
    let completion = unsafe { (*block).callback };
    let call = Arc::new(Call {
        provider,
        context: context as usize,
        identity: identity as usize,
        block: block as usize,
        completion,
        lifecycle: Mutex::default(),
        result: AtomicI32::new(E_PENDING),
        cleaned: AtomicBool::new(false),
        done: Mutex::new(false),
        finished: Condvar::new(),
    });
    let guard = call.enter_begin(block);
    lock(&CALLS).insert(block as usize, call.clone());
    // A failed begin fails the call, so the completion routine still runs.
    let hr = call.provider(OP_BEGIN, block, 0, std::ptr::null_mut());
    guard.finish(if hr < 0 { hr } else { E_PENDING }, 0);
    S_OK
}

/// Queues the provider's `DoWork` on the block's work port. The work
/// belongs to the call that scheduled it: when it runs, that call may be
/// over and the block may hold another.
pub fn XAsyncSchedule(block: *mut AsyncBlock, delay_ms: u32) -> HRESULT {
    struct Work {
        call: Arc<Call>,
        block: *mut AsyncBlock,
    }
    extern "system" fn work(context: *mut c_void, canceled: bool) {
        // SAFETY: boxed in `XAsyncSchedule`; a task runs once.
        let Work { call, block } = *unsafe { Box::from_raw(context.cast::<Work>()) };
        if canceled {
            complete_call(&call, block, E_ABORT, 0);
            return;
        }
        let Some(guard) = call.enter_work(block) else {
            return;
        };
        let hr = call.provider(OP_DO_WORK, block, 0, std::ptr::null_mut());
        guard.finish(hr, 0);
    }
    if block.is_null() {
        return E_INVALIDARG;
    }
    let Some(call) = call(block) else {
        return E_INVALIDARG;
    };
    // SAFETY: a block in flight is valid.
    let handle = unsafe { (*block).queue };
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    let context = Box::into_raw(Box::new(Work { call, block }));
    let hr = queue.submit(WORK, delay_ms, Task::new(context.cast(), work));
    if hr < 0 {
        // Refused, so the task never runs.
        // SAFETY: still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// Records the result (the first one wins), wakes waiters and queues the
/// completion routine on the block's completion port.
pub fn XAsyncComplete(block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
    if let Some(call) = call(block) {
        complete_call(&call, block, result, required_size);
    }
}

/// [`XAsyncComplete`] for `call`, which is or was in flight on `block`. Our
/// own completions name the call they belong to, not the block: if the
/// provider already completed it, the title may have begun another call on
/// the block, which a second complete by block would end.
fn complete_call(call: &Arc<Call>, block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
    if result == E_PENDING {
        return;
    }
    let finish = {
        let me = std::thread::current().id();
        let mut state = lock(&call.lifecycle);
        if state.pending.is_some() {
            return;
        }
        state.pending = Some((result, required_size));
        if !state.finalizing && state.active.is_empty() {
            state.finalizing = true;
            state.finalizer = Some(me);
            true
        } else {
            false
        }
    };
    if finish {
        finalize(call, block, result, required_size);
    }
}

extern "system" fn complete(context: *mut c_void, _canceled: bool) {
    // SAFETY: Arc::into_raw in finalize; the task runs exactly once.
    let call = unsafe { Arc::from_raw(context.cast::<Call>() as *const Call) };
    if let Some(routine) = call.completion {
        routine(call.block as *mut AsyncBlock);
    }
}

fn finalize(call: &Arc<Call>, block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
    // Read before publishing: once the status is set, the title may free
    // the block.
    // SAFETY: a block in flight is valid.
    let handle = unsafe { (*block).queue };
    let (status, size) = words(block);
    let cleanup = result < 0 || required_size == 0;
    if cleanup {
        // Keep the call discoverable while provider cleanup runs. Waiters
        // must not miss it and read the block's old E_PENDING status.
        cleanup_call(call, block);
    }
    // Serialize the public status transition with block lookup/reuse. A
    // non-waiting status reader may observe the store below and immediately
    // zero/reuse the block; XAsyncBegin must then see either the completed
    // old call or no old call, never a published status with done=false.
    let mut calls = lock(&CALLS);
    let key = block as usize;
    let current = calls
        .get(&key)
        .is_some_and(|current| Arc::ptr_eq(current, call));
    if current {
        call.result.store(result, Ordering::Release);
        size.store(required_size, Ordering::Release);
        status.store(result as u32 as usize, Ordering::Release);
        #[cfg(test)]
        pause_after_status_publication(block);
        if cleanup {
            calls.remove(&key);
        }
    } else {
        // The block may already belong to a newer call. Wake waiters holding
        // this Arc without publishing stale state into the reused block.
        call.result.store(result, Ordering::Release);
    }
    *lock(&call.done) = true;
    call.finished.notify_all();
    drop(calls);
    if call.completion.is_some() {
        let context = Arc::into_raw(call.clone()) as *mut c_void;
        let task = Task::new(context, complete);
        match queue(handle) {
            Some(queue) if queue.submit(COMPLETION, 0, task) == S_OK => {}
            _ => task.run(true),
        }
    }
}

fn wait_for_status(call: &Arc<Call>) -> HRESULT {
    let done = lock(&call.done);
    if !*done && call.cannot_wait_on_current_thread() {
        return E_PENDING;
    }
    #[cfg(test)]
    if !*done {
        notify_waiter_entered(call);
    }
    drop(call.finished.wait_while(done, |d| !*d));
    call.result.load(Ordering::Acquire)
}

pub fn XAsyncGetStatus(block: *mut AsyncBlock, wait: bool) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    if wait && let Some(call) = call(block) {
        return wait_for_status(&call);
    }
    status(block)
}

pub fn XAsyncGetResultSize(block: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    if block.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    // Any nonnegative completion carries a result, not just S_OK: S_FALSE
    // is a success code too and a provider may return data with it.
    if hr >= 0 {
        // SAFETY: checked non-null.
        unsafe { out.write(words(block).1.load(Ordering::Acquire)) };
    }
    hr
}

/// Asks the provider to cancel; the call fails with `E_ABORT` unless the
/// provider completed it first. A completed call is left alone.
pub fn XAsyncCancel(block: *mut AsyncBlock) {
    if let Some(call) = call(block)
        && let Some(guard) = call.enter_cancel(block)
    {
        call.provider(OP_CANCEL, block, 0, std::ptr::null_mut());
        guard.finish(E_ABORT, 0);
    }
}

/// Has the provider write the result into `buffer`, then ends the call.
pub fn XAsyncGetResult(
    block: *mut AsyncBlock,
    identity: *const c_void,
    buffer_size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    let size = words(block).1.load(Ordering::Acquire);
    if hr < 0 || size == 0 {
        if hr >= 0 && !used.is_null() {
            // SAFETY: checked non-null.
            unsafe { used.write(0) };
        }
        return hr;
    }
    let Some(call) = call(block) else {
        return E_ILLEGAL_METHOD_CALL;
    };
    if call.identity != identity as usize {
        return E_INVALIDARG;
    }
    if buffer_size < size || buffer.is_null() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    let hr = call.provider(OP_GET_RESULT, block, buffer_size, buffer);
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(size) };
    }
    release(block);
    hr
}

/// Starts a call on `block` that is already over: it completes with `hr`
/// and no result data, through the block's completion port as usual.
pub fn finished(block: *mut AsyncBlock, identity: *const c_void, hr: HRESULT) -> HRESULT {
    finished_with(
        block,
        identity,
        if hr < 0 { Err(hr) } else { Ok(Vec::new()) },
    )
}

/// Like [`finished`], with the bytes `XAsyncGetResult` hands out for
/// `identity` (an error has none).
pub fn finished_with(
    block: *mut AsyncBlock,
    identity: *const c_void,
    result: Result<Vec<u8>, HRESULT>,
) -> HRESULT {
    type Data = Result<Vec<u8>, HRESULT>;
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; the context is the
        // boxed result until cleanup frees it.
        let data = unsafe { &*data };
        let result = data.context.cast::<Data>();
        match op {
            // SAFETY: boxed in `finished_with`; nothing runs after cleanup.
            OP_CLEANUP => drop(unsafe { Box::from_raw(result) }),
            OP_BEGIN => {
                // SAFETY: as above. Copied out: completing may clean up.
                let (hr, len) = match unsafe { &*result } {
                    Ok(bytes) => (S_OK, bytes.len()),
                    Err(hr) => (*hr, 0),
                };
                XAsyncComplete(data.block, hr, len);
            }
            // SAFETY: as above; the runtime checked the buffer's size.
            OP_GET_RESULT => unsafe {
                if let Ok(bytes) = &*result {
                    data.buffer
                        .cast::<u8>()
                        .copy_from(bytes.as_ptr(), bytes.len());
                }
            },
            _ => {}
        }
        S_OK
    }
    let context = Box::into_raw(Box::new(result));
    let hr = XAsyncBegin(
        block,
        context.cast(),
        identity,
        std::ptr::null(),
        Some(provider),
    );
    if hr < 0 {
        // Refused before the call existed, so no cleanup will come.
        // SAFETY: still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// Like [`finished_with`], but the result comes from `work`, run on a
/// thread of ours; the call stays pending until it returns (a dialog, say).
pub fn pending_with(
    block: *mut AsyncBlock,
    identity: *const c_void,
    work: impl FnOnce() -> Result<Vec<u8>, HRESULT> + Send + 'static,
) -> HRESULT {
    type Data = Mutex<(Option<Box<dyn FnOnce() -> Result<Vec<u8>, HRESULT> + Send>>, Vec<u8>)>;
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; the context is the
        // boxed state until cleanup frees it.
        let data = unsafe { &*data };
        let state = data.context.cast::<Data>();
        match op {
            // SAFETY: boxed in `pending_with`; nothing runs after cleanup.
            OP_CLEANUP => drop(unsafe { Box::from_raw(state) }),
            OP_BEGIN => {
                // SAFETY: as above; cleanup waits for completion.
                let work = unsafe { &*state }.lock().unwrap_or_else(|e| e.into_inner()).0.take();
                let (block, state) = (data.block as usize, state as usize);
                std::thread::spawn(move || {
                    let result = work.map_or(Err(E_ABORT), |work| work());
                    let (hr, len) = match &result {
                        Ok(bytes) => (S_OK, bytes.len()),
                        Err(hr) => (*hr, 0),
                    };
                    if let Ok(bytes) = result {
                        // SAFETY: the call is pending, so its state lives.
                        let state = unsafe { &*(state as *const Data) };
                        state.lock().unwrap_or_else(|e| e.into_inner()).1 = bytes;
                    }
                    XAsyncComplete(block as *mut AsyncBlock, hr, len);
                });
            }
            // SAFETY: as above; the runtime checked the buffer's size.
            OP_GET_RESULT => unsafe {
                let state = (*state).lock().unwrap_or_else(|e| e.into_inner());
                data.buffer
                    .cast::<u8>()
                    .copy_from(state.1.as_ptr(), state.1.len());
            },
            _ => {}
        }
        S_OK
    }
    let state: Box<Data> = Box::new(Mutex::new((Some(Box::new(work)), Vec::new())));
    let context = Box::into_raw(state);
    let hr = XAsyncBegin(block, context.cast(), identity, std::ptr::null(), Some(provider));
    if hr < 0 {
        // SAFETY: refused before the call existed; still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// The result of a call begun with [`finished_with`] for `identity`, as a
/// `T` written through `out`.
pub fn result<T>(block: *mut AsyncBlock, identity: *const c_void, out: *mut T) -> HRESULT {
    XAsyncGetResult(
        block,
        identity,
        size_of::<T>(),
        out.cast(),
        std::ptr::null_mut(),
    )
}

/// The bytes of a value, for [`finished_with`].
pub fn bytes<T: Copy>(value: T) -> Vec<u8> {
    // SAFETY: a `Copy` value is plain data; padding bytes are never read.
    unsafe { std::slice::from_raw_parts((&raw const value).cast::<u8>(), size_of::<T>()) }.to_vec()
}

type Work = extern "system" fn(*mut AsyncBlock) -> HRESULT;

/// Runs `work` on the block's work port as an async call of its own; its
/// return value is the call's result.
pub fn XAsyncRun(block: *mut AsyncBlock, work: Option<Work>) -> HRESULT {
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => XAsyncSchedule(data.block, 0),
            OP_DO_WORK => {
                // SAFETY: the context is the `Work` given to `XAsyncRun`.
                let work = unsafe { std::mem::transmute::<*mut c_void, Work>(data.context) };
                work(data.block)
            }
            _ => S_OK,
        }
    }
    let Some(work) = work else {
        return E_INVALIDARG;
    };
    let identity = XAsyncRun as *const c_void;
    XAsyncBegin(
        block,
        work as *mut c_void,
        identity,
        std::ptr::null(),
        Some(provider),
    )
}

// The object `QueryApiImpl` returns. Each method takes the object first,
// then the API's arguments; slot offsets as titles call them.

pub static OBJECT: Object = Object::new("task queue", &VTABLE);

/// Its interface id is the provider's.
pub const INTERFACES: &[Interface] = &[(PROVIDER, PROVIDER, &OBJECT)];

type This = *const Object;
extern "system" fn m_get_status(_: This, b: *mut AsyncBlock, wait: u8) -> HRESULT {
    XAsyncGetStatus(b, wait != 0)
}
extern "system" fn m_get_result_size(_: This, b: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    XAsyncGetResultSize(b, out)
}
extern "system" fn m_cancel(_: This, b: *mut AsyncBlock) {
    XAsyncCancel(b)
}
extern "system" fn m_run(_: This, b: *mut AsyncBlock, work: Option<Work>) -> HRESULT {
    XAsyncRun(b, work)
}
extern "system" fn m_begin(
    _: This,
    b: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    name: *const u8,
    provider: Option<Provider>,
) -> HRESULT {
    XAsyncBegin(b, context, identity, name, provider)
}
extern "system" fn m_schedule(_: This, b: *mut AsyncBlock, delay: u32) -> HRESULT {
    XAsyncSchedule(b, delay)
}
extern "system" fn m_complete(_: This, b: *mut AsyncBlock, hr: HRESULT, size: usize) {
    XAsyncComplete(b, hr, size)
}
extern "system" fn m_get_result(
    _: This,
    b: *mut AsyncBlock,
    identity: *const c_void,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    XAsyncGetResult(b, identity, size, buffer, used)
}
extern "system" fn m_create(
    _: This,
    work: u32,
    completion: u32,
    out: *mut *const Queue,
) -> HRESULT {
    XTaskQueueCreate(work, completion, out)
}
extern "system" fn m_create_composite(
    _: This,
    work: *const c_void,
    completion: *const c_void,
    out: *mut *const Queue,
) -> HRESULT {
    XTaskQueueCreateComposite(work, completion, out)
}
extern "system" fn m_get_port(
    _: This,
    q: *const Queue,
    port: u32,
    out: *mut *const c_void,
) -> HRESULT {
    XTaskQueueGetPort(q, port, out)
}
extern "system" fn m_duplicate(_: This, q: *const Queue, out: *mut *const Queue) -> HRESULT {
    XTaskQueueDuplicateHandle(q, out)
}
extern "system" fn m_dispatch(_: This, q: *const Queue, port: u32, timeout: u32) -> bool {
    XTaskQueueDispatch(q, port, timeout)
}
extern "system" fn m_close(_: This, q: *const Queue) {
    XTaskQueueCloseHandle(q)
}
extern "system" fn m_submit(
    _: This,
    q: *const Queue,
    port: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    XTaskQueueSubmitDelayedCallback(q, port, 0, context, callback)
}
extern "system" fn m_submit_delayed(
    _: This,
    q: *const Queue,
    port: u32,
    delay: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    XTaskQueueSubmitDelayedCallback(q, port, delay, context, callback)
}
extern "system" fn m_terminate(
    _: This,
    q: *const Queue,
    wait: u8,
    context: *mut c_void,
    callback: Option<TerminatedCallback>,
) -> HRESULT {
    XTaskQueueTerminate(q, wait != 0, context, callback)
}
extern "system" fn m_register_monitor(
    _: This,
    q: *const Queue,
    context: *mut c_void,
    callback: Option<MonitorCallback>,
    token: *mut u64,
) -> HRESULT {
    XTaskQueueRegisterMonitor(q, context, callback, token)
}
extern "system" fn m_unregister_monitor(_: This, q: *const Queue, token: u64) {
    XTaskQueueUnregisterMonitor(q, token)
}
extern "system" fn m_get_process_queue(_: This, out: *mut *const Queue) -> bool {
    XTaskQueueGetCurrentProcessTaskQueue(out)
}
extern "system" fn m_set_process_queue(_: This, q: *const Queue) {
    XTaskQueueSetCurrentProcessTaskQueue(q)
}
/// The runtime's debug check that the caller is not a time-critical thread,
/// which every runtime call in a title makes first.
extern "system" fn m_check_thread(_: This) {}

static VTABLE: Methods<31> = Methods([
    missing::<0> as *const (),         // 0x00 QueryInterface
    add_ref as *const (),              // 0x08 AddRef
    add_ref as *const (),              // 0x10 Release: the object is static
    m_get_status as *const (),         // 0x18
    m_get_result_size as *const (),    // 0x20
    m_cancel as *const (),             // 0x28
    m_run as *const (),                // 0x30
    m_begin as *const (),              // 0x38
    missing::<8> as *const (),         // 0x40
    m_schedule as *const (),           // 0x48
    m_complete as *const (),           // 0x50
    m_get_result as *const (),         // 0x58
    m_create as *const (),             // 0x60
    m_create_composite as *const (),   // 0x68
    m_get_port as *const (),           // 0x70
    m_duplicate as *const (),          // 0x78
    m_dispatch as *const (),           // 0x80
    m_close as *const (),              // 0x88
    m_submit as *const (),             // 0x90
    m_submit_delayed as *const (),     // 0x98
    missing::<20> as *const (),        // 0xa0 register a wait handle
    missing::<21> as *const (),        // 0xa8 unregister it
    m_terminate as *const (),          // 0xb0
    m_register_monitor as *const (),   // 0xb8
    m_unregister_monitor as *const (), // 0xc0
    m_get_process_queue as *const (),  // 0xc8
    m_set_process_queue as *const (),  // 0xd0
    missing::<27> as *const (),        // 0xd8 thread time-sensitivity
    missing::<28> as *const (),        // 0xe0
    m_check_thread as *const (),       // 0xe8
    missing::<30> as *const (),        // 0xf0
]);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn slot<F: Copy>(offset: usize) -> F {
        crate::modules::xgameruntime::method(&OBJECT, offset)
    }

    static RAN: AtomicU32 = AtomicU32::new(0);
    extern "system" fn count(context: *mut c_void, canceled: bool) {
        RAN.fetch_add(
            if canceled { 100 } else { context as u32 },
            Ordering::SeqCst,
        );
    }

    #[test]
    fn manual_ports_run_callbacks_when_dispatched() {
        type Create = extern "system" fn(This, u32, u32, *mut *const Queue) -> HRESULT;
        type Submit = extern "system" fn(This, *const Queue, u32, *mut c_void, Callback) -> HRESULT;
        type Dispatch = extern "system" fn(This, *const Queue, u32, u32) -> bool;
        type Close = extern "system" fn(This, *const Queue);
        let this: This = &OBJECT;
        let mut q = std::ptr::null();
        assert_eq!(slot::<Create>(0x60)(this, 0, 0, &mut q), S_OK);
        assert_eq!(
            slot::<Submit>(0x90)(this, q, COMPLETION, 5 as *mut c_void, count),
            S_OK
        );
        assert_eq!(RAN.load(Ordering::SeqCst), 0, "manual: nothing runs yet");
        assert!(
            !slot::<Dispatch>(0x80)(this, q, WORK, 0),
            "work port is empty"
        );
        assert!(slot::<Dispatch>(0x80)(this, q, COMPLETION, 0));
        assert_eq!(RAN.load(Ordering::SeqCst), 5);
        assert!(!slot::<Dispatch>(0x80)(this, q, COMPLETION, 0));
        slot::<Close>(0x88)(this, q);
    }

    /// A provider that schedules its work and returns a u64 result.
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => XAsyncSchedule(data.block, 0),
            OP_DO_WORK => {
                XAsyncComplete(data.block, S_OK, 8);
                S_OK
            }
            OP_GET_RESULT => {
                // SAFETY: at least 8 bytes, checked by the runtime.
                unsafe { data.buffer.cast::<u64>().write(data.context as u64) };
                S_OK
            }
            _ => S_OK,
        }
    }

    static COMPLETED: AtomicU32 = AtomicU32::new(0);
    extern "system" fn completed(_: *mut AsyncBlock) {
        COMPLETED.fetch_add(1, Ordering::SeqCst);
    }

    /// A title that never takes a call's result and frees its block: the
    /// next block allocated at that address (zeroed, as a new block must
    /// be) begins a call like any other.
    #[test]
    fn a_new_block_where_a_result_was_left_begins_a_call() {
        static CLEANED: AtomicU32 = AtomicU32::new(0);
        /// Completes at once with one byte to fetch; counts its cleanups.
        extern "system" fn left(op: u32, data: *const ProviderData) -> HRESULT {
            match op {
                // SAFETY: the runtime passes valid provider data.
                OP_BEGIN => XAsyncComplete(unsafe { (*data).block }, S_OK, 1),
                OP_CLEANUP => drop(CLEANED.fetch_add(1, Ordering::SeqCst)),
                _ => {}
            }
            S_OK
        }
        /// Never completes by itself.
        extern "system" fn idle(_: u32, _: *const ProviderData) -> HRESULT {
            S_OK
        }
        let fresh = || AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let (none, identity) = (std::ptr::null_mut(), 0x1e as *const c_void);
        let begin = |block: &mut AsyncBlock, provider: Provider| {
            XAsyncBegin(block, none, identity, std::ptr::null(), Some(provider))
        };

        let mut block = fresh();
        assert_eq!(begin(&mut block, left), S_OK);
        assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
        // The block as the call left it is still that call's.
        assert_eq!(begin(&mut block, left), E_INVALIDARG);
        assert_eq!(CLEANED.load(Ordering::SeqCst), 0);

        // Freed with its result never taken, and a new block in its place.
        block = fresh();
        assert_eq!(finished_with(&mut block, identity, Ok(vec![9])), S_OK);
        assert_eq!(CLEANED.load(Ordering::SeqCst), 1, "the call left behind");
        let mut value = 0u8;
        assert_eq!(result(&mut block, identity, &mut value), S_OK);
        assert_eq!(value, 9);
        assert!(call(&mut block).is_none());

        // A call still in flight keeps its address, whatever the block holds.
        let mut block = fresh();
        assert_eq!(begin(&mut block, idle), S_OK);
        block = fresh();
        assert_eq!(begin(&mut block, left), E_INVALIDARG);
        XAsyncComplete(&mut block, E_ABORT, 0);
        assert!(call(&mut block).is_none());
    }

    #[test]
    fn async_calls_complete_and_return_results() {
        let mut q = std::ptr::null();
        // Work on a thread pool, completion dispatched by hand.
        assert_eq!(XTaskQueueCreate(1, 0, &mut q), S_OK);
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: Some(completed),
            internal: [0; 4],
        };
        let identity = 0x1d as *const c_void;
        assert_eq!(
            XAsyncBegin(
                &mut block,
                42 as *mut c_void,
                identity,
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, true), S_OK);
        let mut size = 0;
        assert_eq!(XAsyncGetResultSize(&mut block, &mut size), S_OK);
        assert_eq!(size, 8);
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 0, "completion is manual");
        assert!(XTaskQueueDispatch(q, COMPLETION, INFINITE));
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 1);

        let mut value = 0u64;
        let mut small = 0u32;
        let out = (&mut small as *mut u32).cast();
        assert_eq!(
            XAsyncGetResult(&mut block, identity, 4, out, std::ptr::null_mut()),
            E_NOT_SUFFICIENT_BUFFER
        );
        let out = (&mut value as *mut u64).cast();
        let mut used = 0;
        assert_eq!(
            XAsyncGetResult(&mut block, identity, 8, out, &mut used),
            S_OK
        );
        assert_eq!((value, used), (42, 8));
        assert!(call(&mut block).is_none(), "the call ends with its result");
        XTaskQueueCloseHandle(q);
    }

    /// What a test sees of a block that a title reuses: its completion
    /// routine begins the next call on the same block.
    #[derive(Default)]
    struct Reuse {
        completions: AtomicU32,
        next_begun: AtomicBool,
        next_worked: AtomicBool,
    }

    /// A block whose completion routine is [`begin_next`].
    fn reused_block(queue: *const Queue, reuse: &Reuse) -> AsyncBlock {
        AsyncBlock {
            queue,
            context: std::ptr::from_ref(reuse).cast_mut().cast(),
            callback: Some(begin_next),
            internal: [0; 4],
        }
    }

    /// Begins a call without an identity.
    fn begin(block: *mut AsyncBlock, context: *mut c_void, provider: Provider) -> HRESULT {
        let none = std::ptr::null();
        XAsyncBegin(block, context, none, none.cast(), Some(provider))
    }

    /// Begins the next call on the block the first time a call completes,
    /// as a title that keeps one block for repeated calls does.
    extern "system" fn begin_next(block: *mut AsyncBlock) {
        // SAFETY: the block is from `reused_block`, its `Reuse` still alive.
        let context = unsafe { (*block).context };
        // SAFETY: as above.
        let reuse = unsafe { &*context.cast::<Reuse>() };
        if reuse.completions.fetch_add(1, Ordering::SeqCst) == 0 {
            let hr = begin(block, context, next_call);
            reuse.next_begun.store(hr == S_OK, Ordering::SeqCst);
        }
    }

    /// The next call's provider: it never ends on its own, and notes being
    /// asked to work.
    extern "system" fn next_call(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; its context is
        // the test's `Reuse`.
        let reuse = unsafe { &*(*data).context.cast::<Reuse>() };
        if op == OP_DO_WORK {
            reuse.next_worked.store(true, Ordering::SeqCst);
            return E_PENDING;
        }
        S_OK
    }

    /// A provider that ends its call itself, in `DoWork` and on cancel, then
    /// returns `S_OK`, as providers do.
    extern "system" fn ends_itself(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let block = unsafe { (*data).block };
        match op {
            OP_DO_WORK => XAsyncComplete(block, S_OK, 0),
            OP_CANCEL => XAsyncComplete(block, E_ABORT, 0),
            _ => {}
        }
        S_OK
    }

    #[test]
    fn a_finished_calls_work_result_leaves_the_next_call_alone() {
        let mut q = std::ptr::null();
        // Work dispatched by hand; completion routines run where the call
        // completes, inside the provider's `DoWork`.
        assert_eq!(XTaskQueueCreate(0, 3, &mut q), S_OK);
        let reuse = Reuse::default();
        let mut block = reused_block(q, &reuse);
        assert_eq!(begin(&mut block, std::ptr::null_mut(), ends_itself), S_OK);
        assert_eq!(XAsyncSchedule(&mut block, 0), S_OK);
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert!(reuse.next_begun.load(Ordering::SeqCst));
        assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
        assert_eq!(reuse.completions.load(Ordering::SeqCst), 1);

        XAsyncCancel(&mut block);
        assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn a_canceled_call_leaves_the_next_call_alone() {
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 3, &mut q), S_OK);
        let reuse = Reuse::default();
        let mut block = reused_block(q, &reuse);
        assert_eq!(begin(&mut block, std::ptr::null_mut(), ends_itself), S_OK);
        assert_eq!(XAsyncSchedule(&mut block, 0), S_OK);
        // The provider ends the call on cancel, before its work has run.
        XAsyncCancel(&mut block);
        assert!(reuse.next_begun.load(Ordering::SeqCst));
        assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
        // The canceled call's work is still queued; it runs nothing now.
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert!(!reuse.next_worked.load(Ordering::SeqCst));
        assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
        assert_eq!(reuse.completions.load(Ordering::SeqCst), 1);

        XAsyncCancel(&mut block);
        assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn a_nonnegative_completion_still_returns_its_payload() {
        const S_FALSE: HRESULT = 1;
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            match op {
                OP_BEGIN => XAsyncSchedule(data.block, 0),
                OP_DO_WORK => {
                    XAsyncComplete(data.block, S_FALSE, 8);
                    S_OK
                }
                OP_GET_RESULT => {
                    // SAFETY: the caller checked eight bytes.
                    unsafe { data.buffer.cast::<u64>().write(0x1234) };
                    S_OK
                }
                _ => S_OK,
            }
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let identity = 0x77 as *const c_void;
        assert_eq!(
            XAsyncBegin(&mut block, std::ptr::null_mut(), identity, std::ptr::null(), Some(provider)),
            S_OK
        );
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert_eq!(XAsyncGetStatus(&mut block, false), S_FALSE);
        let mut size = 0;
        assert_eq!(XAsyncGetResultSize(&mut block, &mut size), S_FALSE);
        assert_eq!(size, 8);
        let mut value = 0u64;
        let mut used = 0;
        assert_eq!(
            XAsyncGetResult(&mut block, identity, 8, (&mut value as *mut u64).cast(), &mut used),
            S_OK
        );
        assert_eq!((value, used), (0x1234, 8));
        assert!(call(&raw mut block).is_none(), "the call ends with its result");
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn canceling_a_completed_call_leaves_it_alone() {
        static CANCELS: AtomicU32 = AtomicU32::new(0);
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            match op {
                OP_BEGIN => {
                    XAsyncComplete(data.block, S_OK, 1);
                    S_OK
                }
                OP_CANCEL => {
                    CANCELS.fetch_add(1, Ordering::SeqCst);
                    S_OK
                }
                OP_GET_RESULT => {
                    // SAFETY: the runtime checked one byte.
                    unsafe { data.buffer.cast::<u8>().write(0x5a) };
                    S_OK
                }
                _ => S_OK,
            }
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let identity = 0x78 as *const c_void;
        assert_eq!(
            XAsyncBegin(
                &mut block,
                std::ptr::null_mut(),
                identity,
                std::ptr::null(),
                Some(provider),
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
        XAsyncCancel(&mut block);
        assert_eq!(CANCELS.load(Ordering::SeqCst), 0);
        assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);
        let mut byte = 0u8;
        let mut used = 0;
        assert_eq!(
            XAsyncGetResult(
                &mut block,
                identity,
                1,
                (&mut byte as *mut u8).cast(),
                &mut used,
            ),
            S_OK
        );
        assert_eq!((byte, used), (0x5a, 1));
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn run_returns_the_work_result() {
        extern "system" fn work(_: *mut AsyncBlock) -> HRESULT {
            E_ABORT
        }
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        assert_eq!(
            slot::<extern "system" fn(This, *mut AsyncBlock, Option<Work>) -> HRESULT>(0x30)(
                &OBJECT,
                &mut block,
                Some(work)
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, true), E_ABORT);
    }

    #[test]
    fn pool_callbacks_may_wait_for_each_other() {
        static SECOND: AtomicBool = AtomicBool::new(false);
        static FIRST: AtomicBool = AtomicBool::new(false);
        extern "system" fn waits(_: *mut c_void, _: bool) {
            let start = std::time::Instant::now();
            while !SECOND.load(Ordering::SeqCst) && start.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            FIRST.store(SECOND.load(Ordering::SeqCst), Ordering::SeqCst);
        }
        extern "system" fn releases(_: *mut c_void, _: bool) {
            SECOND.store(true, Ordering::SeqCst);
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(1, 1, &mut q), S_OK);
        let none = std::ptr::null_mut();
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, none, Some(waits)),
            S_OK
        );
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, none, Some(releases)),
            S_OK
        );
        let start = std::time::Instant::now();
        while !FIRST.load(Ordering::SeqCst) && start.elapsed().as_secs() < 10 {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(FIRST.load(Ordering::SeqCst));
        XTaskQueueCloseHandle(q);
    }

    /// A dispatcher thread's loop, `while (XTaskQueueDispatch(queue, port,
    /// INFINITE));`, ends when its queue is terminated: nothing can be
    /// submitted to it any more, so nothing else would ever wake it.
    #[test]
    fn terminate_ends_a_waiting_dispatch() {
        static RAN: AtomicU32 = AtomicU32::new(0);
        extern "system" fn ran(_: *mut c_void, _: bool) {
            RAN.fetch_add(1, Ordering::SeqCst);
        }
        /// Dispatches `queue`'s work until a dispatch returns false, then
        /// says so.
        fn dispatcher(queue: *const Queue) -> std::sync::mpsc::Receiver<()> {
            let (done, ended) = std::sync::mpsc::channel();
            let queue = queue as usize;
            std::thread::spawn(move || {
                while XTaskQueueDispatch(queue as *const Queue, WORK, INFINITE) {}
                let _ = done.send(());
            });
            ended
        }
        let run_one = |queue, total| {
            let none = std::ptr::null_mut();
            assert_eq!(
                XTaskQueueSubmitDelayedCallback(queue, WORK, 0, none, Some(ran)),
                S_OK
            );
            for _ in 0..500 {
                if RAN.load(Ordering::SeqCst) == total {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("callback {total} did not run");
        };
        let (long, short) = (Duration::from_secs(5), Duration::from_millis(200));
        let none = std::ptr::null_mut();

        // Two queues on the same manual ports.
        let (mut a, mut b) = (std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueCreate(0, 0, &mut a), S_OK);
        let (mut work, mut completion) = (std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueGetPort(a, WORK, &mut work), S_OK);
        assert_eq!(XTaskQueueGetPort(a, COMPLETION, &mut completion), S_OK);
        assert_eq!(XTaskQueueCreateComposite(work, completion, &mut b), S_OK);

        let ended = dispatcher(a);
        run_one(a, 1);
        // The other queue's end is not this one's: the wait goes on and
        // still runs what comes in.
        assert_eq!(XTaskQueueTerminate(b, false, none, None), S_OK);
        assert!(
            ended.recv_timeout(short).is_err(),
            "ended with another queue"
        );
        run_one(a, 2);
        // Its own queue's end is.
        assert_eq!(XTaskQueueTerminate(a, false, none, None), S_OK);
        assert!(
            ended.recv_timeout(long).is_ok(),
            "still waiting on a terminated queue"
        );
        // And a dispatch that starts after the end returns at once.
        assert!(!XTaskQueueDispatch(a, WORK, INFINITE));
        XTaskQueueCloseHandle(a);
        XTaskQueueCloseHandle(b);
    }

    #[test]
    fn terminate_cancels_what_is_queued() {
        static TERMINATED: AtomicBool = AtomicBool::new(false);
        extern "system" fn terminated(_: *mut c_void) {
            TERMINATED.store(true, Ordering::SeqCst);
        }
        extern "system" fn note(context: *mut c_void, canceled: bool) {
            // SAFETY: the test passes a pointer to its flag.
            unsafe { *context.cast::<bool>() = canceled };
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let mut canceled = false;
        let flag = (&mut canceled as *mut bool).cast();
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, flag, Some(note)),
            S_OK
        );
        assert_eq!(
            XTaskQueueTerminate(q, true, std::ptr::null_mut(), Some(terminated)),
            S_OK
        );
        assert!(canceled && TERMINATED.load(Ordering::SeqCst));
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, flag, Some(note)),
            E_ABORT
        );
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn terminate_cancels_delayed_callbacks() {
        static EVENTS: Mutex<Vec<&str>> = Mutex::new(Vec::new());
        extern "system" fn delayed(_: *mut c_void, canceled: bool) {
            lock(&EVENTS).push(if canceled { "canceled" } else { "ran" });
        }
        extern "system" fn terminated(_: *mut c_void) {
            lock(&EVENTS).push("terminated");
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(1, 1, &mut q), S_OK);
        let none = std::ptr::null_mut();
        let hr = XTaskQueueSubmitDelayedCallback(q, WORK, 50, none, Some(delayed));
        assert_eq!(hr, S_OK);
        assert_eq!(XTaskQueueTerminate(q, true, none, Some(terminated)), S_OK);
        assert_eq!(*lock(&EVENTS), ["canceled", "terminated"]);
        // Its delay passes without it running.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(*lock(&EVENTS), ["canceled", "terminated"]);
        XTaskQueueCloseHandle(q);
    }

    /// The call a test provider looks at in its cleanup, and what it saw
    /// (1: the completion lock was free, 2: held).
    static CLEANED: Mutex<Option<Arc<Call>>> = Mutex::new(None);
    static SEEN: AtomicU32 = AtomicU32::new(0);

    extern "system" fn cleanup_checks(op: u32, _: *const ProviderData) -> HRESULT {
        if op == OP_CLEANUP {
            let call = CLEANED.lock().unwrap().clone();
            let free = call.is_some_and(|c| c.done.try_lock().is_ok());
            SEEN.store(if free { 1 } else { 2 }, Ordering::SeqCst);
        }
        S_OK
    }

    /// A failed call's cleanup runs without the completion lock: a cleanup
    /// that waits for a thread completing the same call would otherwise
    /// deadlock with it. The status is published after the cleanup.
    #[test]
    fn cleanup_runs_without_the_completion_lock() {
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let (context, none) = (std::ptr::null_mut(), std::ptr::null());
        let provider = Some(cleanup_checks as Provider);
        let begun = XAsyncBegin(&mut block, context, none, none.cast(), provider);
        assert_eq!(begun, S_OK);
        *CLEANED.lock().unwrap() = call(&mut block);
        XAsyncComplete(&mut block, E_ABORT, 0);
        assert_eq!(SEEN.load(Ordering::SeqCst), 1, "cleanup ran under the lock");
        assert_eq!(XAsyncGetStatus(&mut block, true), E_ABORT);
        *CLEANED.lock().unwrap() = None;
    }

    /// Canceling while `OP_DO_WORK` is running must still deliver `OP_CANCEL`
    /// without blocking the caller, yet defer `OP_CLEANUP` and completion
    /// publication until `OP_DO_WORK` has returned so neither provider context
    /// nor the async block is freed underneath it.
    #[test]
    fn cancel_waits_for_in_flight_do_work_before_cleanup_and_completion() {
        struct Context {
            work_entered: std::sync::Barrier,
            release_work: std::sync::Barrier,
            cancels: AtomicU32,
            cleanups: AtomicU32,
            cleaned_during_work: AtomicBool,
            completed: AtomicU32,
        }

        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            // SAFETY: `Context` lives on the test stack until every thread joins.
            let ctx = unsafe { &*data.context.cast::<Context>() };
            match op {
                OP_BEGIN => XAsyncSchedule(data.block, 0),
                OP_DO_WORK => {
                    ctx.work_entered.wait();
                    ctx.release_work.wait();
                    if ctx.cleanups.load(Ordering::SeqCst) != 0 {
                        ctx.cleaned_during_work.store(true, Ordering::SeqCst);
                    }
                    E_PENDING
                }
                OP_CANCEL => {
                    ctx.cancels.fetch_add(1, Ordering::SeqCst);
                    S_OK
                }
                OP_CLEANUP => {
                    ctx.cleanups.fetch_add(1, Ordering::SeqCst);
                    S_OK
                }
                _ => S_OK,
            }
        }

        extern "system" fn on_complete(block: *mut AsyncBlock) {
            // SAFETY: `Context` lives on the test stack until every thread joins.
            let ctx = unsafe { &*(*block).context.cast::<Context>() };
            ctx.completed.fetch_add(1, Ordering::SeqCst);
        }

        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(1, 3, &mut q), S_OK);
        let ctx = Context {
            work_entered: std::sync::Barrier::new(2),
            release_work: std::sync::Barrier::new(2),
            cancels: AtomicU32::new(0),
            cleanups: AtomicU32::new(0),
            cleaned_during_work: AtomicBool::new(false),
            completed: AtomicU32::new(0),
        };
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::from_ref(&ctx).cast_mut().cast(),
            callback: Some(on_complete),
            internal: [0; 4],
        };
        let null = std::ptr::null();
        assert_eq!(
            XAsyncBegin(
                &mut block,
                std::ptr::from_ref(&ctx).cast_mut().cast(),
                null,
                null.cast(),
                Some(provider),
            ),
            S_OK
        );

        // Wait until the thread-pool worker is inside `OP_DO_WORK`.
        ctx.work_entered.wait();

        // Cancel must invoke `OP_CANCEL` and return without deadlocking on
        // `OP_DO_WORK`, while keeping `OP_CLEANUP` and completion deferred.
        XAsyncCancel(&mut block);
        let cancels_during = ctx.cancels.load(Ordering::SeqCst);
        let cleanups_during = ctx.cleanups.load(Ordering::SeqCst);
        let completed_during = ctx.completed.load(Ordering::SeqCst);
        let status_during = XAsyncGetStatus(&mut block, false);

        // A second cancel must neither re-invoke `OP_CANCEL` nor clean up early.
        XAsyncCancel(&mut block);
        let cancels_after_second = ctx.cancels.load(Ordering::SeqCst);
        let cleanups_after_second = ctx.cleanups.load(Ordering::SeqCst);

        // Let `OP_DO_WORK` observe the still-live context and return.
        ctx.release_work.wait();
        let final_status = XAsyncGetStatus(&mut block, true);
        XTaskQueueCloseHandle(q);

        assert_eq!(cancels_during, 1);
        assert_eq!(
            cleanups_during, 0,
            "cleanup freed provider context while OP_DO_WORK was still running"
        );
        assert_eq!(
            completed_during, 0,
            "completion callback ran while OP_DO_WORK was still running"
        );
        assert_eq!(status_during, E_PENDING);
        assert_eq!(cancels_after_second, 1);
        assert_eq!(cleanups_after_second, 0);
        assert_eq!(final_status, E_ABORT);
        assert!(!ctx.cleaned_during_work.load(Ordering::SeqCst));
        assert_eq!(ctx.cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(ctx.completed.load(Ordering::SeqCst), 1);
    }

    /// Completing a call while `OP_CANCEL` is running must defer `OP_CLEANUP`
    /// until `OP_CANCEL` returns, and a racing `XAsyncCancel` must never enter
    /// `OP_CANCEL` after completion or cleanup.
    #[test]
    fn complete_waits_for_in_flight_cancel_and_blocks_late_cancel() {
        struct Context {
            cancel_entered: std::sync::Barrier,
            release_cancel: std::sync::Barrier,
            cancels: AtomicU32,
            cleanups: AtomicU32,
            cleaned_during_cancel: AtomicBool,
        }

        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: `Context` lives on the test stack until all scoped threads join.
            let ctx = unsafe { &*(*data).context.cast::<Context>() };
            match op {
                OP_CANCEL => {
                    ctx.cancels.fetch_add(1, Ordering::SeqCst);
                    ctx.cancel_entered.wait();
                    ctx.release_cancel.wait();
                    if ctx.cleanups.load(Ordering::SeqCst) != 0 {
                        ctx.cleaned_during_cancel.store(true, Ordering::SeqCst);
                    }
                    S_OK
                }
                OP_CLEANUP => {
                    ctx.cleanups.fetch_add(1, Ordering::SeqCst);
                    S_OK
                }
                _ => S_OK,
            }
        }

        let ctx = Context {
            cancel_entered: std::sync::Barrier::new(2),
            release_cancel: std::sync::Barrier::new(2),
            cancels: AtomicU32::new(0),
            cleanups: AtomicU32::new(0),
            cleaned_during_cancel: AtomicBool::new(false),
        };
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let null = std::ptr::null();
        assert_eq!(
            XAsyncBegin(
                &mut block,
                std::ptr::from_ref(&ctx).cast_mut().cast(),
                null,
                null.cast(),
                Some(provider),
            ),
            S_OK
        );
        let block_addr = (&raw mut block) as usize;
        let (cleanups_during, status_during, cancels_during) = std::thread::scope(|scope| {
            let canceler = scope.spawn(move || {
                XAsyncCancel(block_addr as *mut AsyncBlock);
            });
            ctx.cancel_entered.wait();

            // Complete while `OP_CANCEL` is in flight: `S_OK` wins the result,
            // but cleanup and status publication wait until `OP_CANCEL` returns.
            XAsyncComplete(block_addr as *mut AsyncBlock, S_OK, 0);
            let cleanups_during = ctx.cleanups.load(Ordering::SeqCst);
            let status_during = XAsyncGetStatus(block_addr as *mut AsyncBlock, false);

            // A second concurrent cancel sees the call is already canceling/claimed.
            XAsyncCancel(block_addr as *mut AsyncBlock);
            let cancels_during = ctx.cancels.load(Ordering::SeqCst);

            ctx.release_cancel.wait();
            canceler.join().unwrap();
            (cleanups_during, status_during, cancels_during)
        });

        assert_eq!(
            cleanups_during, 0,
            "cleanup ran while OP_CANCEL was still executing"
        );
        assert_eq!(status_during, E_PENDING);
        assert_eq!(cancels_during, 1);
        assert_eq!(XAsyncGetStatus(&mut block, true), S_OK);
        assert!(!ctx.cleaned_during_cancel.load(Ordering::SeqCst));
        assert_eq!(ctx.cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(ctx.cancels.load(Ordering::SeqCst), 1);

        // Unsynchronized race: `OP_CANCEL` must never run after `OP_CLEANUP`,
        // and `OP_CLEANUP` must run exactly once.
        struct Race {
            cancels: AtomicU32,
            cleanups: AtomicU32,
            cancel_after_cleanup: AtomicBool,
        }
        extern "system" fn race_provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: `Race` outlives the scoped threads.
            let race = unsafe { &*(*data).context.cast::<Race>() };
            match op {
                OP_CANCEL => {
                    race.cancels.fetch_add(1, Ordering::SeqCst);
                    if race.cleanups.load(Ordering::SeqCst) != 0 {
                        race.cancel_after_cleanup.store(true, Ordering::SeqCst);
                    }
                    S_OK
                }
                OP_CLEANUP => {
                    race.cleanups.fetch_add(1, Ordering::SeqCst);
                    S_OK
                }
                _ => S_OK,
            }
        }
        for _ in 0..64 {
            let race = Race {
                cancels: AtomicU32::new(0),
                cleanups: AtomicU32::new(0),
                cancel_after_cleanup: AtomicBool::new(false),
            };
            let mut block = AsyncBlock {
                queue: std::ptr::null(),
                context: std::ptr::null_mut(),
                callback: None,
                internal: [0; 4],
            };
            assert_eq!(
                XAsyncBegin(
                    &mut block,
                    std::ptr::from_ref(&race).cast_mut().cast(),
                    null,
                    null.cast(),
                    Some(race_provider),
                ),
                S_OK
            );
            let addr = (&raw mut block) as usize;
            let start = std::sync::Barrier::new(3);
            std::thread::scope(|scope| {
                let c1 = scope.spawn(|| {
                    start.wait();
                    XAsyncCancel(addr as *mut AsyncBlock);
                });
                let c2 = scope.spawn(|| {
                    start.wait();
                    XAsyncCancel(addr as *mut AsyncBlock);
                });
                start.wait();
                XAsyncComplete(addr as *mut AsyncBlock, S_OK, 0);
                c1.join().unwrap();
                c2.join().unwrap();
            });
            let hr = XAsyncGetStatus(&mut block, true);
            assert!(hr == S_OK || hr == E_ABORT, "unexpected status {hr:#x}");
            assert!(!race.cancel_after_cleanup.load(Ordering::SeqCst));
            assert!(race.cancels.load(Ordering::SeqCst) <= 1);
            assert_eq!(race.cleanups.load(Ordering::SeqCst), 1);
        }
    }

    /// Completion from another thread while `OP_BEGIN` or `OP_CANCEL` is still
    /// running defers finalization until that operation returns, preserving
    /// payload results and preventing premature cleanup.
    #[test]
    fn concurrent_completion_defers_cleanup_during_begin_and_preserves_payload_on_cancel() {
        // Case 1: a worker completes with no payload while `OP_BEGIN` is still running.
        struct BeginCtx {
            completed_called: std::sync::Barrier,
            cleanups: AtomicU32,
            cleaned_during_begin: AtomicBool,
        }
        extern "system" fn begin_provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            // SAFETY: `BeginCtx` outlives the call and its worker thread.
            let ctx = unsafe { &*data.context.cast::<BeginCtx>() };
            match op {
                OP_BEGIN => {
                    let block = data.block as usize;
                    let ctx_ptr = data.context as usize;
                    let worker = std::thread::spawn(move || {
                        XAsyncComplete(block as *mut AsyncBlock, S_OK, 0);
                        // SAFETY: `BeginCtx` lives until `worker.join()`.
                        let ctx = unsafe { &*(ctx_ptr as *const BeginCtx) };
                        ctx.completed_called.wait();
                    });
                    ctx.completed_called.wait();
                    if ctx.cleanups.load(Ordering::SeqCst) != 0 {
                        ctx.cleaned_during_begin.store(true, Ordering::SeqCst);
                    }
                    worker.join().unwrap();
                    S_OK
                }
                OP_CLEANUP => {
                    ctx.cleanups.fetch_add(1, Ordering::SeqCst);
                    S_OK
                }
                _ => S_OK,
            }
        }
        let begin_ctx = BeginCtx {
            completed_called: std::sync::Barrier::new(2),
            cleanups: AtomicU32::new(0),
            cleaned_during_begin: AtomicBool::new(false),
        };
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let null = std::ptr::null();
        assert_eq!(
            XAsyncBegin(
                &mut block,
                std::ptr::from_ref(&begin_ctx).cast_mut().cast(),
                null,
                null.cast(),
                Some(begin_provider),
            ),
            S_OK
        );
        assert!(!begin_ctx.cleaned_during_begin.load(Ordering::SeqCst));
        assert_eq!(begin_ctx.cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(XAsyncGetStatus(&mut block, false), S_OK);

        // Case 2: `OP_DO_WORK` wins with a payload while `OP_CANCEL` is active.
        struct PayloadCtx {
            work_entered: std::sync::Barrier,
            cancel_entered: std::sync::Barrier,
            work_completed: std::sync::Barrier,
            release_cancel: std::sync::Barrier,
            cleanups: AtomicU32,
            value: u64,
        }
        extern "system" fn payload_provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            // SAFETY: `PayloadCtx` lives on the test stack until all threads finish.
            let ctx = unsafe { &*data.context.cast::<PayloadCtx>() };
            match op {
                OP_BEGIN => XAsyncSchedule(data.block, 0),
                OP_DO_WORK => {
                    ctx.work_entered.wait();
                    ctx.cancel_entered.wait();
                    XAsyncComplete(data.block, S_OK, size_of::<u64>());
                    ctx.work_completed.wait();
                    S_OK
                }
                OP_CANCEL => {
                    ctx.cancel_entered.wait();
                    ctx.release_cancel.wait();
                    S_OK
                }
                OP_GET_RESULT => {
                    // SAFETY: the runtime checked room for 8 bytes.
                    unsafe { data.buffer.cast::<u64>().write(ctx.value) };
                    S_OK
                }
                OP_CLEANUP => {
                    ctx.cleanups.fetch_add(1, Ordering::SeqCst);
                    S_OK
                }
                _ => S_OK,
            }
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(1, 0, &mut q), S_OK);
        let payload_ctx = PayloadCtx {
            work_entered: std::sync::Barrier::new(2),
            cancel_entered: std::sync::Barrier::new(2),
            work_completed: std::sync::Barrier::new(2),
            release_cancel: std::sync::Barrier::new(2),
            cleanups: AtomicU32::new(0),
            value: 0xfeed_face_cafe_beef,
        };
        let identity = 0x99 as *const c_void;
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        assert_eq!(
            XAsyncBegin(
                &mut block,
                std::ptr::from_ref(&payload_ctx).cast_mut().cast(),
                identity,
                null.cast(),
                Some(payload_provider),
            ),
            S_OK
        );
        // Ensure `OP_DO_WORK` is already executing before starting `XAsyncCancel`.
        payload_ctx.work_entered.wait();
        let addr = (&raw mut block) as usize;
        let status_during_cancel = std::thread::scope(|scope| {
            let canceler = scope.spawn(move || {
                XAsyncCancel(addr as *mut AsyncBlock);
            });
            // Wait until `OP_CANCEL` has entered and `OP_DO_WORK` has called
            // `XAsyncComplete(..., S_OK, 8)` while `OP_CANCEL` is still active.
            payload_ctx.work_completed.wait();
            let status_during_cancel = XAsyncGetStatus(addr as *mut AsyncBlock, false);
            payload_ctx.release_cancel.wait();
            canceler.join().unwrap();
            status_during_cancel
        });
        assert_eq!(
            status_during_cancel, E_PENDING,
            "status published while OP_CANCEL was still executing"
        );
        assert_eq!(XAsyncGetStatus(&mut block, true), S_OK);
        assert_eq!(payload_ctx.cleanups.load(Ordering::SeqCst), 0);
        let mut got = 0u64;
        let mut used = 0usize;
        assert_eq!(
            XAsyncGetResult(
                &mut block,
                identity,
                size_of::<u64>(),
                (&raw mut got).cast(),
                &mut used,
            ),
            S_OK
        );
        assert_eq!((got, used), (0xfeed_face_cafe_beef, size_of::<u64>()));
        assert_eq!(payload_ctx.cleanups.load(Ordering::SeqCst), 1);
        XTaskQueueCloseHandle(q);
    } 
    /// A waiter must still find the call while provider cleanup is running;
    /// the finalizer itself must not deadlock by waiting on its own cleanup.
    #[test]
    fn status_waiter_sees_call_until_cleanup_and_publication_finish() {
        struct Context {
            cleanup_entered: std::sync::Barrier,
            release_cleanup: std::sync::Barrier,
            cleanups: AtomicU32,
            cleanup_status: AtomicI32,
        }

        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            // SAFETY: Context lives until the begin thread and waiter join.
            let ctx = unsafe { &*data.context.cast::<Context>() };
            match op {
                OP_BEGIN => E_ABORT,
                OP_CLEANUP => {
                    ctx.cleanups.fetch_add(1, Ordering::SeqCst);
                    // Reentrant wait from the finalizer thread must not deadlock.
                    ctx.cleanup_status.store(
                        XAsyncGetStatus(data.block, true),
                        Ordering::SeqCst,
                    );
                    ctx.cleanup_entered.wait();
                    ctx.release_cleanup.wait();
                    S_OK
                }
                _ => S_OK,
            }
        }

        let ctx = Context {
            cleanup_entered: std::sync::Barrier::new(2),
            release_cleanup: std::sync::Barrier::new(2),
            cleanups: AtomicU32::new(0),
            cleanup_status: AtomicI32::new(S_OK),
        };
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::from_ref(&ctx).cast_mut().cast(),
            callback: None,
            internal: [0; 4],
        };
        let block_addr = (&raw mut block) as usize;
        let ctx_addr = std::ptr::from_ref(&ctx) as usize;
        let begin = std::thread::spawn(move || {
            XAsyncBegin(
                block_addr as *mut AsyncBlock,
                ctx_addr as *mut c_void,
                std::ptr::null(),
                std::ptr::null(),
                Some(provider),
            )
        });

        // OP_CLEANUP is now blocked, before status publication.
        ctx.cleanup_entered.wait();
        let pending = call(&mut block).expect("call remains registered during cleanup");
        let call_address = Arc::as_ptr(&pending) as usize;
        let (waiter_entered_tx, waiter_entered_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            *lock(&WAIT_FOR_STATUS_TEST_HOOK) = Some(WaitForStatusTestHook {
                call: call_address,
                thread: std::thread::current().id(),
                entered: waiter_entered_tx,
            });
            wait_for_status(&pending)
        });
        let waiter_entered = waiter_entered_rx.recv_timeout(Duration::from_secs(2));
        ctx.release_cleanup.wait();

        assert_eq!(begin.join().unwrap(), S_OK);
        assert_eq!(waiter.join().unwrap(), E_ABORT);
        let leftover_waiter_hook = lock(&WAIT_FOR_STATUS_TEST_HOOK).take();
        assert!(waiter_entered.is_ok(), "waiter did not enter the done wait before cleanup was released");
        assert!(leftover_waiter_hook.is_none(), "waiter-entered test hook was not consumed");
        assert_eq!(ctx.cleanup_status.load(Ordering::SeqCst), E_PENDING);
        assert_eq!(ctx.cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);
        assert!(call(&mut block).is_none(), "failed call removed after publication");
    }

    /// XAsyncComplete from inside OP_BEGIN must defer cleanup until the
    /// provider callback returns, not merely until its current thread is the
    /// only active provider thread.
    #[test]
    fn synchronous_completion_inside_begin_defers_cleanup_until_return() {
        struct Context {
            cleanups: AtomicU32,
            cleaned_before_begin_return: AtomicBool,
        }

        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            // SAFETY: Context lives for the duration of XAsyncBegin.
            let ctx = unsafe { &*data.context.cast::<Context>() };
            match op {
                OP_BEGIN => {
                    XAsyncComplete(data.block, S_OK, 0);
                    if ctx.cleanups.load(Ordering::SeqCst) != 0 {
                        ctx.cleaned_before_begin_return.store(true, Ordering::SeqCst);
                    }
                    S_OK
                }
                OP_CLEANUP => {
                    ctx.cleanups.fetch_add(1, Ordering::SeqCst);
                    S_OK
                }
                _ => S_OK,
            }
        }

        let ctx = Context {
            cleanups: AtomicU32::new(0),
            cleaned_before_begin_return: AtomicBool::new(false),
        };
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::from_ref(&ctx).cast_mut().cast(),
            callback: None,
            internal: [0; 4],
        };
        assert_eq!(
            XAsyncBegin(
                &mut block,
                std::ptr::from_ref(&ctx).cast_mut().cast(),
                std::ptr::null(),
                std::ptr::null(),
                Some(provider),
            ),
            S_OK
        );
        assert!(!ctx.cleaned_before_begin_return.load(Ordering::SeqCst));
        assert_eq!(ctx.cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(XAsyncGetStatus(&mut block, true), S_OK);
        assert!(call(&mut block).is_none());
    }

    #[test]
    fn block_reuse_waits_for_atomic_completion_publication() {
        extern "system" fn idle(_: u32, _: *const ProviderData) -> HRESULT { S_OK }
        let mut storage = Box::new(AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        });
        let block = (&mut *storage) as *mut AsyncBlock;
        assert_eq!(
            XAsyncBegin(block, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), Some(idle)),
            S_OK
        );
        let entered = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        *lock(&FINALIZE_TEST_HOOK) = Some(FinalizeTestHook {
            block: block as usize,
            entered: entered.clone(),
            resume: resume.clone(),
        });
        let address = block as usize;
        let finalizer = std::thread::spawn(move || {
            XAsyncComplete(address as *mut AsyncBlock, E_ABORT, 0);
        });

        // Stop after status becomes visible, before map removal and done publication.
        entered.wait();
        let published = XAsyncGetStatus(block, false);
        let calls_locked = CALLS.try_lock().is_err();
        {
            let (status, size) = words(block);
            status.store(0, Ordering::Release);
            size.store(0, Ordering::Release);
        }

        let (contended_tx, contended_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let begin_thread = std::thread::spawn(move || {
            let block = address as *mut AsyncBlock;
            *lock(&CALL_LOOKUP_TEST_HOOK) = Some(CallLookupTestHook {
                block: address,
                thread: std::thread::current().id(),
                contended: contended_tx,
            });
            let result = XAsyncBegin(
                block, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), Some(idle),
            );
            result_tx.send(result).unwrap();
        });
        // This is emitted only after the actual lookup thread's try_lock sees
        // WouldBlock, not merely after the worker is scheduled.
        let contention = contended_rx.recv_timeout(Duration::from_secs(2));

        // Always release the finalizer before asserting, avoiding a stranded worker on failure.
        resume.wait();
        let final_result = result_rx.recv_timeout(Duration::from_secs(2));
        finalizer.join().unwrap();
        begin_thread.join().unwrap();
        let leftover_lookup_hook = lock(&CALL_LOOKUP_TEST_HOOK).take();

        assert_eq!(published, E_ABORT);
        assert!(calls_locked, "CALLS was not held across status publication");
        assert!(contention.expect("reuse thread never reached CALLS lookup"), "reuse thread did not observe CALLS contention");
        assert!(leftover_lookup_hook.is_none(), "CALLS lookup test hook was not consumed");
        assert_eq!(final_result.expect("reuse remained blocked after publication"), S_OK);
        assert_eq!(XAsyncGetStatus(block, false), E_PENDING);
        XAsyncComplete(block, E_ABORT, 0);
        assert_eq!(XAsyncGetStatus(block, false), E_ABORT);
        assert!(call(block).is_none());
    }
}
