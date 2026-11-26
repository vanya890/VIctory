//! Command queues. Submitted lists run on the host GPU and are waited for,
//! so fence signals complete at once.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUEUE, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const QUEUE_IIDS: [Iid; 4] = [
    guid(
        0x0ec870a6,
        0x5d7e,
        0x4c22,
        [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDesc {
    kind: u32,
    priority: i32,
    flags: u32,
    nodes: u32,
}

/// The title reads the list type right after the header.
#[repr(C)]
struct Queue {
    object: Object,
    kind: u32,
    desc: QueueDesc,
}
const _: () = assert!(std::mem::offset_of!(Queue, kind) == 0x10);

fn queue<'a>(this: This) -> &'a Queue {
    // SAFETY: called from queue methods only.
    unsafe { &*this.cast::<Queue>() }
}

pub(crate) extern "system" fn create(
    _: This,
    desc: *const QueueDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "command queue");
    let queue = Box::new(Queue {
        object: if crate::device::newer() {
            Object::new(&NEWER_FUNCTIONS)
        } else {
            Object::new(&FUNCTIONS)
        },
        kind: desc.kind,
        desc,
    });
    object::hand_out(QUEUE, queue, iid, &QUEUE_IIDS, out)
}

/// The console's queue description: an engine or pipe and a queue on it
/// in place of PC's priority and node mask.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDescX {
    kind: u32,
    flags: u32,
    engine_or_pipe: u32,
    queue: u32,
}

/// Device method `CreateCommandQueueX`. The host picks engines itself, so
/// the queue is created as on PC.
pub(crate) extern "system" fn create_x(
    this: This,
    desc: *const QueueDescX,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&x) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(desc = ?x, "console command queue");
    let desc = QueueDesc {
        kind: x.kind,
        priority: 0,
        flags: x.flags,
        nodes: 1,
    };
    create(this, &desc, iid, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUEUE,
        iid,
        object::is(iid, &QUEUE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Queue>(this)
}

/// Queues the lists' host lists behind what the queue holds, and runs what
/// may run (`crate::hold`; decision 0007).
extern "system" fn execute(this: This, count: u32, lists: *const This) {
    if lists.is_null() {
        return;
    }
    #[cfg(windows)]
    if crate::host::host().is_some() {
        // SAFETY: the title passes `count` lists.
        let lists = unsafe { std::slice::from_raw_parts(lists, count as usize) };
        let mut held = held();
        for &l in lists {
            for segment in crate::list::segments(l) {
                let waits = segment.waits();
                held.push(this as usize, Item::Segment(segment, waits));
            }
        }
        account().submissions += 1;
        pump(&mut held, Why::Submission);
        return;
    }
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::Relaxed) {
        tracing::warn!(count, "command lists submitted; no host GPU runs them");
    }
}

/// What ran held work: the title's submission, a queue's Signal or Wait, a
/// CPU fence call, PresentX, or our timer.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Why {
    Submission,
    Signal,
    Wait,
    Fence,
    Present,
    Timer,
}

#[cfg(windows)]
use crate::hold::Item;

/// A fence reference kept alive while a queue holds a `Signal` or `FenceWait`
/// for it (`crate::fence::keep` / `crate::fence::let_go`).
#[cfg(windows)]
struct KeptFence(usize);

#[cfg(windows)]
impl KeptFence {
    fn new(fence: This) -> Self {
        crate::fence::keep(fence);
        Self(fence as usize)
    }

    fn as_this(&self) -> This {
        self.0 as This
    }
}

#[cfg(windows)]
impl Drop for KeptFence {
    fn drop(&mut self) {
        crate::fence::let_go(self.0 as This);
    }
}

/// Every queue's held work.
#[cfg(windows)]
type Holding = crate::hold::Held<std::sync::Arc<crate::host::HostList>, KeptFence>;

#[cfg(windows)]
static HELD: std::sync::Mutex<Holding> = std::sync::Mutex::new(crate::hold::Held::new());

#[cfg(windows)]
fn held() -> std::sync::MutexGuard<'static, Holding> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs held work that may run now (`why` for the frame line).
pub(crate) fn pump_held(why: Why) {
    #[cfg(windows)]
    pump(&mut held(), why);
    #[cfg(not(windows))]
    let _ = why;
}

/// The same unless another thread is at it (calls that poll: a fence's
/// value, our timer).
pub(crate) fn pump_held_if_free(why: Why) {
    #[cfg(windows)]
    if let Ok(mut held) = HELD.try_lock() {
        pump(&mut held, why);
    }
    #[cfg(not(windows))]
    let _ = why;
}

