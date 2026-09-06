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

    /// Current bit offset into the stream (for tests and future mmap skip hooks).
    pub fn position(&self) -> usize {
        self.pos
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
            // `1u64 << 64` overflows; legitimate Elias-δ never needs len_l >= 64.
            if len_l >= 64 {
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
    // Each Elias-δ value needs ≥1 bit; reject absurd counts before allocating.
    let max_bits = bytes.len().saturating_mul(8);
    if count > max_bits {
        anyhow::bail!("posting count {count} exceeds bitstream capacity ({max_bits} bits)");
    }
    let mut r = BitReader::new(bytes);
    let mut out = Vec::with_capacity(count);
    let mut prev = 0u32;
    for i in 0..count {
        let gap_u64 = r
            .read_delta()
            .ok_or_else(|| anyhow::anyhow!("truncated or corrupt Elias-δ stream"))?;
        let gap: u32 = gap_u64
            .try_into()
            .map_err(|_| anyhow::anyhow!("Elias-δ gap does not fit in u32"))?;
        let p = if i == 0 {
            gap.checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("bad first Elias-δ gap"))?
        } else {
            prev.checked_add(gap)
                .ok_or_else(|| anyhow::anyhow!("position overflow while decoding postings"))?
        };
        out.push(p);
        prev = p;
    }
    Ok(out)
}

/// One occurrence position inside a posting list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OccPos {
    pub doc_id: u32,
    pub line: u32,
    pub col: u32,
}

/// Encode occurrence payloads for one posting list (sorted by doc_id, line, col).
pub fn encode_occurrences(occs: &[OccPos]) -> Vec<u8> {
    if occs.is_empty() {
        return Vec::new();
    }
    let mut w = BitWriter::new();
    let mut prev_doc = 0u32;
    for (i, o) in occs.iter().enumerate() {
        if i == 0 || o.doc_id != prev_doc {
            w.write_bit(false);
            let gap = if i == 0 {
                o.doc_id.checked_add(1).expect("doc_id overflow")
            } else {
                assert!(o.doc_id > prev_doc, "occurrences must be sorted by doc_id");
                o.doc_id - prev_doc
            };
            w.write_delta(u64::from(gap));
            prev_doc = o.doc_id;
        } else {
            w.write_bit(true);
        }
        w.write_delta(u64::from(o.line) + 1);
        w.write_delta(u64::from(o.col) + 1);
    }
    w.finish()
}

fn decode_occurrence(
    r: &mut BitReader<'_>,
    index: usize,
    doc_id: &mut u32,
) -> anyhow::Result<OccPos> {
    let same_doc = r
        .read_bit()
        .ok_or_else(|| anyhow::anyhow!("truncated occurrence posting"))?;
    if index == 0 && same_doc {
        anyhow::bail!("invalid occurrence posting: first entry cannot set same_doc");
    }
    if !same_doc {
        let gap_u64 = r
            .read_delta()
            .ok_or_else(|| anyhow::anyhow!("truncated doc_id in occurrence posting"))?;
        let gap: u32 = gap_u64
            .try_into()
            .map_err(|_| anyhow::anyhow!("doc_id gap does not fit in u32"))?;
        *doc_id = if index == 0 {
            gap.checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("bad first doc_id gap"))?
        } else {
            doc_id
                .checked_add(gap)
                .ok_or_else(|| anyhow::anyhow!("doc_id overflow in occurrence posting"))?
        };
    }
    let line_u64 = r
        .read_delta()
        .ok_or_else(|| anyhow::anyhow!("truncated line in occurrence posting"))?;
    let line_raw: u32 = line_u64
        .try_into()
        .map_err(|_| anyhow::anyhow!("line does not fit in u32"))?;
    let line = line_raw
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("invalid line value in occurrence posting"))?;
    let col_u64 = r
        .read_delta()
        .ok_or_else(|| anyhow::anyhow!("truncated col in occurrence posting"))?;
    let col_raw: u32 = col_u64
        .try_into()
        .map_err(|_| anyhow::anyhow!("col does not fit in u32"))?;
    let col = col_raw
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("invalid col value in occurrence posting"))?;
    Ok(OccPos {
        doc_id: *doc_id,
        line,
        col,
    })
}

