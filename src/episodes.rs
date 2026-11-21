//! Operating points (§5): plateau episodes (E1), light-load windows (E2) and the daily busy bins.
//!
//! Everything here is pure: rows go in, in time order, and closed records come out. A record
//! leaves without a verdict, because whether a plateau is hot depends on references, which
//! `rules` owns. Tests and `replay` drive the detector with synthetic rows.

use crate::RULES_VERSION;
use crate::clock::Moment;
use crate::model::{Bin, Bins, Early, PlateauRecord, Record, UnjudgedRecord};
use crate::profile::{Chassis, DUO};
use crate::sampler::{Gap, Row};
use std::collections::{BTreeMap, VecDeque};

/// B10 at or above this opens a plateau.
pub const OPEN_B10: f64 = 0.85;
/// The plateau stays open while B10 holds at or above this.
pub const STAY_B10: f64 = 0.75;
/// B10 below `STAY_B10` this long closes the plateau.
pub const CLOSE_LOW_S: f64 = 5.0;
/// A plateau shorter than this is `too_short`.
pub const QUALIFY_S: f64 = 120.0;
pub const BLOCK_S: f64 = 120.0;
/// A last partial block is kept when it is at least this long.
pub const PARTIAL_BLOCK_S: f64 = 45.0;
/// A block needs this many measurable seconds to be valid.
pub const BLOCK_MEASURABLE_S: f64 = 45.0;
/// The Theil-Sen slope of T over a valid block's measurable seconds, °C/s.
pub const SLOPE_MAX: f64 = 0.03;
/// A second counts as busy at this fraction; a run below it starts a dip.
pub const MEASURABLE_B: f64 = 0.80;
/// Throttle in a second whose 3-s maximum T is below TCC minus this is `cold`: EC, charger or VR
/// PROCHOT, which the package counter also counts.
pub const COLD_BELOW_TCC: f64 = 5.0;
/// Cold plus PROCHOT throttle above this share of measurable time makes a block `power_delivery`.
pub const POWER_DELIVERY_MAX: f64 = 0.02;
/// The power-limited test: a duty this high, ...
pub const PL1_DUTY: f64 = 0.01;
/// ... a median frequency this far under the all-core maximum, ...
pub const PL1_F_MARGIN_MHZ: f64 = 50.0;
/// ... or xe `reason_pl1` set in this share of the measurable seconds.
pub const PL1_XE_SHARE: f64 = 0.5;
/// Generic settle: the fan's range over the last 20 s at most 200 rpm.
pub const FAN_SETTLE_WINDOW_S: f64 = 20.0;
pub const FAN_SETTLE_RANGE: u32 = 200;
/// Generic settle with no fan sensor.
pub const NO_FAN_SETTLE_S: f64 = 120.0;
/// With no throttle counter, a measurable second at TCC minus this or above counts as throttled.
pub const NO_COUNTER_BELOW_TCC: f64 = 2.0;

/// Light windows are fixed minutes of the wall clock.
pub const WINDOW_S: i64 = 60;
/// A window missing more than a few seconds (start-up, a late tick) is not judged.
pub const WINDOW_MIN_S: f64 = 57.0;
pub const LIGHT_B_MIN: f64 = 0.05;
pub const LIGHT_B_MAX: f64 = 0.30;
/// No second with B5 at or above this in the window or in the `LIGHT_QUIET_S` before it.
pub const LIGHT_B5_HIGH: f64 = 0.75;
pub const LIGHT_QUIET_S: f64 = 90.0;
/// The window's median xe `act_freq` must be below this: iGPU work heats the same package.
pub const LIGHT_XE_MAX_MHZ: f64 = 900.0;
/// A window whose maximum T reaches TCC minus this books its throttle as hot.
pub const LIGHT_HOT_BELOW_TCC: f64 = 6.0;

/// Rounding slack for comparing MONOTONIC seconds.
const EPS: f64 = 1e-3;
/// Rows kept for backdating an onset. A run at b ≥ 0.85 that has not yet lifted B10 to 0.85 is
/// shorter than B10's 10 s, so 30 s is plenty.
const HIST_S: f64 = 30.0;

/// The dip guard (§5.1): after a run of `dip_s` seconds below b 0.80, this many seconds are not
/// measurable. The PL1 budget (τ ≈ 28 s) refills during a dip and the package then bursts at PL2
/// for about 3.3 s per second of dip; the +10 s covers heat soak and 75 s is a cold start.
pub fn dip_guard_s(dip_s: f64) -> f64 {
    (10.0 + 3.5 * dip_s).min(75.0)
}

// ---------------------------------------------------------------------------------------------
// Statistics shared with the rules.

/// The `p` quantile (0..=1) with linear interpolation between ranks; NaNs are left out.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| !x.is_nan()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let pos = p.clamp(0.0, 1.0) * (v.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    Some(v[lo] + (v[hi] - v[lo]) * (pos - lo as f64))
}

pub fn median(values: &[f64]) -> Option<f64> {
    percentile(values, 0.5)
}

/// The Theil-Sen slope: the median of the pairwise slopes. One burst cannot tilt it the way it
/// tilts a least-squares fit, which matters for 1-°C readings with a 1.6 °C spread.
pub fn theil_sen(points: &[(f64, f64)]) -> Option<f64> {
    let mut slopes = Vec::with_capacity(points.len() * points.len().saturating_sub(1) / 2);
    for (i, a) in points.iter().enumerate() {
        for b in &points[i + 1..] {
            let dx = b.0 - a.0;
            if dx.abs() > 1e-9 {
                slopes.push((b.1 - a.1) / dx);
            }
        }
    }
    median(&slopes)
}

/// `p-20261203-1405` and the like, from the local time.
pub fn record_id(prefix: &str, at: &Moment) -> String {
    let c = at.civil();
    format!(
        "{prefix}-{:04}{:02}{:02}-{:02}{:02}",
        c.date.y, c.date.m, c.date.d, c.hour, c.minute
    )
}

// ---------------------------------------------------------------------------------------------
// The detector.

/// What the detector needs to know about the machine. `rules` keeps it up to date.
#[derive(Clone, Debug)]
pub struct Tuning {
    pub chassis: Chassis,
    /// The TCC point under the current key, °C.
    pub tcc_c: f64,
    pub has_fan: bool,
    /// A package throttle counter exists. Without one, duty is the share of measurable seconds
    /// at TCC − 2 or above (§6.3).
    pub has_throttle: bool,
    /// Known for the Duo i7, learned in generic mode; None until then.
    pub f_allcore_max: Option<u32>,
    pub nproc: usize,
    pub bios: String,
    pub cpu: String,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            chassis: Chassis::Generic,
            tcc_c: 95.0,
            has_fan: false,
            has_throttle: true,
            f_allcore_max: None,
            nproc: 1,
            bios: String::new(),
            cpu: String::new(),
        }
    }
}

/// A closed qualifying light window (§5.2). Non-qualifying windows are not reported.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LightWindow {
    pub start: Moment,
    pub key_id: String,
    /// Measured seconds covered (about 60).
    pub seconds: f64,
    pub busy: f64,
    /// Mean busy × nproc.
    pub threads: f64,
    pub t_p90: Option<f64>,
    pub t_max: Option<f64>,
    pub fan_p90: Option<f64>,
    pub hot_ms: u64,
    pub cold_ms: u64,
    pub reconcile_share: Option<f64>,
    /// Fraction of the window with a charger deficit.
    pub deficit: f64,
}

impl LightWindow {
    pub fn duty_hot(&self) -> f64 {
        self.hot_ms as f64 / (self.seconds * 1000.0)
    }

    pub fn duty_cold(&self) -> f64 {
        self.cold_ms as f64 / (self.seconds * 1000.0)
    }
}

#[derive(Clone, Debug)]
pub enum Output {
    /// A plateau opened, backdated to `onset`. The service snapshots /proc for the top consumers
    /// and switches the sampler to detail.
    PlateauOpened { onset: Moment },
    /// A plateau closed: `Record::Plateau` when it has at least one valid block (its verdict is
    /// still neutral and `top` empty), otherwise `Record::Unjudged` with the reason.
    Plateau(Record),
    Window(LightWindow),
}