/// How long a queue's head may wait before it runs anyway.
// shortcut: a safety valve against a wait nothing satisfies (a write we do
// not make), which would hold the title's fences until its 30 s watchdog
// ends the run; counted and logged, remove once runs show it never fires.
#[cfg(windows)]
const VALVE: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(windows)]
fn pump(held: &mut Holding, why: Why) {
    if held.is_empty() {
        return;
    }
    let Some(host) = crate::host::host() else { return };
    let read = read_value;
    for (queue, waits) in held.force_older_than(VALVE) {
        account().forced += 1;
        tracing::warn!(queue = kind(queue), ?waits, "held work runs without its waits after {VALVE:?} (safety valve)");
    }
    let fence = |f: &KeptFence| crate::fence::value(f.as_this());
    let started = std::time::Instant::now();
    let mut ran = std::time::Duration::ZERO;
    let mut run = |_: usize, segments: Vec<std::sync::Arc<crate::host::HostList>>| {
        let lists: Vec<&crate::host::HostList> = segments.iter().map(|s| &**s).collect();
        let at = std::time::Instant::now();
        host.execute(&lists);
        ran += at.elapsed();
        drop(lists);
        segments.into_iter().for_each(crate::list::recycle);
    };
    let mut signal = |f: KeptFence, value: u64| {
        crate::fence::set(f.as_this(), value);
        drop(f);
    };
    let pumped = held.pump(&mut crate::hold::World {
        read: &read,
        fence: &fence,
        run: &mut run,
        signal: &mut signal,
    });
    if pumped.segments > 0 || pumped.unknown > 0 {
        let mut a = account();
        a.pump += started.elapsed().saturating_sub(ran);
        a.pieces += pumped.pieces;
        a.segments += pumped.segments;
        a.unknown += pumped.unknown;
        for (queue, waited) in pumped.waited {
            a.released.push((kind(queue), waited, why));
        }
    }
    // Once work stays held, a timer tests it every 2 ms: memory the title
    // writes from the CPU, or the valve, may let it go with no call of ours.
    // The pump already knows whether any queue remains blocked. Avoid
    // re-reading the same wait words just to decide whether to start polling.
    if !pumped.blocked {
        return;
    }
    static TIMER: std::sync::Once = std::sync::Once::new();
    TIMER.call_once(|| {
        let _ = std::thread::Builder::new().name("victory held work".into()).spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_millis(2));
            pump_held_if_free(Why::Timer);
        });
    });
}

/// The value a wait tests (4 or 8 bytes), if the memory can be read.
#[cfg(windows)]
fn read_value(address: u64, bytes: u8) -> Option<u64> {
    let mut value = [0u8; 8];
    crate::host::read_into(address as *const u8, &mut value[..usize::from(bytes.min(8))]).then(|| u64::from_le_bytes(value))
}

/// A queue's type, for logs (0 graphics, 2 compute, 3 copy). Titles do not
/// release queues that hold work.
#[cfg(windows)]
fn kind(queue: usize) -> u32 {
    self::queue(queue as This).kind
}

/// What queues did with held work since the last presented frame.
#[cfg(windows)]
#[derive(Default)]
struct Account {
    submissions: u32,
    pieces: u32,
    segments: u32,
    /// Segments that waited: queue type, how long, what ran them.
    released: Vec<(u32, std::time::Duration, Why)>,
    unknown: u32,
    forced: u32,
    mappings: u32,
    /// Time in the pump outside host submissions.
    pump: std::time::Duration,
}

#[cfg(windows)]
fn account() -> std::sync::MutexGuard<'static, Account> {
    static ACCOUNT: std::sync::OnceLock<std::sync::Mutex<Account>> = std::sync::OnceLock::new();
    ACCOUNT.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// The frame line of held work: what waited, how long and what released it,
/// and what is still held at the present, with the waits that do not hold.
#[cfg(windows)]
fn account_frame(frame: u64) {
    let (cuts, early) = crate::list::take_cuts();
    let (fence_waits, fence_waited) = crate::fence::take_waited();
    let a = std::mem::take(&mut *account());
    let blocked = held().blocked(&read_value);
    if a.released.is_empty() && blocked.is_empty() && a.forced == 0 && a.unknown == 0 && a.mappings == 0 {
        return;
    }
    let longest = a.released.iter().map(|r| r.1).max().unwrap_or_default();
    let mut by = [0u32; 6];
    for r in &a.released {
        by[r.2 as usize] += 1;
    }
    tracing::info!(
        frame,
        submissions = a.submissions,
        pieces = a.pieces,
        segments = a.segments,
        cuts,
        early_resets = early,
        waited = a.released.len(),
        longest_ms = longest.as_secs_f64() * 1e3,
        pump_ms = a.pump.as_secs_f64() * 1e3,
        fence_waits,
        fence_wait_ms = fence_waited.as_secs_f64() * 1e3,
        "held work this frame (submissions; host submissions, segments; lists cut at waits; lists reset or released \
         while held, each keeping its own host list; segments that waited, released by \
         submission/signal/wait/fence/present/timer {by:?}; time in the pump outside host submissions; events the          title waited on for fence values and their summed waits); held at the present: {}",
        blocked.len()
    );
    tracing::debug!(
        frame,
        "held work released this frame (queue type, ms, by): {:?}",
        a.released.iter().map(|(k, d, w)| (k, d.as_secs_f64() * 1e3, w)).collect::<Vec<_>>()
    );
    for (queue, items, waited, unmet) in &blocked {
        let level_warn = blocked.len() > 1;
        let unmet: Vec<String> = unmet
            .iter()
            .map(|w| format!("{:#x} cmp {} {:#x} (is {:?})", w.address, w.comparison, w.reference, read_value(w.address, w.bytes)))
            .collect();
        if level_warn {
            tracing::warn!(frame, queue = kind(*queue), items, waited_ms = waited.as_millis(), ?unmet, "two or more queues hold work at the present");
        } else {
            tracing::info!(frame, queue = kind(*queue), items, waited_ms = waited.as_millis(), ?unmet, "a queue holds work at the present");
        }
    }
    if a.forced > 0 || a.unknown > 0 || a.mappings > 0 {
        tracing::warn!(
            frame,
            forced = a.forced,
            unknown = a.unknown,
            mappings = a.mappings,
            "held work: run without its waits (valve), waits with an unknown comparison (taken as holding), \
             page mappings done ahead of held work"
        );
    }
}

