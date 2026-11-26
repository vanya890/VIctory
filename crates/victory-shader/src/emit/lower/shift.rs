use super::{Bin, Lower, V, pred};

impl Lower<'_> {
    /// A 64-bit shift of the dwords `a` (low) and `b` (high) by the 32-bit
    /// count `n`, as the two dwords of the result: [`Bin::Shl`],
    /// [`Bin::LShr`] or [`Bin::AShr`].
    ///
    /// A 64-bit DXIL shift needs the SFI0 INT64 feature. To use only 32-bit
    /// shifts, take the low five bits of the hardware's six-bit count as
    /// the within-dword count and use the sixth bit to select the result
    /// for counts of 32 or more.
    pub(super) fn shift64(&mut self, a: V, b: V, n: V, op: Bin) -> (V, V) {
        let c = self.cur;
        let (k31, k32, k63) = (self.s.m.i32(31), self.s.m.i32(32), self.s.m.i32(63));
        let zero = self.s.m.i32(0);
        let n = self.s.m.bin(c, Bin::And, n, k63);
        let cnt = self.s.m.bin(c, Bin::And, n, k31);
        let big = self.s.m.cmp(c, pred::IUGE, n, k32);
        let small = self.s.m.cmp(c, pred::EQ, cnt, zero);
        // Keep the complementary shift in range even when `cnt` is zero.
        // The `small` selects discard the cross-dword bits in that case.
        let back = self.s.m.bin(c, Bin::Sub, k32, cnt);
        let back = self.s.m.bin(c, Bin::And, back, k31);
        // The dwords the count moves between: `a`'s top bits for a shift
        // left, `b`'s bottom ones otherwise.
        let across = |l: &mut Self, x: V, k: V, left: bool| {
            let op = if left { Bin::LShr } else { Bin::Shl };
            l.s.m.bin(c, op, x, k)
        };
        match op {
            Bin::Shl => {
                let a_up = self.s.m.bin(c, Bin::Shl, a, cnt);
                let b_up = self.s.m.bin(c, Bin::Shl, b, cnt);
                let a_down = across(self, a, back, true);
                let hi = self.s.m.bin(c, Bin::Or, b_up, a_down);
                let hi = self.s.m.select(c, small, b, hi);
                let hi = self.s.m.select(c, big, a_up, hi);
                let lo = self.s.m.select(c, big, zero, a_up);
                (lo, hi)
            }
            Bin::LShr => {
                let a_down = self.s.m.bin(c, Bin::LShr, a, cnt);
                let b_up = across(self, b, back, false);
                let lo = self.s.m.bin(c, Bin::Or, a_down, b_up);
                let lo = self.s.m.select(c, small, a, lo);
                let b_down = self.s.m.bin(c, Bin::LShr, b, cnt);
                // A count of 32 or more moves the high dword down into the
                // low one, and leaves nothing above it.
                let lo = self.s.m.select(c, big, b_down, lo);
                let hi = self.s.m.select(c, big, zero, b_down);
                (lo, hi)
            }
            _ => {
                let a_down = self.s.m.bin(c, Bin::LShr, a, cnt);
                let b_up = across(self, b, back, false);
                let lo = self.s.m.bin(c, Bin::Or, a_down, b_up);
                let lo = self.s.m.select(c, small, a, lo);
                let b_down = self.s.m.bin(c, Bin::LShr, b, cnt);
                // Fill the top `cnt` bits with the sign. When `cnt` is zero,
                // keep `b` unchanged.
                let sign = self.s.m.bin(c, Bin::AShr, b, k31);
                let fill = self.s.m.bin(c, Bin::Shl, sign, back);
                let down = self.s.m.bin(c, Bin::Or, b_down, fill);
                let down = self.s.m.select(c, small, b, down);
                // A count of 32 or more moves the high dword down into the
                // low one (with the sign above it) and leaves the high dword
                // as the sign.
                let lo = self.s.m.select(c, big, down, lo);
                let hi = self.s.m.select(c, big, sign, down);
                (lo, hi)
            }
        }
    }
}
