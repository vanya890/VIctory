//! Preserve the previous VGPR value across guarded pixel interpolation writes.

use super::{Bin, Interp, Lower, Site, V, Val, Why};
use crate::gfx10::{Fields, Operand};
use crate::ir::{Inst, Reg, Value};

impl Lower<'_> {
    /// [`Self::write_placed`], preserving `old` in lanes outside `guard`.
    pub(super) fn write_placed_with_old(
        &mut self,
        inst: &Inst,
        reg: Reg,
        x: V,
        guard: Option<V>,
        old: Option<Value>,
    ) {
        let Some(new) = inst.write(reg) else { return };
        let x = match (guard, reg) {
            (Some(g), Reg::V(_)) => {
                let read = old.is_some_and(|o| self.read_phis.contains(&o));
                let prev = match old.map(|o| self.i32_of(o)) {
                    Some(Ok(o)) => o,
                    // The validator refuses reads of undef.
                    _ => self.s.m.i32(0),
                };
                // What lanes keep is zero where it has no DXIL form (as
                // above), so keeping a phi does not make it live.
                if let Some(o) = old.filter(|_| !read) {
                    self.read_phis.remove(&o);
                }
                self.s.m.select(self.cur, g, x, prev)
            }
            _ => x,
        };
        self.vals.insert(new, Val::I32(x));
    }

    /// [`Self::write_half`], preserving `guard_old` (when given, else the
    /// instruction's own read of `vdst`) in lanes outside `guard`.
    pub(super) fn write_half_with_old(
        &mut self,
        inst: &Inst,
        x: V,
        guard: Option<V>,
        guard_old: Option<Value>,
    ) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a 16-bit result outside a VGPR");
        };
        let mut high = v.opsel >> 3 & 1 != 0;
        // SDWA: the word written, the other one kept (preserve) or zero
        // (pad; a sign extension above the high word adds nothing).
        let mut keep = true;
        if let Some(sd) = v.sdwa {
            match (sd.dst_sel, sd.dst_unused) {
                (4, 2) => {}
                (5, 2) => high = true,
                (4 | 6, _) => keep = false,
                (5, _) => (high, keep) = (true, false),
                _ => return Err("an SDWA destination of a 16-bit operation"),
            }
        }
        let c = self.cur;
        let old = match inst.read(Reg::V(reg)).map(|o| self.i32_of(o)) {
            Some(Ok(o)) if keep => o,
            _ => self.s.m.i32(0),
        };
        let (k16, low) = (self.s.m.i32(16), self.s.m.i32(0xFFFF));
        let x = self.s.m.bin(c, Bin::And, x, low);
        let r = if high {
            let o = self.s.m.bin(c, Bin::And, old, low);
            let x = self.s.m.bin(c, Bin::Shl, x, k16);
            self.s.m.bin(c, Bin::Or, o, x)
        } else {
            let keep = self.s.m.i32(0xFFFF_0000);
            let o = self.s.m.bin(c, Bin::And, old, keep);
            self.s.m.bin(c, Bin::Or, o, x)
        };
        let old_for_guard = guard_old.or_else(|| inst.read(Reg::V(reg)));
        self.write_placed_with_old(inst, Reg::V(reg), r, guard, old_for_guard);
        Ok(())
    }

    /// The VGPR value to retain outside this interpolation's guard. If the
    /// paired first half wrote the same VGPR under a compatible guard, use
    /// the value it read before that write instead of the missing value.
    pub(super) fn interp_old(
        &self,
        inst: &Inst,
        site: Site,
        reg: u8,
        interp: &Interp,
    ) -> Option<Value> {
        let old = inst.read(Reg::V(reg))?;
        if !matches!(self.vals.get(&old), Some(Val::Missing(_))) {
            return Some(old);
        }
        let Interp::Interpolated { first, .. } = interp else {
            return Some(old);
        };
        let (b, i) = site;
        let g2 = self.x.st.guards[b][i]?;
        for &(b1, i1) in first {
            if b1 != b
                && self
                    .x
                    .st
                    .bools
                    .meet(&[(self.x.st.reach[b], true), (self.x.st.reach[b1], false)])
            {
                continue;
            }
            let p1 = &self.x.p.blocks[b1].insts[i1];
            if p1.write(Reg::V(reg)) != Some(old) {
                continue;
            }
            let Some(g1) = self.x.st.guards[b1][i1] else {
                continue;
            };
            if !self
                .x
                .st
                .bools
                .meet(&[(self.x.st.reach[b], true), (g2, false), (g1, true)])
            {
                return p1.read(Reg::V(reg));
            }
        }
        Some(old)
    }
}
