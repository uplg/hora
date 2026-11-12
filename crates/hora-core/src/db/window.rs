//! The status page's rolling windows, read without rescanning raw history.
//!
//! The 24h availability and latency percentiles, the 24h sparklines and the
//! 90-day bars used to aggregate every raw check in their window on each
//! refresh: tens of seconds on a 575M-row database. Here the ended hours come
//! from the hourly roll-ups (`checks_hourly`, counts plus a latency
//! histogram) and only what lies outside them is read raw - the partial hour
//! at the window's start and the rows above the roll-up frontier, the same
//! principle [`daily_all`](super::daily_all) applies to the bars.
//!
//! The sparklines and bars need a finer or wider grouping than the roll-ups
//! hold, so they are cached incrementally instead ([`SparklineCache`],
//! [`DailyCache`]): what is final (closed sparkline buckets, rolled-up
//! hours) is aggregated once and kept, and each refresh only reads what is
//! new. Both return exactly what their one-shot counterparts return; the
//! tests compare them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::Store;

use super::DayRow;
use super::aggregates::{DayCounts, Point, merge_days, read_daily_buckets, read_raw_days};
use crate::SECONDS_PER_DAY;
use crate::histogram::LatencyHistogram;

const SECONDS_PER_HOUR: i64 = 3600;

/// One monitor's figures over a window.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WindowStats {
    /// Up or degraded checks.
    pub available: i64,
    /// Every check.
    pub total: i64,
    /// Every latency sample (any status).
    pub latency: LatencyHistogram,
}

/// The roll-up frontier: hours below it are in `checks_hourly`. `None`
/// before the first roll-up.
async fn hourly_frontier(store: &Store) -> sqlx::Result<Option<i64>> {
    let newest: Option<i64> = sqlx::query_scalar("SELECT MAX(hour) FROM checks_hourly")
        .fetch_one(store.sqlx())
        .await?;
    Ok(newest.map(|hour| hour + SECONDS_PER_HOUR))
}

/// [`WindowStats`] per monitor for every check at or after `since`.
///
/// Availability equals [`availability_all`](super::availability_all)
/// exactly; the latency histogram counts the same samples
/// [`latency_percentiles_all`](super::latency_percentiles_all) ranks (see
/// [`crate::histogram`] for the percentile error bound). Ended hours come
/// from the roll-ups; raw rows are read only for the partial first hour,
/// above the roll-up frontier, and - for latency only - for hours rolled up
/// before the buckets carried a histogram.
///
/// # Errors
///
/// Returns an error if a query fails.
pub async fn window_stats_all(
    store: &Store,
    since: i64,
) -> sqlx::Result<HashMap<String, WindowStats>> {
    let first_hour = ceil_to(since, SECONDS_PER_HOUR);
    let frontier = hourly_frontier(store).await?.unwrap_or(first_hour);
    let rolled_end = frontier.max(first_hour);
    let mut stats: HashMap<String, WindowStats> = HashMap::new();
    add_raw(store, &mut stats, since, first_hour, Raw::Add).await?;
    add_rolled_up(store, &mut stats, first_hour, rolled_end).await?;
    add_raw(store, &mut stats, rolled_end, i64::MAX, Raw::Add).await?;
    Ok(stats)
}

impl WindowStats {
    fn add(&mut self, other: &Self) {
        self.available += other.available;
        self.total += other.total;
        self.latency.merge(&other.latency);
    }
}

/// The window's figures kept between refreshes: the rolled-up hours are
/// merged once per roll-up, the partial first hour is read once per hour and
/// then only shrunk by the rows the window slid past, and the raw rows above
/// the frontier are read once they are a minute old. A refresh reads seconds
/// of raw checks, not an hour or two. Returns exactly what
/// [`window_stats_all`] returns.
#[derive(Debug, Default)]
pub struct WindowCache {
    ready: bool,
    /// The window's first whole hour; the lead covers `[lead_since, first_hour)`.
    first_hour: i64,
    lead_since: i64,
    lead: HashMap<String, WindowStats>,
    /// Rolled-up hours `[first_hour, middle_end)`.
    middle_end: i64,
    middle: HashMap<String, WindowStats>,
    /// Raw rows `[middle_end, tail_end)`, final (older than [`TAIL_LAG_SECS`]).
    tail_end: i64,
    tail: HashMap<String, WindowStats>,
}