/// One plane of a `PresentX` call (0x48-byte stride): frame token,
/// resource count, and pointer to the plane's resources.
#[repr(C)]
pub(crate) struct Plane {
    token: u64,
    count: u32,
    resources: *const This,
    _rest: [u64; 6],
}
const _: () = assert!(std::mem::size_of::<Plane>() == 0x48);
/// Submission tagged with the frame it belongs to `(count, lists, frame
/// token, flags)`; frames are paced elsewhere, so it runs as a plain one.
extern "system" fn execute_x(this: This, count: u32, lists: *const This, _token: u64, _flags: u32) {
    execute(this, count, lists);
}
/// Signal with sync settings for the top and bottom of the pipe; our
/// submissions are complete when they run, so it signals as Signal does.
extern "system" fn signal2_x(
    this: This,
    fence: This,
    value: u64,
    _bottom: *const u8,
    _top: *const u8,
) -> HRESULT {
    signal(this, fence, value)
}
/// Markers and events only matter to profilers.
extern "system" fn marker(_: This) {}
/// `SuspendX(flags)`: on the console, a title's suspend routine stops its
/// queues' GPU work (`ResumeX` restarts it). Our submissions have run when
/// Execute returns (work held for a wait runs as soon as the wait is met),
/// and a suspended title is never resumed here, since the process ends
/// (`lifecycle::end_title`), so there is nothing to stop.
extern "system" fn suspend_x(_: This, flags: u32) -> HRESULT {
    tracing::debug!(flags, "command queue suspended");
    S_OK
}
/// `ResumeX`: nothing was stopped (see [`suspend_x`]).
extern "system" fn resume_x(_: This) -> HRESULT {
    S_OK
}
/// At once, or behind the work the queue holds (its fence, and the memory
/// the title reads it at, move only once that work has run).
extern "system" fn signal(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    #[cfg(windows)]
    {
        let mut held = held();
        if held.holds_work(this as usize) {
            held.push(this as usize, Item::Signal(KeptFence::new(fence), value));
        } else {
            crate::fence::set(fence, value);
        }
        // Another queue may wait for this fence.
        pump(&mut held, Why::Signal);
    }
    #[cfg(not(windows))]
    {
        let _ = this;
        crate::fence::set(fence, value);
    }
    S_OK
}
/// The queue's later work waits until the fence reaches `value`.
extern "system" fn wait(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    #[cfg(windows)]
    if crate::host::host().is_some() {
        let mut held = held();
        if crate::fence::value(fence) < value {
            held.push(this as usize, Item::FenceWait(KeptFence::new(fence), value));
            pump(&mut held, Why::Wait);
        }
    }
    #[cfg(not(windows))]
    let _ = (this, value);
    S_OK
}
/// Ticks per second of GPU timestamps (see [`Written::GpuTimestamp`]).
extern "system" fn timestamp_frequency(_: This, out: *mut u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(TIMESTAMP_FREQUENCY) };
    S_OK
}
const TIMESTAMP_FREQUENCY: u64 = 100_000_000;

/// What a list's bottom-of-pipe write stores when the list has run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Written {
    Value(u64),
    /// The GPU clock: [`TIMESTAMP_FREQUENCY`] ticks per second since the
    /// process started (our GPU work is done on the CPU's timeline).
    GpuTimestamp,
    /// The CPU's performance counter.
    SystemTimestamp,
}

impl Written {
    /// Stores the low `bytes` (4 or 8) of the value at `at`.
    pub(crate) fn write(self, at: *mut u8, bytes: usize) {
        let value = match self {
            Written::Value(v) => v,
            Written::GpuTimestamp => {
                static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
                (elapsed.as_nanos() / (1_000_000_000 / u128::from(TIMESTAMP_FREQUENCY))) as u64
            }
            Written::SystemTimestamp => system_timestamp(),
        };
        // SAFETY: the title names writable GPU memory (unified with ours).
        unsafe {
            if bytes == 4 {
                at.cast::<u32>().write_unaligned(value as u32);
            } else {
                at.cast::<u64>().write_unaligned(value);
            }
        }
    }
}

fn system_timestamp() -> u64 {
    #[cfg(windows)]
    {
        let mut count = 0;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
        count as u64
    }
    #[cfg(not(windows))]
    0
}

/// Frames presented so far.
static FRAMES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn presented() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