/// One row, reduced to what the episode and window rules use.
#[derive(Clone, Debug)]
struct Sec {
    at: Moment,
    /// MONOTONIC seconds at the start and end of the interval the row covers.
    start: f64,
    end: f64,
    dt: f64,
    b: Option<f64>,
    t: Option<f64>,
    thr_ms: Option<u64>,
    fan: Option<u32>,
    f: Option<u32>,
    prochot: bool,
    pl1: bool,
    xe_act: Option<u32>,
    ac: Option<bool>,
    deficit: bool,
    cooling: bool,
    thermald: bool,
    core_spread: Option<f64>,
    core_thr: Vec<Option<u64>>,
    reconcile: Option<f64>,
}

impl Sec {
    fn of(row: &Row) -> Sec {
        let end = row.at.mono_s();
        let cores: Vec<f64> = row.core_t.iter().flatten().copied().collect();
        let core_spread = (cores.len() >= 2).then(|| {
            cores.iter().copied().fold(f64::MIN, f64::max)
                - cores.iter().copied().fold(f64::MAX, f64::min)
        });
        Sec {
            at: row.at,
            start: end - row.dt,
            end,
            dt: row.dt,
            b: row.b,
            t: row.t,
            thr_ms: row.thr_ms,
            fan: row.fan,
            f: row.f_med,
            prochot: row.xe.is_some_and(|x| x.prochot),
            pl1: row.xe.is_some_and(|x| x.pl1),
            xe_act: row.xe_act,
            ac: row.ac,
            deficit: row.deficit,
            cooling: row.cooling_active,
            thermald: row.thermald,
            core_spread,
            core_thr: row.core_thr_ms.clone(),
            reconcile: row.reconcile_share,
        }
    }

    fn start_moment(&self) -> Moment {
        self.at.plus(-self.dt)
    }

    fn busy(&self, at: f64) -> bool {
        self.b.is_some_and(|b| b >= at)
    }
}

/// One 120-s block of a settled plateau while it fills.
#[derive(Clone, Debug, Default)]
struct Block {
    start: f64,
    /// (end time, T) of the measurable seconds.
    ts: Vec<(f64, f64)>,
    measurable_s: f64,
    /// Seconds left out because xe reported PROCHOT.
    prochot_s: f64,
    thermal_ms: u64,
    cold_ms: u64,
    prochot_ms: u64,
    fs: Vec<f64>,
    fans: Vec<f64>,
    pl1_s: f64,
    busy_s: f64,
    cooling: bool,
}

#[derive(Clone, Debug)]
struct BlockResult {
    measurable_s: f64,
    ts: Vec<f64>,
    t_med: Option<f64>,
    duty: f64,
    cold_ms: u64,
    prochot_ms: u64,
    f_med: Option<f64>,
    fan_med: Option<f64>,
    busy_s: f64,
    /// None when valid; otherwise the reason it is not.
    invalid: Option<&'static str>,
}

impl Block {
    fn finish(self, len_s: f64, tune: &Tuning) -> BlockResult {
        let temps: Vec<f64> = self.ts.iter().map(|&(_, t)| t).collect();
        let ms = self.measurable_s * 1000.0;
        let duty = if ms > 0.0 {
            self.thermal_ms as f64 / ms
        } else {
            0.0
        };
        let f_med = median(&self.fs);
        let invalid = if self.cooling {
            Some("void:cooling")
        } else if self.measurable_s < BLOCK_MEASURABLE_S - EPS {
            // What took the time away: PROCHOT seconds, or dips and their guards.
            let lost = len_s - self.measurable_s;
            Some(if self.prochot_s > lost / 2.0 {
                "power_delivery"
            } else {
                "dips"
            })
        } else if theil_sen(&self.ts).is_none_or(|s| s.abs() > SLOPE_MAX) {
            Some("slope")
        } else if !(f_med
            .zip(tune.f_allcore_max)
            .is_some_and(|(f, max)| f <= max as f64 - PL1_F_MARGIN_MHZ)
            || duty >= PL1_DUTY
            || self.pl1_s >= PL1_XE_SHARE * self.measurable_s)
        {
            Some("below_pl1")
        } else if (self.cold_ms + self.prochot_ms) as f64 > POWER_DELIVERY_MAX * ms {
            Some("power_delivery")
        } else {
            None
        };
        BlockResult {
            measurable_s: self.measurable_s,
            t_med: median(&temps),
            ts: temps,
            duty,
            cold_ms: self.cold_ms,
            prochot_ms: self.prochot_ms,
            f_med,
            fan_med: median(&self.fans),
            busy_s: self.busy_s,
            invalid,
        }
    }
}

/// An open plateau episode (§5.1).
#[derive(Clone, Debug)]
struct Plateau {
    key_id: String,
    onset: Moment,
    onset_s: f64,
    last_end: (Moment, f64),
    /// The current run of rows with B10 below 0.75. They join the episode if B10 recovers and
    /// are dropped if the run closes it: the end is the first of them.
    low: Vec<Sec>,
    settle_s: Option<f64>,
    fan_low: bool,
    fans: VecDeque<(f64, u32)>,
    /// Length of the current run below b 0.80, and the end of the last dip's guard.
    dip_s: f64,
    guard_until: f64,
    /// T of the two rows before the current one, for the 3-s maximum.
    prev_t: [Option<f64>; 2],
    block: Option<Block>,
    blocks: Vec<BlockResult>,
    deficit_s: f64,
    total_s: f64,
    first_thr_s: Option<f64>,
    early_ms: u64,
    early_s: f64,
    fan6500_s: Option<f64>,
    f_first10: Vec<f64>,
    reconcile: Vec<f64>,
    core_thr: Vec<u64>,
    core_spreads: Vec<f64>,
    xe_acts: Vec<f64>,
    fan_settle: Option<u32>,
    cooling: bool,
    /// thermald was already running at the onset. Where a distribution runs it all the time it
    /// is part of what the references learn; only thermald appearing mid-episode voids (§4).
    thermald_base: bool,
    thermald: bool,
}

impl Plateau {
    fn open(first: &Sec, key_id: &str) -> Plateau {
        let onset = first.start_moment();
        Plateau {
            key_id: key_id.to_owned(),
            onset,
            onset_s: first.start,
            last_end: (onset, first.start),
            low: Vec::new(),
            settle_s: None,
            fan_low: false,
            fans: VecDeque::new(),
            dip_s: 0.0,
            guard_until: f64::NEG_INFINITY,
            prev_t: [None, None],
            block: None,
            blocks: Vec::new(),
            deficit_s: 0.0,
            total_s: 0.0,
            first_thr_s: None,
            early_ms: 0,
            early_s: 0.0,
            fan6500_s: None,
            f_first10: Vec::new(),
            reconcile: Vec::new(),
            core_thr: Vec::new(),
            core_spreads: Vec::new(),
            xe_acts: Vec::new(),
            fan_settle: None,
            cooling: false,
            thermald_base: first.thermald,
            thermald: false,
        }
    }

    /// The settle point (§5.1), if this row reaches it.
    fn settles(&mut self, s: &Sec, tune: &Tuning) -> Option<f64> {
        let rel = s.start - self.onset_s;
        let past = |secs: f64| rel >= secs - 0.5;
        if !tune.has_fan {
            return past(NO_FAN_SETTLE_S).then_some(s.start);
        }
        match tune.chassis {
            Chassis::Duo => {
                if past(DUO.settle_s) && s.fan.is_some_and(|f| f >= DUO.settle_fan) {
                    Some(s.start)
                } else if past(DUO.settle_deadline_s) {
                    self.fan_low = true;
                    Some(s.start)
                } else {
                    None
                }
            }
            Chassis::Generic => {
                let span = self
                    .fans
                    .front()
                    .zip(self.fans.back())
                    .map_or(0.0, |(a, b)| b.0 - a.0);
                let (lo, hi) = self
                    .fans
                    .iter()
                    .fold((u32::MAX, 0), |(lo, hi), &(_, f)| (lo.min(f), hi.max(f)));
                (past(DUO.settle_s)
                    && span >= FAN_SETTLE_WINDOW_S - 5.0
                    && hi.saturating_sub(lo) <= FAN_SETTLE_RANGE)
                    .then_some(s.start)
            }
        }
    }

