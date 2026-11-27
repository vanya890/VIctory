//! Host-wave safety decisions for native group barriers and wave64 shaders.
use std::collections::HashSet;

use crate::gfx10::Fields;
use crate::ir::{Program, Value};
use crate::native::StageIo;
use crate::structure::{Atom, Bool, LaneExit, Structured};

use super::super::WaveMode;

pub(crate) struct Plan {
    pub wave64_unsupported: bool,
    pub relaxed_barriers: HashSet<(usize, usize)>,
    pub console_lanes: bool,
    pub wave_size: Option<u32>,
}

/// Proves when a divergent group barrier is only a fence because the entire
/// group is guaranteed to fit in one host wave. Use the D3D12 minimum lane
/// count for this proof; the maximum is only a capability ceiling.
pub(crate) fn plan(
    p: &Program,
    st: &Structured,
    io: &StageIo,
    native_wave64: bool,
    wave: WaveMode,
    waterfall_reads: &[Value],
    lds_dwords: u32,
) -> Plan {
    let is_compute = matches!(io, StageIo::Cs(_));
    let console_lanes = if is_compute {
        p.blocks.iter().flat_map(|b| &b.insts).any(|i| {
            let n = i.name();
            n.starts_with("ds_swizzle")
                || n.contains("permute")
                || n.starts_with("v_mbcnt")
                || matches!(n, "v_readlane_b32" | "v_writelane_b32")
                // A descriptor waterfall's readfirstlane is scalarized by
                // the waterfall lowering; an ordinary readfirstlane still
                // observes the console's native lane mapping.
                || (n == "v_readfirstlane_b32"
                    && !i
                        .writes
                        .first()
                        .is_some_and(|(_, value)| waterfall_reads.contains(value)))
                || matches!(&i.fields, Fields::Valu(v) if v.dpp.is_some())
                || super::reads_mask_as_data(st, i)
        }) || {
            // Masks held as numbers or constant lane sets: the lane tests
            // its own bit by its console lane.
            let whens = st.exits.iter().filter_map(|e| match *e {
                LaneExit::Branch { when, .. } => Some(when),
                _ => None,
            });
            let mut all: Vec<Bool> = st
                .guards
                .iter()
                .flatten()
                .flatten()
                .copied()
                .chain(whens)
                .collect();
            all.sort_unstable();
            all.dedup();
            all.into_iter().any(|g| {
                st.bools
                    .support(g)
                    .iter()
                    .any(|a| matches!(a, Atom::Opaque(_) | Atom::Constant(_)))
            })
        }
    } else {
        false
    };

    let group_lanes = match io {
        StageIo::Cs(cs) => cs
            .threads
            .iter()
            .try_fold(1u32, |n, &dimension| n.checked_mul(dimension)),
        StageIo::Ms(ms) => Some(ms.threads),
        _ => None,
    };
    let console_wave = if native_wave64 { 64 } else { 32 };
    let one_console_wave = is_compute
        && group_lanes.is_some_and(|n| n > 0 && n <= console_wave);
    let one_host_wave =
        group_lanes.is_some_and(|n| n > 0 && wave.min_lanes > 0 && n <= wave.min_lanes);

    // If LDS is shared by a group that fits in one console wave but is not
    // guaranteed to fit in one host wave, the compiler's omitted group
    // barriers depend on native wave width. When the group fits in one host
    // wave, the host's minimum width proves the same property without a
    // fixed WaveSize annotation.
    let needs_single_wave_width = one_console_wave && lds_dwords > 0 && !one_host_wave;
    let wave_size = (console_lanes || needs_single_wave_width).then_some(console_wave);
    let requires_wave64_semantics = if is_compute {
        console_lanes || needs_single_wave_width
    } else {
        // Preserve the conservative behavior for non-compute stages until
        // their lane-sensitive requirements are analyzed independently.
        native_wave64
    };
    let wave64_unsupported =
        native_wave64 && requires_wave64_semantics && wave.max_lanes < 64 && !wave.wide64;
    let emulated_wave64 = native_wave64 && wave.max_lanes < 64 && wave.wide64;

    let single_wave = group_lanes.is_some_and(|n| n > 0 && n <= wave.min_lanes);
    let mut relaxed_barriers = HashSet::new();
    if single_wave && wave.relaxes_divergent_barriers() && !emulated_wave64 {
        for (b, block) in p.blocks.iter().enumerate() {
            for (i, inst) in block.insts.iter().enumerate() {
                if inst.name() != "s_barrier" {
                    continue;
                }
                // None means the instruction has no per-lane guard: it runs
                // for every lane reaching the block. Only an explicit non-TRUE
                // guard makes this barrier divergent.
                let guard = st
                    .guards
                    .get(b)
                    .and_then(|g| g.get(i))
                    .copied()
                    .flatten();
                let divergent = st.reach.get(b).is_some_and(|r| *r != Bool::TRUE)
                    || guard.is_some_and(|g| g != Bool::TRUE);
                if divergent {
                    relaxed_barriers.insert((b, i));
                }
            }
        }
    }
    Plan {
        wave64_unsupported,
        relaxed_barriers,
        console_lanes,
        wave_size,
    }
}

/// D3D12 barrier flags: thread-group synchronization, UAV fence, and
/// group-shared-memory fence. Relaxation removes only the synchronization bit.
pub(crate) fn barrier_mode(writes_uavs: bool, relaxed: bool) -> u32 {
    match (writes_uavs, relaxed) {
        (true, false) => 1 | 2 | 8,
        (true, true) => 2 | 8,
        (false, false) => 1 | 8,
        (false, true) => 8,
    }
}