/// How old a raw row above the frontier must be to be kept in the cache:
/// checks are stamped when recorded, so this only covers a late commit.
const TAIL_LAG_SECS: i64 = 60;

impl WindowCache {
    /// [`WindowStats`] per monitor since `since`, as of `now`.
    ///
    /// # Errors
    ///
    /// Returns an error if a query fails; the cache is then reset.
    pub async fn refresh(
        &mut self,
        store: &Store,
        since: i64,
        now: i64,
    ) -> sqlx::Result<HashMap<String, WindowStats>> {
        let result = self.refresh_inner(store, since, now).await;
        if result.is_err() {
            *self = Self::default();
        }
        result
    }

    async fn refresh_inner(
        &mut self,
        store: &Store,
        since: i64,
        now: i64,
    ) -> sqlx::Result<HashMap<String, WindowStats>> {
        let first_hour = ceil_to(since, SECONDS_PER_HOUR);
        let frontier = hourly_frontier(store).await?.unwrap_or(first_hour);
        let rolled_end = frontier.max(first_hour);
        let fresh = !self.ready
            || first_hour != self.first_hour
            || since < self.lead_since
            || rolled_end < self.middle_end;
        if fresh {
            *self = Self {
                ready: true,
                first_hour,
                lead_since: since,
                middle_end: first_hour,
                tail_end: first_hour,
                ..Self::default()
            };
            add_raw(store, &mut self.lead, since, first_hour, Raw::Add).await?;
        } else if self.lead_since < since {
            add_raw(store, &mut self.lead, self.lead_since, since, Raw::Remove).await?;
            self.lead_since = since;
        }
        if self.middle_end < rolled_end {
            add_rolled_up(store, &mut self.middle, self.middle_end, rolled_end).await?;
            self.middle_end = rolled_end;
            // The tail's rows below the new frontier are in the middle now.
            self.tail = HashMap::new();
            self.tail_end = rolled_end;
        }
        let settled = (now - TAIL_LAG_SECS).max(self.tail_end);
        if self.tail_end < settled {
            add_raw(store, &mut self.tail, self.tail_end, settled, Raw::Add).await?;
            self.tail_end = settled;
        }
        let mut stats = HashMap::new();
        add_raw(store, &mut stats, self.tail_end, i64::MAX, Raw::Add).await?;
        for part in [&self.lead, &self.middle, &self.tail] {
            for (id, figures) in part {
                stats.entry(id.clone()).or_default().add(figures);
            }
        }
        // A monitor the window slid past entirely is absent, as in a one-shot
        // read.
        stats.retain(|_, figures| figures.total > 0);
        Ok(stats)
    }
}

/// How [`add_raw`] folds raw checks in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Raw {
    /// Count them.
    Add,
    /// Count their latency only (their counts are already in).
    LatencyOnly,
    /// Uncount them: the window slid past.
    Remove,
}

