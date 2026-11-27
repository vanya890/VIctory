# Issue #153: frame timing investigation

**Date:** 2026-11-26  
**Repository:** [vanya890/VIctory](https://github.com/vanya890/VIctory)  
**Investigated branch:** `dev`  
**dev HEAD at investigation:** `b12aaf73f0ce1f916ffdcbe6c741b19b1cb50bfe`  
**Failing CI merge commit:** `f92d664c91f271bfaee9bb4c448c87e032f4e637`  
**Source:** [crates/victory-gpu/src/frame.rs](https://github.com/vanya890/VIctory/blob/dev/crates/victory-gpu/src/frame.rs)  
**Source blob:** `4a68b3533feefae5c27240fe8e5e9a225db7398e`  
**CI run:** [#45857648756](https://github.com/vanya890/VIctory/actions/runs/45857648756), job `Tests (default)`, job ID `140040136882`

> Report-only investigation. No Rust source, D3D12 code, `main`, or existing branch was modified. A proposed testability refactor is included below, not applied.

## 1. Executive conclusion

The Windows CI failure is **not evidence of a lost wakeup, mutex race, atomic visibility failure, or incorrect frame counter**.

The failing assertion is at `crates/victory-gpu/src/frame.rs:231`:

```text
assertion `left == right` failed
  left: 0
 right: 1
```

The assertion expects `TIMEOUT == S_FALSE` from an origin wait with a zero-millisecond timeout. The implementation can legitimately return `S_OK` when an unconsumed tick has already passed by the time the wait is evaluated. That is explicitly the documented late-wait policy in `next_tick`: a passed event remains available until a waiter takes it.

The test uses a 2 ms interval and real wall-clock scheduling, then assumes the next zero-timeout poll must occur before another tick. Windows hosted-runner scheduling does not guarantee that. A scheduling delay crossing a 2 ms boundary makes `next_tick` return a zero duration; `wait()` then returns `S_OK`, exactly as its current contract specifies.

**Established root cause:** a timing-sensitive test assertion assumes a zero-timeout poll cannot observe an already-passed, unconsumed tick. This is an invalid scheduling assumption, not a demonstrated production concurrency defect.

**Recommended fix:** make the tick-selection/timeout decision testable against explicit synthetic elapsed time. Assert both cases deterministically: a future tick beyond a zero timeout returns timeout; an already-passed unconsumed tick returns success. Keep the existing late-event semantics and frame-token assertions. Do not add sleeps or relax the semantic assertion.

## 2. Version and CI evidence

The current `dev` ref was resolved directly through GitHub compare-ref (`dev...dev`) and points to:

```text
b12aaf73f0ce1f916ffdcbe6c741b19b1cb50bfe
```

The reported CI run tests PR #152's merge commit:

```text
f92d664c91f271bfaee9bb4c448c87e032f4e637
Merge 2510222470cdff65da5ab6ee08e96c7422e1485c into
c619e15c61d1af13e13c21cc2a86ba42b54b109c
```

The Windows default test job log establishes:

- OS: Windows Server 2025, build 10.0.26100.
- Runner image: `windows-2025-vs2026`, version `20261116.315.1`.
- Azure hosted runner, region `centralus`.
- Workspace compilation completed successfully.
- Failure at `frame.rs:231:9`.
- Result: 278 passed, 1 failed, 1 ignored.
- The failing test is `frame::tests::waits_end_on_ticks_and_count_frames`.

The exact panic is an HRESULT comparison, not a counter comparison: `S_OK` is zero and `S_FALSE` is one. The failure therefore says that the zero-timeout wait completed successfully rather than timing out.

The job log was inspected directly. No adapter/GPU dependency is involved in this frame unit test.

## 3. Architecture and lifecycle

Relevant implementation in `crates/victory-gpu/src/frame.rs`:

| Element | Role | Synchronization |
|---|---|---|
| `Clock { start, interval, first }` | Absolute periodic clock; tick numbers continue across interval changes | Protected by `CLOCK` |
| `CLOCK: Mutex<Option<Clock>>` | Owns the current clock configuration | `Mutex` |
| `FRAME: AtomicU64` | Monotonic frame-token counter | Atomic increment/load |
| `LAST: [AtomicU64; 3]` | Last consumed tick per event kind (origin, vblank, flip) | Atomics; normal tick selection occurs while holding `CLOCK` |
| `set_interval()` | Replaces clock epoch/interval while preserving last handed-out tick | Reads `LAST`, then locks and replaces `CLOCK` |
| `next_tick()` | Chooses next available/passed tick and marks it consumed when within timeout | Holds `CLOCK` lock across selection and `LAST.fetch_max` |
| `wait()` | Sleeps for computed duration or timeout; returns `S_OK`/ `S_FALSE`; increments origin frame token | Calls `next_tick`, then sleeps outside the mutex |
| `presented()` / `log()` | Diagnostics only | Relaxed atomic operations |

There is **no Condvar, channel, event object, or notify/wait pair** in this implementation. The blocking primitive is `std::thread::sleep`.

Lifecycle:

1. `set_interval` starts/restarts the clock from `Instant::now()`, preserving a tick base derived from `LAST`.
2. `wait` calls `next_tick`.
3. `next_tick` acquires `CLOCK`, creates a default clock if needed, computes elapsed time and the tick, and reserves that tick with `fetch_max` when `wait <= limit`.
4. The mutex is released when `next_tick` returns.
5. `wait` sleeps for either the computed wait or the timeout.
6. Origin waits increment `FRAME` if they return `S_OK`, or if `TOKEN_ON_TIMEOUT` is set.
7. No explicit teardown/reset exists: clock and counters are process-global statics.

The frame clock has no per-device ownership. Its state is global to the process.

## 4. Exact failing sequence

The first part of `waits_end_on_ticks_and_count_frames`:

1. `set_interval(..., 2_000, ...)` configures a 2 ms interval.
2. First origin `wait(..., timeout_ms=1_000, flags=0)` waits for/consumes an origin tick and returns `S_OK`.
3. Second origin wait does the same; the test checks that its frame token is greater than the first.
4. The test checks a 50 ms upper bound for the elapsed section.
5. It calls `wait(..., timeout_ms=0, flags=0)` and unconditionally expects `TIMEOUT` (`S_FALSE`).

The fifth step is not deterministic under a real clock. Between the prior wait and this poll, the runner can be descheduled for longer than the 2 ms interval. If another origin tick has passed and is still unconsumed, the tick-selection expression chooses that current tick:

```rust
let tick = (c.first + (elapsed / interval) as u64)
    .max(last(kind).load(Ordering::Relaxed) + 1);
let at = u128::from(tick - c.first) * interval;
let wait = Duration::from_nanos(at.saturating_sub(elapsed) as u64);
if wait <= limit {
    last(kind).fetch_max(tick, Ordering::Relaxed);
}
```

With `limit == Duration::ZERO`, a passed tick gives `wait == 0`; therefore `wait <= limit` is true. `wait()` takes the `S_OK` branch and sleeps for zero duration. The observed `left: 0` is thus the direct output of the current implementation.

This behavior is also consistent with the nearby source documentation: a wait begun after its tick passed ends immediately on the latest passed tick, as an event that remains signalled until consumed would.

The following `TOKEN_ON_TIMEOUT` test also calls a zero-timeout origin wait. Its token progression is meaningful, but its unconditional `TIMEOUT` expectation has the same scheduling flaw.

## 5. Synchronization analysis

### Lost wakeups / notify-before-wait / spurious wakeups

Not applicable to the current implementation: it contains no notification primitive and no condition-variable wait. `sleep` does not use a stored notification that could be lost. There is no basis to label this failure a lost wakeup.

### Mutex and tick reservation

The normal `next_tick` path holds `CLOCK` while it reads the clock configuration, reads the event's last tick, calculates the candidate, and reserves it with `fetch_max`. Two ordinary callers of `next_tick` therefore serialize their tick selection under the same mutex. A second caller observes the first caller's reservation and cannot reserve the same tick.

The sleep is intentionally outside the mutex. This avoids holding the global clock lock for the entire frame interval and does not undo the reservation, which has already happened.

### Atomics and memory ordering

`FRAME`, `LAST`, and diagnostics use `Ordering::Relaxed`. For the state currently represented by these atomics, the values are numeric counters/tick identifiers; no unrelated memory payload is published through them. The mutex serializes the ordinary `next_tick` critical section. Stronger ordering would not make a 2 ms wall-clock test deterministic and would not change the result of the failing comparison.

`fetch_max` is important: it prevents a lower tick from overwriting a higher reservation if accesses outside the normal serialized path occur. It is not a publication barrier for unrelated data.

### Publication and interval replacement

There is a separate area worth reviewing, but it is not shown to cause this CI assertion: `set_interval` samples `LAST` before it acquires `CLOCK`, then replaces the clock under the mutex. A concurrent waiter could reserve a tick after that sample but before `set_interval` obtains the mutex. The new clock's `first` could then be based on a stale snapshot.

This is a plausible interval-reconfiguration ordering issue, not the demonstrated failure at line 231. A follow-up should move the `LAST` snapshot inside the `CLOCK` critical section if the API permits concurrent `set_interval` and waits, and should define the intended interaction. Do not conflate this with the zero-timeout assertion.

### Teardown

There is no dynamic clock object teardown. Static lifetime means no use-after-free or destruction-order race is apparent in this module. The test does not construct/drop a separate clock instance.

### Test-level global state

Both frame tests operate on process-global statics. The first calls `set_interval` and mutates origin/flip state; the second calls `next_tick` and mutates vblank state. They can run concurrently under Rust's default test harness. The second test does not call `set_interval` and uses event kind 1, whereas the failing assertion uses origin kind 0. No evidence from the CI log establishes that cross-test interference caused this particular HRESULT mismatch. Global state makes test isolation weaker, but the line-231 failure is already explained by the real-time 2 ms scheduling assumption.

## 6. Upstream comparison

The upstream project has related frame-timing work:

- [Upstream PR #174: a frame wait takes its tick before it sleeps](https://github.com/Marvisak/VIctory/pull/174) moved tick reservation into `next_tick`, under the clock lock. Its stated reason was that two concurrent waits could otherwise both read the same last tick and both return on it.
- [Upstream PR #319: end a late flip wait on the tick it missed, not the next](https://github.com/Marvisak/VIctory/pull/319) introduced a late-flip fast path.
- The upstream PR #319 discussion documents an interaction with #174: its early return initially skipped the new tick reservation. The reported fix adds `last(kind).fetch_max(passed, Ordering::Relaxed)` before that early return. The discussion reports repeated reproduction and successful regression testing on the upstream merge train.

The inspected fork `dev` implementation already reserves a tick inside `next_tick` before sleeping. It does **not** contain the specific late-flip early-return branch described in the upstream #319 interaction. The exact upstream #174/#319 bug is therefore not the explanation for this fork's line-231 failure.

No upstream code should be transplanted blindly. The relevant shared design fact is that passed ticks are consumed immediately; that makes the fork's zero-timeout expectation invalid under scheduling delay.

## 7. Root cause and unknowns

### Proven from source and CI

- The failing assertion compares a zero-timeout origin wait against `TIMEOUT`.
- The implementation returns `S_OK` when `next_tick` computes a zero wait.
- A zero wait is possible when the next unconsumed tick has already passed.
- The implementation's comments explicitly permit a late wait to complete on a passed tick.
- The CI failure is an HRESULT mismatch at line 231, not a wrong frame-token value.
- The 2 ms test interval makes crossing a tick boundary during ordinary scheduler delay plausible and requires no data race.

### Not proven

- The precise amount of scheduler delay in the failing process cannot be recovered: CI did not log `elapsed`, `tick`, `wait`, or `LAST[0]` at the assertion.
- We cannot distinguish a descheduling pause from other ordinary execution delay at sub-2-ms granularity using the existing log.
- The separate `set_interval` snapshot ordering concern has not been reproduced.
- The current run does not establish a production frame-pacing defect.

## 8. Proposed fix (not applied)

Do not change production frame pacing semantics merely to force this assertion to pass. Make the arithmetic/decision deterministic and test the actual contract.

Refactor the calculation into a pure helper that accepts the elapsed time and last tick explicitly. `next_tick` remains responsible for taking `CLOCK`, reading the clock state, calling the helper, and reserving the returned tick under the existing lock.

Illustrative shape:

```rust
fn tick_at(
    first: u64,
    interval: u128,
    elapsed: u128,
    last_tick: u64,
) -> (u64, Duration) {
    let interval = interval.max(1);
    let tick = (first + (elapsed / interval) as u64).max(last_tick + 1);
    let at = u128::from(tick - first) * interval;
    let wait = Duration::from_nanos(at.saturating_sub(elapsed) as u64);
    (tick, wait)
}
```

Then `next_tick` can use:

```rust
let (tick, wait) = tick_at(
    c.first,
    c.interval.as_nanos(),
    c.start.elapsed().as_nanos(),
    last(kind).load(Ordering::Relaxed),
);
if wait <= limit {
    last(kind).fetch_max(tick, Ordering::Relaxed);
}
(tick, wait)
```

Deterministic tests should cover:

```rust
// Tick is still in the future: zero timeout must time out.
let (_, wait) = tick_at(10, 2_000_000, 1_000_000, 10);
assert!(wait > Duration::ZERO);

// Tick has passed and is unconsumed: zero timeout observes the event.
let (_, wait) = tick_at(10, 2_000_000, 2_000_000, 10);
assert_eq!(wait, Duration::ZERO);
```

Keep a test for `TOKEN_ON_TIMEOUT` and frame token increments, but do not make that test depend on the assumption that a live zero-timeout call must return `S_FALSE`. The token-on-timeout behavior can be tested separately with a controlled clock/time source or by extracting the return/token decision into a pure helper.

The illustrative helper is intentionally not a complete patch: before implementation, preserve the current saturating/overflow behavior and check that `first + elapsed / interval` and `last_tick + 1` cannot overflow under the module's actual invariants. The current source itself does not guard those additions.

### Why this is correct

- It preserves the production rule that a late wait can consume a passed event immediately.
- It does not add delays, weaken assertions, or change expected behavior.
- It removes scheduler timing from the unit test's arithmetic assertions.
- It preserves the mutex critical section and tick reservation order.
- It gives explicit coverage for both possible outcomes of a zero-timeout poll.

## 9. Reproduction status

The Windows CI failure was inspected from the complete job log and mapped to the source assertion.

An isolated local rerun was **not performed** in this environment: the repository could not be cloned because outbound DNS/network access to GitHub was unavailable, and the available local runtime is not the Windows hosted runner. No claim of local reproduction is made.

A deterministic reproduction does not require waiting for a real 2 ms boundary: the pure helper tests above can exercise both branches with fixed elapsed-time inputs. The original Windows CI log remains the observed failure.

## 10. Risks and follow-up

1. The proposed pure helper must preserve integer arithmetic and overflow behavior exactly.
2. Avoid changing the event contract for origin waits without title-side evidence; the documented late-event policy is currently generic.
3. Consider a separate follow-up to serialize `set_interval`'s `LAST` snapshot with the `CLOCK` mutex if concurrent interval changes are supported.
4. Consider isolating global frame-clock state in tests, or using a private clock instance for unit tests. Do not claim test parallelism caused this exact failure without a reproduction.
5. Do not introduce a Condvar: there is no notification-based wait in the current implementation and it is not needed to fix this test.

## 11. Handoff to the primary developer

**Frame-timing finding:** line 231's `S_FALSE` expectation is invalid under the implementation's documented late-tick semantics. A zero-timeout origin poll may return `S_OK` if a new unconsumed tick has already passed.

**Recommended action:** replace the real-time assumption with deterministic tests around a pure tick calculation/time source. Keep production behavior unchanged unless separate title evidence demonstrates a semantic bug.

**Not this task:** D3D12, rustfmt/rustdoc cleanup, workspace-wide CI changes, or any changes to `main`.

**No source patch was applied.** This PR contains only this investigation report.
