use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};

pub struct Counter(AtomicU64);

impl Counter {
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn inc(&self) {
        self.add(1);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Counter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Counter").field(&self.get()).finish()
    }
}

impl Serialize for Counter {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.get())
    }
}

pub struct Gauge {
    value: AtomicI64,
    peak: AtomicI64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GaugeSnapshot {
    pub value: i64,
    pub peak: i64,
}

impl Gauge {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            value: AtomicI64::new(0),
            peak: AtomicI64::new(0),
        }
    }

    pub fn set(&self, v: i64) {
        self.value.store(v, Ordering::Relaxed);
        self.peak.fetch_max(v, Ordering::Relaxed);
    }

    pub fn add(&self, d: i64) {
        let now = self.value.fetch_add(d, Ordering::Relaxed).wrapping_add(d);
        self.peak.fetch_max(now, Ordering::Relaxed);
    }

    pub fn sub(&self, d: i64) {
        self.value.fetch_sub(d, Ordering::Relaxed);
    }

    pub fn inc(&self) {
        self.add(1);
    }

    pub fn dec(&self) {
        self.sub(1);
    }

    #[must_use]
    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn peak(&self) -> i64 {
        self.peak.load(Ordering::Relaxed)
    }

    pub fn reset_peak(&self) {
        self.peak.store(self.get(), Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> GaugeSnapshot {
        GaugeSnapshot {
            value: self.get(),
            peak: self.peak(),
        }
    }
}

impl Default for Gauge {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Gauge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gauge")
            .field("value", &self.get())
            .field("peak", &self.peak())
            .finish()
    }
}

impl Serialize for Gauge {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.snapshot().serialize(s)
    }
}

const SUB_BITS: u32 = 4;
const SUB: usize = 1 << SUB_BITS;
pub const BUCKETS: usize = (64 - SUB_BITS as usize + 1) * SUB;

#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub const fn bucket_of(v: u64) -> usize {
    if v < SUB as u64 {
        return v as usize;
    }
    let shift = v.ilog2() - SUB_BITS;
    let sub = ((v >> shift) as usize) & (SUB - 1);
    (shift as usize + 1) * SUB + sub
}

#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub const fn bucket_low(i: usize) -> u64 {
    if i < SUB {
        return i as u64;
    }
    let shift = (i / SUB - 1) as u32;
    let sub = (i % SUB) as u64;
    (SUB as u64 + sub) << shift
}

#[must_use]
pub const fn bucket_high(i: usize) -> u64 {
    if i + 1 >= BUCKETS {
        u64::MAX
    } else {
        bucket_low(i + 1) - 1
    }
}

pub struct LogHistogram {
    buckets: [AtomicU64; BUCKETS],
    sum: AtomicU64,
    min: AtomicU64,
    max: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistogramSnapshot {
    pub count: u64,
    pub sum: u64,
    pub min: Option<u64>,
    pub max: Option<u64>,
    pub p50: Option<u64>,
    pub p90: Option<u64>,
    pub p99: Option<u64>,
    pub buckets: Vec<[u64; 2]>,
}

impl HistogramSnapshot {
    #[must_use]
    pub fn quantile(&self, q: f64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let q = q.clamp(0.0, 1.0);
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let rank = ((q * self.count as f64).ceil() as u64).clamp(1, self.count);
        let mut seen = 0u64;
        for [high, n] in &self.buckets {
            seen += n;
            if seen >= rank {
                return Some(self.max.map_or(*high, |m| (*high).min(m)));
            }
        }
        self.max
    }
}

impl LogHistogram {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; BUCKETS],
            sum: AtomicU64::new(0),
            min: AtomicU64::new(u64::MAX),
            max: AtomicU64::new(0),
        }
    }

    pub fn record(&self, v: u64) {
        self.sum.fetch_add(v, Ordering::Relaxed);
        self.min.fetch_min(v, Ordering::Relaxed);
        self.max.fetch_max(v, Ordering::Relaxed);
        self.buckets[bucket_of(v)].fetch_add(1, Ordering::Release);
    }

    #[must_use]
    pub fn count(&self) -> u64 {
        self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum()
    }

    pub fn reset(&self) {
        for b in &self.buckets {
            b.store(0, Ordering::Relaxed);
        }
        self.sum.store(0, Ordering::Relaxed);
        self.min.store(u64::MAX, Ordering::Relaxed);
        self.max.store(0, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> HistogramSnapshot {
        let mut buckets = Vec::new();
        let mut count = 0u64;
        for (i, b) in self.buckets.iter().enumerate() {
            let n = b.load(Ordering::Acquire);
            if n > 0 {
                count += n;
                buckets.push([bucket_high(i), n]);
            }
        }
        let (min, max) = if count == 0 {
            (None, None)
        } else {
            (
                Some(self.min.load(Ordering::Relaxed)),
                Some(self.max.load(Ordering::Relaxed)),
            )
        };
        let mut snap = HistogramSnapshot {
            count,
            sum: self.sum.load(Ordering::Relaxed),
            min,
            max,
            p50: None,
            p90: None,
            p99: None,
            buckets,
        };
        snap.p50 = snap.quantile(0.50);
        snap.p90 = snap.quantile(0.90);
        snap.p99 = snap.quantile(0.99);
        snap
    }
}

impl Default for LogHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for LogHistogram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.snapshot();
        f.debug_struct("LogHistogram")
            .field("count", &s.count)
            .field("p50", &s.p50)
            .field("p99", &s.p99)
            .field("max", &s.max)
            .finish()
    }
}

