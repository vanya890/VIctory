//! Frame pacing: the title sets a frame interval and waits for each frame
//! to begin. Ours is a clock started by the first interval set, ticking
//! once per interval; the token is the frame number. Each event kind
//! (origin, vblank, flip) happens once per tick, so two waits for the same
//! kind never end on the same tick: Grand Theft Auto VI polls the origin
//! with "token on time-out" (starting its next frame at once) and is paced
//! by a blocking wait for the flip before each present.
//!
//! shortcut: event offsets are ignored (every event is at the tick); tie
//! the clock to presentation if a title paces by them.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_FALSE, S_OK, This};

/// What a wait returns when its time is up first.
const TIMEOUT: HRESULT = S_FALSE;
/// The frame-origin event: the one that starts frames and makes tokens.
const ORIGIN: u32 = 0;
/// Make the frame's token even if the wait times out (the title starts the
/// frame early).
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);

struct Clock {
    start: Instant,
    interval: Duration,
    /// The tick number at `start` (ticks go on across interval changes).
    first: u64,
}

struct Pacer {
    clock: Mutex<Option<Clock>>,
    frame: AtomicU64,
    /// Per event kind (origin, vblank, flip and later kinds): the last tick a
    /// wait for it ended on.
    last: [AtomicU64; 3],
}

impl Pacer {
    const fn new() -> Self {
        Self {
            clock: Mutex::new(None),
            frame: AtomicU64::new(0),
            last: [const { AtomicU64::new(0) }; 3],
        }
    }

    fn clock(&self) -> std::sync::MutexGuard<'_, Option<Clock>> {
        self.clock.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn last(&self, kind: u32) -> &AtomicU64 {
        &self.last[(kind as usize).min(self.last.len() - 1)]
    }

    fn set_interval(&self, length: u32, now: Instant) -> Duration {
        let interval = match length {
            0 => SIXTIETH,
            us => Duration::from_micros(u64::from(us)),
        };
        // A new interval starts from now, after the last tick handed out.
        let first = self
            .last
            .iter()
            .map(|t| t.load(Ordering::Relaxed))
            .max()
            .unwrap_or(0);
        *self.clock() = Some(Clock {
            start: now,
            interval,
            first,
        });
        interval
    }

    /// The tick a wait for `kind` ends on, and how long until it: the first
    /// one after the last a wait for `kind` took (an event happens once per
    /// tick: a wait woken a little early must not let the next one end on the
    /// same tick), the latest one passed if that is later. A wait begun after
    /// its tick passed (a frame slower than the interval) so ends at once, as
    /// an event signalled at the tick and left signalled until a wait takes it
    /// would (assumed: nothing we have says what a late wait does); waiting for
    /// the next tick instead cost the cutscene's frames 0-33 ms each (4 % of
    /// the frame thread, run perf3). A wait that comes within `limit` takes the
    /// tick here, under the clock's lock, so two threads waiting for one kind
    /// never take the same tick.
    fn next_tick(&self, kind: u32, limit: Duration, now: Instant) -> (u64, Duration) {
        let mut clock = self.clock();
        let c = clock.get_or_insert_with(|| Clock {
            start: now,
            interval: SIXTIETH,
            first: 0,
        });
        let elapsed = now.saturating_duration_since(c.start).as_nanos();
        let interval = c.interval.as_nanos().max(1);
        let tick = (c.first + (elapsed / interval) as u64)
            .max(self.last(kind).load(Ordering::Relaxed) + 1);
        let at = u128::from(tick - c.first) * interval;
        let wait = Duration::from_nanos(at.saturating_sub(elapsed) as u64);
        if wait <= limit {
            self.last(kind).fetch_max(tick, Ordering::Relaxed);
        }
        (tick, wait)
    }

    fn wait_with(
        &self,
        kind: u32,
        timeout_ms: u32,
        flags: u32,
        token: *mut u64,
        now: Instant,
        sleep: impl FnOnce(Duration),
    ) -> (HRESULT, u64, Duration) {
        let origin = kind == ORIGIN;
        let limit = Duration::from_millis(u64::from(timeout_ms));
        let (tick, wait) = self.next_tick(kind, limit, now);
        let hr = if wait > limit {
            if !limit.is_zero() {
                sleep(limit);
            }
            TIMEOUT
        } else {
            if !wait.is_zero() {
                sleep(wait);
            }
            S_OK
        };
        // A frame begun on a time-out takes no tick: its flip still does.
        if origin && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
            // Tokens count frames (consecutive), not ticks.
            let frame = self.frame.fetch_add(1, Ordering::Relaxed) + 1;
            if !token.is_null() {
                // SAFETY: checked non-null; the title passes its token.
                unsafe { token.write(frame) };
            }
        }
        (hr, tick, wait)
    }
}

static PACER: Pacer = Pacer::new();

