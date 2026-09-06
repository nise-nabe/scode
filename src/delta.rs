//! Gap + Elias-δ posting list encoding.

/// Bit writer for Elias codes.
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    bytes: Vec<u8>,
    bit: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write_bit(&mut self, bit: bool) {
        if self.bit == 0 {
            self.bytes.push(0);
        }
        if bit {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (7 - self.bit);
        }
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
        }
    }

    pub fn write_bits(&mut self, value: u64, nbits: u32) {
        for i in (0..nbits).rev() {
            self.write_bit(((value >> i) & 1) == 1);
        }
    }

    /// Elias-δ encode a positive integer (>= 1).
    pub fn write_delta(&mut self, n: u64) {
        assert!(n >= 1, "Elias-δ encodes positive integers");
        // Elias-δ(x): Elias-γ(⌊log2(x)⌋+1) then the ⌊log2(x)⌋ LSBs of x.
        let l = n.ilog2();
        let len = l + 1;
        let len_l = len.ilog2();
        for _ in 0..len_l {
            self.write_bit(false);
        }
        self.write_bits(u64::from(len), len_l + 1);
        if l > 0 {
            let mask = (1u64 << l) - 1;
            self.write_bits(n & mask, l);
        }
    }

    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Bit reader for Elias codes.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub fn read_bit(&mut self) -> Option<bool> {
        let byte_i = self.pos / 8;
        if byte_i >= self.bytes.len() {
            return None;
        }
        let bit_i = self.pos % 8;
        let bit = ((self.bytes[byte_i] >> (7 - bit_i)) & 1) == 1;
        self.pos += 1;
        Some(bit)
    }

    pub fn read_bits(&mut self, nbits: u32) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..nbits {
            v = (v << 1) | u64::from(self.read_bit()?);
        }
        Some(v)
    }

    pub fn read_delta(&mut self) -> Option<u64> {
        let mut len_l = 0u32;
        loop {
            let b = self.read_bit()?;
            if b {
                break;
            }
            len_l += 1;
            if len_l > 64 {
                return None;
            }
        }
        let rest = if len_l == 0 {
            0
        } else {
            self.read_bits(len_l)?
        };
        let len = (1u64 << len_l) | rest;
        if len == 0 || len > 64 {
            return None;
        }
        let l = (len - 1) as u32;
        let low = if l == 0 { 0 } else { self.read_bits(l)? };
        Some((1u64 << l) | low)
    }
}

/// Encode a strictly increasing sequence of positions as gap+Elias-δ.
///
/// # Panics
/// Panics if `positions` contains duplicates or is not sorted ascending
/// (Elias-δ gaps must be ≥ 1).
pub fn encode_gaps(positions: &[u32]) -> Vec<u8> {
    if positions.is_empty() {
        return Vec::new();
    }
    let mut w = BitWriter::new();
    let mut prev = 0u32;
    for (i, &p) in positions.iter().enumerate() {
        let gap = if i == 0 {
            p.checked_add(1).expect("position overflow")
        } else {
            assert!(
                p > prev,
                "positions must be strictly increasing (got {prev} then {p})"
            );
            p - prev
        };
        w.write_delta(u64::from(gap));
        prev = p;
    }
    w.finish()
}

/// Decode `count` positions from gap+Elias-δ bytes.
pub fn decode_gaps(bytes: &[u8], count: usize) -> anyhow::Result<Vec<u32>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut r = BitReader::new(bytes);
    let mut out = Vec::with_capacity(count);
    let mut prev = 0u32;
    for i in 0..count {
        let gap = r
            .read_delta()
            .ok_or_else(|| anyhow::anyhow!("truncated or corrupt Elias-δ stream"))?
            as u32;
        let p = if i == 0 {
            gap.checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("bad first Elias-δ gap"))?
        } else {
            prev
                .checked_add(gap)
                .ok_or_else(|| anyhow::anyhow!("position overflow while decoding postings"))?
        };
        out.push(p);
        prev = p;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_roundtrip_small() {
        let mut w = BitWriter::new();
        for n in 1..=32u64 {
            w.write_delta(n);
        }
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        for n in 1..=32u64 {
            assert_eq!(r.read_delta(), Some(n));
        }
    }

    #[test]
    fn gap_roundtrip() {
        let positions = vec![0, 1, 2, 5, 100, 101, 1000, 50_000];
        let enc = encode_gaps(&positions);
        let dec = decode_gaps(&enc, positions.len()).unwrap();
        assert_eq!(dec, positions);
    }

    #[test]
    fn empty_gaps() {
        assert!(encode_gaps(&[]).is_empty());
        assert!(decode_gaps(&[], 0).unwrap().is_empty());
    }

    #[test]
    fn decode_rejects_truncated() {
        assert!(decode_gaps(&[], 1).is_err());
        let enc = encode_gaps(&[1, 2, 3]);
        assert!(decode_gaps(&enc, 4).is_err());
    }

    #[test]
    #[should_panic(expected = "strictly increasing")]
    fn gaps_reject_duplicates() {
        let _ = encode_gaps(&[1, 1, 2]);
    }
}