impl Serialize for LogHistogram {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.snapshot().serialize(s)
    }
}

pub trait Label: Copy + Eq + 'static {
    const ALL: &'static [Self];
    fn index(self) -> usize;
    fn name(self) -> &'static str;
}

pub struct Family<L: Label, const N: usize> {
    cells: [Counter; N],
    label: PhantomData<fn() -> L>,
}

impl<L: Label, const N: usize> Family<L, N> {
    /// ```
    /// kanshou::metric_labels! {
    ///     enum Pair {
    ///         One = "one",
    ///         Two = "two",
    ///     }
    /// }
    /// static EXACT: kanshou::metrics::Family<Pair, 2> = kanshou::metrics::Family::new();
    /// EXACT.inc(Pair::One);
    /// ```
    ///
    /// ```compile_fail,E0080
    /// kanshou::metric_labels! {
    ///     enum Pair {
    ///         One = "one",
    ///         Two = "two",
    ///     }
    /// }
    /// static NARROW: kanshou::metrics::Family<Pair, 3> = kanshou::metrics::Family::new();
    /// NARROW.inc(Pair::One);
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        assert!(
            N == L::ALL.len(),
            "a Family's width must equal its label set"
        );
        Self {
            cells: [const { Counter::new() }; N],
            label: PhantomData,
        }
    }

    pub fn inc(&self, l: L) {
        self.cells[l.index()].inc();
    }

    pub fn add(&self, l: L, n: u64) {
        self.cells[l.index()].add(n);
    }

    #[must_use]
    pub fn get(&self, l: L) -> u64 {
        self.cells[l.index()].get()
    }

    #[must_use]
    pub fn total(&self) -> u64 {
        self.cells.iter().map(Counter::get).sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = (L, u64)> + '_ {
        L::ALL.iter().map(move |l| (*l, self.get(*l)))
    }

    pub fn reset(&self) {
        for c in &self.cells {
            c.reset();
        }
    }
}

impl<L: Label, const N: usize> Default for Family<L, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<L: Label, const N: usize> fmt::Debug for Family<L, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.iter().map(|(l, n)| (l.name(), n)))
            .finish()
    }
}

impl<L: Label, const N: usize> Serialize for Family<L, N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(N))?;
        for (l, n) in self.iter() {
            m.serialize_entry(l.name(), &n)?;
        }
        m.end()
    }
}