/// Device method: a frame every `length` microseconds. The period is how
/// many intervals a frame may take to reach the display (pipeline depth),
/// not part of the rate.
pub(crate) extern "system" fn set_interval(
    _: This,
    _output: This,
    length: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    let interval = PACER.set_interval(length, Instant::now());
    tracing::info!(?interval, "frame interval");
    S_OK
}

/// Device method: events are scheduled at every tick.
pub(crate) extern "system" fn schedule(
    _: This,
    _kind: u32,
    _offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    S_OK
}

/// Frames begun so far (frame waits that ended).
pub(crate) fn current() -> u64 {
    PACER.frame.load(Ordering::Relaxed)
}

/// Device method: blocks until the next tick (or the time-out); an origin
/// wait then hands out the new frame's token. Other events keep the token
/// the title passes. Ticks are absolute (start plus whole intervals), so
/// lateness never accumulates.
pub(crate) extern "system" fn wait(
    _: This,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    // The tick is only logged (diagnostics); taking it is what counts.
    #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
    let (hr, tick, wait) = PACER.wait_with(
        kind,
        timeout_ms,
        flags,
        token,
        Instant::now(),
        std::thread::sleep,
    );
    #[cfg(feature = "diagnostics")]
    {
        WAITS[0].fetch_add(1, Ordering::Relaxed);
        if hr == S_OK && wait.is_zero() {
            WAITS[1].fetch_add(1, Ordering::Relaxed);
        }
        tracing::debug!(
            kind,
            timeout_ms,
            flags,
            hr,
            tick,
            wait_us = wait.as_micros() as u64,
            frame = current(),
            "frame wait"
        );
    }
    hr
}

/// Diagnostics: frame waits, and those that ended at once (their tick had
/// passed); presents whose flip tick was the previous present's.
#[cfg(feature = "diagnostics")]
static WAITS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
#[cfg(feature = "diagnostics")]
static PRESENT_TICK: AtomicU64 = AtomicU64::new(0);

/// Diagnostics: a frame was presented (counts presents sharing a flip tick).
#[cfg(feature = "diagnostics")]
pub(crate) fn presented() {
    let flip = PACER.last[2].load(Ordering::Relaxed);
    if flip != 0 && PRESENT_TICK.swap(flip, Ordering::Relaxed) == flip {
        WAITS[2].fetch_add(1, Ordering::Relaxed);
    }
}

/// Diagnostics: logs [`WAITS`] since the last call.
#[cfg(feature = "diagnostics")]
pub(crate) fn log() {
    let [waits, at_once, shared] = [0, 1, 2].map(|i| WAITS[i].swap(0, Ordering::Relaxed));
    tracing::info!("frame waits: {waits}, {at_once} of them ended at once; presents sharing a flip tick: {shared}");
}

