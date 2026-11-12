//! Mergeable latency histograms: what the 24h p50/p95/p99 are read from.
//!
//! Sorting a day of raw samples per monitor to rank them cost ~20 s on a
//! 575M-row database. A histogram per hourly roll-up (stored with the bucket,
//! see `db::retention`) merges in microseconds instead, and only the raw rows
//! not yet rolled up are ever binned on the fly.
//!
//! Bucketing is log-linear (HDR style): values below 64 ms are exact, above
//! that every power of two is split into 32 equal sub-buckets. A percentile
//! is answered with its bucket's midpoint, so it is within 1/64 (±1.6%) of a
//! sample that really holds that rank - and exact below 64 ms. A bucket is a
//! `u16` index (at most 895, for `u32::MAX` ms), so a histogram stays small
//! however many samples it counts.

/// Values below this many ms each get their own bucket.
const LINEAR_BELOW: u64 = 64;
/// Sub-buckets per power of two above [`LINEAR_BELOW`] (`2^SUB_BITS`).
const SUB_BITS: u32 = 5;
/// Leading byte of the stored encoding, so it can evolve.
const ENCODING_V1: u8 = 1;

/// Sample counts per latency bucket. Dense (indexed by bucket, up to the
/// highest one in use - a couple of hundred for second-scale latencies), so
/// merging the day's hourly histograms is a vector addition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LatencyHistogram {
    /// Never ends with a zero: equal histograms compare equal.
    counts: Vec<u64>,
}

impl LatencyHistogram {
    /// Count `count` samples of `latency_ms`. Negative values count as 0.
    pub fn record(&mut self, latency_ms: i64, count: u64) {
        self.add(usize::from(bucket_of(latency_ms)), count);
    }

    /// Uncount `count` samples of `latency_ms` (a window sliding past them).
    /// Saturating: never below zero.
    pub fn unrecord(&mut self, latency_ms: i64, count: u64) {
        let bucket = usize::from(bucket_of(latency_ms));
        if let Some(slot) = self.counts.get_mut(bucket) {
            *slot = slot.saturating_sub(count);
        }
        while self.counts.last() == Some(&0) {
            self.counts.pop();
        }
    }

    fn add(&mut self, bucket: usize, count: u64) {
        if count == 0 {
            return;
        }
        if self.counts.len() <= bucket {
            self.counts.resize(bucket + 1, 0);
        }
        // Saturating, here and wherever counts are summed: a count is read
        // back from a stored blob, which may hold anything.
        self.counts[bucket] = self.counts[bucket].saturating_add(count);
    }

    /// Add every sample of `other`.
    pub fn merge(&mut self, other: &Self) {
        if self.counts.len() < other.counts.len() {
            self.counts.resize(other.counts.len(), 0);
        }
        for (mine, theirs) in self.counts.iter_mut().zip(&other.counts) {
            *mine = mine.saturating_add(*theirs);
        }
    }

    /// Add every sample of an encoded histogram ([`Self::encode`]) without
    /// materializing it. `None` (and nothing added) if it is malformed.
    pub fn merge_encoded(&mut self, bytes: &[u8]) -> Option<()> {
        // Validate the whole blob first, so a corrupt one adds nothing.
        for entry in Entries::new(bytes)? {
            entry?;
        }
        for (bucket, count) in Entries::new(bytes)?.flatten() {
            self.add(bucket, count);
        }
        Some(())
    }

    /// How many samples were recorded.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.counts
            .iter()
            .fold(0, |total: u64, count| total.saturating_add(*count))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// The nearest-rank percentile (`rank = ceil(p * n / 100)`, clamped into
    /// `[1, n]`), as its bucket's midpoint; `None` without samples.
    #[must_use]
    pub fn percentile(&self, p: u64) -> Option<i64> {
        let n = self.len();
        if n == 0 {
            return None;
        }
        // In 128 bits: `p * n` passes `u64` from 2^57 samples on.
        let rank = (u128::from(p) * u128::from(n)).div_ceil(100);
        let rank = u64::try_from(rank).unwrap_or(n).clamp(1, n);
        let mut seen: u64 = 0;
        for (bucket, &count) in self.counts.iter().enumerate() {
            seen = seen.saturating_add(count);
            if seen >= rank {
                return Some(midpoint(u64::try_from(bucket).unwrap_or(u64::MAX)));
            }
        }
        None
    }

    /// `(p50, p95, p99)`, or `None` without samples.
    #[must_use]
    pub fn p50_p95_p99(&self) -> Option<(i64, i64, i64)> {
        Some((
            self.percentile(50)?,
            self.percentile(95)?,
            self.percentile(99)?,
        ))
    }