/// Fold the hourly buckets starting in `[start, end)` into `stats`. Hours
/// whose histograms are missing are re-read raw for their latency, for every
/// monitor (a roll-up writes an hour's histograms together, so a missing one
/// means the hour predates them).
async fn add_rolled_up(
    store: &Store,
    stats: &mut HashMap<String, WindowStats>,
    start: i64,
    end: i64,
) -> sqlx::Result<()> {
    if start >= end {
        return Ok(());
    }
    let buckets = sqlx::query_as::<_, (String, i64, i64, i64, Option<Vec<u8>>)>(
        "SELECT monitor_id, hour, up_count + degraded_count, \
            up_count + down_count + degraded_count, latency_hist \
         FROM checks_hourly WHERE hour >= ? AND hour < ?",
    )
    .bind(start)
    .bind(end)
    .fetch_all(store.sqlx())
    .await?;
    let mut raw_hours: BTreeSet<i64> = BTreeSet::new();
    let mut histograms = Vec::with_capacity(buckets.len());
    for (id, hour, available, total, hist) in buckets {
        let entry = stats.entry(id.clone()).or_default();
        entry.available += available;
        entry.total += total;
        match hist {
            Some(bytes) => histograms.push((id, hour, bytes)),
            None => {
                raw_hours.insert(hour);
            }
        }
    }
    for (id, hour, bytes) in histograms {
        if raw_hours.contains(&hour) {
            continue;
        }
        if let Some(entry) = stats.get_mut(&id)
            && entry.latency.merge_encoded(&bytes).is_none()
        {
            tracing::warn!(monitor = %id, hour, "unreadable latency histogram, skipped");
        }
    }
    for (start, end) in hour_ranges(&raw_hours) {
        add_raw(store, stats, start, end, Raw::LatencyOnly).await?;
    }
    Ok(())
}

/// Fold the raw checks in `[start, end)` into `stats` (see [`Raw`]).
async fn add_raw(
    store: &Store,
    stats: &mut HashMap<String, WindowStats>,
    start: i64,
    end: i64,
    mode: Raw,
) -> sqlx::Result<()> {
    if start >= end {
        return Ok(());
    }
    // Grouped by value, not returned row by row: an hour of a 10s monitor is
    // 360 rows but rarely more than a hundred distinct latencies.
    let rows = sqlx::query_as::<_, (String, Option<i64>, i64, i64)>(concat!(
        "SELECT monitor_id, latency_ms, ",
        available_total!(),
        " FROM checks WHERE time >= ? AND time < ? GROUP BY monitor_id, latency_ms"
    ))
    .bind(start)
    .bind(end)
    .fetch_all(store.sqlx())
    .await?;
    for (id, latency_ms, available, total) in rows {
        let entry = stats.entry(id).or_default();
        let samples = u64::try_from(total).unwrap_or(0);
        match mode {
            Raw::Add => {
                entry.available += available;
                entry.total += total;
                if let Some(latency_ms) = latency_ms {
                    entry.latency.record(latency_ms, samples);
                }
            }
            Raw::LatencyOnly => {
                if let Some(latency_ms) = latency_ms {
                    entry.latency.record(latency_ms, samples);
                }
            }
            Raw::Remove => {
                entry.available -= available;
                entry.total -= total;
                if let Some(latency_ms) = latency_ms {
                    entry.latency.unrecord(latency_ms, samples);
                }
            }
        }
    }
    Ok(())
}

/// Contiguous `[start, end)` ranges covering a set of hour starts.
fn hour_ranges(hours: &BTreeSet<i64>) -> impl Iterator<Item = (i64, i64)> + '_ {
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    for &hour in hours {
        match ranges.last_mut() {
            Some((_, end)) if *end == hour => *end += SECONDS_PER_HOUR,
            _ => ranges.push((hour, hour + SECONDS_PER_HOUR)),
        }
    }
    ranges.into_iter()
}

/// `value` rounded up to a multiple of `step`.
fn ceil_to(value: i64, step: i64) -> i64 {
    value.div_euclid(step) * step + if value.rem_euclid(step) == 0 { 0 } else { step }
}

/// A sparkline bucket being averaged: its first sample time and the latency
/// sum and count (SQLite's `AVG` is the same `sum / count` in floating point).
#[derive(Debug, Clone, Copy)]
struct Bucket {
    first: i64,
    sum: i64,
    count: i64,
}

