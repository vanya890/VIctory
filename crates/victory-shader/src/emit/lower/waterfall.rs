//! Recognise the console compiler's SCC-producing resource waterfall loop.

use super::Lower;
use crate::ir::{Exit, Inst, Reg, EXEC};

impl Lower<'_> {
    /// Whether `inst` is the `s_andn2_wrexec` of the console compiler's
    /// waterfall loop: one block that `s_cbranch_execnz` closes, running the
    /// lanes whose VGPR equals its `v_readfirstlane` (so each pass drops at
    /// least the first lane: at most 64 passes). It is left only once no
    /// lane remains, so the SCC it writes (EXEC != 0) is false wherever a
    /// block after the loop reads it. Not when `loop_limit` could cut it
    /// short, nor for a phi of the block taking it (a later pass's).
    pub(super) fn leaves_with_no_lane(&self, inst: &Inst) -> bool {
        let b = self.cur_ir;
        let block = &self.x.p.blocks[b];
        let n = block.insts.len();
        // The first lane's key must be read before the matching compare.
        // The compare must actually write EXEC, and no later instruction may
        // replace that mask before s_andn2_wrexec consumes it.
        let keyed = block.insts.iter().enumerate().any(|(i, rf)| {
            let (Some(&(Reg::V(_), v)), Some(&(Reg::S(_), k))) =
                (rf.reads.first(), rf.writes.first())
            else {
                return false;
            };
            rf.name() == "v_readfirstlane_b32"
                && block.insts.iter().enumerate().any(|(j, c)| {
                    j > i
                        && j < n.saturating_sub(2)
                        && c.name().starts_with("v_cmpx_eq_")
                        && c.write(Reg::S(EXEC)).is_some()
                        && [v, k].iter().all(|x| c.reads.iter().any(|r| r.1 == *x))
                        && block.insts[j + 1..n - 2]
                            .iter()
                            .all(|later| later.write(Reg::S(EXEC)).is_none())
                })
        });
        matches!(inst.name(), "s_andn2_wrexec_b32" | "s_andn2_wrexec_b64")
            && inst.write(Reg::S(EXEC)).is_some()
            && super::super::loop_limit() >= 64
            && keyed
            && n >= 2
            && std::ptr::eq(&block.insts[n - 2], inst)
            && block.insts[n - 1].name() == "s_cbranch_execnz"
            && matches!(block.exit, Exit::Branch { taken, .. } if taken == b)
            && inst
                .write(Reg::Scc)
                .is_some_and(|scc| block.phis.iter().all(|phi| !phi.args.contains(&scc)))
    }
}