    fn commit(&mut self, s: Sec, tune: &Tuning) {
        let rel_start = s.start - self.onset_s;
        let rel_end = s.end - self.onset_s;
        self.last_end = (s.at, s.end);
        self.total_s += s.dt;
        if s.deficit {
            self.deficit_s += s.dt;
        }
        let thermald_appeared = s.thermald && !self.thermald_base;
        self.cooling |= s.cooling;
        self.thermald |= thermald_appeared;
        // The early numbers of §6.1, for display and the post-clean comparison.
        if let Some(thr) = s.thr_ms {
            if thr > 0 && self.first_thr_s.is_none() {
                self.first_thr_s = Some(rel_end);
            }
            if rel_start >= 15.0 - EPS && rel_end <= 60.0 + EPS {
                self.early_ms += thr;
                self.early_s += s.dt;
            }
        }
        if s.fan.is_some_and(|f| f >= DUO.settle_fan) && self.fan6500_s.is_none() {
            self.fan6500_s = Some(rel_end);
        }
        if rel_end <= 10.0 + EPS
            && let Some(f) = s.f
        {
            self.f_first10.push(f as f64);
        }
        if let Some(r) = s.reconcile {
            self.reconcile.push(r);
        }
        if let Some(f) = s.fan {
            self.fans.push_back((s.end, f));
            while self
                .fans
                .front()
                .is_some_and(|&(t, _)| t < s.end - FAN_SETTLE_WINDOW_S)
            {
                self.fans.pop_front();
            }
        }

        // The dip guard runs from the onset: a dip at 70 s still guards 80 s.
        let busy = s.busy(MEASURABLE_B);
        if s.b.is_some() && !busy {
            self.dip_s += s.dt;
        } else if busy && self.dip_s > 0.0 {
            // A short dip inside a long dip's guard must not cut that guard short: the PL2 burst
            // after the long one is still running.
            self.guard_until = self.guard_until.max(s.start + dip_guard_s(self.dip_s));
            self.dip_s = 0.0;
        }
        let guarded = s.start < self.guard_until - EPS;
        let t_max3 = [s.t, self.prev_t[0], self.prev_t[1]]
            .into_iter()
            .flatten()
            .fold(f64::NEG_INFINITY, f64::max);
        self.prev_t = [s.t, self.prev_t[0]];

        if self.settle_s.is_none()
            && let Some(at) = self.settles(&s, tune)
        {
            self.settle_s = Some(at);
            self.fan_settle = s.fan;
            self.block = Some(Block {
                start: at,
                ..Block::default()
            });
        }
        let Some(mut block) = self.block.take() else {
            return;
        };
        // Consecutive 120-s blocks of wall time from the settle point.
        while s.start >= block.start + BLOCK_S - EPS {
            let next = block.start + BLOCK_S;
            self.blocks.push(block.finish(BLOCK_S, tune));
            block = Block {
                start: next,
                ..Block::default()
            };
        }
        block.cooling |= s.cooling || thermald_appeared;
        if busy && !guarded {
            let thr = s.thr_ms.unwrap_or(0);
            if s.prochot {
                // Not measurable; its throttle time still counts against power delivery.
                block.prochot_ms += thr;
                block.prochot_s += s.dt;
            } else if let Some(t) = s.t {
                block.measurable_s += s.dt;
                block.ts.push((s.end, t));
                if !tune.has_throttle {
                    if t >= tune.tcc_c - NO_COUNTER_BELOW_TCC {
                        block.thermal_ms += (s.dt * 1000.0).round() as u64;
                    }
                } else if t_max3 < tune.tcc_c - COLD_BELOW_TCC {
                    block.cold_ms += thr;
                } else {
                    block.thermal_ms += thr;
                }
                if let Some(f) = s.f {
                    block.fs.push(f as f64);
                }
                if let Some(f) = s.fan {
                    block.fans.push(f as f64);
                }
                if s.pl1 {
                    block.pl1_s += s.dt;
                }
                block.busy_s += s.b.unwrap_or(0.0) * s.dt;
                if let Some(sp) = s.core_spread {
                    self.core_spreads.push(sp);
                }
                if let Some(a) = s.xe_act {
                    self.xe_acts.push(a as f64);
                }
                if self.core_thr.len() < s.core_thr.len() {
                    self.core_thr.resize(s.core_thr.len(), 0);
                }
                for (sum, v) in self.core_thr.iter_mut().zip(&s.core_thr) {
                    *sum += v.unwrap_or(0);
                }
            }
        }
        self.block = Some(block);
    }

    fn unjudged(&self, reason: String, duration_s: f64) -> Record {
        let void = reason.starts_with("void:");
        Record::Unjudged(UnjudgedRecord {
            rules_version: RULES_VERSION,
            onset: self.onset.stamp(),
            duration_s,
            reason,
            key_id: self.key_id.clone(),
            // A void episode may have run under another key or been cut short: it teaches
            // nothing about the all-core frequency.
            f_p95_first10: if void {
                None
            } else {
                percentile(&self.f_first10, 0.95)
            },
        })
    }

    fn void(self, cause: &str) -> Record {
        let duration = self.last_end.1 - self.onset_s;
        self.unjudged(format!("void:{cause}"), duration)
    }

    fn close(mut self, tune: &Tuning) -> Record {
        let (end, end_s) = match self.low.first() {
            Some(s) => (s.start_moment(), s.start),
            None => self.last_end,
        };
        if let Some(block) = self.block.take() {
            let len = (end_s - block.start).min(BLOCK_S);
            if len >= PARTIAL_BLOCK_S - EPS {
                self.blocks.push(block.finish(len, tune));
            }
        }
        let duration = end_s - self.onset_s;
        if duration < QUALIFY_S - EPS {
            return self.unjudged("too_short".to_owned(), duration);
        }
        if self.settle_s.is_none() || self.blocks.is_empty() {
            return self.unjudged("unsettled".to_owned(), duration);
        }
        let valid: Vec<&BlockResult> = self
            .blocks
            .iter()
            .filter(|b| b.invalid.is_none())
            .collect();
        if valid.is_empty() {
            // The commonest reason; ties go to the first in this order.
            let order = [
                "void:cooling",
                "power_delivery",
                "below_pl1",
                "slope",
                "dips",
            ];
            let reason = order
                .iter()
                .enumerate()
                .max_by_key(|&(i, r)| {
                    let n = self.blocks.iter().filter(|b| b.invalid == Some(*r)).count();
                    (n, std::cmp::Reverse(i))
                })
                .map_or("dips", |(_, r)| *r);
            return self.unjudged(reason.to_owned(), duration);
        }

        let pick = |f: fn(&BlockResult) -> Option<f64>| -> Vec<f64> {
            valid.iter().filter_map(|&b| f(b)).collect()
        };
        let measurable: f64 = valid.iter().map(|b| b.measurable_s).sum();
        let all_t: Vec<f64> = valid.iter().flat_map(|b| b.ts.iter().copied()).collect();
        let cold: u64 = valid.iter().map(|b| b.cold_ms).sum();
        let prochot: u64 = valid.iter().map(|b| b.prochot_ms).sum();
        let core_total: u64 = self.core_thr.iter().sum();
        let mut flags = Vec::new();
        if self.fan_low {
            flags.push("fan_low".to_owned());
        }
        if self.cooling {
            flags.push("cooling_active".to_owned());
        }
        if self.thermald {
            flags.push("thermald".to_owned());
        }
        Record::Plateau(Box::new(PlateauRecord {
            id: record_id("p", &self.onset),
            rules_version: RULES_VERSION,
            onset: self.onset.stamp(),
            end: end.stamp(),
            key_id: self.key_id.clone(),
            bios: tune.bios.clone(),
            cpu: tune.cpu.clone(),
            duration_s: duration,
            measurable_s: self.blocks.iter().map(|b| b.measurable_s).sum(),
            blocks: self.blocks.len() as u32,
            valid_blocks: valid.len() as u32,
            busy: (measurable > 0.0)
                .then(|| valid.iter().map(|b| b.busy_s).sum::<f64>() / measurable),
            t_ss: median(&pick(|b| b.t_med)),
            t_p90: percentile(&all_t, 0.9),
            duty: median(&valid.iter().map(|b| b.duty).collect::<Vec<_>>()),
            cold_frac: Some(cold as f64 / (measurable * 1000.0)),
            prochot_frac: Some(prochot as f64 / (measurable * 1000.0)),
            fan_med: median(&pick(|b| b.fan_med)),
            f_med: median(&pick(|b| b.f_med)),
            core_spread: median(&self.core_spreads),
            core_throttle_share: if core_total > 0 {
                self.core_thr
                    .iter()
                    .map(|&c| c as f64 / core_total as f64)
                    .collect()
            } else {
                Vec::new()
            },
            deficit_frac: (self.total_s > 0.0).then(|| self.deficit_s / self.total_s),
            reconcile_share: (!self.reconcile.is_empty())
                .then(|| self.reconcile.iter().sum::<f64>() / self.reconcile.len() as f64),
            igpu_act_med: median(&self.xe_acts),
            top: Vec::new(),
            early: Early {
                t_first_throttle: self.first_thr_s,
                duty_early: (self.early_s > 0.0)
                    .then(|| self.early_ms as f64 / (self.early_s * 1000.0)),
                t_fan6500: self.fan6500_s,
                f_p95_first10: percentile(&self.f_first10, 0.95),
                fan_settle: self.fan_settle.map(f64::from),
            },
            flags,
            ..PlateauRecord::default()
        }))
    }
}