impl Bucket {
    fn add(&mut self, other: Self) {
        self.first = self.first.min(other.first);
        self.sum += other.sum;
        self.count += other.count;
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn point(self) -> Point {
        Point {
            t: self.first,
            latency_ms: (self.sum as f64 / self.count as f64) as i64,
        }
    }
}

/// How long after a sparkline bucket ends it is taken as final: checks are
/// stamped when recorded, so this only covers an insert committed late.
const SPARKLINE_CLOSE_LAG_SECS: i64 = 60;

/// The 24h sparklines, kept between refreshes: closed buckets are aggregated
/// once; a refresh reads only the window's partial first bucket and the raw
/// rows since the last closed one (minutes, not a day). Returns exactly what
/// [`latency_sparkline_all`](super::latency_sparkline_all) returns.
#[derive(Debug, Default)]
pub struct SparklineCache {
    bucket_secs: i64,
    /// Closed buckets cover `[.., closed_until)`.
    closed_until: i64,
    closed: HashMap<String, BTreeMap<i64, Bucket>>,
}

impl SparklineCache {
    /// The sparkline series since `since` in buckets of `bucket_secs`
    /// (`>= 1`), oldest first, as of `now`.
    ///
    /// # Errors
    ///
    /// Returns an error if a query fails; the cache is then left as it was.
    pub async fn refresh(
        &mut self,
        store: &Store,
        since: i64,
        bucket_secs: i64,
        now: i64,
    ) -> sqlx::Result<HashMap<String, Vec<Point>>> {
        let first_bucket = since.div_euclid(bucket_secs);
        let lead_end = (first_bucket + 1) * bucket_secs;
        if bucket_secs != self.bucket_secs || self.closed_until < lead_end {
            *self = Self {
                bucket_secs,
                closed_until: lead_end,
                closed: HashMap::new(),
            };
        }
        let close_before = (now - SPARKLINE_CLOSE_LAG_SECS).div_euclid(bucket_secs) * bucket_secs;

        let lead = sparkline_rows(store, since, lead_end, bucket_secs).await?;
        let fresh = sparkline_rows(store, self.closed_until, i64::MAX, bucket_secs).await?;

        for series in self.closed.values_mut() {
            series.retain(|&bucket, _| bucket > first_bucket);
        }
        self.closed.retain(|_, series| !series.is_empty());
        let mut open: Vec<(String, i64, Bucket)> = Vec::new();
        for (id, bucket, data) in fresh {
            if (bucket + 1) * bucket_secs <= close_before {
                self.closed.entry(id).or_default().insert(bucket, data);
            } else {
                open.push((id, bucket, data));
            }
        }
        self.closed_until = self.closed_until.max(close_before);

        let mut series: HashMap<String, BTreeMap<i64, Bucket>> = self.closed.clone();
        for (id, bucket, data) in lead.into_iter().chain(open) {
            series
                .entry(id)
                .or_default()
                .entry(bucket)
                .and_modify(|existing| existing.add(data))
                .or_insert(data);
        }
        Ok(series
            .into_iter()
            .map(|(id, buckets)| (id, buckets.into_values().map(Bucket::point).collect()))
            .collect())
    }
}

/// Raw latency in `[start, end)` per monitor and bucket.
async fn sparkline_rows(
    store: &Store,
    start: i64,
    end: i64,
    bucket_secs: i64,
) -> sqlx::Result<Vec<(String, i64, Bucket)>> {
    if start >= end {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, (String, i64, i64, i64, i64)>(
        "SELECT monitor_id, time / ?3, MIN(time), SUM(latency_ms), COUNT(*) FROM checks \
         WHERE time >= ?1 AND time < ?2 AND latency_ms IS NOT NULL \
         GROUP BY monitor_id, time / ?3",
    )
    .bind(start)
    .bind(end)
    .bind(bucket_secs)
    .fetch_all(store.sqlx())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, bucket, first, sum, count)| (id, bucket, Bucket { first, sum, count }))
        .collect())
}