/// Shows the first resource of each presented plane in our window.
extern "system" fn present(
    _: This,
    planes: u32,
    plane_parameters: *const Plane,
    _parameters: *const u8,
) -> HRESULT {
    // What may run before the frame is shown does.
    // shortcut: PresentX does not wait behind work the queue still holds (it
    // shows the frame as far as it ran); the frame line names such work.
    pump_held(Why::Present);
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    #[cfg(windows)]
    account_frame(frame);
    #[cfg(feature = "diagnostics")]
    crate::frame::presented();
    if frame.is_power_of_two() {
        let (draws, dispatches) = crate::list::take_counts();
        #[cfg(windows)]
        let (drawn, dispatched) = {
            static DRAWN: AtomicU64 = AtomicU64::new(0);
            static DISPATCHED: AtomicU64 = AtomicU64::new(0);
            let since = |last: &AtomicU64, now: u64| now - last.swap(now, Ordering::Relaxed);
            (
                since(&DRAWN, crate::draw::drawn()),
                since(&DISPATCHED, crate::draw::dispatched()),
            )
        };
        #[cfg(not(windows))]
        let (drawn, dispatched) = (0, 0);
        tracing::info!(
            frame,
            draws,
            drawn,
            dispatches,
            dispatched,
            "frame presented (since the last line: draws and dispatches, and those run on the host)"
        );
        #[cfg(windows)]
        if frame >= 256 {
            let (hits, misses) = crate::shader_cache::counts();
            tracing::info!(hits, misses, "shaders from the cache and translated so far");
        }
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        crate::capture::presented(frame);
        if crate::capture::listed(frame) || crate::capture::has_targets() {
            crate::capture::dump_targets(frame);
        }
    }
    #[cfg(windows)]
    if planes > 0
        && !plane_parameters.is_null()
        && let Some(host) = crate::host::host()
    {
        // SAFETY: the title passes the plane descriptions.
        let slice = unsafe { std::slice::from_raw_parts(plane_parameters, planes.min(8) as usize) };
        let mut resources: Vec<(This, std::sync::Arc<crate::resource::HostTexture>)> =
            Vec::with_capacity(slice.len());
        for plane in slice {
            if plane.count == 0 || plane.resources.is_null() {
                continue;
            }
            // SAFETY: the plane lists `count` resources.
            let resource = unsafe { *plane.resources };
            if let Some(copy) = crate::resource::host_copy(resource) {
                resources.push((resource, copy));
            }
        }
        if !resources.is_empty() {
            let sources: Vec<_> = resources
                .iter()
                .map(|(resource, copy)| {
                    crate::host::PresentPlane::Tracked(copy, crate::resource::view_format_of(*resource, copy))
                })
                .collect();
            let (shown, state) = host.present_planes(&sources, frame);
            #[cfg(feature = "diagnostics")]
            blinks(frame, resources[0].0, state, shown);
            #[cfg(not(feature = "diagnostics"))]
            let _ = (shown, state);
        }
    }
    #[cfg(not(windows))]
    let _ = (planes, plane_parameters);
    S_OK
}
/// Diagnostics: logs a presented frame that is black after one that was
/// not, with what was presented and how many draws ran since.
#[cfg(all(windows, feature = "diagnostics"))]
fn blinks(frame: u64, resource: This, state: u32, shown: Option<bool>) {
    static LAST: std::sync::Mutex<(bool, u64, u64)> = std::sync::Mutex::new((false, 0, 0));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let drawn = crate::draw::drawn();
    let black = shown.unwrap_or(false);
    let draws = crate::draw::take_frame();
    if black && !last.0 {
        last.2 += 1;
        tracing::warn!(
            frame,
            resource = ?resource,
            state = format_args!("{state:#x}"),
            draws = drawn - last.1,
            copied = shown.is_some(),
            black_frames = last.2,
            levels = ?crate::capture::recent_levels(),
            "a black frame after a frame that was not"
        );
        for d in draws {
            tracing::warn!("black frame {frame}: {d}");
        }
        AFTER.store(frame + 6, Ordering::Relaxed);
    }
    // Six frames on: the levels around it (a fade or a single drop).
    static AFTER: AtomicU64 = AtomicU64::new(0);
    if AFTER.load(Ordering::Relaxed) == frame {
        tracing::warn!(frame, levels = ?crate::capture::recent_levels(), "levels after the black frame");
    }
    *last = (black, drawn, last.2);
}

/// A run of destination pages: how they are mapped (0 pool pages from
/// `start` on, 1 pool page `start` each, 2 unmapped, 3 left as they are)
/// and how many.
#[repr(C)]
pub(crate) struct PageRange {
    kind: u32,
    start: u32,
    count: u32,
}

/// The ranges mapped from a destination address on.
#[repr(C)]
pub(crate) struct PageBatch {
    destination: u64,
    count: u32,
    ranges: *const PageRange,
}

/// Bytes per mapped page.
const PAGE: u64 = 64 << 10;

/// `CopyPageMappingsBatchX(count, batches, pool, pool pages, flags)`: maps
/// pages of a pool (memory the title mapped physical pages into) at the
/// batches' addresses. Grand Theft Auto VI streams textures into pool pages
/// and maps them into its tiled heap this way (title+0xf2c470, ranges of
/// one constant page or of unmapped pages). Done at once.
///
/// shortcut: done ahead of work the queue still holds (counted in the frame
/// line); queue it as an item like Signal if that count is not 0.
extern "system" fn copy_page_mappings(
    this: This,
    count: u32,
    batches: *const PageBatch,
    pool: u64,
    pool_pages: u32,
    _flags: u32,
) -> HRESULT {
    let Some(map) = crate::resource::page_mapper() else {
        return E_INVALIDARG;
    };
    if batches.is_null() && count > 0 {
        return E_INVALIDARG;
    }
    #[cfg(windows)]
    if held().holds_work(this as usize) {
        account().mappings += 1;
    }
    #[cfg(not(windows))]
    let _ = this;
    let batches = (count > 0).then(|| {
        // SAFETY: the title passes `count` batches.
        unsafe { std::slice::from_raw_parts(batches, count as usize) }
    });
    let mut ok = true;
    for b in batches.into_iter().flatten() {
        if b.ranges.is_null() {
            continue;
        }
        // SAFETY: each batch holds `count` ranges.
        let ranges = unsafe { std::slice::from_raw_parts(b.ranges, b.count as usize) };
        let mut at = b.destination;
        for r in ranges {
            let len = u64::from(r.count) * PAGE;
            let inside = |pages: u32| r.start.checked_add(pages).is_some_and(|e| e <= pool_pages);
            let source = pool + u64::from(r.start) * PAGE;
            ok &= match r.kind {
                0 if inside(r.count) => map(at, source, len),
                1 if inside(1) => (0..u64::from(r.count)).all(|k| map(at + k * PAGE, source, PAGE)),
                2 => map(at, 0, len),
                3 => true,
                _ => false,
            };
            at += len;
        }
    }
    if !ok {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!(pool = format_args!("{pool:#x}"), "a page mapping failed"));
    }
    S_OK
}