#[macro_export]
macro_rules! metric_labels {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$vmeta:meta])* $variant:ident = $wire:literal),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            #[allow(dead_code)]
            $vis const COUNT: usize = <Self as $crate::metrics::Label>::ALL.len();
        }

        impl $crate::metrics::Label for $name {
            const ALL: &'static [Self] = &[$(Self::$variant),+];

            fn index(self) -> usize {
                self as usize
            }

            fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    crate::metric_labels! {
        enum Colour {
            Red = "red",
            Green = "green",
            Blue = "blue",
        }
    }

    crate::metric_labels! {
        enum Pair {
            One = "one",
            Two = "two",
        }
    }

    #[test]
    fn a_counter_counts_and_resets() {
        static C: Counter = Counter::new();
        C.inc();
        C.add(41);
        assert_eq!(C.get(), 42);
        assert_eq!(serde_json::to_value(&C).unwrap(), serde_json::json!(42));
        C.reset();
        assert_eq!(C.get(), 0);
    }

    #[test]
    fn a_gauge_goes_both_ways_and_keeps_its_peak() {
        let g = Gauge::new();
        g.add(5);
        g.inc();
        g.sub(4);
        g.dec();
        assert_eq!(g.get(), 1);
        assert_eq!(g.peak(), 6);
        g.set(3);
        assert_eq!(g.snapshot(), GaugeSnapshot { value: 3, peak: 6 });
        g.reset_peak();
        assert_eq!(g.peak(), 3);
        assert_eq!(
            serde_json::to_value(&g).unwrap(),
            serde_json::json!({ "value": 3, "peak": 3 })
        );
    }

    #[test]
    fn buckets_tile_the_whole_u64_range_without_gaps() {
        assert_eq!(bucket_low(0), 0);
        for i in 0..BUCKETS - 1 {
            assert_eq!(
                bucket_high(i) + 1,
                bucket_low(i + 1),
                "gap after bucket {i}"
            );
            assert_eq!(bucket_of(bucket_low(i)), i);
            assert_eq!(bucket_of(bucket_high(i)), i);
        }
        assert_eq!(bucket_of(u64::MAX), BUCKETS - 1);
        assert_eq!(bucket_high(BUCKETS - 1), u64::MAX);
    }

    #[test]
    fn small_values_are_exact_and_large_ones_within_one_sixteenth() {
        for v in 0..32u64 {
            assert_eq!(bucket_low(bucket_of(v)), v);
            assert_eq!(bucket_high(bucket_of(v)), v);
        }
        for v in [33u64, 1_000, 65_537, 1 << 40, u64::MAX / 3] {
            let i = bucket_of(v);
            let width = bucket_high(i) - bucket_low(i) + 1;
            assert!(
                width * 16 <= bucket_low(i),
                "bucket {i} is wider than 1/16 of {v}"
            );
        }
    }

    #[test]
    fn a_histogram_reports_its_quantiles() {
        let h = LogHistogram::new();
        assert_eq!(h.snapshot().p50, None);
        for v in 1..=100u64 {
            h.record(v);
        }
        let s = h.snapshot();
        assert_eq!(s.count, 100);
        assert_eq!(s.sum, 5050);
        assert_eq!(s.min, Some(1));
        assert_eq!(s.max, Some(100));
        let p50 = s.p50.unwrap();
        assert!((50..=53).contains(&p50), "p50 {p50}");
        let p99 = s.p99.unwrap();
        assert!((99..=100).contains(&p99), "p99 {p99}");
        assert_eq!(s.quantile(1.0), Some(100));
        let back: HistogramSnapshot =
            serde_json::from_value(serde_json::to_value(&h).unwrap()).unwrap();
        assert_eq!(back, s);
        h.reset();
        assert_eq!(h.count(), 0);
        assert_eq!(h.snapshot().max, None);
    }

    #[test]
    fn concurrent_recording_loses_no_sample() {
        let h = Arc::new(LogHistogram::new());
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let h = Arc::clone(&h);
                std::thread::spawn(move || {
                    for v in 0..10_000u64 {
                        h.record(v * (t + 1));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(h.count(), 80_000);
        assert_eq!(h.snapshot().max, Some(9_999 * 8));
    }

    #[test]
    fn a_family_has_one_counter_per_label() {
        static F: Family<Colour, { Colour::COUNT }> = Family::new();
        F.inc(Colour::Green);
        F.add(Colour::Blue, 3);
        assert_eq!(F.get(Colour::Red), 0);
        assert_eq!(F.get(Colour::Green), 1);
        assert_eq!(F.total(), 4);
        assert_eq!(
            serde_json::to_string(&F).unwrap(),
            r#"{"red":0,"green":1,"blue":3}"#
        );
        assert_eq!(
            Colour::ALL.iter().map(|c| c.name()).collect::<Vec<_>>(),
            ["red", "green", "blue"]
        );
        F.reset();
        assert_eq!(F.total(), 0);
    }

    #[test]
    fn a_family_narrower_than_its_labels_does_not_construct() {
        let built = std::panic::catch_unwind(Family::<Pair, 3>::new);
        assert!(built.is_err());
    }
}
