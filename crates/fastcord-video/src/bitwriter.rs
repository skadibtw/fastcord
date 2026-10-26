//! The one MSB-first RBSP writer and emulation-prevention escaper, shared by
//! the VA-API encoder and the test vectors.

#![cfg_attr(not(test), allow(dead_code))]

/// MSB-first RBSP writer.
#[derive(Default)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    used: u8,
}

impl BitWriter {
    pub(crate) fn flag(&mut self, value: bool) {
        self.current = (self.current << 1) | u8::from(value);
        self.used += 1;
        if self.used == 8 {
            self.bytes.push(self.current);
            self.current = 0;
            self.used = 0;
        }
    }

    /// Writes the low `count` bits of `value`, most significant first.
    pub(crate) fn bits(&mut self, value: u64, count: u32) {
        for bit in (0..count).rev() {
            self.flag((value >> bit) & 1 == 1);
        }
    }

    /// Unsigned Exp-Golomb.
    pub(crate) fn ue(&mut self, value: u32) {
        let code = u64::from(value) + 1;
        let length = 64 - code.leading_zeros();
        self.bits(0, length - 1);
        self.bits(code, length);
    }

    /// Signed Exp-Golomb.
    pub(crate) fn se(&mut self, value: i32) {
        let value = i64::from(value);
        let code = if value > 0 { 2 * value - 1 } else { -2 * value };
        self.ue(u32::try_from(code).expect("se value fits"));
    }

    /// Zero bits up to the next byte boundary.
    pub(crate) fn align_zero(&mut self) {
        while self.used != 0 {
            self.flag(false);
        }
    }

    /// A byte at a byte boundary.
    pub(crate) fn byte(&mut self, value: u8) {
        assert_eq!(self.used, 0, "unaligned byte");
        self.bytes.push(value);
    }

    /// rbsp_trailing_bits: a one bit, then zero bits to the byte boundary.
    pub(crate) fn trailing(&mut self) {
        self.flag(true);
        self.align_zero();
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        assert_eq!(self.used, 0, "RBSP not byte aligned");
        self.bytes
    }
}

/// Inserts emulation-prevention bytes into an RBSP.
pub(crate) fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64);
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}