extern "system" fn desc(this: This, out: *mut QueueDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(queue(this).desc) };
    }
}

/// The queue methods by their slot in older titles' queues (the 2020-era
/// layout): IUnknown 3, object 4, device child 1, queue 26 (12 as on PC,
/// then console ones).
const METHODS: [(usize, *const ()); 17] = [
    (0, query as *const ()),
    (2, release as *const ()),
    (7, object::parent_device as *const ()), // 0x38
    (10, execute as *const ()),              // 0x50
    (11, marker as *const ()),               // 0x58 set marker
    (12, marker as *const ()),               // 0x60 begin event
    (13, marker as *const ()),               // 0x68 end event
    (14, signal as *const ()),               // 0x70
    (15, wait as *const ()),                 // 0x78
    (16, timestamp_frequency as *const ()),  // 0x80
    (21, suspend_x as *const ()),            // 0xa8
    (22, resume_x as *const ()),             // 0xb0
    (26, desc as *const ()),                 // 0xd0
    (29, copy_page_mappings as *const ()),   // 0xe8 page mapping batch
    (30, present as *const ()),              // 0xf0
    (33, execute_x as *const ()),            // 0x108
    (32, signal2_x as *const ()),            // 0x100
];

/// Newer titles' queues have two more methods before Signal, which moved
/// from 0x70 to 0x80: every slot from 0x70 on is two later (taken here as
/// starting exactly at 0x70, the methods before it kept in place).
/// Inferred from Grand Theft Auto VI's calls: Signal 0x80 `(fence,
/// value)`, timestamp frequency 0x90 (title+0x24841a6), suspend and resume
/// 0xb8/0xc0, PresentX 0x100, the frame-token submission 0x118 `(count,
/// lists, token, flags)` (title+0x24821dd).
const fn newer_slot(slot: usize) -> usize {
    if slot >= 14 { slot + 2 } else { slot }
}

