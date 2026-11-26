//! Host-wave safety decisions for native group barriers and wave64 shaders.
use std::collections::HashSet;

use crate::ir::Program;
use crate::native::StageIo;
use crate::structure::{Bool, Structured};

use super::super::WaveMode;

pub(crate) struct Plan {
    pub wave64_unsupported: bool,
    pub relaxed_barriers: HashSet<(usize, usize)>,
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
) -> Plan {
    let wave64_unsupported = native_wave64 && wave.max_lanes < 64 && !wave.wide64;
    let emulated_wave64 = native_wave64 && wave.max_lanes < 64 && wave.wide64;
    let group_lanes = match io {
        StageIo::Cs(cs) => cs
            .threads
            .iter()
            .try_fold(1u32, |n, &dimension| n.checked_mul(dimension)),
        StageIo::Ms(ms) => Some(ms.threads),
        _ => None,
    };
    let single_wave = group_lanes.is_some_and(|n| n > 0 && n <= wave.min_lanes);
    let mut relaxed_barriers = HashSet::new();
    if single_wave && wave.relaxes_divergent_barriers() && !emulated_wave64 {
        for (b, block) in p.blocks.iter().enumerate() {
            for (i, inst) in block.insts.iter().enumerate() {
                if inst.name() != "s_barrier" {
                    continue;
                }
                let divergent = st.reach.get(b).is_some_and(|r| *r != Bool::TRUE)
                    || st
                        .guards
                        .get(b)
                        .and_then(|g| g.get(i))
                        .is_some_and(|g| *g != Some(Bool::TRUE));
                if divergent {
                    relaxed_barriers.insert((b, i));
                }
            }
        }
    }
    Plan {
        wave64_unsupported,
        relaxed_barriers,
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