/// The light window filling now.
#[derive(Clone, Debug)]
struct Window {
    minute: i64,
    start: Moment,
    start_s: f64,
    key_id: String,
    secs: f64,
    busy: f64,
    busy_s: f64,
    ts: Vec<f64>,
    fans: Vec<f64>,
    acts: Vec<f64>,
    thr_ms: u64,
    on_battery: bool,
    deficit_s: f64,
    reconcile: Vec<f64>,
    void: bool,
}

/// The wall-clock minute a row belongs to: the one holding the middle of its interval.
fn minute_of(s: &Sec) -> i64 {
    let mid_ns = s.at.real_ns - (s.dt * 5e8) as i64;
    mid_ns.div_euclid(1_000_000_000).div_euclid(WINDOW_S)
}

/// Turns rows into plateau records and light windows.
#[derive(Clone, Debug, Default)]
pub struct Detector {
    pub tuning: Tuning,
    hist: VecDeque<Sec>,
    plateau: Option<Plateau>,
    window: Option<Window>,
    last_b5_high: Option<f64>,
    continuous_since: Option<f64>,
    last_seq: Option<u32>,
    check: bool,
    /// After a check no plateau opens until B10 has fallen below the open line once, so the
    /// burn's tail never becomes a passive episode (§10.1).
    rearm: bool,
}

impl Detector {
    pub fn new(tuning: Tuning) -> Detector {
        Detector {
            tuning,
            ..Detector::default()
        }
    }

    pub fn plateau_open(&self) -> bool {
        self.plateau.is_some()
    }

    /// Whether a light window starting at about `now_s` (MONOTONIC s) can still qualify
    /// (§5.2): no check, and nothing at B5 ≥ 0.75 and no gap in the last 90 s, which rules out
    /// every minute of a build. The tests are `judge_window`'s against a start no later than the
    /// window's, so they are never stricter: a window that qualifies was reported as possible.
    pub fn window_may_qualify(&self, now_s: f64) -> bool {
        let quiet_from = now_s - LIGHT_QUIET_S;
        !self.check
            && self.last_b5_high.is_none_or(|h| h < quiet_from)
            && self.continuous_since.is_some_and(|c| c <= quiet_from + EPS)
    }

    /// Voids the open plateau and window: the service is stopping ("restart").
    pub fn stop(&mut self) -> Vec<Output> {
        self.window = None;
        self.plateau
            .take()
            .map(|p| Output::Plateau(p.void("restart")))
            .into_iter()
            .collect()
    }

    /// A check starts: passive detection is suspended, and what is open is void.
    pub fn check_started(&mut self) -> Vec<Output> {
        self.check = true;
        self.window = None;
        self.plateau
            .take()
            .map(|p| Output::Plateau(p.void("check")))
            .into_iter()
            .collect()
    }

    /// The check is over. The quiet-before rule keeps its load out of the next light windows;
    /// for plateaus, the rows of the check are forgotten so no onset is backdated into them, and
    /// B10 must fall once before a plateau can open.
    pub fn check_ended(&mut self) {
        self.check = false;
        self.hist.clear();
        self.rearm = true;
    }

    pub fn feed(&mut self, row: &Row, key_id: &str) -> Vec<Output> {
        let mut out = Vec::new();
        let sec = Sec::of(row);
        if row.gap != Gap::None {
            let cause = match row.gap {
                Gap::Suspend => "suspend",
                Gap::Stall => "stall",
                _ => "restart",
            };
            if let Some(p) = self.plateau.take() {
                out.push(Output::Plateau(p.void(cause)));
            }
            self.window = None;
            self.hist.clear();
            self.continuous_since = Some(sec.end);
        }
        if self.last_seq.is_some_and(|s| s != row.key_seq) {
            if let Some(p) = self.plateau.take() {
                out.push(Output::Plateau(p.void("config")));
            }
            if let Some(w) = &mut self.window {
                w.void = true;
            }
            self.hist.clear();
        }
        self.last_seq = Some(row.key_seq);
        if row.b5.is_some_and(|b| b >= LIGHT_B5_HIGH) {
            self.last_b5_high = Some(sec.end);
        }
        if !self.check {
            self.feed_plateau(&sec, row, key_id, &mut out);
        }
        self.feed_window(&sec, key_id, &mut out);
        self.hist.push_back(sec);
        while self
            .hist
            .front()
            .zip(self.hist.back())
            .is_some_and(|(a, b)| a.end < b.end - HIST_S)
        {
            self.hist.pop_front();
        }
        out
    }

    fn feed_plateau(&mut self, sec: &Sec, row: &Row, key_id: &str, out: &mut Vec<Output>) {
        let tune = &self.tuning;
        if let Some(p) = &mut self.plateau {
            if row.b10.is_none_or(|b| b < STAY_B10) {
                p.low.push(sec.clone());
                let span = p.low.last().map_or(0.0, |s| s.end) - p.low[0].start;
                if span >= CLOSE_LOW_S - EPS {
                    let p = self.plateau.take().expect("open");
                    out.push(Output::Plateau(p.close(&self.tuning)));
                }
            } else {
                for s in std::mem::take(&mut p.low) {
                    p.commit(s, tune);
                }
                p.commit(sec.clone(), tune);
            }
            return;
        }
        if self.rearm {
            self.rearm = row.b10.is_some_and(|b| b >= OPEN_B10);
            return;
        }
        // Nothing is judged on battery; the alarms in `rules` still run.
        if row.ac == Some(false) || row.b10.is_none_or(|b| b < OPEN_B10) {
            return;
        }
        // Backdate the onset to the first second of the run at b ≥ 0.85 that lifted B10.
        let mut secs: Vec<Sec> = self.hist.iter().cloned().collect();
        secs.push(sec.clone());
        let first = match secs.iter().rposition(|s| s.busy(OPEN_B10)) {
            Some(mut i) => {
                while i > 0 && secs[i - 1].busy(OPEN_B10) {
                    i -= 1;
                }
                i
            }
            None => secs.len() - 1,
        };
        let mut p = Plateau::open(&secs[first], key_id);
        for s in secs.drain(first..) {
            p.commit(s, tune);
        }
        out.push(Output::PlateauOpened { onset: p.onset });
        self.plateau = Some(p);
    }