const fn queue_table(newer: bool) -> [*const (); 36] {
    let mut f = object::d3d12_object(object::missing_table::<QUEUE, 36>());
    let mut i = 0;
    while i < METHODS.len() {
        let (slot, method) = METHODS[i];
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    f
}

static FUNCTIONS: Table<36> = Table(queue_table(false));
static NEWER_FUNCTIONS: Table<36> = Table(queue_table(true));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    /// Distinct title resources can name the same host texture. Presenting
    /// both planes must preserve that texture's original rendering state.
    #[cfg(windows)]
    #[test]
    fn aliased_present_planes_preserve_the_host_texture_state() {
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_RENDER_TARGET;

        let Some(host) = crate::host::host() else {
            return;
        };
        const TARGET: u32 = D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32;
        let address = crate::watch::tests::pool(1);
        let null = std::ptr::null();
        let iid = crate::object::guid(
            0x696442be,
            0xa72e,
            0x4059,
            [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        #[repr(align(8))]
        struct TextureWords([u32; 14]);
        let desc = TextureWords([3, 0, 0, 0, 32, 0, 32, 1 | 1 << 16, 87, 1, 0, 1, 1, 0]);
        let resources = std::array::from_fn::<_, 2, _>(|_| {
            let mut resource = null;
            assert_eq!(
                crate::resource::create_at_address(
                    null,
                    address as u64,
                    desc.0.as_ptr().cast(),
                    0,
                    null.cast(),
                    &iid,
                    &mut resource,
                ),
                S_OK
            );
            resource
        });
        assert_ne!(resources[0], resources[1]);
        let primary = crate::resource::host_copy(resources[0]).unwrap();
        let alias = crate::resource::host_copy(resources[1]).unwrap();
        assert!(std::sync::Arc::ptr_eq(&primary, &alias));
        let list = host.new_list().unwrap();
        crate::list::ensure_state(&list, resources[0], TARGET);
        // SAFETY: the texture is a live render target, on an open list.
        unsafe {
            list.list.ClearRenderTargetView(
                host.rtv(&primary.texture, None),
                &[0.25, 0.5, 0.75, 1.0],
                None,
            );
        }
        list.close();
        host.execute(&[&list]);
        assert_eq!(primary.state.load(Ordering::Relaxed), TARGET);

        let planes = resources.each_ref().map(|resource| Plane {
            token: 0,
            count: 1,
            resources: std::ptr::from_ref(resource),
            _rest: [0; 6],
        });
        assert_eq!(present(null, 2, planes.as_ptr(), null.cast()), S_OK);
        let restored = primary.state.load(Ordering::Relaxed);
        let release = |resource| slot::<extern "system" fn(This) -> u32>(resource, 0x10)(resource);
        for resource in resources {
            assert_eq!(release(resource), 0);
        }
        drop((list, primary, alias));
        crate::watch::tests::free(address);
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
        assert_eq!(restored, TARGET, "the two planes share one rendering state");
    }

    /// Presenting a texture while another queue submits work on it must never
    /// expose a phantom COPY_SOURCE state, emit a stale before-state barrier,
    /// or clobber the state left by the concurrent submission.
    #[cfg(windows)]
    #[test]
    fn present_does_not_expose_copy_source_or_clobber_concurrent_state() {
        use std::collections::HashMap;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_RENDER_TARGET,
        };
        use windows::core::Interface;

        let Some(host) = crate::host::host() else {
            return;
        };
        const TARGET: u32 = D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32;
        const SHADER: u32 = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0 as u32;
        let address = crate::watch::tests::pool(1);
        let null = std::ptr::null();
        let iid = crate::object::guid(
            0x696442be,
            0xa72e,
            0x4059,
            [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        #[repr(align(8))]
        struct TextureWords([u32; 14]);
        let desc = TextureWords([3, 0, 0, 0, 32, 0, 32, 1 | 1 << 16, 28, 1, 0, 1, 1, 0]);
        let mut resource = null;
        assert_eq!(
            crate::resource::create_at_address(
                null,
                address as u64,
                desc.0.as_ptr().cast(),
                0,
                null.cast(),
                &iid,
                &mut resource,
            ),
            S_OK
        );
        let copy = crate::resource::host_copy(resource).unwrap();
        let raw = copy.texture.as_raw() as usize;
        let setup = host.new_list().unwrap();
        crate::list::ensure_state(&setup, resource, TARGET);
        // SAFETY: live render target on an open list.
        unsafe {
            setup.list.ClearRenderTargetView(
                host.rtv(&copy.texture, None),
                &[0.2, 0.4, 0.6, 1.0],
                None,
            );
        }
        setup.close();
        host.execute(&[&setup]);
        assert_eq!(copy.state.load(Ordering::Relaxed), TARGET);

        // Hold the presenter lock while another thread enters `present`, so a
        // concurrent `Host::execute` runs between entering `present` and the
        // present copy executing.
        let hold = crate::host::tests::hold_presenter(host);
        let (started_tx, started_rx) = mpsc::channel();
        let resource_addr = resource as usize;
        let presenter_thread = std::thread::spawn(move || {
            let resource = resource_addr as This;
            let plane = Plane {
                token: 0,
                count: 1,
                resources: std::ptr::from_ref(&resource),
                _rest: [0; 6],
            };
            let _ = crate::host::tests::issued();
            started_tx.send(()).unwrap();
            let hr = present(std::ptr::null(), 1, &plane, std::ptr::null());
            (hr, crate::host::tests::issued())
        });
        started_rx.recv().unwrap();
        let deadline = Instant::now() + Duration::from_millis(50);
        while copy.state.load(Ordering::Relaxed) == TARGET && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let observed_during_present = copy.state.load(Ordering::Relaxed);

        let mut gpu_states = HashMap::from([(raw, TARGET)]);
        let _ = crate::host::tests::issued();
        let concurrent = host.new_list().unwrap();
        crate::list::ensure_state(&concurrent, resource, SHADER);
        concurrent.close();
        host.execute(&[&concurrent]);
        let concurrent_barriers = crate::host::tests::issued();
        let concurrent_replay = crate::host::tests::replay(&concurrent_barriers, &mut gpu_states);

        drop(hold);
        let (hr, present_barriers) = presenter_thread.join().unwrap();
        let present_replay = crate::host::tests::replay(&present_barriers, &mut gpu_states);
        let state_after_present = copy.state.load(Ordering::Relaxed);

        let _ = crate::host::tests::issued();
        let followup = host.new_list().unwrap();
        crate::list::ensure_state(&followup, resource, TARGET);
        followup.close();
        host.execute(&[&followup]);
        let followup_barriers = crate::host::tests::issued();
        let followup_replay = crate::host::tests::replay(&followup_barriers, &mut gpu_states);

        let release = slot::<extern "system" fn(This) -> u32>(resource, 0x10);
        assert_eq!(release(resource), 0);
        drop((setup, concurrent, followup, copy));
        crate::watch::tests::free(address);

        assert_eq!(hr, S_OK);
        assert_eq!(
            observed_during_present, TARGET,
            "present must not mutate tracked state before acquiring the submission lock"
        );
        assert_eq!(concurrent_replay, Ok(()), "concurrent submission barriers");
        assert_eq!(present_replay, Ok(()), "present copy barriers");
        assert_eq!(
            state_after_present, SHADER,
            "present must not clobber the state left by a concurrent submission"
        );
        assert_eq!(followup_replay, Ok(()), "follow-up submission barriers");
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Where a queue's signal is: newer layouts have it two slots later.
    /// Another test may switch layouts while this one runs, so the queue's
    /// own table decides.
    fn signal_slot(q: This) -> usize {
        // SAFETY: one of our queues; its first field is the table.
        let table = unsafe { *q.cast::<*const *const ()>() };
        if table == NEWER_FUNCTIONS.0.as_ptr() {
            0x80
        } else {
            0x70
        }
    }

    #[test]
    fn queue_signals_complete_at_once() {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        // SAFETY: the title reads the type at +0x10.
        assert_eq!(unsafe { q.cast::<u8>().add(0x10).cast::<u32>().read() }, 0);
        let mut fence = null;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let signal = slot::<extern "system" fn(This, This, u64) -> HRESULT>(q, signal_slot(q));
        assert_eq!(signal(q, fence, 7), S_OK);
        assert_eq!(
            slot::<extern "system" fn(This) -> u64>(fence, 0x40)(fence),
            7
        );
        assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    }

    /// Grand Theft Auto VI's compute queue: `{COMPUTE, 0, pipe 1, queue 0}`.
    #[test]
    fn console_queues_are_created_as_on_pc() {
        let null = std::ptr::null();
        let desc = QueueDescX {
            kind: 2,
            flags: 0,
            engine_or_pipe: 1,
            queue: 0,
        };
        let mut q = null;
        assert_eq!(create_x(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        assert_eq!(queue(q).desc.kind, 2);
        let none = std::ptr::null();
        assert_eq!(create_x(null, none, &QUEUE_IIDS[0], &mut q), E_INVALIDARG);
    }

    #[test]
    fn newer_queues_have_their_methods_two_slots_later() {
        let (old, new) = (queue_table(false), queue_table(true));
        assert_eq!(old[10], new[10]); // submission stays at 0x50
        assert_eq!(old[14], new[16]); // signal 0x70 → 0x80
        assert_eq!(old[30], new[32]); // present 0xf0 → 0x100
        assert_eq!(new[35], execute_x as *const ()); // 0x118
    }

    /// Grand Theft Auto VI's suspend routine calls `SuspendX(0)` at 0xb8;
    /// `ResumeX` follows at 0xc0. Both succeed, in either queue layout.
    #[test]
    fn queues_suspend_and_resume() {
        let (old, new) = (queue_table(false), queue_table(true));
        assert_eq!(new[23], suspend_x as *const ()); // 0xb8
        assert_eq!(new[24], resume_x as *const ()); // 0xc0
        assert_eq!(old[21], new[23]); // 0xa8 in older queues
        assert_eq!(old[22], new[24]); // 0xb0
        for (table, at) in [(&FUNCTIONS, 0xa8), (&NEWER_FUNCTIONS, 0xb8)] {
            let object = Queue {
                object: Object::new(table),
                kind: 0,
                desc: QueueDesc {
                    kind: 0,
                    priority: 0,
                    flags: 0,
                    nodes: 0,
                },
            };
            let q: This = std::ptr::from_ref(&object).cast();
            let suspend = slot::<extern "system" fn(This, u32) -> HRESULT>(q, at);
            let resume = slot::<extern "system" fn(This) -> HRESULT>(q, at + 8);
            assert_eq!(suspend(q, 0), S_OK);
            assert_eq!(resume(q), S_OK);
        }
    }

    #[test]
    fn timestamps_and_values_are_written() {
        let mut at = [0u64; 2];
        Written::Value(0x1_0000_0002).write(at.as_mut_ptr().cast(), 4);
        assert_eq!(at[0], 2);
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        let first = at[1];
        std::thread::sleep(std::time::Duration::from_millis(2));
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        assert!(at[1] > first);
    }

    /// Cross-queue `ID3D12CommandQueue::Wait` must hold its own reference to
    /// the fence until the wait is satisfied and consumed:
    /// 1. `queue_a` holds `[wait gate, Signal(f, 1)]` and `queue_b` holds
    ///    `[Wait(f, 1), write done]`; the title releases `f` before `gate`
    ///    opens, so `Signal` drops its ref in the same `pump()` before
    ///    `FenceWait` tests `f` and unblocks `queue_b`.
    /// 2. `queue_b` holds `[Wait(f2, 1), write done2]`; polling `pump` keeps
    ///    `f2` alive until CPU `ID3D12Fence::Signal(1)` satisfies it, drops the
    ///    wait's ref exactly once, and an already-satisfied `Wait` does not
    ///    add a reference.
    /// 3. `queue_b` holds `[Wait(f3, 1), write done3]`; the title releases its
    ///    sole reference immediately and later satisfies `f3` via a GPU
    ///    bottom-of-pipe write to `f3->GetGPUVirtualAddress()`.
    #[cfg(windows)]
    #[test]
    fn cross_queue_fence_wait_keeps_fence_alive_until_satisfied() {
        type Close = extern "system" fn(This) -> HRESULT;
        type Execute = extern "system" fn(This, u32, *const This);
        type QueueFence = extern "system" fn(This, This, u64) -> HRESULT;
        type FenceSignal = extern "system" fn(This, u64) -> HRESULT;
        type FenceGpuAddr = extern "system" fn(This) -> u64;
        type Release = extern "system" fn(This) -> u32;

        if crate::host::host().is_none() {
            return;
        }
        let null = std::ptr::null();
        let alloc_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );

        let make_queue = |kind: u32| -> This {
            let desc = QueueDesc {
                kind,
                priority: 0,
                flags: 0,
                nodes: 0,
            };
            let mut q = null;
            assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
            q
        };
        let queue_a = make_queue(0);
        let queue_b = make_queue(2);
        let sig_slot_a = signal_slot(queue_a);
        let wait_slot_b = signal_slot(queue_b) + 8;

        let mut alloc = null;
        assert_eq!(
            crate::list::create_allocator(null, 0, &alloc_iid, &mut alloc),
            S_OK
        );
        let make_list = |kind: u32| -> This {
            let mut l = null;
            assert_eq!(
                crate::list::create_list(null, 0, kind, alloc, null, &list_iid, &mut l),
                S_OK
            );
            l
        };
        let make_fence = || -> This {
            let mut f = null;
            assert_eq!(
                crate::fence::create(null, 0, 0, &crate::fence::FENCE_IIDS[0], &mut f),
                S_OK
            );
            f
        };

        let buf = crate::resource::test_buffer(256);
        let (ptr, _) = crate::resource::buffer_bytes(buf).unwrap();
        let words = ptr.cast::<u64>();
        // SAFETY: `buf` is a 256-byte committed buffer (32 u64 words).
        let (gate, done1, done2, done3) =
            unsafe { (words, words.add(1), words.add(2), words.add(3)) };
        // SAFETY: zero-initialize our 4 distinct u64 slots.
        unsafe {
            gate.write_volatile(0);
            done1.write_volatile(0);
            done2.write_volatile(0);
            done3.write_volatile(0);
        }
        const EQUAL: u32 = 3;

        // Case 1: cross-queue Signal + Wait where the title releases its own
        // reference before `queue_a` unblocks.
        let list_a1 = make_list(0);
        crate::list::wait64(list_a1, gate as u64, EQUAL, 1, 0);
        assert_eq!(slot::<Close>(list_a1, 0x48)(list_a1), S_OK);
        slot::<Execute>(queue_a, 0x50)(queue_a, 1, &list_a1);

        let f1 = make_fence();
        assert_eq!(object::refs(f1), 1);
        assert_eq!(
            slot::<QueueFence>(queue_a, sig_slot_a)(queue_a, f1, 1),
            S_OK
        );
        assert_eq!(object::refs(f1), 2, "queued Signal holds one reference");
        assert_eq!(
            slot::<QueueFence>(queue_b, wait_slot_b)(queue_b, f1, 1),
            S_OK
        );
        assert_eq!(
            object::refs(f1),
            3,
            "queued FenceWait must hold its own reference"
        );

        let list_b1 = make_list(2);
        crate::list::write64(list_b1, done1 as u64, 0x5A5A_A5A5, 0, 0);
        assert_eq!(slot::<Close>(list_b1, 0x48)(list_b1), S_OK);
        slot::<Execute>(queue_b, 0x50)(queue_b, 1, &list_b1);
        // SAFETY: `done1` is inside `buf`.
        assert_eq!(unsafe { done1.read_volatile() }, 0);

        // Legal in D3D12: title drops its reference while both queues still use `f1`.
        assert_eq!(
            slot::<Release>(f1, 0x10)(f1),
            2,
            "both Signal and FenceWait must keep f1 alive"
        );

        // Unblock `queue_a`: Signal runs and drops its ref, then FenceWait in the
        // same pump sees value 1, drops the last ref, and runs `list_b1`.
        // SAFETY: `gate` is inside `buf`.
        unsafe { gate.write_volatile(1) };
        pump_held(Why::Fence);
        // SAFETY: `done1` is inside `buf`.
        assert_eq!(unsafe { done1.read_volatile() }, 0x5A5A_A5A5);
        assert!(!held().holds_work(queue_a as usize));
        assert!(!held().holds_work(queue_b as usize));

        // Case 2: observer-ref balance across polling, CPU Signal, already-satisfied
        // Wait, and null fence argument.
        let f2 = make_fence();
        assert_eq!(
            slot::<QueueFence>(queue_b, wait_slot_b)(queue_b, f2, 1),
            S_OK
        );
        assert_eq!(object::refs(f2), 2);

        let list_b2 = make_list(2);
        crate::list::write64(list_b2, done2 as u64, 0x1234_5678, 0, 0);
        assert_eq!(slot::<Close>(list_b2, 0x48)(list_b2), S_OK);
        slot::<Execute>(queue_b, 0x50)(queue_b, 1, &list_b2);

        pump_held(Why::Timer);
        pump_held(Why::Timer);
        // SAFETY: `done2` is inside `buf`.
        assert_eq!(unsafe { done2.read_volatile() }, 0);
        assert_eq!(object::refs(f2), 2, "repeated polls must keep f2 alive");

        assert_eq!(slot::<FenceSignal>(f2, 0x50)(f2, 1), S_OK);
        // SAFETY: `done2` is inside `buf`.
        assert_eq!(unsafe { done2.read_volatile() }, 0x1234_5678);
        assert_eq!(
            object::refs(f2),
            1,
            "satisfied FenceWait must release its reference exactly once"
        );

        // Already-satisfied Wait and null fence must not retain a reference.
        assert_eq!(
            slot::<QueueFence>(queue_b, wait_slot_b)(queue_b, f2, 1),
            S_OK
        );
        assert_eq!(object::refs(f2), 1);
        assert_eq!(
            slot::<QueueFence>(queue_b, wait_slot_b)(queue_b, null, 1),
            E_INVALIDARG
        );
        assert_eq!(slot::<Release>(f2, 0x10)(f2), 0);

        // Case 3: title releases its sole reference immediately after `queue_b->Wait`,
        // then satisfies the fence via a GPU bottom-of-pipe write to its GPU address.
        let f3 = make_fence();
        let f3_va = slot::<FenceGpuAddr>(f3, 0x60)(f3);
        assert_eq!(
            slot::<QueueFence>(queue_b, wait_slot_b)(queue_b, f3, 1),
            S_OK
        );
        let list_b3 = make_list(2);
        crate::list::write64(list_b3, done3 as u64, 0xBEEF_CAFE, 0, 0);
        assert_eq!(slot::<Close>(list_b3, 0x48)(list_b3), S_OK);
        slot::<Execute>(queue_b, 0x50)(queue_b, 1, &list_b3);

        assert_eq!(
            slot::<Release>(f3, 0x10)(f3),
            1,
            "pending FenceWait must keep f3 alive after the title releases it"
        );
        pump_held(Why::Timer);
        // SAFETY: `done3` is inside `buf`.
        assert_eq!(unsafe { done3.read_volatile() }, 0);

        let list_a3 = make_list(0);
        crate::list::write64(list_a3, f3_va, 1, 0, 0);
        assert_eq!(slot::<Close>(list_a3, 0x48)(list_a3), S_OK);
        slot::<Execute>(queue_a, 0x50)(queue_a, 1, &list_a3);
        // SAFETY: `done3` is inside `buf`.
        assert_eq!(unsafe { done3.read_volatile() }, 0xBEEF_CAFE);
        assert!(!held().holds_work(queue_a as usize));
        assert!(!held().holds_work(queue_b as usize));

        for obj in [
            list_a1, list_b1, list_b2, list_b3, list_a3, alloc, queue_a, queue_b, buf,
        ] {
            assert_eq!(slot::<Release>(obj, 0x10)(obj), 0);
        }
    }
}