/// The daily bars, kept between refreshes: the whole days of rolled-up hours
/// are summed once and only extended as the roll-up frontier advances, so a
/// refresh reads the window's partial first day, the new roll-ups and the
/// raw tail instead of three months of buckets. Returns exactly what
/// [`daily_all`](super::daily_all) returns.
#[derive(Debug, Default)]
pub struct DailyCache {
    /// The cached days cover the rolled-up hours in `[start, end)`.
    start: i64,
    end: i64,
    days: HashMap<String, BTreeMap<i64, DayCounts>>,
    /// Settled raw rows `[tail_start, tail_end)` above the roll-up frontier
    /// (older than [`TAIL_LAG_SECS`]), summed per day: a refresh then reads
    /// the last minute of raw checks instead of everything since the
    /// frontier (up to an hour and five minutes of every monitor).
    tail_start: i64,
    tail_end: i64,
    tail: HashMap<String, BTreeMap<i64, DayCounts>>,
}

impl DailyCache {
    /// The daily rows over `[since, until]`, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error if a query fails; the cache is then reset.
    pub async fn refresh(
        &mut self,
        store: &Store,
        since: i64,
        until: i64,
    ) -> sqlx::Result<HashMap<String, Vec<DayRow>>> {
        let result = self.refresh_inner(store, since, until).await;
        if result.is_err() {
            *self = Self::default();
        }
        result
    }

    async fn refresh_inner(
        &mut self,
        store: &Store,
        since: i64,
        until: i64,
    ) -> sqlx::Result<HashMap<String, Vec<DayRow>>> {
        // The same frontier as `daily_all`.
        let frontier = hourly_frontier(store)
            .await?
            .map_or(since, |frontier| since.max(frontier).min(until + 1));
        let first_day = ceil_to(since, SECONDS_PER_DAY);
        let cached_end = frontier.max(first_day);
        if self.start > first_day || self.end < first_day || self.end > cached_end {
            *self = Self {
                start: first_day,
                end: first_day,
                ..Self::default()
            };
        }
        // Whole days slid out of the window.
        if self.start < first_day {
            let keep_from = first_day / SECONDS_PER_DAY;
            for days in self.days.values_mut() {
                days.retain(|&day, _| day >= keep_from);
            }
            self.days.retain(|_, days| !days.is_empty());
            self.start = first_day;
        }
        if self.end < cached_end {
            for (id, day, counts) in hourly_days(store, self.end, cached_end).await? {
                self.days
                    .entry(id)
                    .or_default()
                    .entry(day)
                    .or_default()
                    .add(counts);
            }
            self.end = cached_end;
        }

        if self.tail_start != frontier || self.tail_end < frontier {
            self.tail_start = frontier;
            self.tail_end = frontier;
            self.tail = HashMap::new();
        }
        let settled = (until - TAIL_LAG_SECS).max(self.tail_end);
        if self.tail_end < settled {
            // `read_raw_days` bounds are inclusive.
            for (id, day, counts) in read_raw_days(store, self.tail_end, settled - 1).await? {
                self.tail
                    .entry(id)
                    .or_default()
                    .entry(day)
                    .or_default()
                    .add(counts);
            }
            self.tail_end = settled;
        }
        let mut sums = self.days.clone();
        for (id, days) in &self.tail {
            let into = sums.entry(id.clone()).or_default();
            for (&day, &counts) in days {
                into.entry(day).or_default().add(counts);
            }
        }
        let head = hourly_days(store, since, first_day.min(frontier)).await?;
        let raw = read_raw_days(store, self.tail_end, until).await?;
        for (id, day, counts) in head.into_iter().chain(raw) {
            sums.entry(id)
                .or_default()
                .entry(day)
                .or_default()
                .add(counts);
        }
        let daily = read_daily_buckets(store, since, until).await?;
        Ok(merge_days(sums, daily))
    }
}

/// Hourly buckets in `[start, end)` summed per monitor and UTC day.
async fn hourly_days(
    store: &Store,
    start: i64,
    end: i64,
) -> sqlx::Result<Vec<(String, i64, DayCounts)>> {
    if start >= end {
        return Ok(Vec::new());
    }
    super::aggregates::read_hourly_days(store, start, end).await
}
