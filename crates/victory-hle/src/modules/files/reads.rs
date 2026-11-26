//! Kernel file reads into title memory, synchronous and overlapped.
//!
//! The GPU tracker marks 64 KiB pages of title buffers and physical-page views
//! `PAGE_READONLY` so CPU stores fault into its vectored handler. Kernel writes
//! (`ReadFile`, `ReadFileEx`, `ReadFileScatter`) do not fault into user-mode
//! handlers: they fail with `ERROR_NOACCESS` unless the destination pages are
//! opened first with [`victory_gpu::kernel_will_write`].
//!
//! For synchronous reads the `KernelWrite` lease lives on the call stack across
//! the host call. For overlapped reads (`ERROR_IO_PENDING`, or `ReadFileEx`),
//! dropping the lease when the initiating call returns would let a concurrent
//! GPU refresh re-guard the destination pages while the driver's DMA/read is
//! still in flight and miss the final dirty mark when the bytes land. Those
//! leases stay alive until completion is observed (`GetOverlappedResult`/`Ex`,
//! `GetQueuedCompletionStatus`/`Ex`, `ReadFileEx` completion routines, waits on
//! the completion event or file handle, `OVERLAPPED` reuse, or `CloseHandle`).

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{GetLastError, SetLastError};

use super::Handle;

const WATCH_PAGE: usize = 64 << 10;
const ERROR_IO_PENDING: u32 = 997;
const STATUS_PENDING: usize = 0x0000_0103;
const WAIT_OBJECT_0: u32 = 0;
const MAXIMUM_WAIT_OBJECTS: u32 = 64;

#[repr(C)]
pub struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: Handle,
}

const _: () = assert!(size_of::<Overlapped>() == 32);

#[repr(C)]
pub struct OverlappedEntry {
    key: usize,
    overlapped: *mut Overlapped,
    internal: usize,
    bytes: u32,
}

const _: () = assert!(size_of::<OverlappedEntry>() == 32);

pub type CompletionRoutine = Option<unsafe extern "system" fn(u32, u32, *mut Overlapped)>;

struct Pending {
    id: u64,
    file: usize,
    overlapped: usize,
    /// Event (low-order IOCP-skip bit stripped) or file handle that signals
    /// completion for `WaitFor*`; 0 before submission returns, after
    /// `CloseHandle`, or when `ReadFileEx` uses an APC completion routine
    /// (`ReadFileEx` leaves `OVERLAPPED::hEvent` to the application).
    wait_handle: usize,
    started: bool,
    routine: CompletionRoutine,
    writes: Vec<victory_gpu::KernelWrite>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Number of entries in [`PENDING`] that still hold `KernelWrite` leases, for
/// lock-free fast paths on wait and handle calls.
static PENDING_COUNT: AtomicUsize = AtomicUsize::new(0);
static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());

fn with_pending<R>(f: impl FnOnce(&mut Vec<Pending>) -> R) -> R {
    let mut list = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let out = f(&mut list);
    let active = list.iter().filter(|p| !p.writes.is_empty()).count();
    PENDING_COUNT.store(active, Ordering::Release);
    out
}

/// Runs `f` without disturbing the calling thread's Win32 `LastError` (dropping
/// a [`victory_gpu::KernelWrite`] calls `VirtualProtect`).
pub(super) fn keep_last_error<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: reads and restores this thread's Win32 last-error slot.
    unsafe {
        let error = GetLastError();
        let out = f();
        SetLastError(error);
        out
    }
}

pub(super) fn will_write(at: usize, len: usize) -> Option<victory_gpu::KernelWrite> {
    (at != 0 && len != 0).then(|| victory_gpu::kernel_will_write(at, len))
}

fn os_page_size() -> usize {
    #[repr(C)]
    struct SystemInfo {
        arch_and_reserved: u32,
        page_size: u32,
        _rest: [usize; 5],
    }
    const _: () = assert!(size_of::<SystemInfo>() == 48);

    static PAGE_SIZE: OnceLock<usize> = OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        let mut info = SystemInfo {
            arch_and_reserved: 0,
            page_size: 0,
            _rest: [0; 5],
        };
        host!("kernel32.dll", "GetSystemInfo": extern "system" fn(*mut SystemInfo))(&mut info);
        usize::try_from(info.page_size)
            .ok()
            .filter(|&p| p != 0)
            .unwrap_or(4096)
    })
}

/// Opens every 64 KiB tracked page touched by a `ReadFileScatter` segment
/// list, coalescing adjacent or repeated 4 KiB segments into minimal runs.
fn scatter_writes(segments: *const u64, len: u32) -> Vec<victory_gpu::KernelWrite> {
    if segments.is_null() || len == 0 {
        return Vec::new();
    }
    let step = os_page_size();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let (mut remaining, mut index) = (len as usize, 0usize);
    while remaining > 0 {
        // SAFETY: `ReadFileScatter` takes `ceil(len / page_size)` 8-byte
        // `FILE_SEGMENT_ELEMENT` entries terminated by a null element.
        let at = unsafe { segments.add(index).read_unaligned() } as usize;
        if at == 0 {
            break;
        }
        let chunk = remaining.min(step);
        let (first, last) = (
            at / WATCH_PAGE,
            at.saturating_add(chunk).div_ceil(WATCH_PAGE),
        );
        match spans.last_mut() {
            Some((prev_first, prev_last)) if first <= *prev_last && *prev_first <= last => {
                *prev_first = (*prev_first).min(first);
                *prev_last = (*prev_last).max(last);
            }
            _ => spans.push((first, last)),
        }
        remaining -= chunk;
        index += 1;
    }
    if spans.len() > 1 {
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for (first, last) in spans {
            match merged.last_mut() {
                Some((_, end)) if first <= *end => *end = (*end).max(last),
                _ => merged.push((first, last)),
            }
        }
        spans = merged;
    }
    spans
        .into_iter()
        .filter(|(first, last)| first < last)
        .map(|(first, last)| {
            victory_gpu::kernel_will_write(
                first.saturating_mul(WATCH_PAGE),
                (last - first).saturating_mul(WATCH_PAGE),
            )
        })
        .collect()
}

/// Whether the kernel has finished updating `ov` (`HasOverlappedIoCompleted`).
///
/// # Safety
/// `ov` must point to a live `OVERLAPPED` structure.
unsafe fn overlapped_done(ov: usize) -> bool {
    let ptr = ov as *const Overlapped;
    // SAFETY: caller guarantees `ov` is live; `read_volatile` on a raw field
    // pointer avoids forming `&Overlapped` while the kernel may mutate it.
    unsafe { (&raw const (*ptr).internal).read_volatile() != STATUS_PENDING }
}