    fn feed_window(&mut self, sec: &Sec, key_id: &str, out: &mut Vec<Output>) {
        let minute = minute_of(sec);
        if self.window.as_ref().is_some_and(|w| w.minute != minute) {
            let w = self.window.take().expect("open");
            if let Some(lw) = self.judge_window(w) {
                out.push(Output::Window(lw));
            }
        }
        if self.check {
            return;
        }
        let w = self.window.get_or_insert_with(|| {
            let into = (sec.at.real_ns - minute * WINDOW_S * 1_000_000_000) as f64 / 1e9;
            Window {
                minute,
                start: sec.at.plus(-into),
                start_s: sec.end - into,
                key_id: key_id.to_owned(),
                secs: 0.0,
                busy: 0.0,
                busy_s: 0.0,
                ts: Vec::new(),
                fans: Vec::new(),
                acts: Vec::new(),
                thr_ms: 0,
                on_battery: false,
                deficit_s: 0.0,
                reconcile: Vec::new(),
                void: false,
            }
        });
        w.secs += sec.dt;
        if let Some(b) = sec.b {
            w.busy += b * sec.dt;
            w.busy_s += sec.dt;
        }
        if let Some(t) = sec.t {
            w.ts.push(t);
        }
        if let Some(f) = sec.fan {
            w.fans.push(f as f64);
        }
        if let Some(a) = sec.xe_act {
            w.acts.push(a as f64);
        }
        w.thr_ms += sec.thr_ms.unwrap_or(0);
        w.on_battery |= sec.ac == Some(false);
        if sec.deficit {
            w.deficit_s += sec.dt;
        }
        if let Some(r) = sec.reconcile {
            w.reconcile.push(r);
        }
    }