/// Device method: no statistics are kept, so none are returned.
pub(crate) extern "system" fn statistics(
    _: This,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one wait on `pacer` at virtual time `now`, advancing `now` by the
    /// duration slept and returning `(hr, slept)`.
    fn step(
        pacer: &Pacer,
        now: &mut Instant,
        kind: u32,
        timeout_ms: u32,
        flags: u32,
        token: &mut u64,
    ) -> (HRESULT, Duration) {
        let mut slept = Duration::ZERO;
        let (hr, _, _) = pacer.wait_with(kind, timeout_ms, flags, token, *now, |d| {
            slept = d;
            *now += d;
        });
        (hr, slept)
    }

    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        let pacer = Pacer::new();
        let mut now = Instant::now();
        assert_eq!(pacer.set_interval(2_000, now), Duration::from_millis(2));
        let mut first = 0;
        let (hr, slept) = step(&pacer, &mut now, ORIGIN, 1_000, 0, &mut first);
        assert_eq!((hr, slept), (S_OK, Duration::from_millis(2)));
        let mut second = 0;
        let (hr, slept) = step(&pacer, &mut now, ORIGIN, 1_000, 0, &mut second);
        assert_eq!((hr, slept), (S_OK, Duration::from_millis(2)));
        assert_eq!(second, first + 1);
        // Right on the tick just taken, a zero-timeout poll times out without
        // sleeping or advancing the token.
        let (hr, slept) = step(&pacer, &mut now, ORIGIN, 0, 0, &mut second);
        assert_eq!((hr, slept), (TIMEOUT, Duration::ZERO));
        assert_eq!(second, first + 1);
        // Tokens count frames, a frame started early on a time-out too.
        let mut early = 0;
        let (hr, slept) = step(&pacer, &mut now, ORIGIN, 0, TOKEN_ON_TIMEOUT, &mut early);
        assert_eq!((hr, slept), (TIMEOUT, Duration::ZERO));
        let mut next = 0;
        let (hr, slept) = step(&pacer, &mut now, ORIGIN, 1_000, 0, &mut next);
        assert_eq!((hr, slept), (S_OK, Duration::from_millis(2)));
        assert!(early == second + 1 && next == early + 1);
        assert!(pacer.last[0].load(Ordering::Relaxed) >= 2);
        // If a tick has already passed (e.g. host overslept past a 2 ms tick),
        // even a zero-timeout poll takes it at once; the next poll times out.
        now += Duration::from_millis(3);
        let mut late_origin = 0;
        let (hr, slept) = step(&pacer, &mut now, ORIGIN, 0, 0, &mut late_origin);
        assert_eq!((hr, slept), (S_OK, Duration::ZERO));
        assert_eq!(late_origin, next + 1);
        let (hr, slept) = step(&pacer, &mut now, ORIGIN, 0, 0, &mut late_origin);
        assert_eq!((hr, slept), (TIMEOUT, Duration::ZERO));

        // The title's loop: an origin poll (starting each frame immediately),
        // then a blocking flip wait. Each flip takes its own tick; the first
        // flip takes `c.first` (already elapsed, never taken by flip yet) and
        // each following one waits one full 50 ms interval.
        const FLIP: u32 = 2;
        assert_eq!(pacer.set_interval(50_000, now), Duration::from_millis(50));
        let mut token = 0;
        let mut prev_token = late_origin;
        let flips: Vec<u64> = (0..4)
            .map(|i| {
                let (hr, slept) = step(&pacer, &mut now, ORIGIN, 0, TOKEN_ON_TIMEOUT, &mut token);
                assert!(hr == S_OK || hr == TIMEOUT);
                assert_eq!(slept, Duration::ZERO);
                assert_eq!(token, prev_token + 1);
                prev_token = token;
                let (hr, slept) = step(&pacer, &mut now, FLIP, u32::MAX, 0, &mut token);
                let expected_sleep = if i == 0 {
                    Duration::ZERO
                } else {
                    Duration::from_millis(50)
                };
                assert_eq!((hr, slept), (S_OK, expected_sleep));
                pacer.last[2].load(Ordering::Relaxed)
            })
            .collect();
        assert!(flips.windows(2).all(|w| w[1] == w[0] + 1), "{flips:?}");
        // Even when a flip wait begins on the tick another just ended on.
        let (tick, wait) = pacer.next_tick(FLIP, Duration::ZERO, now);
        assert!(tick > flips[3]);
        assert_eq!(wait, Duration::from_millis(50));
        // A flip wait begun after its tick passed (two ticks missed) ends at
        // once without sleeping, on the latest tick passed; the next one waits
        // only the remainder until the following tick.
        now += Duration::from_millis(110);
        let (hr, slept) = step(&pacer, &mut now, FLIP, u32::MAX, 0, &mut token);
        assert_eq!((hr, slept), (S_OK, Duration::ZERO));
        let late = pacer.last[2].load(Ordering::Relaxed);
        assert_eq!(late, flips[3] + 2);
        let (hr, slept) = step(&pacer, &mut now, FLIP, u32::MAX, 0, &mut token);
        assert_eq!((hr, slept), (S_OK, Duration::from_millis(40)));
        assert_eq!(pacer.last[2].load(Ordering::Relaxed), late + 1);
    }

    /// Two waits for one kind that begin together (two threads) take two
    /// ticks: the first takes its tick before it sleeps. A time-out takes
    /// none. The old code marked the tick after the sleep, so both got it.
    #[test]
    fn waits_begun_together_take_different_ticks() {
        const VBLANK: u32 = 1;
        let pacer = Pacer::new();
        let now = Instant::now();
        let long = Duration::from_secs(1);
        let (first, _) = pacer.next_tick(VBLANK, long, now);
        let (second, _) = pacer.next_tick(VBLANK, long, now);
        assert!(second > first, "{second} after {first}");
        pacer.next_tick(VBLANK, Duration::ZERO, now);
        assert_eq!(pacer.last(VBLANK).load(Ordering::Relaxed), second);
    }

    /// Exercises the FFI entry points against the global pacer without relying
    /// on tight OS sleep timing.
    #[test]
    fn ffi_entry_points_pace_with_the_global_clock() {
        let null = std::ptr::null();
        // Long interval (10 s): a zero-timeout poll always times out even on a
        // stalled CI runner, and TOKEN_ON_TIMEOUT still hands out a token.
        assert_eq!(set_interval(null, null, 10_000_000, 1, 0), S_OK);
        let mut early = 0;
        assert_eq!(
            wait(null, ORIGIN, 0, std::ptr::null(), TOKEN_ON_TIMEOUT, &mut early),
            TIMEOUT
        );
        assert!(early >= 1);
        // Tiny interval (1 us) with a generous timeout: always succeeds.
        assert_eq!(set_interval(null, null, 1, 1, 0), S_OK);
        let mut next = 0;
        assert_eq!(wait(null, ORIGIN, 5_000, std::ptr::null(), 0, &mut next), S_OK);
        assert_eq!(next, early + 1);
    }
}