/// Releases the `KernelWrite` leases of `list[index]`, returning `true` when
/// the entry was removed (`swap_remove`) or `false` when kept in place so a
/// queued `ReadFileEx` APC can still forward to its completion routine.
fn take_entry_writes(
    list: &mut Vec<Pending>,
    index: usize,
    out: &mut Vec<victory_gpu::KernelWrite>,
) -> bool {
    if list[index].routine.is_some() {
        out.extend(std::mem::take(&mut list[index].writes));
        false
    } else {
        out.extend(list.swap_remove(index).writes);
        true
    }
}

fn release_stale_overlapped(ov_addr: usize) {
    if ov_addr == 0 || PENDING_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let mut stale = Vec::new();
    with_pending(|list| {
        let mut i = 0;
        while i < list.len() {
            if list[i].started && list[i].overlapped == ov_addr {
                stale.extend(list.swap_remove(i).writes);
            } else {
                i += 1;
            }
        }
    });
    drop(stale);
}

fn submit_overlapped(
    file: Handle,
    overlapped: *mut Overlapped,
    writes: Vec<victory_gpu::KernelWrite>,
    routine: CompletionRoutine,
    is_ex: bool,
    call: impl FnOnce() -> i32,
) -> i32 {
    let ov_addr = overlapped as usize;
    if writes.is_empty() {
        release_stale_overlapped(ov_addr);
        return call();
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut stale = Vec::new();
    with_pending(|list| {
        // Reusing an `OVERLAPPED` whose prior read completed without an
        // explicit harvest call releases that prior lease now, after the new
        // lease already keeps any shared pages in `Guards::busy`.
        let mut i = 0;
        while i < list.len() {
            if list[i].started && list[i].overlapped == ov_addr {
                stale.extend(list.swap_remove(i).writes);
            } else {
                i += 1;
            }
        }
        list.push(Pending {
            id,
            file: file as usize,
            overlapped: ov_addr,
            wait_handle: 0,
            started: false,
            routine,
            writes,
        });
    });
    drop(stale);

    let ok = call();
    // SAFETY: captures the host call's Win32 error before any cleanup runs.
    let error = unsafe { GetLastError() };
    let queued = if is_ex {
        ok != 0
    } else {
        ok == 0 && error == ERROR_IO_PENDING
    };

    let mut finished = Vec::new();
    with_pending(|list| {
        let Some(pos) = list.iter().position(|p| p.id == id) else {
            return; // Already harvested on another thread (e.g. IOCP).
        };
        if !queued {
            finished.extend(list.swap_remove(pos).writes);
            return;
        }
        // SAFETY: the kernel accepted `overlapped` for this operation, so it
        // points to valid writable memory; volatile raw reads avoid `&Overlapped`.
        let (internal, raw_event) = unsafe {
            (
                (&raw const (*overlapped).internal).read_volatile(),
                (&raw const (*overlapped).event).read_volatile() as usize,
            )
        };
        if internal != STATUS_PENDING && routine.is_none() {
            finished.extend(list.swap_remove(pos).writes);
            return;
        }
        let entry = &mut list[pos];
        entry.wait_handle = if is_ex {
            if routine.is_some() { 0 } else { file as usize }
        } else {
            let ev = raw_event & !1;
            if ev != 0 { ev } else { file as usize }
        };
        entry.started = true;
    });
    drop(finished);
    // SAFETY: restores the exact Win32 error set by `call`.
    unsafe { SetLastError(error) };
    ok
}

unsafe extern "system" fn read_ex_done(error: u32, bytes: u32, overlapped: *mut Overlapped) {
    let ov = overlapped as usize;
    let (mut finished, mut routine) = (Vec::new(), None);
    with_pending(|list| {
        if let Some(pos) = list
            .iter()
            .position(|p| p.overlapped == ov && p.routine.is_some())
        {
            let entry = list.swap_remove(pos);
            routine = entry.routine;
            finished = entry.writes;
        }
    });
    keep_last_error(|| drop(finished));
    if let Some(cb) = routine {
        // SAFETY: forwards the kernel APC arguments to the title's callback
        // after releasing the `KernelWrite` lease so dirty tracking is current.
        unsafe { cb(error, bytes, overlapped) };
    }
}

pub extern "system" fn ReadFile(
    file: Handle,
    buffer: *mut u8,
    len: u32,
    read: *mut u32,
    overlapped: *mut c_void,
) -> i32 {
    let call = || {
        host!("kernel32.dll", "ReadFile": extern "system" fn(Handle, *mut u8, u32, *mut u32, *mut c_void) -> i32)(
            file, buffer, len, read, overlapped,
        )
    };
    if overlapped.is_null() {
        let writing = will_write(buffer as usize, len as usize);
        let ok = call();
        if writing.is_some() {
            keep_last_error(|| drop(writing));
        }
        return ok;
    }
    let writes: Vec<_> = will_write(buffer as usize, len as usize)
        .into_iter()
        .collect();
    submit_overlapped(file, overlapped.cast(), writes, None, false, call)
}

pub extern "system" fn ReadFileEx(
    file: Handle,
    buffer: *mut u8,
    len: u32,
    overlapped: *mut Overlapped,
    routine: CompletionRoutine,
) -> i32 {
    let host_read_ex = host!(
        "kernel32.dll",
        "ReadFileEx": extern "system" fn(Handle, *mut u8, u32, *mut Overlapped, CompletionRoutine) -> i32
    );
    if overlapped.is_null() {
        return host_read_ex(file, buffer, len, overlapped, routine);
    }
    let writes: Vec<_> = will_write(buffer as usize, len as usize)
        .into_iter()
        .collect();
    if writes.is_empty() {
        return host_read_ex(file, buffer, len, overlapped, routine);
    }
    let wrapped: CompletionRoutine = routine.map(|_| read_ex_done as _);
    submit_overlapped(file, overlapped, writes, routine, true, || {
        host_read_ex(file, buffer, len, overlapped, wrapped)
    })
}

pub extern "system" fn ReadFileScatter(
    file: Handle,
    segments: *const u64,
    len: u32,
    reserved: *mut u32,
    overlapped: *mut Overlapped,
) -> i32 {
    let host_scatter = host!(
        "kernel32.dll",
        "ReadFileScatter": extern "system" fn(Handle, *const u64, u32, *mut u32, *mut Overlapped) -> i32
    );
    if overlapped.is_null() {
        return host_scatter(file, segments, len, reserved, overlapped);
    }
    let writes = scatter_writes(segments, len);
    submit_overlapped(file, overlapped, writes, None, false, || {
        host_scatter(file, segments, len, reserved, overlapped)
    })
}

fn harvest_overlapped_result(file: Handle, overlapped: *mut Overlapped) {
    if overlapped.is_null() || PENDING_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let (f, ov) = (file as usize, overlapped as usize);
    keep_last_error(|| {
        let mut finished = Vec::new();
        with_pending(|list| {
            let mut i = 0;
            while i < list.len() {
                // SAFETY: caller just passed `overlapped` to `GetOverlappedResult*`.
                if list[i].file == f && list[i].overlapped == ov && unsafe { overlapped_done(ov) } {
                    if !take_entry_writes(list, i, &mut finished) {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
        });
        drop(finished);
    });
}

pub extern "system" fn GetOverlappedResult(
    file: Handle,
    overlapped: *mut Overlapped,
    transferred: *mut u32,
    wait: i32,
) -> i32 {
    let ok = host!(
        "kernel32.dll",
        "GetOverlappedResult": extern "system" fn(Handle, *mut Overlapped, *mut u32, i32) -> i32
    )(file, overlapped, transferred, wait);
    harvest_overlapped_result(file, overlapped);
    ok
}

pub extern "system" fn GetOverlappedResultEx(
    file: Handle,
    overlapped: *mut Overlapped,
    transferred: *mut u32,
    milliseconds: u32,
    alertable: i32,
) -> i32 {
    let ok = host!(
        "kernel32.dll",
        "GetOverlappedResultEx": extern "system" fn(Handle, *mut Overlapped, *mut u32, u32, i32) -> i32
    )(file, overlapped, transferred, milliseconds, alertable);
    harvest_overlapped_result(file, overlapped);
    ok
}

fn harvest_iocp(overlappeds: &[usize]) {
    if overlappeds.is_empty() || PENDING_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    keep_last_error(|| {
        let mut finished = Vec::new();
        with_pending(|list| {
            let mut i = 0;
            while i < list.len() {
                let ov = list[i].overlapped;
                // SAFETY: `ov` was just dequeued from an I/O completion port.
                if overlappeds.contains(&ov) && unsafe { overlapped_done(ov) } {
                    if !take_entry_writes(list, i, &mut finished) {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
        });
        drop(finished);
    });
}

pub extern "system" fn GetQueuedCompletionStatus(
    port: Handle,
    bytes: *mut u32,
    key: *mut usize,
    overlapped: *mut *mut Overlapped,
    milliseconds: u32,
) -> i32 {
    let ok = host!(
        "kernel32.dll",
        "GetQueuedCompletionStatus": extern "system" fn(Handle, *mut u32, *mut usize, *mut *mut Overlapped, u32) -> i32
    )(port, bytes, key, overlapped, milliseconds);
    if PENDING_COUNT.load(Ordering::Acquire) != 0 && !overlapped.is_null() {
        // SAFETY: `overlapped` is the caller's out-pointer populated by the host.
        let ov = unsafe { *overlapped } as usize;
        if ov != 0 {
            harvest_iocp(&[ov]);
        }
    }
    ok
}

pub extern "system" fn GetQueuedCompletionStatusEx(
    port: Handle,
    entries: *mut OverlappedEntry,
    count: u32,
    removed: *mut u32,
    milliseconds: u32,
    alertable: i32,
) -> i32 {
    let ok = host!(
        "kernel32.dll",
        "GetQueuedCompletionStatusEx": extern "system" fn(Handle, *mut OverlappedEntry, u32, *mut u32, u32, i32) -> i32
    )(port, entries, count, removed, milliseconds, alertable);
    if ok != 0
        && PENDING_COUNT.load(Ordering::Acquire) != 0
        && !entries.is_null()
        && !removed.is_null()
    {
        // SAFETY: the host succeeded and wrote `*removed` entries into `entries`.
        let n = (unsafe { *removed } as usize).min(count as usize);
        let mut ovs = Vec::with_capacity(n);
        for i in 0..n {
            // SAFETY: `i < n` within the caller's `entries` buffer.
            let ov = unsafe { (*entries.add(i)).overlapped } as usize;
            if ov != 0 {
                ovs.push(ov);
            }
        }
        harvest_iocp(&ovs);
    }
    ok
}

fn harvest_waited_handles(handles: &[usize]) {
    if handles.is_empty() || PENDING_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    keep_last_error(|| {
        let mut finished = Vec::new();
        with_pending(|list| {
            let mut i = 0;
            while i < list.len() {
                let p = &list[i];
                let done = p.started
                    && p.wait_handle != 0
                    && handles.contains(&p.wait_handle)
                    // SAFETY: caller is actively waiting on `p.wait_handle` for
                    // this started operation, so `p.overlapped` is live.
                    && unsafe { overlapped_done(p.overlapped) };
                if done {
                    if !take_entry_writes(list, i, &mut finished) {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
        });
        drop(finished);
    });
}

pub extern "system" fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32 {
    let rc = host!(
        "kernel32.dll",
        "WaitForSingleObject": extern "system" fn(Handle, u32) -> u32
    )(handle, milliseconds);
    if rc == WAIT_OBJECT_0 && PENDING_COUNT.load(Ordering::Acquire) != 0 {
        harvest_waited_handles(&[handle as usize]);
    }
    rc
}

pub extern "system" fn WaitForSingleObjectEx(
    handle: Handle,
    milliseconds: u32,
    alertable: i32,
) -> u32 {
    let rc = host!(
        "kernel32.dll",
        "WaitForSingleObjectEx": extern "system" fn(Handle, u32, i32) -> u32
    )(handle, milliseconds, alertable);
    if rc == WAIT_OBJECT_0 && PENDING_COUNT.load(Ordering::Acquire) != 0 {
        harvest_waited_handles(&[handle as usize]);
    }
    rc
}

pub extern "system" fn WaitForMultipleObjects(
    count: u32,
    handles: *const Handle,
    wait_all: i32,
    milliseconds: u32,
) -> u32 {
    let rc = host!(
        "kernel32.dll",
        "WaitForMultipleObjects": extern "system" fn(u32, *const Handle, i32, u32) -> u32
    )(count, handles, wait_all, milliseconds);
    if PENDING_COUNT.load(Ordering::Acquire) != 0
        && count > 0
        && count <= MAXIMUM_WAIT_OBJECTS
        && rc < WAIT_OBJECT_0 + count
        && !handles.is_null()
    {
        // SAFETY: `handles` has `count` valid handle slots for this wait call.
        let slice = unsafe { std::slice::from_raw_parts(handles.cast::<usize>(), count as usize) };
        harvest_waited_handles(slice);
    }
    rc
}

pub extern "system" fn WaitForMultipleObjectsEx(
    count: u32,
    handles: *const Handle,
    wait_all: i32,
    milliseconds: u32,
    alertable: i32,
) -> u32 {
    let rc = host!(
        "kernel32.dll",
        "WaitForMultipleObjectsEx": extern "system" fn(u32, *const Handle, i32, u32, i32) -> u32
    )(count, handles, wait_all, milliseconds, alertable);
    if PENDING_COUNT.load(Ordering::Acquire) != 0
        && count > 0
        && count <= MAXIMUM_WAIT_OBJECTS
        && rc < WAIT_OBJECT_0 + count
        && !handles.is_null()
    {
        // SAFETY: `handles` has `count` valid handle slots for this wait call.
        let slice = unsafe { std::slice::from_raw_parts(handles.cast::<usize>(), count as usize) };
        harvest_waited_handles(slice);
    }
    rc
}

fn harvest_cancelled(file: Handle, overlapped: *const Overlapped) {
    if PENDING_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let (f, ov_filter) = (file as usize, overlapped as usize);
    keep_last_error(|| {
        let mut finished = Vec::new();
        with_pending(|list| {
            let mut i = 0;
            while i < list.len() {
                let p = &list[i];
                // `CancelIo`/`CancelIoEx` only request cancellation; any IRP
                // still at `STATUS_PENDING` keeps its lease until drained.
                let done = p.started
                    && p.file == f
                    && (ov_filter == 0 || p.overlapped == ov_filter)
                    // SAFETY: `p.overlapped` belongs to an active IRP on `file`.
                    && unsafe { overlapped_done(p.overlapped) };
                if done {
                    if !take_entry_writes(list, i, &mut finished) {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
        });
        drop(finished);
    });
}

pub extern "system" fn CancelIo(file: Handle) -> i32 {
    let ok = host!("kernel32.dll", "CancelIo": extern "system" fn(Handle) -> i32)(file);
    harvest_cancelled(file, std::ptr::null());
    ok
}

pub extern "system" fn CancelIoEx(file: Handle, overlapped: *const Overlapped) -> i32 {
    let ok = host!(
        "kernel32.dll",
        "CancelIoEx": extern "system" fn(Handle, *const Overlapped) -> i32
    )(file, overlapped);
    harvest_cancelled(file, overlapped);
    ok
}

pub extern "system" fn CloseHandle(handle: Handle) -> i32 {
    let ok = host!("kernel32.dll", "CloseHandle": extern "system" fn(Handle) -> i32)(handle);
    if ok != 0 && PENDING_COUNT.load(Ordering::Acquire) != 0 {
        let h = handle as usize;
        keep_last_error(|| {
            let mut finished = Vec::new();
            with_pending(|list| {
                let mut i = 0;
                while i < list.len() {
                    if list[i].wait_handle == h {
                        list[i].wait_handle = 0;
                    }
                    if list[i].file == h {
                        if !take_entry_writes(list, i, &mut finished) {
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
            });
            drop(finished);
        });
    }
    ok
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use windows_sys::Win32::Foundation::{
        ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_ALWAYS, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
        MapViewOfFile, PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, SleepEx};

    use super::*;
    use crate::{HleResolver, HostPolicy};
    use victory_loader::{ImportResolver, Provenance, Resolution};
    use victory_pe::ImportSymbol;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn protection(at: usize) -> u32 {
        // SAFETY: plain query struct populated by `VirtualQuery`.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: valid out pointer and size.
        unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        info.Protect
    }

    struct MappedView(usize);

    impl MappedView {
        fn new(pages: usize) -> Self {
            let len = pages * WATCH_PAGE;
            // SAFETY: creates a pagefile-backed section mapped read-write and
            // closes the section handle once the view holds its reference.
            let view = unsafe {
                let section = CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    0,
                    len as u32,
                    std::ptr::null(),
                );
                assert!(!section.is_null());
                let addr = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
                windows_sys::Win32::Foundation::CloseHandle(section);
                addr
            };
            assert!(view != 0 && view.is_multiple_of(WATCH_PAGE));
            Self(view)
        }
    }

    impl Drop for MappedView {
        fn drop(&mut self) {
            // SAFETY: unmaps the view created in `MappedView::new`.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.0 as *mut _,
                });
            }
        }
    }

    /// Drives `victory_gpu::descriptor::sync_heaps` (`Tracked::take`) on a
    /// placed shader-visible sampler heap over `view` so tests exercise real
    /// `watch::Guards` read-only page protection without private GPU hooks.
    struct GuardRig {
        queue: *const victory_gpu::object::Object,
        allocator: *const victory_gpu::object::Object,
        list: *const victory_gpu::object::Object,
        heap: *const victory_gpu::object::Object,
    }

    /// Calls a COM vtable slot at byte offset `offset`.
    fn slot<F: Copy>(object: *const victory_gpu::object::Object, offset: usize) -> F {
        // SAFETY: `object` is a live COM-style `Object` whose first word points
        // to its vtable, and `offset` names a method of signature `F`.
        unsafe {
            let vtable = object.cast::<*const usize>().read();
            vtable.byte_add(offset).cast::<F>().read()
        }
    }

    impl GuardRig {
        fn attach(view: usize, pages: usize) -> Option<Self> {
            use victory_gpu::object::{Iid, Object, S_OK, guid};

            #[repr(C)]
            struct QueueDesc {
                kind: u32,
                priority: i32,
                flags: u32,
                nodes: u32,
            }
            #[repr(C)]
            struct HeapDesc {
                kind: u32,
                count: u32,
                flags: u32,
                nodes: u32,
            }
            #[repr(C)]
            struct Placement {
                address: u64,
                size: u64,
            }
            #[repr(C)]
            struct ExtendedHeap(HeapDesc, Placement);

            let version = 3u32 << 16;
            let mut dev: *const Object = std::ptr::null();
            if victory_gpu::device::create(
                std::ptr::null(),
                &version,
                &victory_gpu::device::IID,
                &mut dev,
            ) != S_OK
            {
                return None;
            }
            let q_iid: Iid = guid(
                0x0ec870a6,
                0x5d7e,
                0x4c22,
                [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
            );
            let a_iid: Iid = guid(
                0x6102dee4,
                0xaf59,
                0x4b09,
                [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
            );
            let l_iid: Iid = guid(
                0x5b160d0f,
                0xac1b,
                0x4185,
                [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
            );
            let h_iid: Iid = guid(
                0x8efb471d,
                0x616c,
                0x4f49,
                [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
            );

            let mut queue: *const Object = std::ptr::null();
            let q_desc = QueueDesc {
                kind: 0,
                priority: 0,
                flags: 0,
                nodes: 0,
            };
            assert_eq!(
                slot::<
                    extern "system" fn(
                        *const Object,
                        *const QueueDesc,
                        *const Iid,
                        *mut *const Object,
                    ) -> i32,
                >(dev, 0x40)(dev, &q_desc, &q_iid, &mut queue),
                S_OK
            );

            let mut allocator: *const Object = std::ptr::null();
            assert_eq!(
                slot::<extern "system" fn(*const Object, u32, *const Iid, *mut *const Object) -> i32>(
                    dev, 0x48
                )(dev, 0, &a_iid, &mut allocator),
                S_OK
            );

            let mut list: *const Object = std::ptr::null();
            assert_eq!(
                slot::<
                    extern "system" fn(
                        *const Object,
                        u32,
                        u32,
                        *const Object,
                        *const Object,
                        *const Iid,
                        *mut *const Object,
                    ) -> i32,
                >(dev, 0x60)(
                    dev, 0, 0, allocator, std::ptr::null(), &l_iid, &mut list
                ),
                S_OK
            );
            assert_eq!(
                slot::<extern "system" fn(*const Object) -> i32>(list, 0x48)(list),
                S_OK
            );

            let size = (pages * WATCH_PAGE) as u64;
            let ext = ExtendedHeap(
                HeapDesc {
                    kind: 1, // SAMPLERS (32 bytes per descriptor; max 2048 per shader-visible sampler heap)
                    count: (size / 32) as u32,
                    flags: 1, // SHADER_VISIBLE
                    nodes: 0,
                },
                Placement {
                    address: view as u64,
                    size,
                },
            );
            let mut heap: *const Object = std::ptr::null();
            assert_eq!(
                slot::<
                    extern "system" fn(
                        *const Object,
                        *const HeapDesc,
                        *const Iid,
                        *mut *const Object,
                    ) -> i32,
                >(dev, 0x70)(dev, &ext.0, &h_iid, &mut heap),
                S_OK
            );

            let rig = Self {
                queue,
                allocator,
                list,
                heap,
            };
            rig.refresh();
            if protection(view) != PAGE_READONLY {
                return None;
            }
            Some(rig)
        }

        fn refresh(&self) {
            use victory_gpu::object::{Object, S_OK};
            assert_eq!(
                slot::<extern "system" fn(*const Object, *const Object, *const Object) -> i32>(
                    self.list, 0x50
                )(self.list, self.allocator, std::ptr::null()),
                S_OK
            );
            slot::<extern "system" fn(*const Object, u32, *const *const Object)>(self.list, 0x100)(
                self.list, 1, &self.heap,
            );
            assert_eq!(
                slot::<extern "system" fn(*const Object) -> i32>(self.list, 0x48)(self.list),
                S_OK
            );
            slot::<extern "system" fn(*const Object, u32, *const *const Object)>(self.queue, 0x50)(
                self.queue, 1, &self.list,
            );
        }
    }

    impl Drop for GuardRig {
        fn drop(&mut self) {
            use victory_gpu::object::Object;
            for obj in [self.list, self.allocator, self.queue, self.heap] {
                slot::<extern "system" fn(*const Object) -> u32>(obj, 0x10)(obj);
            }
        }
    }

    /// Creates a named pipe pair where the client read handle has
    /// `FILE_FLAG_OVERLAPPED`, so `ReadFile` / `ReadFileEx` deterministically
    /// return `ERROR_IO_PENDING` until bytes are written to the server end.
    fn overlapped_pipe(tag: &str) -> (Handle, Handle) {
        static SEQ: AtomicU32 = AtomicU32::new(1);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let name = w(&format!(r"\\.\pipe\victory_hle_{tag}_{pid}_{id}"));
        const PIPE_ACCESS_OUTBOUND: u32 = 0x0000_0002;
        const PIPE_TYPE_BYTE: u32 = 0;
        let server = host!(
            "kernel32.dll",
            "CreateNamedPipeW": extern "system" fn(*const u16, u32, u32, u32, u32, u32, u32, *const c_void) -> Handle
        )(
            name.as_ptr(),
            PIPE_ACCESS_OUTBOUND,
            PIPE_TYPE_BYTE,
            1,
            64 << 10,
            64 << 10,
            0,
            std::ptr::null(),
        );
        assert_ne!(server, INVALID_HANDLE_VALUE);
        let client = super::super::CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        );
        assert_ne!(client, INVALID_HANDLE_VALUE);
        let connected = host!(
            "kernel32.dll",
            "ConnectNamedPipe": extern "system" fn(Handle, *mut c_void) -> i32
        )(server, std::ptr::null_mut());
        if connected == 0 {
            // SAFETY: reads `GetLastError` after `ConnectNamedPipe`.
            assert_eq!(unsafe { GetLastError() }, 535); // ERROR_PIPE_CONNECTED
        }
        (server, client)
    }

    fn write_all(handle: Handle, bytes: &[u8]) {
        let mut written = 0u32;
        let ok = host!(
            "kernel32.dll",
            "WriteFile": extern "system" fn(Handle, *const u8, u32, *mut u32, *mut c_void) -> i32
        )(
            handle,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        );
        assert!(ok != 0 && written as usize == bytes.len());
    }

    fn zero_overlapped(event: Handle) -> Overlapped {
        Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event,
        }
    }

    #[test]
    fn io_handle_and_wait_api_sets_resolve_to_hle() {
        let r = HleResolver::new(HostPolicy::default());
        for (dll, sym) in [
            ("api-ms-win-core-file-l1-1-0.dll", "ReadFile"),
            ("api-ms-win-core-file-l1-1-0.dll", "ReadFileEx"),
            ("api-ms-win-core-file-l1-1-0.dll", "ReadFileScatter"),
            ("api-ms-win-core-io-l1-1-0.dll", "GetOverlappedResult"),
            ("api-ms-win-core-io-l1-1-1.dll", "GetOverlappedResultEx"),
            ("api-ms-win-core-io-l1-1-0.dll", "GetQueuedCompletionStatus"),
            (
                "api-ms-win-core-io-l1-1-1.dll",
                "GetQueuedCompletionStatusEx",
            ),
            ("api-ms-win-core-io-l1-1-0.dll", "CancelIo"),
            ("api-ms-win-core-io-l1-1-0.dll", "CancelIoEx"),
            ("api-ms-win-core-handle-l1-1-0.dll", "CloseHandle"),
            ("api-ms-win-core-synch-l1-1-0.dll", "WaitForSingleObject"),
            ("api-ms-win-core-synch-l1-1-0.dll", "WaitForSingleObjectEx"),
            ("api-ms-win-core-synch-l1-1-0.dll", "WaitForMultipleObjects"),
            (
                "api-ms-win-core-synch-l1-1-0.dll",
                "WaitForMultipleObjectsEx",
            ),
        ] {
            let symbol = ImportSymbol::Name {
                hint: 0,
                name: sym.into(),
            };
            assert!(
                matches!(
                    r.resolve(dll, &symbol),
                    Resolution::Found {
                        provenance: Provenance::Hle,
                        ..
                    }
                ),
                "{dll}!{sym} must bind to HLE"
            );
        }
    }

    /// Pending overlapped `ReadFile` (`ERROR_IO_PENDING`) keeps a guarded GPU
    /// view writable across a mid-flight GPU refresh, preserves `LastError`,
    /// and re-guards only after completion is harvested.
    #[test]
    fn pending_overlapped_read_keeps_guarded_view_open_across_refresh() {
        let view = MappedView::new(1);
        let rig = GuardRig::attach(view.0, 1);

        let (server, client) = overlapped_pipe("pending_get_overlapped");
        // SAFETY: creates a manual-reset event for `OVERLAPPED`.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!event.is_null());
        let mut ov = zero_overlapped(event);

        let ok = ReadFile(
            client,
            (view.0 + 128) as *mut u8,
            16,
            std::ptr::null_mut(),
            (&raw mut ov).cast(),
        );
        // SAFETY: checks `LastError` preserved across `ReadFile` wrapper.
        let err = unsafe { GetLastError() };
        assert_eq!((ok, err), (0, ERROR_IO_PENDING));

        if let Some(rig) = &rig {
            rig.refresh();
            assert_eq!(
                protection(view.0),
                PAGE_READWRITE,
                "mid-flight GPU refresh must not re-guard a pending overlapped destination"
            );
        }

        // Non-blocking poll while still pending keeps the lease alive.
        let mut transferred = 0u32;
        assert_eq!(GetOverlappedResult(client, &mut ov, &mut transferred, 0), 0);
        // SAFETY: checks `LastError` preserved across `GetOverlappedResult`.
        assert_eq!(unsafe { GetLastError() }, ERROR_IO_INCOMPLETE);
        if let Some(rig) = &rig {
            rig.refresh();
            assert_eq!(protection(view.0), PAGE_READWRITE);
        }

        write_all(server, b"streamed-payload");
        assert_ne!(
            GetOverlappedResultEx(client, &mut ov, &mut transferred, 5_000, 0),
            0
        );
        assert_eq!(transferred, 16);
        // SAFETY: the 16 bytes just landed in our mapped view.
        let landed = unsafe { std::slice::from_raw_parts((view.0 + 128) as *const u8, 16) };
        assert_eq!(landed, b"streamed-payload");

        if let Some(rig) = &rig {
            rig.refresh();
            assert_eq!(
                protection(view.0),
                PAGE_READONLY,
                "completed and harvested overlapped read lets the next refresh re-guard"
            );
            // Proof of negative control: the un-wrapped host `ReadFile` fails
            // with `ERROR_NOACCESS` against the re-guarded view.
            let (mut r_pipe, mut w_pipe): (Handle, Handle) =
                (std::ptr::null_mut(), std::ptr::null_mut());
            assert_ne!(
                host!(
                    "kernel32.dll",
                    "CreatePipe": extern "system" fn(*mut Handle, *mut Handle, *const c_void, u32) -> i32
                )(&mut r_pipe, &mut w_pipe, std::ptr::null(), 64),
                0
            );
            write_all(w_pipe, b"blocked!");
            let mut raw_n = 0u32;
            let raw_ok = host!(
                "kernel32.dll",
                "ReadFile": extern "system" fn(Handle, *mut u8, u32, *mut u32, *mut c_void) -> i32
            )(
                r_pipe,
                (view.0 + 256) as *mut u8,
                8,
                &mut raw_n,
                std::ptr::null_mut(),
            );
            // SAFETY: checks `ERROR_NOACCESS` (998) from the un-wrapped host call.
            assert_eq!((raw_ok, unsafe { GetLastError() }), (0, 998));
            CloseHandle(r_pipe);
            CloseHandle(w_pipe);
        }

        CloseHandle(event);
        CloseHandle(client);
        CloseHandle(server);
    }

    /// Unbuffered overlapped `ReadFileScatter` into guarded pages opens every
    /// target segment, coalesces 4 KiB pages per 64 KiB block, and releases the
    /// lease when awaited via `WaitForSingleObject` on a low-bit-tagged event.
    #[test]
    fn read_file_scatter_opens_guarded_segments_and_harvests_on_event_wait() {
        let view_a = MappedView::new(1);
        let view_b = MappedView::new(1);
        let rig_a = GuardRig::attach(view_a.0, 1);
        let rig_b = GuardRig::attach(view_b.0, 1);

        let dir = tempfile::tempdir().unwrap();
        let path = w(&dir.path().join("scatter.bin").display().to_string());
        let page = os_page_size();
        assert_eq!(page, 4096);
        let mut payload = vec![0u8; 3 * page];
        payload[..page].fill(0xA1);
        payload[page..2 * page].fill(0xB2);
        payload[2 * page..].fill(0xC3);

        let writer = super::super::CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null(),
            CREATE_ALWAYS,
            0,
            std::ptr::null_mut(),
        );
        assert_ne!(writer, INVALID_HANDLE_VALUE);
        write_all(writer, &payload);
        CloseHandle(writer);

        let reader = super::super::CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | FILE_FLAG_NO_BUFFERING,
            std::ptr::null_mut(),
        );
        assert_ne!(reader, INVALID_HANDLE_VALUE);

        // Two non-adjacent 4 KiB pages in `view_a` plus one 4 KiB page in
        // `view_b`, terminated by 0 as `ReadFileScatter` requires.
        let segments = [
            (view_a.0 + 2 * page) as u64,
            view_b.0 as u64,
            view_a.0 as u64,
            0u64,
        ];

        // Negative control when guarded: direct host `ReadFileScatter` fails
        // with `ERROR_NOACCESS` (998).
        if rig_a.is_some() && rig_b.is_some() {
            // SAFETY: creates a manual-reset event.
            let raw_ev = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            let mut raw_ov = zero_overlapped(raw_ev);
            let raw_ok = host!(
                "kernel32.dll",
                "ReadFileScatter": extern "system" fn(Handle, *const u64, u32, *mut u32, *mut Overlapped) -> i32
            )(
                reader,
                segments.as_ptr(),
                (3 * page) as u32,
                std::ptr::null_mut(),
                &mut raw_ov,
            );
            // SAFETY: checks `ERROR_NOACCESS` (998).
            assert_eq!((raw_ok, unsafe { GetLastError() }), (0, 998));
            CloseHandle(raw_ev);
        }

        // SAFETY: creates a manual-reset event and tags bit 0 (skip IOCP).
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!event.is_null());
        let tagged_event = ((event as usize) | 1) as Handle;
        let mut ov = zero_overlapped(tagged_event);

        let ok = ReadFileScatter(
            reader,
            segments.as_ptr(),
            (3 * page) as u32,
            std::ptr::null_mut(),
            &mut ov,
        );
        // SAFETY: reads `GetLastError` if pending.
        let err = unsafe { GetLastError() };
        assert!(ok != 0 || err == ERROR_IO_PENDING, "ok={ok} err={err}");
        if ok == 0 {
            assert_eq!(WaitForSingleObject(event, INFINITE), WAIT_OBJECT_0);
        }
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 0);

        // SAFETY: all three 4 KiB target segments lie in our mapped views.
        unsafe {
            assert!(
                std::slice::from_raw_parts((view_a.0 + 2 * page) as *const u8, page)
                    .iter()
                    .all(|&b| b == 0xA1)
            );
            assert!(
                std::slice::from_raw_parts(view_b.0 as *const u8, page)
                    .iter()
                    .all(|&b| b == 0xB2)
            );
            assert!(
                std::slice::from_raw_parts(view_a.0 as *const u8, page)
                    .iter()
                    .all(|&b| b == 0xC3)
            );
        }

        if let (Some(rig_a), Some(rig_b)) = (&rig_a, &rig_b) {
            rig_a.refresh();
            rig_b.refresh();
            assert_eq!(protection(view_a.0), PAGE_READONLY);
            assert_eq!(protection(view_b.0), PAGE_READONLY);
        }

        CloseHandle(event);
        CloseHandle(reader);
    }

    /// Covers IOCP (`GetQueuedCompletionStatus` and `GetQueuedCompletionStatusEx`),
    /// `ReadFileEx` alertable APC completion (with arbitrary user data in
    /// `OVERLAPPED::hEvent`), async `CancelIoEx`, and synchronous EOF error
    /// preservation.
    #[test]
    fn iocp_read_file_ex_cancellation_and_eof_preserve_semantics() {
        // 1. IOCP harvest (`GetQueuedCompletionStatus` & `GetQueuedCompletionStatusEx`).
        let create_iocp = host!(
            "kernel32.dll",
            "CreateIoCompletionPort": extern "system" fn(Handle, Handle, usize, u32) -> Handle
        );
        let (server1, client1) = overlapped_pipe("iocp_single");
        let port = create_iocp(client1, std::ptr::null_mut(), 0x77, 0);
        assert!(!port.is_null());

        let view = MappedView::new(1);
        let rig = GuardRig::attach(view.0, 1);

        let mut ov1 = zero_overlapped(std::ptr::null_mut());
        assert_eq!(
            ReadFile(
                client1,
                view.0 as *mut u8,
                8,
                std::ptr::null_mut(),
                (&raw mut ov1).cast(),
            ),
            0
        );
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 1);
        write_all(server1, b"iocp-one");

        let (mut bytes, mut key, mut out_ov) = (0u32, 0usize, std::ptr::null_mut());
        assert_ne!(
            GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut out_ov, 5_000),
            0
        );
        assert_eq!(
            (bytes, key, out_ov as usize),
            (8, 0x77, &raw mut ov1 as usize)
        );
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 0);

        let mut ov2 = zero_overlapped(std::ptr::null_mut());
        assert_eq!(
            ReadFile(
                client1,
                (view.0 + 64) as *mut u8,
                8,
                std::ptr::null_mut(),
                (&raw mut ov2).cast(),
            ),
            0
        );
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 1);
        write_all(server1, b"iocp-two");

        let mut entry = OverlappedEntry {
            key: 0,
            overlapped: std::ptr::null_mut(),
            internal: 0,
            bytes: 0,
        };
        let mut removed = 0u32;
        assert_ne!(
            GetQueuedCompletionStatusEx(port, &mut entry, 1, &mut removed, 5_000, 0),
            0
        );
        assert_eq!((removed, entry.bytes, entry.key), (1, 8, 0x77));
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 0);
        CloseHandle(port);
        CloseHandle(client1);
        CloseHandle(server1);

        // 2. `ReadFileEx` APC completion with a non-handle user context stored
        // in `OVERLAPPED::hEvent` (explicitly permitted by Win32 `ReadFileEx`).
        static APC_BYTES: AtomicU32 = AtomicU32::new(0);
        static APC_CTX: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "system" fn on_apc(err: u32, transferred: u32, ov: *mut Overlapped) {
            assert_eq!(err, 0);
            APC_BYTES.store(transferred, Ordering::SeqCst);
            // SAFETY: `ov` is our live `OVERLAPPED` whose `event` holds context.
            APC_CTX.store(unsafe { (*ov).event } as usize, Ordering::SeqCst);
        }

        if let Some(rig) = &rig {
            rig.refresh();
            assert_eq!(protection(view.0), PAGE_READONLY);
        }
        let (server_ex, client_ex) = overlapped_pipe("read_file_ex");
        let mut ov_ex = zero_overlapped(0xDEAD_BEEF_CAFE_0001usize as Handle);
        assert_ne!(
            ReadFileEx(
                client_ex,
                (view.0 + 512) as *mut u8,
                12,
                &mut ov_ex,
                Some(on_apc),
            ),
            0
        );
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 1);
        write_all(server_ex, b"alertable-io");
        // SAFETY: alertable sleep dispatches the queued completion APC.
        unsafe { SleepEx(5_000, 1) };
        assert_eq!(APC_BYTES.load(Ordering::SeqCst), 12);
        assert_eq!(APC_CTX.load(Ordering::SeqCst), 0xDEAD_BEEF_CAFE_0001);
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 0);
        // SAFETY: 12 bytes written into `view`.
        let apc_slice = unsafe { std::slice::from_raw_parts((view.0 + 512) as *const u8, 12) };
        assert_eq!(apc_slice, b"alertable-io");

        // 3. `CancelIoEx` + `GetOverlappedResult` drains cancelled pending read.
        let mut ov_cancel = zero_overlapped(std::ptr::null_mut());
        assert_eq!(
            ReadFile(
                client_ex,
                (view.0 + 1024) as *mut u8,
                16,
                std::ptr::null_mut(),
                (&raw mut ov_cancel).cast(),
            ),
            0
        );
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 1);
        assert_ne!(CancelIoEx(client_ex, &ov_cancel), 0);
        let mut cancelled_bytes = 0u32;
        assert_eq!(
            GetOverlappedResult(client_ex, &mut ov_cancel, &mut cancelled_bytes, 1),
            0
        );
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 0);
        CloseHandle(client_ex);
        CloseHandle(server_ex);

        // 4. Synchronous overlapped EOF preserves `ERROR_HANDLE_EOF` (38).
        let empty = tempfile::NamedTempFile::new().unwrap();
        let empty_w = w(&empty.path().display().to_string());
        let h_empty = super::super::CreateFileW(
            empty_w.as_ptr(),
            GENERIC_READ,
            7,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        assert_ne!(h_empty, INVALID_HANDLE_VALUE);
        let mut ov_eof = zero_overlapped(std::ptr::null_mut());
        let mut n_eof = 0u32;
        let ok_eof = ReadFile(
            h_empty,
            (view.0 + 2048) as *mut u8,
            16,
            &mut n_eof,
            (&raw mut ov_eof).cast(),
        );
        // SAFETY: verifies `ERROR_HANDLE_EOF` survived `KernelWrite` drop.
        assert_eq!((ok_eof, unsafe { GetLastError() }), (0, ERROR_HANDLE_EOF));
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 0);
        CloseHandle(h_empty);
    }

    /// Covers `WaitForMultipleObjects`, pre-parked IOCP worker threads,
    /// `OVERLAPPED` reuse after `HasOverlappedIoCompleted` polling, and
    /// `CloseHandle` cleanup.
    #[test]
    fn multi_wait_overlapped_reuse_and_close_handle_leave_no_stale_leases() {
        let view = MappedView::new(1);
        let rig = GuardRig::attach(view.0, 1);

        // 1. Two concurrent pending reads harvested together by `WaitForMultipleObjects`.
        let (s1, c1) = overlapped_pipe("multi_1");
        let (s2, c2) = overlapped_pipe("multi_2");
        // SAFETY: creates two manual-reset events.
        let e1 = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        // SAFETY: creates second manual-reset event.
        let e2 = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        let mut ov1 = zero_overlapped(e1);
        let mut ov2 = zero_overlapped(e2);

        assert_eq!(
            ReadFile(
                c1,
                view.0 as *mut u8,
                4,
                std::ptr::null_mut(),
                (&raw mut ov1).cast()
            ),
            0
        );
        assert_eq!(
            ReadFile(
                c2,
                (view.0 + 16) as *mut u8,
                4,
                std::ptr::null_mut(),
                (&raw mut ov2).cast()
            ),
            0
        );
        write_all(s1, b"AAAA");
        write_all(s2, b"BBBB");
        let events = [e1, e2];
        assert_eq!(
            WaitForMultipleObjects(2, events.as_ptr(), 1, 5_000),
            WAIT_OBJECT_0
        );
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 0);
        CloseHandle(e1);
        CloseHandle(e2);
        CloseHandle(c1);
        CloseHandle(s1);
        CloseHandle(c2);
        CloseHandle(s2);

        // 2. `OVERLAPPED` polled via `HasOverlappedIoCompleted` (without calling
        // `WaitFor*` or `GetOverlappedResult`), reused for a second read on the
        // same guarded view, then cleaned up via `CloseHandle`.
        if let Some(rig) = &rig {
            rig.refresh();
            assert_eq!(protection(view.0), PAGE_READONLY);
        }
        let (s3, c3) = overlapped_pipe("reuse_ov");
        let mut ov_reuse = zero_overlapped(std::ptr::null_mut());
        assert_eq!(
            ReadFile(
                c3,
                (view.0 + 32) as *mut u8,
                4,
                std::ptr::null_mut(),
                (&raw mut ov_reuse).cast()
            ),
            0
        );
        write_all(s3, b"1111");
        // SAFETY: `ov_reuse` is our live stack `OVERLAPPED` structure.
        while unsafe { !overlapped_done(&raw const ov_reuse as usize) } {
            std::thread::yield_now();
        }
        // Reusing `ov_reuse` drops the previous lease while keeping `view` open
        // for the new read.
        assert_eq!(
            ReadFile(
                c3,
                (view.0 + 48) as *mut u8,
                4,
                std::ptr::null_mut(),
                (&raw mut ov_reuse).cast()
            ),
            0
        );
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 1);
        if let Some(rig) = &rig {
            rig.refresh();
            assert_eq!(protection(view.0), PAGE_READWRITE);
        }
        write_all(s3, b"2222");
        // SAFETY: `ov_reuse` is our live stack `OVERLAPPED` structure.
        while unsafe { !overlapped_done(&raw const ov_reuse as usize) } {
            std::thread::yield_now();
        }
        CloseHandle(c3);
        CloseHandle(s3);
        assert_eq!(PENDING_COUNT.load(Ordering::Acquire), 0);

        if let Some(rig) = &rig {
            rig.refresh();
            assert_eq!(protection(view.0), PAGE_READONLY);
        }
    }
}