/// Decode occurrence payloads from a posting blob.
///
/// Decodes at most `count` entries (the stored posting length). When `limit` is
/// set, stops after that many hits so locate/search can early-exit without
/// decoding unused tails. Hit order follows posting list order (doc_id, line, col).
pub fn decode_occurrences(
    bytes: &[u8],
    count: usize,
    limit: Option<usize>,
) -> anyhow::Result<Vec<OccPos>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let decode_count = match limit {
        Some(0) => return Ok(Vec::new()),
        Some(l) => l.min(count),
        None => count,
    };
    // Each occurrence needs at least one flag bit plus two Elias-δ values.
    let max_bits = bytes.len().saturating_mul(8);
    if decode_count > max_bits {
        anyhow::bail!(
            "occurrence decode count {decode_count} exceeds bitstream capacity ({max_bits} bits)"
        );
    }
    let mut r = BitReader::new(bytes);
    let mut out = Vec::with_capacity(decode_count);
    let mut doc_id = 0u32;
    for i in 0..decode_count {
        out.push(decode_occurrence(&mut r, i, &mut doc_id)?);
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
    fn decode_rejects_gap_not_fitting_u32() {
        // Manually craft Elias-δ for a value > u32::MAX.
        let mut w = BitWriter::new();
        w.write_delta(u64::from(u32::MAX) + 2); // first gap = pos+1
        let enc = w.finish();
        assert!(decode_gaps(&enc, 1).is_err());
    }

    #[test]
    fn decode_rejects_huge_count() {
        assert!(decode_gaps(&[0xff], 10_000).is_err());
    }

    #[test]
    fn read_delta_rejects_len_l_64() {
        // Elias-δ length unary of 64 zeros would make 1u64<<64; must error.
        let mut w = BitWriter::new();
        for _ in 0..64 {
            w.write_bit(false);
        }
        w.write_bit(true); // would terminate unary if we allowed it
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_delta(), None);
    }

    #[test]
    #[should_panic(expected = "strictly increasing")]
    fn gaps_reject_duplicates() {
        let _ = encode_gaps(&[1, 1, 2]);
    }

    #[test]
    fn occurrence_roundtrip_single_doc() {
        let occs = vec![
            OccPos {
                doc_id: 0,
                line: 10,
                col: 5,
            },
            OccPos {
                doc_id: 0,
                line: 10,
                col: 20,
            },
            OccPos {
                doc_id: 0,
                line: 42,
                col: 1,
            },
        ];
        let enc = encode_occurrences(&occs);
        let dec = decode_occurrences(&enc, occs.len(), None).unwrap();
        assert_eq!(dec, occs);
    }

    #[test]
    fn occurrence_roundtrip_multi_doc() {
        let occs = vec![
            OccPos {
                doc_id: 0,
                line: 1,
                col: 0,
            },
            OccPos {
                doc_id: 2,
                line: 100,
                col: 50,
            },
            OccPos {
                doc_id: 2,
                line: 200,
                col: 0,
            },
            OccPos {
                doc_id: 5,
                line: 1,
                col: 1,
            },
        ];
        let enc = encode_occurrences(&occs);
        let dec = decode_occurrences(&enc, occs.len(), None).unwrap();
        assert_eq!(dec, occs);
    }

    #[test]
    fn occurrence_decode_rejects_huge_count() {
        assert!(decode_occurrences(&[0xff], 10_000, None).is_err());
    }

    #[test]
    fn occurrence_decode_rejects_same_doc_on_first_entry() {
        let mut w = BitWriter::new();
        w.write_bit(true); // invalid: same_doc on first entry
        w.write_delta(1);
        w.write_delta(1);
        let enc = w.finish();
        assert!(decode_occurrences(&enc, 1, None).is_err());
    }

    #[test]
    fn occurrence_empty() {
        assert!(encode_occurrences(&[]).is_empty());
        assert!(decode_occurrences(&[], 0, None).unwrap().is_empty());
    }

    fn many_occurrences(n: usize) -> Vec<OccPos> {
        (0..n)
            .map(|i| OccPos {
                doc_id: 0,
                line: (i + 1) as u32,
                col: 0,
            })
            .collect()
    }

    #[test]
    fn decode_occurrences_respects_limit() {
        let occs = many_occurrences(200);
        let enc = encode_occurrences(&occs);
        let limited = decode_occurrences(&enc, occs.len(), Some(5)).unwrap();
        let full_prefix = decode_occurrences(&enc, occs.len(), None)
            .unwrap()
            .into_iter()
            .take(5)
            .collect::<Vec<_>>();
        assert_eq!(limited.len(), 5);
        assert_eq!(limited, full_prefix);
    }

    #[test]
    fn decode_occurrences_limit_stops_before_full_decode() {
        let occs = many_occurrences(100);
        let enc = encode_occurrences(&occs);

        let mut r_limited = BitReader::new(&enc);
        let mut doc_id = 0u32;
        for i in 0..10 {
            decode_occurrence(&mut r_limited, i, &mut doc_id).unwrap();
        }
        let limited_pos = r_limited.position();

        let mut r_full = BitReader::new(&enc);
        doc_id = 0;
        for i in 0..occs.len() {
            decode_occurrence(&mut r_full, i, &mut doc_id).unwrap();
        }
        let full_pos = r_full.position();

        assert!(
            limited_pos < full_pos,
            "limited={limited_pos} full={full_pos}"
        );
    }

    #[test]
    fn decode_occurrences_limit_zero_returns_empty() {
        let occs = many_occurrences(10);
        let enc = encode_occurrences(&occs);
        assert!(
            decode_occurrences(&enc, occs.len(), Some(0))
                .unwrap()
                .is_empty()
        );
    }
}