    /// The stored form: a version byte, then `(bucket delta, count)` pairs
    /// for the non-empty buckets, as LEB128 varints - ~150 bytes for an hour
    /// of samples spread over 30-2300 ms.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![ENCODING_V1];
        let mut previous = 0;
        for (bucket, &count) in self.counts.iter().enumerate().filter(|(_, c)| **c > 0) {
            write_varint(&mut out, u64::try_from(bucket - previous).unwrap_or(0));
            write_varint(&mut out, count);
            previous = bucket;
        }
        out
    }

    /// Parse [`Self::encode`]'s output; `None` for anything malformed.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut hist = Self::default();
        hist.merge_encoded(bytes)?;
        Some(hist)
    }
}

/// The `(bucket, count)` pairs of an encoded histogram; an `Err` item where
/// it stops making sense.
struct Entries<'a> {
    rest: &'a [u8],
    bucket: usize,
}

impl<'a> Entries<'a> {
    fn new(bytes: &'a [u8]) -> Option<Self> {
        let (&version, rest) = bytes.split_first()?;
        (version == ENCODING_V1).then_some(Self { rest, bucket: 0 })
    }
}

impl Iterator for Entries<'_> {
    type Item = Option<(usize, u64)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let entry = (|| {
            let delta = usize::try_from(read_varint(&mut self.rest)?).ok()?;
            let count = read_varint(&mut self.rest)?;
            self.bucket = self.bucket.checked_add(delta)?;
            (self.bucket <= MAX_BUCKET).then_some((self.bucket, count))
        })();
        if entry.is_none() {
            self.rest = &[];
        }
        Some(entry)
    }
}

/// The highest bucket (`u32::MAX` ms and above).
const MAX_BUCKET: usize = 895;

/// The bucket holding `latency_ms`.
fn bucket_of(latency_ms: i64) -> u16 {
    let value = u64::try_from(latency_ms.clamp(0, i64::from(u32::MAX))).unwrap_or(0);
    if value < LINEAR_BELOW {
        return u16::try_from(value).unwrap_or(0);
    }
    // value in [2^e, 2^(e+1)), e >= 6: keep the top SUB_BITS + 1 bits.
    let e = value.ilog2();
    let shift = e - SUB_BITS;
    let index = u64::from(shift) * (1 << SUB_BITS) + (value >> shift);
    u16::try_from(index).unwrap_or(u16::MAX)
}

/// The value a bucket answers with: its midpoint (exact for linear buckets).
fn midpoint(bucket: u64) -> i64 {
    if bucket < LINEAR_BELOW {
        return i64::try_from(bucket).unwrap_or(0);
    }
    let sub = 1_u64 << SUB_BITS;
    // Inverse of `bucket_of`: index = shift * 32 + (value >> shift), where
    // (value >> shift) is in [32, 64).
    let shift = bucket / sub - 1;
    let mantissa = bucket - shift * sub;
    let low = mantissa << shift;
    let width = 1_u64 << shift;
    i64::try_from(low + width / 2).unwrap_or(i64::MAX)
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(u8::try_from(value & 0x7f).unwrap_or(0) | 0x80);
        value >>= 7;
    }
    out.push(u8::try_from(value).unwrap_or(0));
}