    /// The §5.2 qualification and attribution of a finished window.
    fn judge_window(&self, w: Window) -> Option<LightWindow> {
        let busy = (w.busy_s > 0.0).then(|| w.busy / w.busy_s)?;
        let quiet_from = w.start_s - LIGHT_QUIET_S;
        let qualifies = !w.void
            && !w.on_battery
            && w.secs >= WINDOW_MIN_S
            && (LIGHT_B_MIN..=LIGHT_B_MAX).contains(&busy)
            && self.last_b5_high.is_none_or(|h| h < quiet_from)
            && self.continuous_since.is_some_and(|c| c <= quiet_from + EPS)
            // No xe iGPU (or no act_freq file): the gate cannot be read and passes.
            && median(&w.acts).is_none_or(|a| a < LIGHT_XE_MAX_MHZ);
        if !qualifies {
            return None;
        }
        let t_max = w.ts.iter().copied().reduce(f64::max);
        let hot = t_max.is_some_and(|t| t >= self.tuning.tcc_c - LIGHT_HOT_BELOW_TCC);
        Some(LightWindow {
            start: w.start,
            key_id: w.key_id,
            seconds: w.secs,
            busy,
            threads: busy * self.tuning.nproc as f64,
            t_p90: percentile(&w.ts, 0.9),
            t_max,
            fan_p90: percentile(&w.fans, 0.9),
            hot_ms: if hot { w.thr_ms } else { 0 },
            cold_ms: if hot { 0 } else { w.thr_ms },
            reconcile_share: (!w.reconcile.is_empty())
                .then(|| w.reconcile.iter().sum::<f64>() / w.reconcile.len() as f64),
            deficit: w.deficit_s / w.secs,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The daily roll-up (§5.3), display only.

/// One busy bin while the day fills: time-weighted histograms give the medians without keeping
/// every reading.
#[derive(Clone, Debug, Default)]
struct BinAcc {
    secs: f64,
    thr_ms: u64,
    hot_ms: u64,
    temps: BTreeMap<i64, f64>,
    fans: BTreeMap<u32, f64>,
}

fn hist_median<K: Copy>(h: &BTreeMap<K, f64>) -> Option<K> {
    let total: f64 = h.values().sum();
    let mut acc = 0.0;
    for (&k, &w) in h {
        acc += w;
        if acc >= total / 2.0 {
            return Some(k);
        }
    }
    None
}

impl BinAcc {
    fn finish(&self) -> Bin {
        Bin {
            min: self.secs / 60.0,
            thr_ms: self.thr_ms,
            hot_ms: self.hot_ms,
            t_med: hist_median(&self.temps).map(|t| t as f64),
            fan_med: hist_median(&self.fans).map(|f| f as f64 * 100.0),
        }
    }
}

/// Minutes, throttle and hot throttle, median T and fan per busy bin for one local day.
#[derive(Clone, Debug, Default)]
pub struct DayBins {
    bins: [BinAcc; 5],
}

impl DayBins {
    /// Adds one row; `tcc_c` sets the hot line (TCC − 6, as for light windows).
    pub fn add(&mut self, row: &Row, tcc_c: f64) {
        let Some(b) = row.b else { return };
        let i = match b {
            b if b < 0.05 => 0,
            b if b < 0.30 => 1,
            b if b < 0.60 => 2,
            b if b < 0.85 => 3,
            _ => 4,
        };
        let bin = &mut self.bins[i];
        bin.secs += row.dt;
        let thr = row.thr_ms.unwrap_or(0);
        bin.thr_ms += thr;
        if row.t.is_some_and(|t| t >= tcc_c - LIGHT_HOT_BELOW_TCC) {
            bin.hot_ms += thr;
        }
        if let Some(t) = row.t {
            *bin.temps.entry(t.round() as i64).or_default() += row.dt;
        }
        if let Some(f) = row.fan {
            *bin.fans.entry((f + 50) / 100).or_default() += row.dt;
        }
    }

    pub fn finish(&self) -> Bins {
        let [lt5, b5_30, b30_60, b60_85, ge85] = &self.bins;
        Bins {
            lt5: lt5.finish(),
            b5_30: b5_30.finish(),
            b30_60: b30_60.finish(),
            b60_85: b60_85.finish(),
            ge85: ge85.finish(),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::clock::{Date, Timeline};
    use crate::model::{PlateauRecord, Record};
    use crate::sampler::{Deriver, Raw};
    use crate::sensors::{ConfigRead, CpuTimes, PowerRead, XeRead};

    /// Synthetic 1 Hz rows through the real `Deriver`, so B5, B10, gaps and key changes behave
    /// as they do in the service. `tick` takes the busy fraction, T, the throttle ms of this
    /// second and the fan.
    pub(crate) struct Feed {
        pub tl: Timeline,
        pub deriver: Deriver,
        busy: u64,
        idle: u64,
        thr: u64,
        pub config: ConfigRead,
        pub xe: Option<XeRead>,
        pub xe_act: Option<u32>,
        pub ac: bool,
        /// The busy-CPU frequency of rows at b ≥ 0.8.
        pub freq: u32,
        config_due: bool,
    }

    impl Feed {
        pub(crate) fn at(date: Date, h: u32, m: u32) -> Feed {
            Feed {
                tl: Timeline::starting(date, h, m, 0, 0),
                deriver: Deriver::default(),
                busy: 0,
                idle: 0,
                thr: 0,
                config: ConfigRead {
                    platform_profile: Some("performance".into()),
                    epp: Some("performance".into()),
                    pl1_uw: Some(28_000_000),
                    pl2_uw: Some(35_000_000),
                    pl1_max_uw: Some(28_000_000),
                    tcc_offset: Some(8),
                    no_turbo: Some(false),
                    max_perf_pct: Some(100),
                    cooling_max: Some(0),
                    lid_open: Some(true),
                    ..ConfigRead::default()
                },
                xe: Some(XeRead::default()),
                xe_act: Some(0),
                ac: true,
                freq: 3899,
                config_due: true,
            }
        }

        /// The next config poll reads `self.config` again (a key change).
        pub(crate) fn poll_config(&mut self) {
            self.config_due = true;
        }

        pub(crate) fn tick(&mut self, b: f64, t: f64, thr_ms: u64, fan: u32) -> Row {
            self.tick_dt(1.0, b, t, thr_ms, fan)
        }

        pub(crate) fn tick_dt(&mut self, dt: f64, b: f64, t: f64, thr_ms: u64, fan: u32) -> Row {
            let at = self.tl.advance(dt);
            let total = (800.0 * dt).round() as u64;
            let busy = (b * total as f64).round() as u64;
            self.busy += busy;
            self.idle += total - busy;
            self.thr += thr_ms;
            let config = std::mem::take(&mut self.config_due).then(|| self.config.clone());
            self.deriver.derive(
                Raw {
                    at,
                    cpu: Some(CpuTimes {
                        total: self.busy + self.idle,
                        idle: self.idle,
                    }),
                    t: Some(t),
                    pkg_thr_ms: Some(self.thr),
                    fan_read: true,
                    fan: Some(fan),
                    freqs: if b >= 0.8 {
                        vec![self.freq; 8]
                    } else {
                        Vec::new()
                    },
                    xe: self.xe,
                    xe_act: self.xe_act,
                    power: Some(PowerRead {
                        ac: Some(self.ac),
                        ..PowerRead::default()
                    }),
                    config,
                    ..Raw::default()
                },
                dt,
            )
        }
    }

    pub(crate) fn duo_tuning() -> Tuning {
        Tuning {
            chassis: Chassis::Duo,
            tcc_c: 92.0,
            has_fan: true,
            has_throttle: true,
            f_allcore_max: Some(4100),
            nproc: 8,
            bios: "UX482EG.311".into(),
            cpu: "11th Gen Intel(R) Core(TM) i7-1165G7".into(),
        }
    }

    const KEY: &str = "ac|performance|performance|28|35|8|0|100|open";

    fn run(d: &mut Detector, row: Row, out: &mut Vec<Output>) {
        out.extend(d.feed(&row, KEY));
    }

    /// A build: the turbo transient (91 °C, heavy throttle, fan climbing) then the plateau
    /// around `t` °C (t − 1, t, t + 1 in turn: median t, 3-s maximum t + 1) with `ms` of throttle
    /// a second.
    fn build(f: &mut Feed, d: &mut Detector, secs: u32, t: f64, ms: u64, out: &mut Vec<Output>) {
        for i in 0..secs {
            let row = if i < 56 {
                f.tick(1.0, 91.0, 380, 6100)
            } else {
                f.tick(1.0, t + (i % 3) as f64 - 1.0, ms, 6800)
            };
            run(d, row, out);
        }
    }

    fn idle(f: &mut Feed, d: &mut Detector, secs: u32, out: &mut Vec<Output>) {
        for _ in 0..secs {
            run(d, f.tick(0.02, 50.0, 0, 4000), out);
        }
    }

    fn plateaus(out: &[Output]) -> Vec<&Record> {
        out.iter()
            .filter_map(|o| match o {
                Output::Plateau(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    fn day() -> Date {
        Date::new(2026, 12, 3).unwrap()
    }

    #[test]
    fn stats() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&[]), None);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 0.9), Some(4.6));
        let line: Vec<(f64, f64)> = (0..20).map(|i| (i as f64, 2.0 * i as f64 + 1.0)).collect();
        assert_eq!(theil_sen(&line), Some(2.0));
        // One wild point does not tilt it.
        let mut noisy = line.clone();
        noisy[10].1 = 500.0;
        assert_eq!(theil_sen(&noisy), Some(2.0));
        assert_eq!(dip_guard_s(1.0), 13.5);
        assert_eq!(dip_guard_s(30.0), 75.0);
    }

    #[test]
    fn a_build_opens_backdated_and_votes() {
        let mut f = Feed::at(day(), 14, 0);
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        idle(&mut f, &mut d, 120, &mut out);
        let onset = f.tl.now();
        build(&mut f, &mut d, 400, 86.0, 9, &mut out);
        assert!(d.plateau_open());
        let opened = out
            .iter()
            .find_map(|o| match o {
                Output::PlateauOpened { onset } => Some(*onset),
                _ => None,
            })
            .expect("opened");
        assert_eq!(opened, onset, "backdated to the first busy second");
        idle(&mut f, &mut d, 20, &mut out);
        assert!(!d.plateau_open());
        let recs = plateaus(&out);
        assert_eq!(recs.len(), 1);
        let Record::Plateau(p) = recs[0] else {
            panic!("{:?}", recs[0])
        };
        assert_eq!(p.onset, onset.stamp());
        // B10 falls under 0.75 three seconds after the load stops.
        assert!((p.duration_s - 403.0).abs() <= 1.0, "{}", p.duration_s);
        // Settled at 75 s: blocks from 75 and 195 s, and a partial one from 315 s to the end at
        // 402 s, which is over 45 s and kept.
        assert_eq!(p.blocks, 3);
        assert_eq!(p.valid_blocks, 3);
        assert_eq!(p.t_ss, Some(86.0));
        let duty = p.duty.unwrap();
        assert!((duty - 0.009).abs() < 1e-9, "{duty}");
        assert_eq!(p.fan_med, Some(6800.0));
        assert_eq!(p.f_med, Some(3899.0));
        assert_eq!(p.cold_frac, Some(0.0));
        assert_eq!(p.early.t_first_throttle, Some(1.0));
        // The transient (91 °C, 38 %) never reaches a block.
        assert_eq!(p.t_p90, Some(87.0));
        // [15, 60] s: 41 transient seconds at 380 ms and 4 plateau seconds at 9 ms.
        let early = p.early.duty_early.unwrap();
        assert!((early - (41.0 * 380.0 + 36.0) / 45_000.0).abs() < 1e-9, "{early}");
        assert_eq!(p.id, "p-20261203-1402");
    }

    /// `n` rows of one reading: (b, T, throttle ms, fan).
    fn rows(
        f: &mut Feed,
        d: &mut Detector,
        n: u32,
        r: (f64, f64, u64, u32),
        out: &mut Vec<Output>,
    ) {
        for _ in 0..n {
            run(d, f.tick(r.0, r.1, r.2, r.3), out);
        }
    }

    /// The turbo transient of a build: its first 56 s, before any settle point.
    fn transient(f: &mut Feed, d: &mut Detector, out: &mut Vec<Output>) {
        rows(f, d, 56, (1.0, 91.0, 380, 6100), out);
    }

    /// A build's plateau: 85, 86, 87 °C in turn (median 86, every 3-s maximum 87, so all its
    /// throttle is thermal) with `ms` a second and the fan at 6800.
    fn steady(f: &mut Feed, d: &mut Detector, n: u32, ms: u64, out: &mut Vec<Output>) {
        for i in 0..n {
            run(d, f.tick(1.0, 85.0 + (i % 3) as f64, ms, 6800), out);
        }
    }

    fn reasons(out: &[Output]) -> Vec<String> {
        plateaus(out)
            .iter()
            .map(|r| match r {
                Record::Unjudged(u) => u.reason.clone(),
                _ => "plateau".to_owned(),
            })
            .collect()
    }

    fn only_plateau(out: &[Output]) -> &PlateauRecord {
        let recs = plateaus(out);
        assert_eq!(recs.len(), 1, "{recs:?}");
        match recs[0] {
            Record::Plateau(p) => p,
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn short_episodes_are_unjudged_and_a_slow_fan_settles_late() {
        let mut f = Feed::at(day(), 14, 0);
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        idle(&mut f, &mut d, 60, &mut out);
        build(&mut f, &mut d, 100, 86.0, 9, &mut out);
        idle(&mut f, &mut d, 20, &mut out);
        // The fan never reaches 6500: settled at 120 s with fan_low.
        for _ in 0..200 {
            run(&mut d, f.tick(1.0, 86.0, 9, 6100), &mut out);
        }
        idle(&mut f, &mut d, 20, &mut out);
        let recs = plateaus(&out);
        assert!(
            matches!(recs[0], Record::Unjudged(u) if u.reason == "too_short"),
            "{:?}",
            recs[0]
        );
        let Record::Plateau(p) = recs[1] else {
            panic!("{:?}", recs[1])
        };
        assert_eq!(p.flags, vec!["fan_low".to_owned()]);
        assert_eq!(p.early.fan_settle, Some(6100.0));
    }

    type Episode = fn(&mut Feed, &mut Detector, &mut Vec<Output>);

    #[test]
    fn the_unjudged_reasons() {
        let tests: [(&str, Episode); 7] = [
            // Settled at 120 s with the fan on 6100, then only 23 s left: no block.
            ("unsettled", |f, d, out| rows(f, d, 140, (1.0, 86.0, 9, 6100), out)),
            // A 3-s dip every 20 s: each guard (20.5 s) runs into the next dip.
            ("dips", |f, d, out| {
                transient(f, d, out);
                for _ in 0..16 {
                    steady(f, d, 17, 9, out);
                    rows(f, d, 3, (0.5, 80.0, 0, 6800), out);
                }
            }),
            // Still heating at 0.05 °C/s.
            ("slope", |f, d, out| {
                transient(f, d, out);
                for i in 0..260 {
                    run(d, f.tick(1.0, 75.0 + 0.05 * i as f64, 9, 6800), out);
                }
            }),
            // Above f_allcore_max − 50 with no throttle and no xe PL1: not at PL1.
            ("below_pl1", |f, d, out| {
                f.freq = 4080;
                transient(f, d, out);
                steady(f, d, 260, 0, out);
            }),
            // 30 ms a second at 84 °C, under TCC − 5: 3 % cold throttle.
            ("power_delivery", |f, d, out| {
                transient(f, d, out);
                rows(f, d, 260, (1.0, 84.0, 30, 6800), out);
            }),
            // xe PROCHOT all along: nothing is measurable, and PROCHOT took the time.
            ("power_delivery", |f, d, out| {
                transient(f, d, out);
                f.xe = Some(XeRead {
                    prochot: true,
                    ..XeRead::default()
                });
                steady(f, d, 260, 9, out);
            }),
            // A cooling device throttling.
            ("void:cooling", |f, d, out| {
                f.config.cooling_max = Some(1);
                f.poll_config();
                transient(f, d, out);
                steady(f, d, 260, 9, out);
            }),
        ];
        for (want, episode) in tests {
            let mut f = Feed::at(day(), 14, 0);
            let mut d = Detector::new(duo_tuning());
            let mut out = Vec::new();
            idle(&mut f, &mut d, 60, &mut out);
            episode(&mut f, &mut d, &mut out);
            idle(&mut f, &mut d, 20, &mut out);
            assert_eq!(reasons(&out), vec![want.to_owned()], "{want}");
            let Record::Unjudged(u) = plateaus(&out)[0] else {
                unreachable!()
            };
            assert!(u.duration_s >= QUALIFY_S, "{want}: {}", u.duration_s);
            if want == "below_pl1" {
                // Generic mode learns f_allcore_max from these too.
                assert_eq!(u.f_p95_first10, Some(4080.0));
            }
        }
    }

    #[test]
    fn cold_and_prochot_throttle_are_attributed() {
        // 15 ms a second at a flat 84 °C: all of it cold, 1.5 % (under the 2 % line).
        let mut f = Feed::at(day(), 14, 0);
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        idle(&mut f, &mut d, 60, &mut out);
        transient(&mut f, &mut d, &mut out);
        rows(&mut f, &mut d, 260, (1.0, 84.0, 15, 6800), &mut out);
        idle(&mut f, &mut d, 20, &mut out);
        let p = only_plateau(&out);
        assert_eq!((p.t_ss, p.duty), (Some(84.0), Some(0.0)));
        assert!((p.cold_frac.unwrap() - 0.015).abs() < 1e-9, "{:?}", p.cold_frac);

        // One PROCHOT second in 50, with 300 ms in it: left out of T and duty, counted apart.
        let mut f = Feed::at(day(), 14, 0);
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        idle(&mut f, &mut d, 60, &mut out);
        transient(&mut f, &mut d, &mut out);
        for i in 0..260 {
            let prochot = i % 50 == 25;
            f.xe = Some(XeRead {
                prochot,
                ..XeRead::default()
            });
            let ms = if prochot { 300 } else { 9 };
            run(&mut d, f.tick(1.0, 85.0 + (i % 3) as f64, ms, 6800), &mut out);
        }
        idle(&mut f, &mut d, 20, &mut out);
        let p = only_plateau(&out);
        assert_eq!(p.valid_blocks, p.blocks);
        assert!((p.duty.unwrap() - 0.009).abs() < 1e-9, "{:?}", p.duty);
        let pf = p.prochot_frac.unwrap();
        assert!(pf > 0.002 && pf < 0.02, "{pf}");
        assert_eq!(p.cold_frac, Some(0.0));
    }

    #[test]
    fn thermald_voids_only_when_it_appears() {
        // Running before the build (a distribution that always runs it): judged as usual.
        let mut f = Feed::at(day(), 14, 0);
        f.config.thermald = true;
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        idle(&mut f, &mut d, 60, &mut out);
        transient(&mut f, &mut d, &mut out);
        steady(&mut f, &mut d, 260, 9, &mut out);
        idle(&mut f, &mut d, 20, &mut out);
        assert!(only_plateau(&out).flags.is_empty());

        // Appearing mid-build: every block after it is void.
        let mut f = Feed::at(day(), 14, 0);
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        idle(&mut f, &mut d, 60, &mut out);
        transient(&mut f, &mut d, &mut out);
        f.config.thermald = true;
        f.poll_config();
        steady(&mut f, &mut d, 260, 9, &mut out);
        idle(&mut f, &mut d, 20, &mut out);
        assert_eq!(reasons(&out), vec!["void:cooling"]);
    }

    #[test]
    fn the_dip_guard_keeps_the_burst_out() {
        let mut f = Feed::at(day(), 14, 0);
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        idle(&mut f, &mut d, 60, &mut out);
        // One block from the settle point at 75 s to 195 s; the episode ends at about 203 s, so
        // no second block. The burst can only hide in medians if the guard lets it in.
        transient(&mut f, &mut d, &mut out);
        steady(&mut f, &mut d, 44, 9, &mut out);
        // A 3-s dip at b 0.5 (B10 stays at 0.85), then the PL2 burst: 92 °C, 30 % for 15 s.
        rows(&mut f, &mut d, 3, (0.5, 80.0, 0, 6800), &mut out);
        rows(&mut f, &mut d, 15, (1.0, 92.0, 300, 6900), &mut out);
        steady(&mut f, &mut d, 82, 9, &mut out);
        assert!(d.plateau_open(), "a dip does not close the episode");
        idle(&mut f, &mut d, 20, &mut out);
        let p = only_plateau(&out);
        assert_eq!((p.blocks, p.valid_blocks), (1, 1));
        // 120 s less the dip (3) and the guard: min(75, 10 + 3.5·3) = 20.5 s from the first
        // busy second after it, so 21 rows. The 15 burst seconds are among them.
        assert_eq!(p.measurable_s, 96.0);
        assert_eq!(p.t_ss, Some(86.0));
        assert!((p.duty.unwrap() - 0.009).abs() < 1e-9, "{:?}", p.duty);
        assert_eq!(p.t_p90, Some(87.0));
    }

    #[test]
    fn a_short_dip_does_not_cut_a_long_guard_short() {
        let mut f = Feed::at(day(), 14, 0);
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        idle(&mut f, &mut d, 60, &mut out);
        transient(&mut f, &mut d, &mut out);
        steady(&mut f, &mut d, 44, 9, &mut out);
        // An 8-s dip (guard 38 s), 5 s busy, a 1-s dip (guard 13.5 s), then a long PL2 burst
        // that runs past the short guard but not the long one.
        rows(&mut f, &mut d, 8, (0.7, 80.0, 0, 6800), &mut out);
        steady(&mut f, &mut d, 5, 9, &mut out);
        rows(&mut f, &mut d, 1, (0.7, 80.0, 0, 6800), &mut out);
        rows(&mut f, &mut d, 26, (1.0, 92.0, 300, 6900), &mut out);
        steady(&mut f, &mut d, 60, 9, &mut out);
        idle(&mut f, &mut d, 20, &mut out);
        let p = only_plateau(&out);
        assert_eq!((p.blocks, p.valid_blocks), (1, 1));
        // 120 s less 9 dip seconds and the 37 busy seconds of the long guard.
        assert_eq!(p.measurable_s, 74.0);
        assert!((p.duty.unwrap() - 0.009).abs() < 1e-9, "{:?}", p.duty);
        assert_eq!(p.t_ss, Some(86.0));
    }

    #[test]
    fn checks_restarts_and_stalls_void() {
        // A check starting mid-plateau voids it, and nothing is recorded while it runs.
        let mut f = Feed::at(day(), 14, 0);
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        idle(&mut f, &mut d, 60, &mut out);
        build(&mut f, &mut d, 150, 86.0, 9, &mut out);
        out.extend(d.check_started());
        assert_eq!(reasons(&out), vec!["void:check"]);
        let mut during = Vec::new();
        idle(&mut f, &mut d, 90, &mut during);
        rows(&mut f, &mut d, 200, (1.0, 88.0, 20, 6900), &mut during);
        assert!(during.is_empty(), "{during:?}");
        // The burn stops with the check: its tail is never backdated into an episode.
        d.check_ended();
        idle(&mut f, &mut d, 120, &mut during);
        assert!(
            !during
                .iter()
                .any(|o| matches!(o, Output::Plateau(_) | Output::PlateauOpened { .. })),
            "{during:?}"
        );
        // The next build is an episode as usual.
        build(&mut f, &mut d, 300, 86.0, 9, &mut during);
        idle(&mut f, &mut d, 20, &mut during);
        assert_eq!(reasons(&during), vec!["plateau"]);

        // The service stopping, and a stall.
        let mut f = Feed::at(day(), 14, 0);
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        idle(&mut f, &mut d, 60, &mut out);
        build(&mut f, &mut d, 150, 86.0, 9, &mut out);
        out.extend(d.stop());
        let mut f = Feed::at(day(), 15, 0);
        let mut d = Detector::new(duo_tuning());
        idle(&mut f, &mut d, 60, &mut out);
        build(&mut f, &mut d, 150, 86.0, 9, &mut out);
        f.tl.advance(4.0);
        build(&mut f, &mut d, 1, 86.0, 9, &mut out);
        assert_eq!(reasons(&out), vec!["void:restart", "void:stall"]);
    }

    #[test]
    fn battery_and_key_changes_spoil_a_window() {
        for spoil in ["battery", "key"] {
            let mut f = Feed::at(day(), 14, 0);
            let mut d = Detector::new(duo_tuning());
            let mut out = Vec::new();
            for i in 0..300 {
                if i == 190 {
                    match spoil {
                        "battery" => f.ac = false,
                        _ => {
                            f.config.platform_profile = Some("balanced".into());
                            f.poll_config();
                        }
                    }
                }
                if i == 200 {
                    f.ac = true;
                }
                run(&mut d, f.tick(0.15, 70.0, 5, 4600), &mut out);
            }
            run(&mut d, f.tick(0.15, 70.0, 0, 4600), &mut out);
            let starts: Vec<String> = out
                .iter()
                .filter_map(|o| match o {
                    Output::Window(w) => Some(w.start.label()),
                    _ => None,
                })
                .collect();
            // 14:03 is spoiled; 14:04 starts clean under the key then in force.
            assert_eq!(starts, vec!["3/12/2026 14:02", "3/12/2026 14:04"], "{spoil}");
        }
    }

    #[test]
    fn suspend_and_key_change_void() {
        let mut f = Feed::at(day(), 14, 0);
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        idle(&mut f, &mut d, 60, &mut out);
        build(&mut f, &mut d, 200, 86.0, 9, &mut out);
        f.tl.suspend(600.0);
        run(&mut d, f.tick(0.02, 50.0, 0, 4000), &mut out);
        idle(&mut f, &mut d, 30, &mut out);
        build(&mut f, &mut d, 200, 86.0, 9, &mut out);
        f.config.lid_open = Some(false);
        f.poll_config();
        run(&mut d, f.tick(1.0, 86.0, 9, 6800), &mut out);
        let recs = plateaus(&out);
        let reasons: Vec<&str> = recs
            .iter()
            .map(|r| match r {
                Record::Unjudged(u) => u.reason.as_str(),
                _ => "plateau",
            })
            .collect();
        assert_eq!(reasons, vec!["void:suspend", "void:config"]);
    }

    #[test]
    fn light_windows_qualify_and_attribute() {
        let mut f = Feed::at(day(), 14, 0);
        let mut d = Detector::new(duo_tuning());
        let mut out = Vec::new();
        // 3 minutes of quiet light load, the third hot at 91 °C with 400 ms a second.
        for i in 0..240 {
            let row = if (180..240).contains(&i) {
                f.tick(0.15, 91.0, 400, 4600)
            } else {
                f.tick(0.15, 70.0, 5, 4600)
            };
            run(&mut d, row, &mut out);
        }
        run(&mut d, f.tick(0.15, 70.0, 0, 4600), &mut out);
        let windows: Vec<&LightWindow> = out
            .iter()
            .filter_map(|o| match o {
                Output::Window(w) => Some(w),
                _ => None,
            })
            .collect();
        // The first 90 s after start-up are not known to be quiet: the first two windows
        // (14:00 and 14:01) do not qualify.
        assert_eq!(windows.len(), 2, "{windows:?}");
        let cool = windows[0];
        assert_eq!(cool.start.label(), "3/12/2026 14:02");
        assert_eq!((cool.hot_ms, cool.cold_ms), (0, 300));
        assert!((cool.threads - 1.2).abs() < 0.01);
        let hot = windows[1];
        assert_eq!((hot.hot_ms, hot.cold_ms), (24_000, 0));
        assert!((hot.duty_hot() - 0.4).abs() < 1e-9);

        // A build nearby keeps the next windows out (B5 ≥ 0.75 within 90 s).
        let mut out = Vec::new();
        build(&mut f, &mut d, 30, 86.0, 0, &mut out);
        for _ in 0..140 {
            run(&mut d, f.tick(0.15, 70.0, 0, 4600), &mut out);
        }
        assert!(
            !out.iter().any(|o| matches!(o, Output::Window(_))),
            "{out:?}"
        );
        // iGPU busy: no window either.
        f.xe_act = Some(1300);
        let mut out = Vec::new();
        for _ in 0..200 {
            run(&mut d, f.tick(0.15, 70.0, 0, 4600), &mut out);
        }
        assert!(!out.iter().any(|o| matches!(o, Output::Window(_))));
    }

    #[test]
    fn day_bins() {
        let mut f = Feed::at(day(), 14, 0);
        let mut bins = DayBins::default();
        f.tick(0.0, 50.0, 0, 4000);
        for _ in 0..120 {
            bins.add(&f.tick(0.02, 50.0, 0, 4000), 92.0);
        }
        for _ in 0..60 {
            bins.add(&f.tick(1.0, 88.0, 10, 6800), 92.0);
        }
        let b = bins.finish();
        assert_eq!(b.lt5.min, 2.0);
        assert_eq!(b.lt5.t_med, Some(50.0));
        assert_eq!(b.ge85.min, 1.0);
        assert_eq!((b.ge85.thr_ms, b.ge85.hot_ms), (600, 600));
        assert_eq!(b.ge85.fan_med, Some(6800.0));
        assert_eq!(b.b5_30.min, 0.0);
    }
}
