//! The hot-index rule (M03.09 decision 4): an index that takes about a quarter of its lane's
//! **bytes** over a window, on a lane that carried at least a volume floor in that window, is
//! promoted to its own lane. The floor stops a lane where one index wrote once (100% of two
//! records) from promoting for nothing; each promotion is a lane migration.
use std::collections::HashMap;
use std::time::Duration;

use hsearch_format::Id;

/// The thresholds. The quarter and the floor are starting values that M03.08 measures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HotRule {
    /// Length of one tumbling window.
    pub window: Duration,
    /// A share of at least `share_num / share_den` of the window's bytes is hot.
    pub share_num: u64,
    /// Denominator of the share.
    pub share_den: u64,
    /// Records a lane must carry in the window before any index counts as hot.
    pub floor_records: u64,
}

impl Default for HotRule {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(30),
            share_num: 1,
            share_den: 4,
            floor_records: 1000,
        }
    }
}

/// One index over the threshold in a closed window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HotIndex {
    /// The index.
    pub index: Id,
    /// Its records in the window.
    pub records: u64,
    /// Its bytes in the window.
    pub bytes: u64,
}

/// What one closed window looked like.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowReport {
    /// Records the lane carried.
    pub records: u64,
    /// Bytes the lane carried.
    pub bytes: u64,
    /// Indexes at or over the share, only if the lane met the volume floor.
    pub hot: Vec<HotIndex>,
    /// The largest single-index share of bytes (0 below the floor).
    pub hottest_byte_share: f64,
    /// The largest single-index share of records (0 below the floor).
    pub hottest_record_share: f64,
    /// Indexes beyond the tracker's bound that were not counted.
    pub untracked: u64,
}

/// Counts per-index volume for one lane. State is bounded by `max_indexes` (the lane's ceiling).
#[derive(Debug)]
pub struct HotTracker {
    rule: HotRule,
    max_indexes: usize,
    start: Option<Duration>,
    per: HashMap<Id, (u64, u64)>,
    records: u64,
    bytes: u64,
    untracked: u64,
}

impl HotTracker {
    /// A tracker with `rule`, remembering at most `max_indexes` indexes per window.
    #[must_use]
    pub fn new(rule: HotRule, max_indexes: usize) -> Self {
        Self {
            rule,
            max_indexes,
            start: None,
            per: HashMap::new(),
            records: 0,
            bytes: 0,
            untracked: 0,
        }
    }

    fn close(&mut self) -> WindowReport {
        let mut report = WindowReport {
            records: self.records,
            bytes: self.bytes,
            untracked: self.untracked,
            ..WindowReport::default()
        };
        if self.records >= self.rule.floor_records && self.bytes > 0 {
            for (index, &(records, bytes)) in &self.per {
                report.hottest_byte_share = report
                    .hottest_byte_share
                    .max(bytes as f64 / self.bytes as f64);
                report.hottest_record_share = report
                    .hottest_record_share
                    .max(records as f64 / self.records as f64);
                // bytes / total >= num / den, in integers.
                if u128::from(bytes) * u128::from(self.rule.share_den)
                    >= u128::from(self.bytes) * u128::from(self.rule.share_num)
                {
                    report.hot.push(HotIndex {
                        index: *index,
                        records,
                        bytes,
                    });
                }
            }
            report
                .hot
                .sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.index.cmp(&b.index)));
        }
        self.per.clear();
        self.records = 0;
        self.bytes = 0;
        self.untracked = 0;
        report
    }

    /// Count one acknowledged record of `bytes` at `now`. Returns the report of the window this
    /// record closed, if it was the first of a new one. Time moves forward only; an earlier `now`
    /// is counted in the current window.
    pub fn record(&mut self, index: Id, bytes: u64, now: Duration) -> Option<WindowReport> {
        let mut closed = None;
        match self.start {
            None => self.start = Some(now),
            Some(start) if now.saturating_sub(start) >= self.rule.window => {
                closed = Some(self.close());
                self.start = Some(now);
            }
            Some(_) => {}
        }
        self.records += 1;
        self.bytes += bytes;
        if let Some(entry) = self.per.get_mut(&index) {
            entry.0 += 1;
            entry.1 += bytes;
        } else if self.per.len() < self.max_indexes {
            self.per.insert(index, (1, bytes));
        } else {
            self.untracked += 1;
        }
        closed
    }

    /// Close the current window now (end of a run).
    pub fn finish(&mut self) -> WindowReport {
        self.start = None;
        self.close()
    }
}