fn read_varint(input: &mut &[u8]) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = input.split_first()?;
        *input = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The nearest-rank percentile of raw samples, the exact reference.
    fn exact(samples: &mut [i64], p: u64) -> i64 {
        samples.sort_unstable();
        let n = samples.len() as u64;
        let rank = (p * n).div_ceil(100).clamp(1, n);
        samples[usize::try_from(rank - 1).unwrap()]
    }

    #[test]
    fn small_values_are_exact() {
        let mut hist = LatencyHistogram::default();
        for value in 0..64 {
            hist.record(value, 1);
            assert_eq!(midpoint(bucket_of(value).into()), value);
        }
        assert_eq!(hist.percentile(50), Some(31));
        assert_eq!(hist.percentile(100), Some(63));
    }

    #[test]
    fn every_value_lands_within_the_error_bound() {
        // Values around every scale up to u32::MAX: buckets never go down as
        // values grow, and the midpoint stays within 1/64 of the value.
        let mut previous = 0;
        let mut value: i64 = 1;
        while value < i64::from(u32::MAX) {
            for probe in [value - 1, value, value + 1] {
                let bucket = bucket_of(probe);
                assert!(bucket >= previous, "monotonic at {probe}");
                previous = bucket;
                let mid = midpoint(bucket.into());
                assert!((mid - probe).abs() * 64 <= probe.max(1), "{probe} -> {mid}");
            }
            value = (value * 9 / 8).max(value + 3);
        }
        assert_eq!(bucket_of(i64::MAX), bucket_of(i64::from(u32::MAX)));
        assert_eq!(usize::from(bucket_of(i64::from(u32::MAX))), MAX_BUCKET);
        assert_eq!(bucket_of(-5), 0);
    }

    #[test]
    fn percentiles_match_nearest_rank_within_the_bound() {
        // A skewed mix: fast bulk, a slow tail, an outlier.
        let mut samples: Vec<i64> = (0..1000).map(|i| 30 + (i * 37) % 150).collect();
        samples.extend((0..40).map(|i| 1500 + i * 20));
        samples.push(29_000);
        let mut hist = LatencyHistogram::default();
        for &sample in &samples {
            hist.record(sample, 1);
        }
        for p in [50, 95, 99] {
            let want = exact(&mut samples, p);
            let got = hist.percentile(p).unwrap();
            assert!((got - want).abs() * 64 <= want, "p{p}: {got} vs {want}");
        }
        assert_eq!(LatencyHistogram::default().p50_p95_p99(), None);
    }

    #[test]
    fn merging_equals_recording_everything() {
        let mut a = LatencyHistogram::default();
        let mut b = LatencyHistogram::default();
        let mut all = LatencyHistogram::default();
        for value in [5, 70, 70, 900, 12_345] {
            a.record(value, 2);
            all.record(value, 2);
        }
        for value in [5, 64, 100_000] {
            b.record(value, 3);
            all.record(value, 3);
        }
        a.merge(&b);
        assert_eq!(a, all);
        assert_eq!(a.len(), 19);
        // Uncounting is the inverse, down to an equal (trimmed) histogram.
        for value in [5, 64, 100_000] {
            a.unrecord(value, 3);
        }
        a.unrecord(1, 50);
        let mut first = LatencyHistogram::default();
        for value in [5, 70, 70, 900, 12_345] {
            first.record(value, 2);
        }
        assert_eq!(a, first);
    }

    /// A stored blob holds whatever counts its bytes say. Found by the
    /// `histogram` fuzz target: two buckets of 2^62 samples overflowed the
    /// rank (`p * n`), and merging the blob twice overflowed a bucket.
    #[test]
    fn counts_beyond_any_real_sample_count_saturate() {
        let mut blob = vec![ENCODING_V1];
        for (delta, count) in [(0, 1 << 62), (3, 1 << 62)] {
            write_varint(&mut blob, delta);
            write_varint(&mut blob, count);
        }
        let mut hist = LatencyHistogram::decode(&blob).expect("well formed");
        assert_eq!(hist.len(), 1 << 63);
        assert_eq!(hist.p50_p95_p99(), Some((0, 3, 3)));
        // A day of such hours: every sum stops at the top instead of wrapping.
        for _ in 0..4 {
            hist.merge_encoded(&blob).expect("well formed");
        }
        hist.merge(&hist.clone());
        hist.record(3, u64::MAX);
        assert_eq!(hist.len(), u64::MAX);
        assert!(hist.p50_p95_p99().is_some());
        assert_eq!(LatencyHistogram::decode(&hist.encode()), Some(hist));
    }

    #[test]
    fn encoding_round_trips_and_rejects_garbage() {
        let mut hist = LatencyHistogram::default();
        for (value, count) in [(0, 1), (63, 300), (64, 2), (5_000, 1 << 40), (i64::MAX, 7)] {
            hist.record(value, count);
        }
        let bytes = hist.encode();
        assert_eq!(LatencyHistogram::decode(&bytes), Some(hist));
        // An empty histogram is the version byte alone, distinct from NULL.
        let empty = LatencyHistogram::default().encode();
        assert_eq!(empty, vec![ENCODING_V1]);
        assert_eq!(
            LatencyHistogram::decode(&empty),
            Some(LatencyHistogram::default())
        );
        assert_eq!(LatencyHistogram::decode(&[]), None);
        assert_eq!(LatencyHistogram::decode(&[9, 1, 1]), None);
        assert_eq!(LatencyHistogram::decode(&[ENCODING_V1, 0x80]), None);
        // Past the last bucket: corrupt, and merging it adds nothing.
        let mut target = LatencyHistogram::default();
        let mut far = vec![ENCODING_V1];
        write_varint(&mut far, 5);
        write_varint(&mut far, 1);
        write_varint(&mut far, 2000);
        write_varint(&mut far, 1);
        assert_eq!(target.merge_encoded(&far), None);
        assert!(target.is_empty());
    }
}
