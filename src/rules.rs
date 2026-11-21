//! Evidence rules and the state machine (§7, §8, §9): verdicts for plateaus and light windows,
//! the days they add up to, the case from WATCH to WARNING and back through a clean, and the FAN
//! and FAULT alarms.
//!
//! The engine is pure like the detector. It takes the detector's records, every row (for the
//! alarms and the day boundary) and her actions, each with an explicit time, and queues what the
//! store must append as `Effect`s. The service writes them; tests read them.

use crate::clock::{Date, Moment, Stamp};
use crate::episodes::{Detector, LightWindow, Output, Tuning, DayBins, median, percentile, theil_sen};
use crate::model::{
    Case, CaseCheck, CaseOutcome, CaseRecord, ChassisInfo, CheckPurpose, CheckRef, CheckVerdict,
    CleanedMark, Counts, DailyRecord, Drift, EventKind, FanStatus, FaultStatus, FirstEvidence,
    KeyInfo, LastCheck, LastPlateau, Learning, LightRecord, LightVerdict, Notice,
    PassiveSample, PassiveState, PassiveVerification, PlateauRecord, PlateauVerdict, PostClean,
    RBefore, Record, References, Sighting, Snooze, State, Status, UnjudgedCounts, UnjudgedRecord,
    Verification,
};
use crate::present::{self, notice};
use crate::profile::{self, Chassis, ChassisSetting, ConfigKey, DUO, DUO_SEED};
use crate::references::{self, Accepted, Adopted, KeyView, Learned};
use crate::sampler::{Gap, Row};
use crate::sensors::StaticInfo;
use crate::{RULES_VERSION, SCHEMA, VERSION};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// A plateau at this duty with the fan at 96 % of its automatic maximum is hot with no reference.
pub const SAT_PLATEAU_DUTY: f64 = 0.30;
pub const SAT_PLATEAU_FAN: f64 = 0.96;
/// The same for a light window: 5 % hot duty with the fan at 85 % of its maximum.
pub const SAT_LIGHT_DUTY: f64 = 0.05;
pub const SAT_LIGHT_FAN: f64 = 0.85;
/// A light window is hot at duty_hot ≥ max(5 %, 10 × light_p90) (§7.3).
pub const LIGHT_L_MIN: f64 = 0.05;
pub const LIGHT_L_FACTOR: f64 = 10.0;
/// A cold-throttle window is written when its cold duty reaches this; lower, it is the noise a
/// clean machine shows anyway (Floorp spikes at idle).
pub const COLD_WINDOW_DUTY: f64 = 0.01;
/// Over 10 min of cold throttle in a day raises "throttling while cool".
pub const COLD_NOTICE_MS: u64 = 600_000;
/// Whether light-load windows can be dust evidence (E2): what the service passes as
/// `Machine::light_load_evidence`. Off since 21/11/2026, her decision ("just turn the light load
/// check off"): on her clean machine bch-reconcile alone, about one core at nice 19, throttled
/// 42-48 % of light-load time at ≥ 86 °C, so E2 would warn on a clean laptop. Turning it back
/// on changes what a day counts, so bump `RULES_VERSION` with it. A store written with it on is
/// read back under the rules in force: days and records in `Engine::restore`, and an open case
/// there too (`reconsider_light_case`).
pub const LIGHT_LOAD_EVIDENCE: bool = false;
/// An E2 evidence day: 10 hot windows (§7.4).
pub const E2_DAY_WINDOWS: u32 = 10;
/// A qualifying day by light load alone: 30 voting windows.
pub const QUALIFYING_LIGHT: u32 = 30;
/// Passive Not improved (§9.1): this many hot builds, or E2 days, since the clean.
const NOT_IMPROVED_HOT: u32 = 3;
const NOT_IMPROVED_E2_DAYS: u32 = 2;
pub const FAN_SLOW_MARGIN: f64 = 400.0;
/// Fan stopped: 0 rpm for 20 s while T ≥ 85 °C (§7.5).
pub const FAN_STOPPED_S: f64 = 20.0;
pub const FAN_STOPPED_T: f64 = 85.0;
/// TCC not acting: T ≥ TCC + 6 for 5 s.
pub const FAULT_ABOVE_TCC: f64 = 6.0;
pub const FAULT_S: f64 = 5.0;
/// An alarm's first reading stands for at most this long, which is coretemp's cache. A 5-s
/// battery row or the row after a stall is one reading, not seconds of them (§7.5).
pub const ALARM_READING_S: f64 = 1.0;
/// Snooze hides the suspicion actions this long.
pub const SNOOZE_S: f64 = 3.0 * 86_400.0;
/// Days and plateaus kept in memory: the longest rule looks back 30 days (drift, unjudged), the
/// earlier-sighting rule 60.
const KEEP_DAYS: i64 = 90;
/// Missed days further back than this are not settled one by one.
const SETTLE_BACK_DAYS: i64 = 60;
/// A settings change is logged once it has lasted this long (§11.5).
const CONFIG_CHANGE_S: f64 = 600.0;
/// A day settles this long after local midnight. A plateau whose 5-s low run began before
/// midnight closes a few seconds after it, and the 23:59 light window at the first row of the
/// next minute; both must land in their own day before that day settles.
const SETTLE_LAG_S: f64 = 15.0;
/// The firmware-change notice goes after a check or Not dust has measured the new firmware, or
/// after this many days.
const BIOS_NOTICE_DAYS: i64 = 30;
/// A store file set aside at start is reported this long; the copy itself stays beside the store.
const STORE_NOTICE_DAYS: i64 = 30;
/// Unjudged episodes at least this long teach f_allcore_max (§6.3).
const F_EARLY_MIN_S: f64 = 120.0;
/// The wall clock jumping ahead by more than this while the machine runs is a step, not time
/// passing (time asleep moves BOOTTIME as well, so it never looks like one).
const CLOCK_STEP_NS: i64 = 3_600_000_000_000;
/// Days do not advance across such a step until it has stood this long.
const CLOCK_HOLD_S: f64 = 600.0;
const EPS: f64 = 1e-3;

// ---------------------------------------------------------------------------------------------
// Thresholds and verdicts (§7.1-§7.3, §10.6), pure functions `replay` uses too.

/// The duty lines of §7.1. Each multiplicative margin is capped by an additive one, so an
/// accepted high R_duty cannot blind the duty arm while TCC clamps T near 92 °C.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DutyLines {
    pub hot: f64,
    pub only: f64,
    pub clean: f64,
    pub like: f64,
}

pub fn duty_lines(r: f64) -> DutyLines {
    DutyLines {
        hot: (3.0 * r).min(r + 0.03).max(0.03),
        only: (6.0 * r).min(r + 0.05).max(0.08),
        clean: (2.0 * r).min(r + 0.01).max(0.02),
        like: (2.5 * r).min(r + 0.015).max(0.025),
    }
}

/// What a plateau was judged against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basis {
    /// The learned R_passive.
    Passive,
    /// R_active (the seed, or a check) with the wider seed-phase margins.
    SeedPhase,
    /// No reference, but saturated: hot at full fan.
    Saturation,
    /// No reference: stored, not counted.
    NoReference,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlateauJudgement {
    pub verdict: PlateauVerdict,
    pub basis: Basis,
    /// Counts as a voting plateau (a referenced key, or a saturation vote).
    pub voting: bool,
    pub fan_slow: bool,
    /// (R_T, R_duty) it was compared with.
    pub against: Option<(f64, f64)>,
}

/// Machine facts the verdicts need besides the references.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JudgeCtx {
    pub has_fan: bool,
    pub has_throttle: bool,
    pub fan_auto_max: Option<f64>,
    /// "Accept fan as normal" replaces R_fan.
    pub r_fan_override: Option<f64>,
    /// "Accept fan as normal" on a fan that was only slow to ramp up: `fan_low` stands aside.
    pub ramp_accepted: bool,
}

/// The plateau verdict (§7.2) and the `fan_slow` flag.
pub fn judge_plateau(p: &PlateauRecord, view: &KeyView, j: &JudgeCtx) -> PlateauJudgement {
    use PlateauVerdict::{Clean, Hot, Neutral};
    let none = PlateauJudgement {
        verdict: Neutral,
        basis: Basis::NoReference,
        voting: false,
        fan_slow: false,
        against: None,
    };
    let (Some(t), Some(duty)) = (p.t_ss, p.duty) else {
        return none;
    };
    let saturated = j.has_fan
        && duty >= SAT_PLATEAU_DUTY
        && p.fan_med
            .zip(j.fan_auto_max)
            .is_some_and(|(f, m)| f >= SAT_PLATEAU_FAN * m);
    let basis = match (view.passive, view.active.as_ref().and_then(|a| a.t.zip(a.duty))) {
        (Some(ps), _) => Some((Basis::Passive, ps.t, ps.duty)),
        (None, Some((rt, rd))) => Some((Basis::SeedPhase, rt, rd)),
        (None, None) => None,
    };
    let (verdict, basis, against) = match basis {
        Some((basis, rt, rd)) => {
            let d = duty_lines(rd);
            let v = if !j.has_throttle {
                // Duty is the share of seconds at TCC − 2 (§6.3); frequency stands in for it.
                let rf = view.active.as_ref().and_then(|a| a.f);
                if t >= rt + 4.0 && p.f_med.zip(rf).is_some_and(|(f, r)| f <= 0.95 * r) {
                    Hot
                } else if t <= rt + 1.5 && duty <= d.clean {
                    Clean
                } else {
                    Neutral
                }
            } else if basis == Basis::Passive {
                if (t >= rt + 3.0 && duty >= d.hot) || duty >= d.only {
                    Hot
                } else if t <= rt + 1.5 && duty <= d.clean {
                    Clean
                } else {
                    Neutral
                }
            } else if (t >= rt + 4.0 && duty >= d.hot) || duty >= d.only + 0.02 {
                Hot
            } else if t <= rt + 2.5 && duty <= d.clean + 0.02 {
                Clean
            } else {
                Neutral
            };
            (if saturated { Hot } else { v }, basis, Some((rt, rd)))
        }
        None if saturated => (Hot, Basis::Saturation, None),
        None => return none,
    };
    let r_fan = j.r_fan_override.or_else(|| view.r_fan());
    // An accepted fan speed replaces the Duo's 6500-rpm settle line as well as R_fan, and an
    // accepted slow ramp silences that line alone.
    let fan_low = j.r_fan_override.is_none()
        && !j.ramp_accepted
        && p.flags.iter().any(|f| f == "fan_low");
    let fan_slow = j.has_fan
        && verdict != Clean
        && (fan_low
            || p.fan_med
                .zip(r_fan)
                .is_some_and(|(f, r)| f < r - FAN_SLOW_MARGIN));
    PlateauJudgement {
        verdict,
        basis,
        voting: true,
        fan_slow,
        against,
    }
}

/// The light-window verdict (§7.3). Returns the verdict and whether the window votes (E2 is
/// active for its key once the light reference is learned).
pub fn judge_light(w: &LightRecord, view: &KeyView, j: &JudgeCtx) -> (LightVerdict, bool) {
    let secs = if w.seconds > 0.0 { w.seconds } else { 60.0 };
    let duty_hot = w.hot_ms as f64 / (secs * 1000.0);
    let duty_cold = w.cold_ms as f64 / (secs * 1000.0);
    let line = view.light_p90.map(|p| (LIGHT_L_FACTOR * p).max(LIGHT_L_MIN));
    let saturated = j.has_fan
        && duty_hot >= SAT_LIGHT_DUTY
        && w.fan_p90
            .zip(j.fan_auto_max)
            .is_some_and(|(f, m)| f >= SAT_LIGHT_FAN * m);
    let verdict = if line.is_some_and(|l| duty_hot >= l) || saturated {
        LightVerdict::Hot
    } else if w.hot_ms == 0 && duty_cold >= COLD_WINDOW_DUTY {
        LightVerdict::Cold
    } else {
        LightVerdict::Neutral
    };
    (verdict, line.is_some())
}

/// A finished check's numbers (§10.5), as the check phase hands them over.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CheckMetrics {
    pub t_ss: Option<f64>,
    pub duty: Option<f64>,
    pub fan: Option<f64>,
    pub f: Option<f64>,
    pub work: Option<f64>,
    pub djc: Option<f64>,
    pub ths: Option<f64>,
    pub floor: Option<f64>,
    pub t_cool: Option<f64>,
    pub t_idle: Option<f64>,
}

/// The check verdict against a reference (§10.6). `same_rig` is true when the CPU model and the
/// generator version match the reference's, so work and frequency compare.
pub fn judge_check(m: &CheckMetrics, r: &CheckRef, same_rig: bool) -> CheckVerdict {
    let (Some(t), Some(duty), Some(rt), Some(rd)) = (m.t_ss, m.duty, r.t, r.duty) else {
        return CheckVerdict::Inconclusive;
    };
    let d = duty_lines(rd);
    let dt = t - rt;
    let f_low = same_rig && m.f.zip(r.f).is_some_and(|(f, rf)| f <= rf - 150.0);
    if (dt >= 3.0 && duty >= d.hot) || duty >= d.only || (f_low && duty >= d.hot) {
        return CheckVerdict::Worse;
    }
    let fan_ok = t < rt - 1.0 || m.fan.zip(r.fan).is_none_or(|(f, rf)| f >= rf - FAN_SLOW_MARGIN);
    // A missing value is unknown, not passing: work is dropped exactly when other processes made
    // it unreliable, and then the frequency has to decide. Only when neither compares does the
    // clause stand aside.
    let work = m.work.zip(r.work).map(|(w, rw)| w >= 0.97 * rw);
    let freq = m.f.zip(r.f).map(|(f, rf)| f >= rf - 100.0);
    let work_ok = !same_rig
        || work == Some(true)
        || freq == Some(true)
        || (work.is_none() && freq.is_none());
    if dt <= 2.0 && duty <= d.like && fan_ok && work_ok {
        CheckVerdict::Like
    } else {
        CheckVerdict::Borderline
    }
}

/// The post-clean verdict of a verify check (§10.8): P against R_best, with B = R_before.
/// `m` is P over the window R_best was measured in (the matched one for the seed), and `steady`
/// is P over the steady window, which is what a before-check and her builds compare with.
pub fn judge_post_clean(
    m: &CheckMetrics,
    steady: &CheckMetrics,
    best: &CheckRef,
    before: Option<&RBefore>,
    same_rig: bool,
) -> PostClean {
    let (Some(t), Some(duty)) = (m.t_ss, m.duty) else {
        return PostClean::Unknown;
    };
    if judge_check(m, best, same_rig) == CheckVerdict::Like {
        return PostClean::Restored;
    }
    let improved = match before.and_then(|b| b.t_ss.zip(b.duty)) {
        Some((bt, bd)) => {
            let (Some(t), Some(duty)) = (steady.t_ss, steady.duty) else {
                return PostClean::Unknown;
            };
            t <= bt - 2.0 || (duty <= bd / 2.0 && duty <= bd - 0.02)
        }
        None => {
            best.t.is_some_and(|rt| t <= rt + 5.0)
                && best.duty.is_some_and(|rd| duty <= rd + 0.06)
        }
    };
    if improved {
        PostClean::Improved
    } else {
        PostClean::NotImproved
    }
}

// ---------------------------------------------------------------------------------------------
// The engine.

/// What the rules know about the machine, fixed while the service runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Machine {
    pub chassis: Chassis,
    pub setting: ChassisSetting,
    pub info: StaticInfo,
    /// temp1_crit, °C (TjMax).
    pub tjmax_c: Option<f64>,
    pub has_fan: bool,
    pub has_throttle: bool,
    /// Light-load windows can be dust evidence (E2). Off, on every chassis (`LIGHT_LOAD_EVIDENCE`):
    /// a light window never votes, so it makes no E2 day, no qualifying day and no first evidence,
    /// and the light reference is not learned. Windows are still recorded, and cold throttle
    /// still raises "throttling while cool". Tests of the E2 rules turn it on.
    pub light_load_evidence: bool,
    /// Without E2 while `light_load_evidence` is off (`Engine::new` drops it).
    pub classes: Vec<String>,
}

/// The config key in force, as the service resolves it from the deriver.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyCtx {
    /// `Row::key_seq` of the rows under this key.
    pub seq: u32,
    pub key: ConfigKey,
    pub id: String,
    pub tcc_c: f64,
    /// MMIO `constraint_0_max_power_uw` in W, for the generic `power_capped` rule.
    pub pl1_max_w: Option<u32>,
}

impl KeyCtx {
    pub fn new(seq: u32, key: ConfigKey, tcc_c: f64, pl1_max_w: Option<u32>) -> KeyCtx {
        KeyCtx {
            seq,
            id: key.id(),
            key,
            tcc_c,
            pl1_max_w,
        }
    }
}

/// Something for the store to append or rewrite.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Event {
        at: Moment,
        kind: EventKind,
        case: Option<String>,
        text: String,
        data: Value,
    },
    Record(Record),
    Daily(DailyRecord),
    Case(CaseRecord),
    /// references.json changed.
    References,
}

/// A check finished (§10.8). The check phase computes the verdicts with `judge_check` and
/// `judge_post_clean`; the engine applies them.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckOutcome {
    /// When the check started.
    pub at: Moment,
    pub dir: String,
    pub purpose: CheckPurpose,
    pub verdict: CheckVerdict,
    /// Verify checks only.
    pub post_clean: Option<PostClean>,
    /// False after "Run anyway" on a key mismatch: not comparable with the clean reference.
    pub comparable: bool,
    pub key_id: String,
    /// The steady window [75, 178] s: what a stored reference keeps and a before-check gives
    /// R_before.
    pub metrics: CheckMetrics,
    /// The matched window [75, 118] s, which the seed is compared with (§10.5).
    pub matched: Option<CheckMetrics>,
    pub generator: Option<u32>,
    pub cpu: String,
    /// §10.7: fan_ss < R_fan − 400 with T_ss ≥ R_T − 1.
    pub fan_flag: bool,
    /// The §10.8 diagnosis lines, for Improved and Not improved; they go into the history.
    pub diagnosis: Vec<String>,
}

impl CheckOutcome {
    /// The numbers as they were compared with `r`: the matched window against the seed, whose
    /// load ended at 119 s, and the steady one against everything else, R_before included (all
    /// a run-anyway check is compared with). Shown values and bounds use this, so a history line
    /// never pairs a verdict with another window's temperature.
    pub fn against(&self, r: Option<&CheckRef>) -> &CheckMetrics {
        match (&self.matched, r) {
            (Some(m), Some(r)) if self.comparable && r.source == "seed" => m,
            _ => &self.metrics,
        }
    }
}

/// The earliest evidence-class record of a day: what first_evidence stamps (§8.1).
#[derive(Clone, Debug, PartialEq)]
struct Candidate {
    unix: i64,
    at: Stamp,
    class: &'static str,
    summary: String,
    record_id: Option<String>,
    key_id: String,
    top: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct LightDay {
    q: u32,
    values: Vec<f64>,
}

/// One local day's tally.
#[derive(Clone, Debug, Default)]
struct Day {
    voting: u32,
    hot: u32,
    clean: u32,
    /// Voting light windows (E2 active for their key).
    light_q: u32,
    light_hot: u32,
    light_cold_ms: u64,
    /// Every qualifying window per key, with the non-zero duty_hot values.
    light_keys: BTreeMap<String, LightDay>,
    unjudged: BTreeMap<String, u32>,
    first: Option<Candidate>,
    e1: bool,
    e2: bool,
    qualifying: bool,
    settled: bool,
    observed: bool,
    bins: DayBins,
}

impl Day {
    fn offer(&mut self, c: Candidate) {
        if self.first.as_ref().is_none_or(|f| c.unix < f.unix) {
            self.first = Some(c);
        }
    }
}

impl Candidate {
    fn first_evidence(&self) -> FirstEvidence {
        FirstEvidence {
            label: self.at.label(),
            at: self.at.clone(),
            class: self.class.to_owned(),
            summary: self.summary.clone(),
            record_id: self.record_id.clone(),
            key_id: self.key_id.clone(),
            top: self.top.clone(),
        }
    }
}

/// A plateau with metrics, for the 7-day medians, drift, R2 and verification.
#[derive(Clone, Debug)]
struct PlateauSum {
    onset: i64,
    day: Date,
    key_id: String,
    t: f64,
    duty: f64,
    fan: Option<f64>,
    verdict: PlateauVerdict,
    voting: bool,
    fan_slow: bool,
}

#[derive(Clone, Debug)]
struct ClosedCase {
    id: String,
    outcome: CaseOutcome,
    closed: Date,
    first_day: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct Alarms {
    fan_zero_since: Option<f64>,
    hot_since: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct ConfigWatch {
    /// The key without AC and lid, and its label.
    stable: Option<(String, String)>,
    pending: Option<(String, String, f64)>,
}

/// Counts over a run of days.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Tally {
    evidence: u32,
    e2: u32,
    qualifying: u32,
}

impl Tally {
    /// WATCH → SUSPICION over 14 days (§8): ≥ 2 evidence days, at least half the qualifying ones.
    fn suspicious(&self) -> bool {
        self.evidence >= 2 && 2 * self.evidence >= self.qualifying
    }
}

pub struct Engine {
    pub state: State,
    pub refs: References,
    pub machine: Machine,
    days: BTreeMap<Date, Day>,
    plateaus: Vec<PlateauSum>,
    /// The early busy frequency of every qualifying-length episode, valid or not: until
    /// f_allcore_max is known, a clean machine produces only `below_pl1` episodes (§6.3).
    f_early: Vec<(Date, f64)>,
    closed: Vec<ClosedCase>,
    effects: Vec<Effect>,
    key: Option<KeyCtx>,
    /// The latest local day seen; every day before it is settled.
    today: Option<Date>,
    /// REALTIME − BOOTTIME at the last row; it moves only when the wall clock is set.
    wall_base: Option<i64>,
    /// MONOTONIC s until which a forward step of the wall clock is held (`watch_clock`).
    clock_hold: Option<f64>,
    /// The last key on AC. Nothing is judged on battery (§3.3), so the state keeps showing
    /// this one meanwhile rather than an unreferenced battery key that would read LEARNING.
    ac_key: Option<KeyCtx>,
    alarms: Alarms,
    notice_logged: BTreeMap<String, Date>,
    config: ConfigWatch,
    hour: Option<i64>,
    refs_dirty: bool,
}

fn add_day(list: &mut Vec<String>, d: Date) {
    let k = d.key();
    if !list.contains(&k) {
        list.push(k);
        list.sort();
    }
}

fn case_id(at: &Stamp) -> String {
    at.local_civil().map_or_else(
        || "c-unknown".to_owned(),
        |c| {
            format!(
                "c-{:04}{:02}{:02}-{:02}{:02}",
                c.date.y, c.date.m, c.date.d, c.hour, c.minute
            )
        },
    )
}

fn stamp_date(s: &Stamp) -> Option<Date> {
    s.local_civil().map(|c| c.date)
}

/// A reference's date, from labels such as "duo-seed 20/11/2026 (…)", "check 1/12/2026" or
/// "checks 1/12/2026 + 2/12/2026" (the later one).
fn ref_day(r: &CheckRef) -> String {
    r.label
        .split_whitespace()
        .rev()
        .find(|w| Date::parse_dmy(w).is_some())
        .map(str::to_owned)
        .unwrap_or_default()
}

/// The local day an unjudged episode ended on: the day its record counts for, live and after a
/// restart alike.
fn unjudged_day(u: &UnjudgedRecord) -> Option<Date> {
    let c = u.onset.local_civil()?;
    let sod = (c.hour * 3600 + c.minute * 60 + c.second) as i64;
    let secs = c.date.days() * 86_400 + sod + u.duration_s.max(0.0) as i64;
    Some(Date::from_days(secs.div_euclid(86_400)))
}

/// When passive verification starts counting. A date-only clean happened at some unknown hour
/// of that day, and the builds before it were dusty, so it counts from the next midnight.
fn verify_from(v: &Verification) -> Option<i64> {
    v.cleaned_at
        .unix()
        .map(|c| if v.date_only { c + 86_400 } else { c })
}

fn verify_from_day(v: &Verification) -> Option<Date> {
    stamp_date(&v.cleaned_at).map(|d| if v.date_only { d.add_days(1) } else { d })
}

fn before(at: &Stamp, reset: Option<i64>) -> bool {
    reset.is_some_and(|r| at.unix().is_some_and(|u| u < r))
}

impl Engine {
    /// `state` is the loaded state.json, None on the very first start.
    pub fn new(mut machine: Machine, state: Option<State>, refs: References, now: &Moment) -> Engine {
        if !machine.light_load_evidence {
            machine.classes.retain(|c| c != "E2");
        }
        let fresh = state.is_none();
        // The versions are stamped just below, so the default is State::new().
        let mut state = state.unwrap_or_default();
        state.schema = SCHEMA;
        state.rules_version = RULES_VERSION;
        state.version = VERSION.to_owned();
        if fresh {
            state.setup_needed = true;
        }
        let previous_bios = std::mem::take(&mut state.chassis.bios);
        state.chassis = ChassisInfo {
            profile: machine.chassis,
            setting: machine.setting,
            board: machine.info.board_name.clone(),
            product: machine.info.product_name.clone(),
            bios: machine.info.bios_version.clone(),
            cpu: machine.info.cpu_model.clone(),
            tcc_c: state.chassis.tcc_c,
        };
        state.classes = machine.classes.clone();
        state.throttle_counter = Some(machine.has_throttle);
        let mut e = Engine {
            state,
            refs,
            machine,
            days: BTreeMap::new(),
            plateaus: Vec::new(),
            f_early: Vec::new(),
            closed: Vec::new(),
            effects: Vec::new(),
            key: None,
            today: None,
            wall_base: None,
            clock_hold: None,
            ac_key: None,
            alarms: Alarms::default(),
            notice_logged: BTreeMap::new(),
            config: ConfigWatch::default(),
            hour: None,
            refs_dirty: false,
        };
        // A BIOS change against the last run, or (first run) against the seed's firmware on her
        // board. References are kept either way.
        let before = if !previous_bios.is_empty() {
            Some(previous_bios)
        } else if e.machine.chassis == Chassis::Duo && e.machine.info.board_name == DUO_SEED.board
        {
            Some(references::SEED_BIOS.to_owned())
        } else {
            None
        };
        let now_bios = e.machine.info.bios_version.clone();
        if let Some(old) = before
            && !now_bios.is_empty()
            && old != now_bios
        {
            e.event(
                now,
                EventKind::BiosChange,
                format!("Firmware changed: {old} → {now_bios}"),
                json!({"from": old, "to": now_bios}),
            );
            e.notice(notice::BIOS_CHANGE, notice::bios_change(&old, &now_bios), now);
        }
        // Started on battery: the state keeps showing the key she was last judged under.
        if let Some(k) = ConfigKey::parse_id(&e.state.key.id).filter(|k| k.ac)
            && let Some(tcc) = e.state.chassis.tcc_c
        {
            e.ac_key = Some(KeyCtx::new(0, k, tcc, None));
        }
        e.state.learning.needed = references::PASSIVE_N as u32;
        e.state.learning.needed_days = references::PASSIVE_DAYS as u32;
        e.state.learning.light_needed = references::LIGHT_N;
        present::recompute(&mut e.state, now);
        e
    }

    /// Rebuilds the tallies from the store at start (§12.1 step 4). Days the service missed are
    /// settled at the first tick, from these records.
    pub fn restore(
        &mut self,
        daily: &[DailyRecord],
        records: &[Record],
        cases: &[CaseRecord],
        events: &[crate::model::Event],
        now: &Moment,
    ) {
        let oldest = now.date().add_days(-KEEP_DAYS);
        // With light-load evidence off, light windows do not vote. A daily line written while it
        // was on keeps its builds only (no E2 day, qualifying by builds alone), and a light record
        // adds only its cold throttle and its count per key.
        let light = self.light_evidence();
        for line in daily {
            let Some(d) = Date::parse_key(&line.day) else {
                continue;
            };
            if d < oldest {
                continue;
            }
            let minutes = [
                &line.bins.lt5,
                &line.bins.b5_30,
                &line.bins.b30_60,
                &line.bins.b60_85,
                &line.bins.ge85,
            ]
            .iter()
            .map(|b| b.min)
            .sum::<f64>();
            let x = self.days.entry(d).or_default();
            *x = Day {
                voting: line.voting,
                hot: line.hot,
                clean: line.clean,
                light_q: if light { line.light_q } else { 0 },
                light_hot: if light { line.light_hot } else { 0 },
                light_cold_ms: line.light_cold_ms,
                light_keys: line
                    .light_keys
                    .iter()
                    .map(|(k, &q)| {
                        (
                            k.clone(),
                            LightDay {
                                q,
                                values: Vec::new(),
                            },
                        )
                    })
                    .collect(),
                unjudged: line.unjudged.clone(),
                e1: line.e1,
                e2: light && line.e2,
                qualifying: if light {
                    line.qualifying
                } else {
                    line.voting >= 1 || line.e1
                },
                settled: true,
                observed: minutes > 0.0 || line.voting > 0 || line.light_q > 0,
                ..Day::default()
            };
        }
        // Records from before the last evidence reset (a clean, Not dust, a check that cleared
        // the case) were dropped from that day's tally when it happened.
        let reset = self.state.evidence_reset_at.as_ref().and_then(Stamp::unix);
        // First what the verdicts depend on: generic mode's f_allcore_max and fan_auto_max come
        // from the plateaus themselves.
        for r in records {
            match r {
                Record::Plateau(p) => {
                    let Some(d) = stamp_date(&p.end).filter(|d| *d >= oldest) else {
                        continue;
                    };
                    if let Some(f) = p.early.f_p95_first10 {
                        self.f_early.push((d, f));
                    }
                    if let (Some(t), Some(duty)) = (p.t_ss, p.duty) {
                        self.plateaus.push(PlateauSum {
                            onset: p.onset.unix().unwrap_or_default(),
                            day: d,
                            key_id: p.key_id.clone(),
                            t,
                            duty,
                            fan: p.fan_med,
                            verdict: p.verdict,
                            voting: !p.flags.iter().any(|f| f == "no_reference"),
                            fan_slow: p.flags.iter().any(|f| f == "fan_slow"),
                        });
                    }
                }
                Record::Unjudged(u) => {
                    if let (Some(d), Some(f)) = (unjudged_day(u), u.f_p95_first10)
                        && d >= oldest
                        && u.duration_s >= F_EARLY_MIN_S - EPS
                    {
                        self.f_early.push((d, f));
                    }
                }
                Record::Light(_) => {}
            }
        }
        let judge = self.judge_ctx();
        for r in records {
            match r {
                Record::Plateau(p) => {
                    let Some(d) = stamp_date(&p.end).filter(|d| *d >= oldest) else {
                        continue;
                    };
                    let voting = !p.flags.iter().any(|f| f == "no_reference");
                    let against = judge_plateau(p, &self.view(&p.key_id), &judge).against;
                    let x = self.days.entry(d).or_default();
                    if x.settled {
                        continue;
                    }
                    x.observed = true;
                    if !voting {
                        *x.unjudged.entry("no_reference".to_owned()).or_default() += 1;
                    } else if !before(&p.onset, reset) {
                        x.voting += 1;
                        match p.verdict {
                            PlateauVerdict::Hot => {
                                x.hot += 1;
                                x.offer(Self::plateau_candidate(p, against));
                            }
                            PlateauVerdict::Clean => x.clean += 1,
                            PlateauVerdict::Neutral => {}
                        }
                    }
                }
                Record::Light(l) => {
                    let Some(d) = stamp_date(&l.start).filter(|d| *d >= oldest) else {
                        continue;
                    };
                    let view = self.view(&l.key_id);
                    let (verdict, voting) = judge_light(l, &view, &judge);
                    let secs = if l.seconds > 0.0 { l.seconds } else { 60.0 };
                    let duty_hot = l.hot_ms as f64 / (secs * 1000.0);
                    let x = self.days.entry(d).or_default();
                    let settled = x.settled;
                    let lk = x.light_keys.entry(l.key_id.clone()).or_default();
                    if duty_hot > 0.0 {
                        lk.values.push(duty_hot);
                    }
                    if settled {
                        continue;
                    }
                    // Only hot and cold windows are records, so a restart loses the day's
                    // neutral ones; these at least count as they did live.
                    lk.q += 1;
                    x.observed = true;
                    x.light_cold_ms += l.cold_ms;
                    if before(&l.start, reset) || !light {
                        continue;
                    }
                    if voting {
                        x.light_q += 1;
                    }
                    if verdict == LightVerdict::Hot {
                        x.light_hot += 1;
                        x.offer(Self::light_candidate(l, view.light_p90));
                    }
                }
                Record::Unjudged(u) => {
                    let Some(d) = unjudged_day(u).filter(|d| *d >= oldest) else {
                        continue;
                    };
                    let x = self.days.entry(d).or_default();
                    if !x.settled {
                        x.observed = true;
                        *x.unjudged.entry(u.reason.clone()).or_default() += 1;
                    }
                }
            }
        }
        // E2 days of unsettled days count as decided.
        for x in self.days.values_mut() {
            if light && !x.settled && x.light_hot >= E2_DAY_WINDOWS {
                x.e2 = true;
            }
        }
        for c in cases {
            self.closed.push(ClosedCase {
                id: c.id.clone(),
                outcome: c.outcome,
                closed: stamp_date(&c.closed).unwrap_or(now.date()),
                first_day: c.first_evidence.as_ref().map(|f| f.at.date_label()),
            });
        }
        for ev in events {
            if ev.kind == EventKind::Notice
                && let (Some(id), Some(d)) = (
                    ev.data.get("id").and_then(Value::as_str),
                    stamp_date(&ev.at),
                )
            {
                let e = self.notice_logged.entry(id.to_owned()).or_insert(d);
                *e = (*e).max(d);
            }
        }
        // The first tick settles from here: the earliest day not settled yet. That is the day
        // after the last daily line, unless a day before it has records and no line (a plateau
        // that closed in the seconds after midnight, just before a shutdown; an append that
        // failed on a full disk). Settling starts at the earliest of those, and days already
        // settled on the way are skipped.
        let after_settled = self
            .days
            .iter()
            .filter(|(_, x)| x.settled)
            .map(|(d, _)| d.add_days(1))
            .max();
        let first_open = self
            .days
            .iter()
            .filter(|(_, x)| !x.settled)
            .map(|(d, _)| *d)
            .min();
        self.today = after_settled.into_iter().chain(first_open).min();
        self.reconsider_light_case(records, now);
        self.refresh(now);
        present::recompute(&mut self.state, now);
    }

    /// Drains what the store must write.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// Learning samples not yet written: references.json is rewritten when learning completes,
    /// at day settle and hourly, not per window. The service calls this before it exits.
    pub fn flush_references(&mut self) {
        if std::mem::take(&mut self.refs_dirty) {
            self.effects.push(Effect::References);
        }
    }

    /// A references.json write failed. Learned values exist only in memory then, and some
    /// cannot be learned again outside a known-clean window, so the next hourly, settle or
    /// shutdown flush writes the file again.
    pub fn mark_refs_dirty(&mut self) {
        self.refs_dirty = true;
    }

    /// The detector's tuning under `key`.
    pub fn tuning(&self, key: &KeyCtx) -> Tuning {
        Tuning {
            chassis: self.machine.chassis,
            tcc_c: key.tcc_c,
            has_fan: self.machine.has_fan,
            has_throttle: self.machine.has_throttle,
            f_allcore_max: self.f_allcore_max(),
            nproc: self.machine.info.nproc.max(1),
            bios: self.machine.info.bios_version.clone(),
            cpu: self.machine.info.cpu_model.clone(),
        }
    }

    pub fn view(&self, key_id: &str) -> KeyView {
        KeyView::of(
            &self.refs,
            key_id,
            self.machine.chassis,
            &self.machine.info.board_name,
        )
    }

    /// Known for the Duo i7; otherwise the 95th percentile of the busy-CPU frequency in the first
    /// 10 s of episodes of at least 120 s, valid or not, once three exist (§6.3).
    pub fn f_allcore_max(&self) -> Option<u32> {
        profile::f_allcore_max(self.machine.chassis, &self.machine.info.cpu_model).or_else(|| {
            let f: Vec<f64> = self.f_early.iter().map(|&(_, f)| f).collect();
            (f.len() >= 3)
                .then(|| percentile(&f, 0.95))
                .flatten()
                .map(|v| v.round() as u32)
        })
    }

    /// The automatic fan maximum: the Duo constant, or the 99th percentile of plateau fan
    /// medians once three exist.
    pub fn fan_auto_max(&self) -> Option<f64> {
        if self.machine.chassis == Chassis::Duo {
            return Some(DUO.fan_auto_max as f64);
        }
        let f: Vec<f64> = self.plateaus.iter().filter_map(|p| p.fan).collect();
        (f.len() >= 3).then(|| percentile(&f, 0.99)).flatten()
    }

    /// A store file did not parse and the service set it aside at start. The journal line alone
    /// would leave a warning that vanished with no trace in the window or in History, so this
    /// raises a notice and logs it as an event too.
    pub fn store_unreadable(&mut self, id: &str, text: String, now: &Moment) {
        self.notice(id, text, now);
        present::recompute(&mut self.state, now);
    }

    /// `thermal.chassis` changed while the service runs (the 60-s poll or `settings.reload`).
    /// The views, the seed and the detector's tuning all follow `machine.chassis`, so the key
    /// block is refreshed at once rather than at the next hour.
    pub fn set_chassis(&mut self, chassis: Chassis, setting: ChassisSetting, now: &Moment) {
        self.machine.chassis = chassis;
        self.machine.setting = setting;
        self.state.chassis.profile = chassis;
        self.state.chassis.setting = setting;
        self.refresh(now);
        present::recompute(&mut self.state, now);
    }

    /// Whether light-load windows can be dust evidence (E2), off since 21/11/2026. Every rule
    /// that lets a light window vote, learn or count asks this.
    fn light_evidence(&self) -> bool {
        self.machine.light_load_evidence
    }

    fn judge_ctx(&self) -> JudgeCtx {
        JudgeCtx {
            has_fan: self.machine.has_fan,
            has_throttle: self.machine.has_throttle,
            fan_auto_max: self.fan_auto_max(),
            r_fan_override: (self.state.fan.accepted || self.state.fan.relearned)
                .then_some(self.state.fan.r_fan.map(f64::from))
                .flatten(),
            ramp_accepted: self.state.fan.ramp_accepted,
        }
    }

    fn event(&mut self, at: &Moment, kind: EventKind, text: String, data: Value) {
        let case = self.state.case.as_ref().map(|c| c.id.clone());
        self.effects.push(Effect::Event {
            at: *at,
            kind,
            case,
            text,
            data,
        });
    }

    /// Sets a notice and logs it, at most once per id per day.
    fn notice(&mut self, id: &str, text: String, now: &Moment) {
        match self.state.notices.iter_mut().find(|n| n.id == id) {
            Some(n) => n.text = text.clone(),
            None => self.state.notices.push(Notice {
                id: id.to_owned(),
                since: now.stamp(),
                text: text.clone(),
            }),
        }
        let today = now.date();
        if self.notice_logged.get(id) != Some(&today) {
            self.notice_logged.insert(id.to_owned(), today);
            self.event(now, EventKind::Notice, text, json!({ "id": id }));
        }
    }

    fn clear_notice(&mut self, id: &str) {
        self.state.notices.retain(|n| n.id != id);
    }

    fn dust_case_open(&self) -> bool {
        self.state.status.dust_case_open()
    }

    /// The key the state describes: the one in force, or on battery the last one on AC.
    fn shown_key(&self) -> Option<&KeyCtx> {
        match &self.key {
            Some(k) if !k.key.ac => self.ac_key.as_ref().or(Some(k)),
            k => k.as_ref(),
        }
    }

    fn idle_status(&self) -> Status {
        match self.shown_key() {
            Some(k) if self.view(&k.id).referenced() => Status::Clean,
            Some(_) => Status::Learning,
            None => match self.state.status {
                Status::Clean => Status::Clean,
                _ => Status::Learning,
            },
        }
    }

    fn floor(&self) -> Option<Date> {
        self.state.evidence_from.as_deref().and_then(Date::parse_key)
    }

    fn counts_day(&self, d: Date) -> bool {
        self.floor().is_none_or(|f| d >= f)
    }

    fn in_window(&self, d: Date) -> bool {
        references::in_known_clean(self.state.known_clean_until.as_deref(), d)
    }

    /// Evidence counting starts again now: a clean today, a check that cleared the case, Not
    /// dust. Today's tally so far was judged against what has been refuted, so it is dropped,
    /// and `evidence_reset_at` keeps a restart from counting those records again.
    fn reset_evidence(&mut self, now: &Moment) {
        let today = now.date();
        self.state.evidence_from = Some(today.key());
        self.state.evidence_reset_at = Some(now.stamp());
        if let Some(x) = self.days.get_mut(&today) {
            x.voting = 0;
            x.hot = 0;
            x.clean = 0;
            x.light_q = 0;
            x.light_hot = 0;
            x.first = None;
            x.e2 = false;
        }
    }

    /// The day a record counts for: its own, unless that day has already settled (it closed
    /// long after it ended), and then the first day still open.
    fn count_day(&mut self, d: Date, now: &Moment) -> Date {
        self.advance_to(d, now);
        match self.today {
            Some(t) if self.clock_held(now) => t,
            Some(t) => d.max(t),
            None => d,
        }
    }

    /// (evidence, e2, qualifying) of a day as the state machine counts it.
    fn day_flags(&self, d: Date) -> (bool, bool, bool) {
        let Some(x) = self.days.get(&d) else {
            return (false, false, false);
        };
        if !self.counts_day(d) {
            return (false, false, false);
        }
        let evidence = (x.settled && x.e1) || x.e2;
        let qualifying = if x.settled {
            x.qualifying
        } else {
            x.voting >= 1 || x.light_q >= QUALIFYING_LIGHT
        };
        (evidence, x.e2, qualifying || evidence)
    }

    /// The `n` days ending on `end`.
    fn tally(&self, end: Date, n: i64) -> Tally {
        let mut t = Tally::default();
        for i in 0..n {
            let (ev, e2, q) = self.day_flags(end.add_days(-i));
            t.evidence += ev as u32;
            t.e2 += e2 as u32;
            t.qualifying += q as u32;
        }
        t
    }

    fn day_mut(&mut self, d: Date) -> &mut Day {
        self.days.entry(d).or_default()
    }

    // -----------------------------------------------------------------------------------------
    // Feeding.

    /// Hands one detector output to the rules. `top` is the top consumers the service measured
    /// for it (empty in tests); `now` is the time of the row that produced it.
    pub fn handle(&mut self, out: Output, top: Vec<(String, f64)>, now: &Moment) {
        match out {
            Output::PlateauOpened { .. } => return,
            Output::Plateau(Record::Plateau(p)) => self.plateau(*p, top, now),
            Output::Plateau(Record::Unjudged(u)) => {
                let day = self.count_day(unjudged_day(&u).unwrap_or(now.date()), now);
                if let Some(f) = u.f_p95_first10
                    && u.duration_s >= F_EARLY_MIN_S - EPS
                {
                    self.f_early.push((day, f));
                }
                let x = self.day_mut(day);
                x.observed = true;
                *x.unjudged.entry(u.reason.clone()).or_default() += 1;
                if u.reason == "void:cooling" {
                    self.notice(notice::COOLING_ACTIVE, notice::cooling_active(), now);
                }
                self.effects.push(Effect::Record(Record::Unjudged(u)));
                self.refresh(now);
            }
            Output::Plateau(Record::Light(_)) => {}
            Output::Window(w) => self.window(w, top, now),
        }
        present::recompute(&mut self.state, now);
    }

    /// Every row, after its outputs: the key, the day boundary, the bins, the alarms.
    pub fn tick(&mut self, row: &Row, key: &KeyCtx) {
        let now = row.at;
        self.watch_clock(&now);
        if self.key.as_ref() != Some(key) {
            self.key = Some(key.clone());
            if key.key.ac {
                self.ac_key = Some(key.clone());
            }
            if self.state.case.is_none() {
                self.state.status = self.idle_status();
            }
            self.refresh(&now);
        }
        self.advance_to(now.plus(-SETTLE_LAG_S).date(), &now);
        let day = self.day_of(&now);
        let x = self.day_mut(day);
        x.bins.add(row, key.tcc_c);
        x.observed = true;
        self.run_alarms(row, key, &now);
        self.watch_config(key, &now);
        let hour = now.unix_s().div_euclid(3600);
        if self.hour != Some(hour) {
            self.hour = Some(hour);
            self.flush_references();
            self.refresh(&now);
        }
        present::recompute(&mut self.state, &now);
    }

    /// A wall clock that steps more than an hour ahead while the machine runs (a mistyped
    /// `date`, a bad time server) holds the day boundary for ten minutes. Taken at once, a few
    /// seconds a year ahead would settle sixty empty future days, drop the real history from the
    /// tallies and put every later record on that far day until real time caught up. A step
    /// back within the hold cancels it; a step that stands is taken once the hold is over.
    fn watch_clock(&mut self, now: &Moment) {
        let base = now.real_ns - now.boot_ns;
        if let Some(old) = self.wall_base {
            if base - old > CLOCK_STEP_NS {
                self.clock_hold = Some(now.mono_s() + CLOCK_HOLD_S);
            } else if base - old < -CLOCK_STEP_NS {
                self.clock_hold = None;
            }
        }
        self.wall_base = Some(base);
    }

    fn clock_held(&self, now: &Moment) -> bool {
        self.clock_hold.is_some_and(|h| now.mono_s() < h)
    }

    /// The local day `now` belongs to, or the day still open while a clock step is held.
    fn day_of(&self, now: &Moment) -> Date {
        match self.today {
            Some(t) if self.clock_held(now) => t,
            _ => now.date(),
        }
    }

    /// Settles every day before `d` that is not settled yet.
    fn advance_to(&mut self, d: Date, now: &Moment) {
        let Some(cur) = self.today else {
            self.today = Some(d);
            return;
        };
        if d <= cur || self.clock_held(now) {
            return;
        }
        let mut x = cur.max(d.add_days(-SETTLE_BACK_DAYS));
        while x < d {
            self.settle(x, now);
            x = x.add_days(1);
        }
        self.today = Some(d);
        let oldest = d.add_days(-KEEP_DAYS);
        self.days.retain(|k, _| *k >= oldest);
        self.plateaus.retain(|p| p.day >= oldest);
        self.f_early.retain(|&(day, _)| day >= oldest);
    }

    fn plateau_candidate(p: &PlateauRecord, against: Option<(f64, f64)>) -> Candidate {
        let top: Vec<String> = p.top.iter().map(|(c, _)| c.clone()).collect();
        Candidate {
            unix: p.onset.unix().unwrap_or_default(),
            at: p.onset.clone(),
            class: "E1",
            summary: present::build_summary(
                p.t_ss.unwrap_or(f64::NAN),
                p.duty.unwrap_or(f64::NAN),
                against,
                p.fan_med,
                &top,
            ),
            record_id: Some(p.id.clone()),
            key_id: p.key_id.clone(),
            top,
        }
    }

    fn light_candidate(l: &LightRecord, p90: Option<f64>) -> Candidate {
        let top: Vec<String> = l.top.iter().map(|(c, _)| c.clone()).collect();
        let secs = if l.seconds > 0.0 { l.seconds } else { 60.0 };
        Candidate {
            unix: l.start.unix().unwrap_or_default(),
            at: l.start.clone(),
            class: "E2",
            summary: present::light_summary(
                l.threads,
                l.hot_ms as f64 / (secs * 1000.0),
                l.t_max,
                p90,
                &top,
            ),
            record_id: Some(l.id.clone()),
            key_id: l.key_id.clone(),
            top,
        }
    }

    fn plateau(&mut self, mut p: PlateauRecord, top: Vec<(String, f64)>, now: &Moment) {
        // The day it ended, as restore and replay count it.
        let day = self.count_day(stamp_date(&p.end).unwrap_or(now.date()), now);
        p.top = top;
        if let Some(f) = p.early.f_p95_first10 {
            self.f_early.push((day, f));
        }
        let view = self.view(&p.key_id);
        let j = judge_plateau(&p, &view, &self.judge_ctx());
        p.verdict = j.verdict;
        if j.fan_slow {
            p.flags.push("fan_slow".to_owned());
        }
        if !j.voting {
            p.flags.push("no_reference".to_owned());
        }
        let onset = p.onset.unix().unwrap_or(now.unix_s());

        // R_passive learns from her builds under any key while no dust case is open.
        if let (Some(t), Some(duty)) = (p.t_ss, p.duty)
            && !self.dust_case_open()
        {
            let verified = self.in_window(day);
            let hold = self.state.status == Status::Verifying;
            let active_t = view.active.as_ref().and_then(|a| a.t);
            let k = self.refs.keys.entry(p.key_id.clone()).or_default();
            let learning = k.passive.as_ref().is_none_or(|ps| {
                ps.held.is_none() && (ps.state != PassiveState::Learned || ps.relearning)
            });
            if learning {
                self.refs_dirty = true;
                let sample = PassiveSample {
                    day: day.key(),
                    t,
                    duty,
                    fan: p.fan_med,
                };
                if let Some(l) = references::learn_passive(k, sample, active_t, verified, hold) {
                    self.learned(l, &p.key_id, active_t, hold, now);
                }
            }
        }
        if let (Some(t), Some(duty)) = (p.t_ss, p.duty) {
            self.plateaus.push(PlateauSum {
                onset,
                day,
                key_id: p.key_id.clone(),
                t,
                duty,
                fan: p.fan_med,
                verdict: p.verdict,
                voting: j.voting,
                fan_slow: j.fan_slow,
            });
        }

        // Notices that are never evidence.
        if p.flags.iter().any(|f| f == "cooling_active" || f == "thermald") {
            self.notice(notice::COOLING_ACTIVE, notice::cooling_active(), now);
        } else {
            self.clear_notice(notice::COOLING_ACTIVE);
        }
        match self.power_capped(&p) {
            Some(pl1) => self.notice(notice::POWER_CAPPED, notice::power_capped(pl1), now),
            None if j.voting => self.clear_notice(notice::POWER_CAPPED),
            None => {}
        }

        let counts = self.counts_day(day);
        let x = self.day_mut(day);
        x.observed = true;
        if j.voting {
            x.voting += 1;
            match j.verdict {
                PlateauVerdict::Hot => {
                    x.hot += 1;
                    x.offer(Self::plateau_candidate(&p, j.against));
                }
                PlateauVerdict::Clean => x.clean += 1,
                PlateauVerdict::Neutral => {}
            }
        } else {
            *x.unjudged.entry("no_reference".to_owned()).or_default() += 1;
        }

        if j.voting {
            let fan = p.fan_med.map(|f| f.round() as u32);
            self.state.last_plateau = Some(LastPlateau {
                label: p.onset.label(),
                t_ss: p.t_ss.unwrap_or(f64::NAN),
                duty: p.duty.unwrap_or(f64::NAN),
                fan,
                verdict: p.verdict,
                record_id: p.id.clone(),
            });
            if fan.is_some() && self.state.fan.status == FanStatus::Ok {
                self.state.fan.last_fan = fan;
            }
            if j.fan_slow {
                self.fan_slow_rule(&p, &view, now);
            }
            // Passive verification counts the builds since the clean.
            if self.state.status == Status::Verifying
                && counts
                && let Some(v) = &mut self.state.verification
                && verify_from(v).is_some_and(|c| onset >= c)
                && let Some(pv) = &mut v.passive
            {
                pv.voting += 1;
                match p.verdict {
                    PlateauVerdict::Hot => pv.hot += 1,
                    PlateauVerdict::Clean => pv.clean += 1,
                    PlateauVerdict::Neutral => {}
                }
            }
        }
        self.effects.push(Effect::Record(Record::Plateau(Box::new(p))));
        self.refresh(now);
    }

    /// §7.2: PL1 PL2 or the load says the power is capped. Returns PL1 for the text.
    fn power_capped(&self, p: &PlateauRecord) -> Option<Option<u32>> {
        let k = ConfigKey::parse_id(&p.key_id)?;
        let f_max = self.f_allcore_max().map(f64::from);
        let load = p.busy.is_some_and(|b| b >= 0.9)
            && p.t_ss.is_some_and(|t| t < 75.0)
            && p.f_med.zip(f_max).is_some_and(|(f, m)| f < 0.75 * m)
            && p.duty.is_some_and(|d| d < 0.005);
        let pl2_low = k.pl1_w.zip(k.pl2_w).is_some_and(|(a, b)| b < a);
        let pl1_low = match self.machine.chassis {
            Chassis::Duo => k.pl1_w.is_some_and(|w| w < 20),
            Chassis::Generic => {
                let max = self
                    .key
                    .as_ref()
                    .filter(|c| c.id == p.key_id)
                    .and_then(|c| c.pl1_max_w);
                k.pl1_w
                    .zip(max)
                    .is_some_and(|(w, m)| (w as f64) < 0.6 * m as f64)
            }
        };
        (load || pl2_low || pl1_low).then_some(k.pl1_w)
    }

    fn learned(&mut self, l: Learned, key_id: &str, active_t: Option<f64>, held: bool, now: &Moment) {
        self.refs_dirty = true;
        let held_text = if held {
            " (in use once the clean is verified)"
        } else {
            ""
        };
        match l {
            Learned::Passive(p, label) => {
                self.effects.push(Effect::References);
                self.refs_dirty = false;
                self.clear_notice(notice::PASSIVE_SUSPECT);
                self.event(
                    now,
                    EventKind::Reference,
                    format!(
                        "Build reference {label}: {} °C, {} throttled{}{held_text}",
                        present::c(p.t),
                        present::pct(p.duty),
                        p.fan
                            .map(|f| format!(", fan {f:.0} rpm"))
                            .unwrap_or_default()
                    ),
                    json!({"key_id": key_id, "kind": "passive", "t": p.t, "duty": p.duty, "fan": p.fan, "held": held}),
                );
            }
            Learned::Suspect(p) => {
                self.event(
                    now,
                    EventKind::Reference,
                    format!(
                        "Build reference suspect: {} °C is more than 3 °C above the reference check; learning continues",
                        present::c(p.t)
                    ),
                    json!({"key_id": key_id, "kind": "passive_suspect", "t": p.t, "active_t": active_t}),
                );
            }
            Learned::Light { p90, raw, capped } => {
                self.effects.push(Effect::References);
                self.refs_dirty = false;
                self.event(
                    now,
                    EventKind::Reference,
                    format!(
                        "Light-load reference learned: p90 {} throttled{}{held_text}",
                        present::pct(p90),
                        if capped { " (capped)" } else { "" }
                    ),
                    json!({"key_id": key_id, "kind": "light", "p90": p90, "raw": raw, "held": held}),
                );
                if capped {
                    self.notice(notice::LIGHT_CAP, notice::light_cap(raw), now);
                } else if !held {
                    self.clear_notice(notice::LIGHT_CAP);
                }
            }
        }
    }

    fn window(&mut self, w: LightWindow, top: Vec<(String, f64)>, now: &Moment) {
        if !self.machine.has_throttle {
            // E2 needs the package throttle counter (§6.3).
            return;
        }
        let day = self.count_day(w.start.date(), now);
        let view = self.view(&w.key_id);
        let duty_hot = w.duty_hot();
        let mut rec = LightRecord {
            id: crate::episodes::record_id("l", &w.start),
            rules_version: RULES_VERSION,
            start: w.start.stamp(),
            key_id: w.key_id.clone(),
            seconds: w.seconds,
            busy: w.busy,
            threads: w.threads,
            t_p90: w.t_p90,
            t_max: w.t_max,
            fan_p90: w.fan_p90,
            hot_ms: w.hot_ms,
            cold_ms: w.cold_ms,
            reconcile_share: w.reconcile_share,
            deficit: w.deficit,
            top,
            verdict: LightVerdict::Neutral,
        };
        let (verdict, voting) = judge_light(&rec, &view, &self.judge_ctx());
        rec.verdict = verdict;
        // Off (21/11/2026): the window is recorded, counted per key and its cold throttle added
        // up. It does not vote, so it makes no E2 day, no qualifying day and no first evidence. The
        // light reference is not learned either: what it would learn now includes the reconcile
        // windows that made E2 unusable, and a capped learn would tell her to check the vents.
        let light = self.light_evidence();

        if light && !self.dust_case_open() {
            let verified = self.in_window(day);
            let hold = self.state.status == Status::Verifying;
            let k = self.refs.keys.entry(w.key_id.clone()).or_default();
            if k
                .light
                .as_ref()
                .is_none_or(|l| l.held.is_none() && (l.p90.is_none() || l.relearning))
            {
                self.refs_dirty = true;
                if let Some(l) = references::learn_light(k, duty_hot, verified, day, hold) {
                    self.learned(l, &w.key_id, None, hold, now);
                }
            }
        }

        let counts = self.counts_day(day);
        let x = self.day_mut(day);
        x.observed = true;
        let lk = x.light_keys.entry(w.key_id.clone()).or_default();
        lk.q += 1;
        if duty_hot > 0.0 {
            lk.values.push(duty_hot);
        }
        if light && voting {
            x.light_q += 1;
        }
        let cold_before = x.light_cold_ms;
        x.light_cold_ms += w.cold_ms;
        let cold_crossed = cold_before <= COLD_NOTICE_MS && x.light_cold_ms > COLD_NOTICE_MS;
        let mut e2_now = false;
        if light && verdict == LightVerdict::Hot {
            x.light_hot += 1;
            x.offer(Self::light_candidate(&rec, view.light_p90));
            if x.light_hot == E2_DAY_WINDOWS && !x.e2 {
                x.e2 = true;
                e2_now = true;
            }
        }
        if cold_crossed {
            self.notice(notice::COLD_THROTTLE, notice::cold_throttle(), now);
        }
        if rec.hot_ms > 0 || verdict == LightVerdict::Cold {
            self.effects.push(Effect::Record(Record::Light(rec)));
            self.refresh(now);
        }
        if e2_now && counts {
            self.e2_day(day, now);
            self.refresh(now);
        }
    }

    /// The 10th hot window of a day: an E2 evidence day, decided at once (§7.4).
    fn e2_day(&mut self, d: Date, now: &Moment) {
        match self.state.status {
            Status::Watch | Status::Suspicion | Status::Warning => {
                if let Some(c) = &mut self.state.case {
                    add_day(&mut c.evidence_days, d);
                    add_day(&mut c.e2_days, d);
                    add_day(&mut c.qualifying_days, d);
                }
                self.escalate(d, now);
            }
            Status::Verifying => {
                let after_clean = self
                    .state
                    .verification
                    .as_ref()
                    .and_then(verify_from_day)
                    .is_some_and(|c| d >= c);
                if after_clean
                    && let Some(pv) = self
                        .state
                        .verification
                        .as_mut()
                        .and_then(|v| v.passive.as_mut())
                {
                    pv.e2_days += 1;
                }
                self.verify_passively(d, now);
            }
            _ => self.open_case(d, now),
        }
    }

    fn settle(&mut self, d: Date, now: &Moment) {
        let x = self.days.entry(d).or_default();
        if x.settled {
            return;
        }
        x.e1 = x.hot >= 1 && x.hot >= x.clean;
        x.qualifying = x.voting >= 1 || x.light_q >= QUALIFYING_LIGHT || x.e1 || x.e2;
        x.settled = true;
        let line = DailyRecord {
            day: d.key(),
            rules_version: RULES_VERSION,
            bins: x.bins.finish(),
            voting: x.voting,
            hot: x.hot,
            clean: x.clean,
            light_q: x.light_q,
            light_hot: x.light_hot,
            qualifying: x.qualifying,
            e1: x.e1,
            e2: x.e2,
            unjudged: x.unjudged.clone(),
            light_cold_ms: x.light_cold_ms,
            light_keys: x.light_keys.iter().map(|(k, v)| (k.clone(), v.q)).collect(),
        };
        let (evidence, e2, qualifying) = (x.e1 || x.e2, x.e2, x.qualifying);
        let cold = x.light_cold_ms > COLD_NOTICE_MS;
        self.effects.push(Effect::Daily(line));
        if self.counts_day(d) {
            match self.state.status {
                Status::Watch | Status::Suspicion | Status::Warning => {
                    if let Some(c) = &mut self.state.case {
                        if qualifying {
                            add_day(&mut c.qualifying_days, d);
                        }
                        if evidence {
                            add_day(&mut c.evidence_days, d);
                        }
                        if e2 {
                            add_day(&mut c.e2_days, d);
                        }
                    }
                    self.escalate(d, now);
                    self.lapse(d, now);
                }
                Status::Verifying => self.verify_passively(d, now),
                _ => {
                    if evidence {
                        self.open_case(d, now);
                    }
                }
            }
        }
        if !cold {
            self.clear_notice(notice::COLD_THROTTLE);
        }
        for (id, days) in [
            (notice::BIOS_CHANGE, BIOS_NOTICE_DAYS),
            (notice::STATE_UNREADABLE, STORE_NOTICE_DAYS),
            (notice::REFERENCES_UNREADABLE, STORE_NOTICE_DAYS),
        ] {
            let old = self.state.notices.iter().any(|n| {
                n.id == id && stamp_date(&n.since).is_some_and(|s| s <= d.add_days(-days))
            });
            if old {
                self.clear_notice(id);
            }
        }
        self.starvation(d, now);
        self.suspect_notice(d, now);
        self.flush_references();
        self.refresh(now);
    }

    // -----------------------------------------------------------------------------------------
    // The case.

    fn open_case(&mut self, d: Date, now: &Moment) {
        let Some(first) = self.days.get(&d).and_then(|x| x.first.clone()) else {
            return;
        };
        let earlier = self
            .closed
            .iter()
            .rev()
            .find(|c| {
                matches!(c.outcome, CaseOutcome::Lapsed | CaseOutcome::ClearedByCheck)
                    && c.closed >= d.add_days(-60)
            })
            .map(|c| Sighting {
                case_id: c.id.clone(),
                day: c.closed.key(),
                label: c.first_day.clone().unwrap_or_else(|| c.closed.label()),
            });
        let (_, e2, _) = self.day_flags(d);
        let mut case = Case {
            id: case_id(&first.at),
            opened: now.stamp(),
            first_evidence: Some(first.first_evidence()),
            earlier_sighting: earlier.clone(),
            ..Case::default()
        };
        add_day(&mut case.evidence_days, d);
        add_day(&mut case.qualifying_days, d);
        if e2 {
            add_day(&mut case.e2_days, d);
        }
        self.state.case = Some(case);
        self.state.status = Status::Watch;
        self.state.snooze = Snooze::default();
        self.event(
            now,
            EventKind::FirstEvidence,
            present::first_evidence_event(&first.summary, earlier.as_ref().map(|e| e.label.as_str())),
            json!({"class": first.class, "record_id": first.record_id, "key_id": first.key_id,
                   "at": first.at, "top": first.top}),
        );
        self.escalate(d, now);
    }

    /// The R2 guard (§7.4): passive escalation is blocked while the most recent voting plateau of
    /// the last 7 days is clean.
    fn r2_passes(&self, now: &Moment) -> bool {
        let since = now.unix_s() - 7 * 86_400;
        let floor = self.floor();
        self.plateaus
            .iter()
            .filter(|p| p.voting && p.onset >= since && floor.is_none_or(|f| p.day >= f))
            .max_by_key(|p| p.onset)
            .is_none_or(|p| p.verdict != PlateauVerdict::Clean)
    }

    /// WATCH → SUSPICION, and the passive routes (b) (c) (d) to WARNING (§8).
    fn escalate(&mut self, d: Date, now: &Moment) {
        let status = self.state.status;
        if !matches!(status, Status::Watch | Status::Suspicion) {
            return;
        }
        let w21 = self.tally(d, 21);
        let w14 = self.tally(d, 14);
        let r2 = self.r2_passes(now);
        let suspected = self
            .state
            .case
            .as_ref()
            .and_then(|c| c.suspected_at.as_ref())
            .and_then(stamp_date);
        let route = if r2 && w21.evidence >= 5 && 3 * w21.evidence >= 2 * w21.qualifying && w21.e2 >= 1
        {
            Some(("b", w21))
        } else if r2 && w14.e2 >= 3 {
            Some(("c", w14))
        } else if r2
            && status == Status::Suspicion
            // Days s..=d are 21 days in SUSPICION.
            && suspected.is_some_and(|s| s <= d.add_days(-20))
            && w21.evidence >= 7
            && 3 * w21.evidence >= 2 * w21.qualifying
        {
            Some(("d", w21))
        } else {
            None
        };
        if let Some((route, t)) = route {
            self.warn(route, present::warning_event(route, t.evidence, t.qualifying, t.e2), now);
        } else if status == Status::Watch && w14.suspicious() {
            if let Some(c) = &mut self.state.case {
                c.suspected_at = Some(now.stamp());
            }
            self.state.status = Status::Suspicion;
            self.event(
                now,
                EventKind::Suspicion,
                present::suspicion_event(w14.evidence, w14.qualifying),
                json!({"evidence_days": w14.evidence, "qualifying_days": w14.qualifying}),
            );
        }
    }

    fn warn(&mut self, route: &str, text: String, now: &Moment) {
        if let Some(c) = &mut self.state.case {
            c.warning_since = Some(now.stamp());
            c.route = Some(route.to_owned());
        }
        self.state.status = Status::Warning;
        self.event(now, EventKind::Warning, text, json!({ "route": route }));
    }

    /// WATCH lapses after 14 days with no evidence day and at least 3 qualifying days (§8.2).
    fn lapse(&mut self, d: Date, now: &Moment) {
        if self.state.status != Status::Watch {
            return;
        }
        let t = self.tally(d, 14);
        let all_settled = (0..14).all(|i| {
            self.days
                .get(&d.add_days(-i))
                .is_some_and(|x| x.settled)
                || !self.counts_day(d.add_days(-i))
        });
        if t.evidence == 0 && t.qualifying >= 3 && all_settled {
            let first = self.first_label();
            self.event(
                now,
                EventKind::Lapse,
                present::lapse_event(t.qualifying, &first),
                json!({ "qualifying_days": t.qualifying }),
            );
            self.close_case(CaseOutcome::Lapsed, now, "lapsed: no signs for 14 days".to_owned());
        }
    }

    fn first_label(&self) -> String {
        self.state
            .case
            .as_ref()
            .and_then(|c| c.first_evidence.as_ref())
            .map(|f| f.label.clone())
            .unwrap_or_default()
    }

    fn close_case(&mut self, outcome: CaseOutcome, now: &Moment, summary: String) {
        let Some(case) = self.state.case.take() else {
            return;
        };
        self.closed.push(ClosedCase {
            id: case.id.clone(),
            outcome,
            closed: now.date(),
            first_day: case.first_evidence.as_ref().map(|f| f.at.date_label()),
        });
        self.effects.push(Effect::Case(CaseRecord {
            id: case.id,
            rules_version: RULES_VERSION,
            opened: case.opened,
            first_evidence: case.first_evidence,
            closed: now.stamp(),
            outcome,
            cleaned: case.cleaned,
            check_dirs: case
                .checks
                .iter()
                .map(|c| c.dir.clone())
                .filter(|d| !d.is_empty())
                .collect(),
            summary,
        }));
        self.state.verification = None;
        self.state.snooze = Snooze::default();
        self.state.status = self.idle_status();
        self.clear_notice(notice::LIKE_IN_WARNING);
        // References re-learned while the clean was being verified come into use now.
        self.settle_held(true);
    }

    /// Promotes or drops every key's held references (§9.1 step 3).
    fn settle_held(&mut self, promote: bool) {
        let mut changed = false;
        for k in self.refs.keys.values_mut() {
            changed |= references::settle_held(k, promote);
        }
        if changed {
            self.effects.push(Effect::References);
            self.refs_dirty = false;
        }
    }

    /// Passive verification after a clean (§9.1 step 5).
    fn verify_passively(&mut self, d: Date, now: &Moment) {
        if self.state.status != Status::Verifying {
            return;
        }
        let Some(v) = &self.state.verification else {
            return;
        };
        let Some(pv) = v.passive.clone() else { return };
        let (Some(from), Some(deadline)) = (Date::parse_key(&pv.from), Date::parse_key(&pv.deadline))
        else {
            return;
        };
        if d < from || !(pv.voting >= 5 || d >= deadline) {
            return;
        }
        let label = v.label.clone();
        // A state from a build with light-load evidence on may hold E2 days; with it off they
        // count for nothing.
        let e2_days = if self.light_evidence() { pv.e2_days } else { 0 };
        if pv.hot >= NOT_IMPROVED_HOT || e2_days >= NOT_IMPROVED_E2_DAYS {
            self.not_improved(
                present::not_improved_event(&label, pv.hot, e2_days),
                json!({"hot": pv.hot, "e2_days": e2_days}),
                now,
            );
        } else if pv.clean >= 3 && pv.hot == 0 {
            if let Some(v) = &mut self.state.verification {
                v.verdict = Some(PostClean::VerifiedByBuilds);
            }
            let text = present::verified_by_builds(pv.clean, &label);
            self.event(now, EventKind::Verification, text.clone(), json!({"verdict": "verified_by_builds"}));
            self.close_case(CaseOutcome::VerifiedByBuilds, now, text);
        } else if d >= deadline {
            let text = present::unverified(pv.voting, pv.hot, pv.clean, &label);
            self.event(now, EventKind::Verification, text.clone(), json!({"verdict": "unverified"}));
            self.close_case(CaseOutcome::Unverified, now, text);
        }
    }

    /// Not improved: WARNING re-latches in the same case, first_evidence kept (§9.1, §10.8).
    /// The clean did not work, so what was re-learned since is dropped and the known-clean window
    /// it opened ends: nothing measured in it may become a clean reference.
    fn not_improved(&mut self, text: String, data: Value, now: &Moment) {
        if let Some(v) = &mut self.state.verification {
            v.verdict = Some(PostClean::NotImproved);
        }
        if let Some(c) = &mut self.state.case {
            c.warning_since = Some(now.stamp());
        }
        self.state.status = Status::Warning;
        self.state.known_clean_until = None;
        self.settle_held(false);
        self.event(now, EventKind::NotImproved, text, data);
    }

    // -----------------------------------------------------------------------------------------
    // A case left open by a build with light-load evidence on.

    /// With light-load evidence off, an open case that a build with it on left behind is decided
    /// again at start, from the evidence in force. `restore` has already rebuilt the days without
    /// light load; the case is what would otherwise carry it on: a WARNING by route (b) or (c), a
    /// SUSPICION whose clock counts toward route (d), passive learning blocked while it is open.
    /// - **Light load alone** (no build evidence day left, no comparable Worse check): the case
    ///   closes as `withdrawn`, which is no earlier sighting. Nothing is reset: its days already
    ///   count without their light load.
    /// - **Builds or a check as well:** the case keeps those. Its light-load days go, and a
    ///   light-load first evidence becomes its earliest build or check. A SUSPICION, or a WARNING
    ///   by (b) or (c), is decided again from the 14-day tallies, never as suspected earlier than
    ///   it was. A WARNING by (a) or (d), and a case with a Worse check, keep their status: (a) is
    ///   the check, and (d) only fires with no E2 day in its 21 days, since (b) would come first.
    /// - **A Not improved after a clean:** `reconsider_not_improved`.
    fn reconsider_light_case(&mut self, records: &[Record], now: &Moment) {
        if self.light_evidence() || !self.dust_case_open() {
            return;
        }
        if self.state.verification.is_some() {
            self.reconsider_not_improved(now);
            return;
        }
        let Some(case) = self.state.case.clone() else {
            return;
        };
        let light_first = case.first_evidence.as_ref().is_some_and(|f| f.class == "E2");
        if case.e2_days.is_empty() && !light_first {
            return;
        }
        let known = |k: &String| Date::parse_key(k).and_then(|d| self.days.get(&d));
        // An E2 day stays when its daily line says it was a build evidence day too. One older
        // than the tallies cannot be told, and goes with its light load.
        let evidence: Vec<String> = case
            .evidence_days
            .iter()
            .filter(|k| !case.e2_days.contains(k) || known(k).is_some_and(|x| x.settled && x.e1))
            .cloned()
            .collect();
        let qualifying: Vec<String> = case
            .qualifying_days
            .iter()
            .filter(|k| match known(k) {
                Some(x) => x.voting >= 1 || x.e1,
                None => !case.e2_days.contains(k),
            })
            .cloned()
            .collect();
        let worse = case
            .checks
            .iter()
            .filter(|c| c.verdict == Some(CheckVerdict::Worse) && !c.anyway)
            .min_by_key(|c| c.at.unix());
        let first = self.first_label();
        let status = self.state.status;
        if evidence.is_empty() && worse.is_none() {
            let text = present::withdrawn_event(&first);
            self.event(
                now,
                EventKind::Withdrawn,
                text.clone(),
                json!({"status": status, "route": case.route, "e2_days": case.e2_days.len()}),
            );
            self.close_case(CaseOutcome::Withdrawn, now, text);
            return;
        }
        let redecide = worse.is_none()
            && !case.e2_days.is_empty()
            && match status {
                Status::Suspicion => true,
                Status::Warning => matches!(case.route.as_deref(), Some("b" | "c")),
                _ => false,
            };
        let new_first = if light_first {
            self.first_in_force(&evidence, worse, records)
        } else {
            None
        };
        let mut kept = case.clone();
        if redecide {
            let suspected = evidence
                .first()
                .and_then(|k| Date::parse_key(k))
                .and_then(|from| self.suspected_without_light(from, now))
                .map(|s| match &case.suspected_at {
                    Some(old) if old.unix() > s.unix() => old.clone(),
                    _ => s,
                });
            kept.route = None;
            kept.warning_since = None;
            self.state.status = if suspected.is_some() {
                Status::Suspicion
            } else {
                Status::Watch
            };
            kept.suspected_at = suspected;
        }
        let build_days = evidence.len();
        kept.evidence_days = evidence;
        kept.qualifying_days = qualifying;
        kept.e2_days.clear();
        if let Some(f) = new_first {
            kept.first_evidence = Some(f);
        }
        let changed = (self.state.status != status).then_some(self.state.status);
        if kept == case && changed.is_none() {
            return;
        }
        let text = present::light_days_dropped_event(&first, build_days, worse.is_some(), changed);
        self.state.case = Some(kept);
        self.event(
            now,
            EventKind::Withdrawn,
            text,
            json!({"status": self.state.status, "was": status, "route": case.route,
                   "e2_days": case.e2_days.len()}),
        );
    }

    /// When the 14-day tallies, which no longer count light load, first made the case a
    /// SUSPICION: the settle of the first day from `from` (its first build evidence day) to the
    /// last settled one on which `escalate` would have promoted it. None if no day did.
    fn suspected_without_light(&self, from: Date, now: &Moment) -> Option<Stamp> {
        let last = self.today?.add_days(-1);
        let mut d = from;
        while d <= last {
            if self.tally(d, 14).suspicious() {
                let next = d.add_days(1);
                return Some(Stamp::at(next.days() * 86_400 - now.offset_s as i64, now.offset_s));
            }
            d = d.add_days(1);
        }
        None
    }

    /// A first evidence that is not light load: the earliest hot voting plateau of the first
    /// build evidence day in `evidence`, or the earliest comparable Worse check if that came
    /// first. None when neither is at hand (that day's records are gone).
    fn first_in_force(
        &self,
        evidence: &[String],
        worse: Option<&CaseCheck>,
        records: &[Record],
    ) -> Option<FirstEvidence> {
        let reset = self.state.evidence_reset_at.as_ref().and_then(Stamp::unix);
        let judge = self.judge_ctx();
        let day = evidence.first().and_then(|k| Date::parse_key(k));
        let build = records
            .iter()
            .filter_map(|r| match r {
                Record::Plateau(p) => Some(p),
                _ => None,
            })
            .filter(|p| {
                p.verdict == PlateauVerdict::Hot
                    && !p.flags.iter().any(|f| f == "no_reference")
                    && day.is_some()
                    && stamp_date(&p.end) == day
                    && !before(&p.onset, reset)
            })
            .min_by_key(|p| p.onset.unix())
            .map(|p| {
                let against = judge_plateau(p, &self.view(&p.key_id), &judge).against;
                Self::plateau_candidate(p, against)
            });
        let check = worse.map(|c| Candidate {
            unix: c.at.unix().unwrap_or_default(),
            at: c.at.clone(),
            class: "check",
            summary: present::check_summary(
                c.t_ss,
                c.duty,
                self.view(&c.key_id).active.as_ref().and_then(|a| a.t.zip(a.duty)),
            ),
            record_id: None,
            key_id: c.key_id.clone(),
            top: Vec::new(),
        });
        build
            .into_iter()
            .chain(check)
            .min_by_key(|c| c.unix)
            .map(|c| c.first_evidence())
    }

    /// A Not improved after a clean (WARNING re-latched, `verification` kept) that light-load
    /// days alone decided: passive, with too few hot builds for it, and no verify check. With
    /// light-load evidence off it goes back to VERIFYING, so that the builds decide, and the
    /// clean's known-clean window stands again. The references re-learned since the clean, which
    /// the Not improved dropped, stay dropped.
    fn reconsider_not_improved(&mut self, now: &Moment) {
        let Some(v) = &self.state.verification else {
            return;
        };
        let by_light = self.state.status == Status::Warning
            && v.verdict == Some(PostClean::NotImproved)
            && v.check_dir.is_none()
            && v.passive
                .as_ref()
                .is_some_and(|p| p.hot < NOT_IMPROVED_HOT && p.e2_days >= NOT_IMPROVED_E2_DAYS);
        if !by_light {
            return;
        }
        let until = stamp_date(&v.cleaned_at).map(|d| d.add_days(references::KNOWN_CLEAN_DAYS));
        let text = present::not_improved_withdrawn_event(&v.label);
        self.event(
            now,
            EventKind::Withdrawn,
            text,
            json!({"status": Status::Verifying, "was": Status::Warning}),
        );
        if let Some(v) = &mut self.state.verification {
            v.verdict = None;
        }
        self.state.status = Status::Verifying;
        self.state.known_clean_until = until.map(|d| d.key());
    }

    // -----------------------------------------------------------------------------------------
    // Notices at settle.

    /// §8.4: no voting plateau and no qualifying light day for 21 days.
    fn starvation(&mut self, d: Date, now: &Moment) {
        let covered = self.days.keys().next().is_some_and(|first| *first <= d.add_days(-20));
        let fed = (0..21).any(|i| {
            self.days
                .get(&d.add_days(-i))
                .is_some_and(|x| x.voting > 0 || x.light_q >= QUALIFYING_LIGHT)
        });
        if covered && !fed {
            self.notice(notice::STARVATION, notice::starvation(), now);
        } else {
            self.clear_notice(notice::STARVATION);
        }
    }

    fn suspect_notice(&mut self, d: Date, now: &Moment) {
        let Some(k) = self.shown_key().cloned() else { return };
        let Some(p) = self.refs.keys.get(&k.id).and_then(|r| r.passive.clone()) else {
            return;
        };
        let since = p.suspect_since.as_deref().and_then(Date::parse_key);
        if p.state == PassiveState::Suspect
            && since.is_some_and(|s| s <= d.add_days(-references::SUSPECT_NOTICE_DAYS))
        {
            let active = self.view(&k.id).active.and_then(|a| a.t);
            self.notice(
                notice::PASSIVE_SUSPECT,
                notice::passive_suspect(p.t.unwrap_or(f64::NAN), active),
                now,
            );
        }
    }

    // -----------------------------------------------------------------------------------------
    // Alarms (§7.5, §8.5).

    fn run_alarms(&mut self, row: &Row, key: &KeyCtx, now: &Moment) {
        if row.gap != Gap::None {
            self.alarms = Alarms::default();
        }
        let end = now.mono_s();
        let start = end - row.dt.min(ALARM_READING_S);
        let t = row.t;
        if self.machine.has_fan && row.fan == Some(0) && t.is_some_and(|t| t >= FAN_STOPPED_T) {
            let since = *self.alarms.fan_zero_since.get_or_insert(start);
            if end - since >= FAN_STOPPED_S - EPS {
                self.raise_fan(FanStatus::Stopped, "alarm", present::fan_stopped_text(), now);
            }
        } else {
            self.alarms.fan_zero_since = None;
        }
        let limit = key.tcc_c + FAULT_ABOVE_TCC;
        let armed = self.machine.tjmax_c.is_none_or(|tj| limit <= tj);
        match t {
            Some(t) if armed && t >= limit => {
                let since = *self.alarms.hot_since.get_or_insert(start);
                if end - since >= FAULT_S - EPS {
                    self.raise_fault(t, key.tcc_c, now);
                }
            }
            _ => self.alarms.hot_since = None,
        }
    }

    fn raise_fault(&mut self, t: f64, tcc: f64, now: &Moment) {
        let f = &mut self.state.fault;
        if f.status == FaultStatus::Fault {
            if f.t_max.is_none_or(|m| t > m) {
                f.t_max = Some(t);
            }
            return;
        }
        f.status = FaultStatus::Fault;
        f.at = Some(now.stamp());
        f.t_max = Some(t);
        self.event(
            now,
            EventKind::Fault,
            present::fault_text(t, tcc),
            json!({"t": t, "tcc": tcc}),
        );
    }

    fn raise_fan(&mut self, status: FanStatus, class: &str, text: String, now: &Moment) {
        let current = self.state.fan.status;
        if current == status || (current == FanStatus::Stopped && status == FanStatus::Slow) {
            return;
        }
        let fan = &mut self.state.fan;
        fan.status = status;
        if status == FanStatus::Stopped {
            fan.last_fan = Some(0);
            fan.ramp = false;
        }
        if fan.first_evidence.is_none() {
            fan.first_evidence = Some(FirstEvidence {
                at: now.stamp(),
                label: now.label(),
                class: class.to_owned(),
                summary: text.clone(),
                key_id: self.key.as_ref().map(|k| k.id.clone()).unwrap_or_default(),
                ..FirstEvidence::default()
            });
        }
        self.event(now, EventKind::Fan, text, json!({ "status": status, "class": class }));
    }

    /// FAN by `fan_slow` on ≥ 2 voting plateaus on ≥ 2 days within 14 days. Builds from before
    /// the last "Fan fixed" or "Accept fan as normal" were judged against the old fan.
    fn fan_slow_rule(&mut self, p: &PlateauRecord, view: &KeyView, now: &Moment) {
        let since = now.date().add_days(-13);
        let reset = self.state.fan.reset_at.as_ref().and_then(Stamp::unix);
        let slow: Vec<&PlateauSum> = self
            .plateaus
            .iter()
            .filter(|s| {
                s.voting && s.fan_slow && s.day >= since && reset.is_none_or(|r| s.onset >= r)
            })
            .collect();
        let days: BTreeSet<Date> = slow.iter().map(|s| s.day).collect();
        if slow.len() >= 2 && days.len() >= 2 {
            let over = self.judge_ctx().r_fan_override;
            let r_fan = over.or_else(|| view.r_fan());
            // fan_low alone: the EC ramped late and the settled median may be normal, so the
            // speed at the settle point is the one that was slow.
            let below = p
                .fan_med
                .zip(r_fan)
                .is_some_and(|(f, r)| f < r - FAN_SLOW_MARGIN);
            let (fan, text, ramp) = match p.early.fan_settle {
                Some(at_settle) if !below => {
                    (Some(at_settle), present::fan_ramp_text(r_fan, at_settle), true)
                }
                _ => (p.fan_med, present::fan_slow_text(r_fan, p.fan_med), false),
            };
            let st = &mut self.state.fan;
            st.last_fan = fan.map(|f| f.round() as u32);
            // The FAN stays a ramp-only one while every build that raised it was.
            st.ramp = ramp && (st.ramp || st.status == FanStatus::Ok);
            if st.status == FanStatus::Ok && over.is_none() {
                // The speed it was compared with, kept while FAN is up (see `refresh`).
                st.r_fan = r_fan.map(|f| f.round() as u32);
            }
            self.raise_fan(FanStatus::Slow, "E1", text, now);
        }
    }

    fn watch_config(&mut self, key: &KeyCtx, now: &Moment) {
        let part = ConfigKey {
            ac: true,
            lid_open: None,
            ..key.key.clone()
        }
        .id();
        let label = key.key.label(Some(key.tcc_c));
        let t = now.mono_s();
        match &self.config.stable {
            None => self.config.stable = Some((part, label)),
            Some((s, _)) if *s == part => self.config.pending = None,
            Some(_) => {
                let p = self
                    .config
                    .pending
                    .get_or_insert_with(|| (part.clone(), label.clone(), t));
                if p.0 != part {
                    *p = (part, label, t);
                } else if t - p.2 >= CONFIG_CHANGE_S {
                    let (part, label, _) = self.config.pending.take().expect("pending");
                    let old = self.config.stable.replace((part, label.clone())).map(|s| s.1);
                    self.event(
                        now,
                        EventKind::ConfigChange,
                        present::config_change_event(old.as_deref(), &label),
                        json!({ "key_id": key.id }),
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Her actions (§9, §8.5) and checks (§8.3, §10.8). Each returns Err with a reason she can
    // read when the action does not apply now.

    fn today(&self, now: &Moment) -> Date {
        now.date()
    }

    /// "I've cleaned it" (§9.1). `date` defaults to today; a past date is stamped date-only.
    pub fn cleaned(&mut self, now: &Moment, date: Option<Date>) -> Result<(), String> {
        let today = self.today(now);
        let date = date.unwrap_or(today);
        if date > today {
            return Err(format!("{} is in the future", date.label()));
        }
        // Days that have ended settle first. In the seconds after midnight that settle can open
        // a case or close one (a lapse, passive verification), and the clean is about the case
        // as it stands after it.
        self.advance_to(today, now);
        let with_case = self.state.case.is_some();
        if !with_case {
            // With no case the date has no first evidence to bound it, and a clean drops every
            // accepted reference and restarts learning: an old date (a mistyped year) would do
            // that with a known-clean window already over, so nothing could re-learn. The setup
            // answer ignores such a date the same way.
            let until = date.add_days(references::KNOWN_CLEAN_DAYS);
            if until < today {
                return Err(format!(
                    "a clean on {} is too long ago: its {}-day known-clean window ended on {}",
                    date.label(),
                    references::KNOWN_CLEAN_DAYS,
                    until.label()
                ));
            }
            // Nor may it undo her Not dust decision from after that date.
            if let Some(dismissed) = self
                .closed
                .iter()
                .filter(|c| c.outcome == CaseOutcome::Dismissed)
                .map(|c| c.closed)
                .max()
                && date < dismissed
            {
                return Err(format!(
                    "{} is before the Not dust decision of {}, which a clean would undo",
                    date.label(),
                    dismissed.label()
                ));
            }
        }
        if let Some(first) = self
            .state
            .case
            .as_ref()
            .and_then(|c| c.first_evidence.as_ref())
            .and_then(|f| stamp_date(&f.at))
            && date < first
        {
            return Err(format!(
                "{} is before the first evidence ({})",
                date.label(),
                first.label()
            ));
        }
        let date_only = date != today;
        let midnight = |d: Date| Stamp::at(d.days() * 86_400 - now.offset_s as i64, now.offset_s);
        let at = if date_only { midnight(date) } else { now.stamp() };
        let until = date.add_days(references::KNOWN_CLEAN_DAYS);
        self.state.known_clean_until = Some(until.key());
        // A clean date answers the first-run question too (§12.7: `cleaned --date` or the
        // setup card), so the window stops asking.
        self.state.setup_needed = false;
        // Accepted references go, and the rest re-learn, at any clean.
        for k in self.refs.keys.values_mut() {
            references::on_cleaned(k);
        }
        self.effects.push(Effect::References);
        self.refs_dirty = false;
        self.clear_notice(notice::LIKE_IN_WARNING);
        let mark = CleanedMark {
            at: at.clone(),
            date_only,
            label: date.label(),
        };
        let status_before = self.state.status;
        if let Some(case) = self.state.case.clone() {
            let r_before = self.r_before(&case, &at);
            if date_only {
                // She cleaned at some hour of that day: the day itself is before the clean, and
                // today, wholly after it, keeps its tally.
                let from = date.add_days(1);
                self.state.evidence_from = Some(from.key());
                self.state.evidence_reset_at = Some(midnight(from));
            } else {
                self.reset_evidence(now);
            }
            let mut v = Verification {
                cleaned_at: at,
                date_only,
                label: date.label(),
                r_before,
                passive: None,
                verdict: None,
                check_dir: None,
            };
            let mut passive = PassiveVerification {
                from: date.add_days(7).key(),
                deadline: date.add_days(14).key(),
                ..PassiveVerification::default()
            };
            // Builds and E2 days between a past clean date and today already count.
            let from = verify_from(&v).unwrap_or(now.unix_s());
            for p in self.plateaus.iter().filter(|p| p.voting && p.onset >= from) {
                passive.voting += 1;
                match p.verdict {
                    PlateauVerdict::Hot => passive.hot += 1,
                    PlateauVerdict::Clean => passive.clean += 1,
                    PlateauVerdict::Neutral => {}
                }
            }
            let from_day = verify_from_day(&v).unwrap_or(today);
            passive.e2_days = self
                .days
                .iter()
                .filter(|(d, x)| **d >= from_day && x.e2)
                .count() as u32;
            v.passive = Some(passive);
            if let Some(c) = &mut self.state.case {
                c.cleaned.push(mark);
            }
            self.state.verification = Some(v);
            self.state.status = Status::Verifying;
            self.state.snooze = Snooze::default();
        }
        self.event(
            now,
            EventKind::Cleaned,
            present::cleaned_event(date, with_case.then_some(status_before), until),
            json!({"date": date.key(), "date_only": date_only}),
        );
        self.refresh(now);
        present::recompute(&mut self.state, now);
        Ok(())
    }

    /// The key a case is about: its first evidence's, else the current one.
    fn case_key(&self, case: &Case) -> String {
        case.first_evidence
            .as_ref()
            .map(|f| f.key_id.clone())
            .filter(|k| !k.is_empty())
            .or_else(|| self.shown_key().map(|k| k.id.clone()))
            .unwrap_or_default()
    }

    /// R_before (§9.1 step 2), from the first source available.
    fn r_before(&self, case: &Case, cleaned_at: &Stamp) -> Option<RBefore> {
        let cleaned = cleaned_at.unix().unwrap_or(i64::MAX);
        let key = self.case_key(case);
        // Only a comparable before-check under the case's key, run before the clean she reports.
        if let Some(c) = case.checks.iter().rev().find(|c| {
            c.purpose == CheckPurpose::Before
                && !c.anyway
                && (c.key_id.is_empty() || c.key_id == key)
                && c.at.unix().is_some_and(|at| at < cleaned)
                && c.verdict.is_some_and(|v| v != CheckVerdict::Inconclusive)
                && c.t_ss.is_some()
        }) {
            return Some(RBefore {
                source: "check".to_owned(),
                t_ss: c.t_ss,
                duty: c.duty,
                fan: c.fan,
                e2_per_day: None,
            });
        }
        let week = cleaned - 7 * 86_400;
        let end = stamp_date(cleaned_at).unwrap_or(Date::from_days(cleaned.div_euclid(86_400)));
        // None while light-load evidence is off: no window is counted hot then.
        let e2_per_day = self.light_evidence().then(|| {
            (0..7)
                .map(|i| self.days.get(&end.add_days(-i)).map_or(0, |x| x.light_hot))
                .sum::<u32>() as f64
                / 7.0
        });
        let from = |ps: Vec<&PlateauSum>, source: &str| -> Option<RBefore> {
            if ps.is_empty() {
                return None;
            }
            let fans: Vec<f64> = ps.iter().filter_map(|p| p.fan).collect();
            Some(RBefore {
                source: source.to_owned(),
                t_ss: median(&ps.iter().map(|p| p.t).collect::<Vec<_>>()),
                duty: median(&ps.iter().map(|p| p.duty).collect::<Vec<_>>()),
                fan: median(&fans),
                e2_per_day,
            })
        };
        // Builds under the case's key only, as for the before-check: another profile's builds,
        // or saturation votes under an unreferenced key, were judged against other numbers.
        let week_ps: Vec<&PlateauSum> = self
            .plateaus
            .iter()
            .filter(|p| p.voting && p.key_id == key && p.onset >= week && p.onset < cleaned)
            .collect();
        if let Some(r) = from(week_ps, "plateaus_7d") {
            return Some(r);
        }
        let opened = case
            .first_evidence
            .as_ref()
            .and_then(|f| f.at.unix())
            .unwrap_or(i64::MIN);
        let hot: Vec<&PlateauSum> = self
            .plateaus
            .iter()
            .filter(|p| {
                p.voting
                    && p.key_id == key
                    && p.verdict == PlateauVerdict::Hot
                    && p.onset >= opened
                    && p.onset < cleaned
            })
            .collect();
        from(hot, "case").or_else(|| {
            e2_per_day.filter(|r| *r > 0.0).map(|r| RBefore {
                source: "case".to_owned(),
                e2_per_day: Some(r),
                ..RBefore::default()
            })
        })
    }

    /// "Not dust (accept as normal)" (§9.2): in WATCH, SUSPICION and WARNING (Not improved
    /// included).
    pub fn not_dust(&mut self, now: &Moment) -> Result<(), String> {
        // As for a clean: a settle in the seconds after midnight may have closed the case.
        let today = self.today(now);
        self.advance_to(today, now);
        let case = match &self.state.case {
            Some(c) if self.dust_case_open() => c.clone(),
            _ => return Err(present::NOT_DUST_NO_CASE.to_owned()),
        };
        // The key the evidence came from, not whatever she is on when she clicks (battery, lid
        // closed, another profile): that is the reference the case was judged against.
        let key_id = self.case_key(&case);
        // The latest valid comparable check of the case under that key, else the last 7 days of
        // builds (at least 3).
        let check = case.checks.iter().rev().find(|c| {
            c.verdict.is_some_and(|v| v != CheckVerdict::Inconclusive)
                && !c.anyway
                && (c.key_id.is_empty() || c.key_id == key_id)
                && c.t_ss.is_some()
                && c.duty.is_some()
        });
        let since = now.unix_s() - 7 * 86_400;
        let week: Vec<&PlateauSum> = self
            .plateaus
            .iter()
            .filter(|p| p.voting && p.onset >= since && p.key_id == key_id)
            .collect();
        let adopted = match check {
            Some(c) => Some((
                Accepted {
                    t: c.t_ss.unwrap_or_default(),
                    duty: c.duty.unwrap_or_default(),
                    fan: c.fan,
                    f: None,
                    work: None,
                    generator: None,
                },
                format!("the check of {}", c.at.date_label()),
            )),
            None if week.len() >= 3 => {
                let fans: Vec<f64> = week.iter().filter_map(|p| p.fan).collect();
                Some((
                    Accepted {
                        t: median(&week.iter().map(|p| p.t).collect::<Vec<_>>()).unwrap_or_default(),
                        duty: median(&week.iter().map(|p| p.duty).collect::<Vec<_>>())
                            .unwrap_or_default(),
                        fan: median(&fans),
                        f: None,
                        work: None,
                        generator: None,
                    },
                    format!("the last 7 days of builds ({})", week.len()),
                ))
            }
            None => None,
        };
        let (bios, cpu) = (
            self.machine.info.bios_version.clone(),
            self.machine.info.cpu_model.clone(),
        );
        let mut adopted_text = match &adopted {
            Some((a, source)) => format!(
                "{source} ({} °C, {}) become the reference",
                present::c(a.t),
                present::pct(a.duty)
            ),
            None => "no recent check or builds to adopt, so the references stay".to_owned(),
        };
        let (n, values) = (0..7).fold((0u32, Vec::new()), |(n, mut v), i| {
            if let Some(lk) = self
                .days
                .get(&today.add_days(-i))
                .and_then(|x| x.light_keys.get(&key_id))
            {
                v.extend_from_slice(&lk.values);
                (n + lk.q, v)
            } else {
                (n, v)
            }
        });
        // The light reference is not re-learned while light-load evidence is off, as it is not
        // learned then either.
        let light = self.light_evidence();
        let k = self.refs.keys.entry(key_id.clone()).or_default();
        if let Some((a, _)) = &adopted {
            references::accept(k, a, today, &bios, &cpu);
        }
        if light && n > 0 {
            references::accept_light(k, n, &values, today);
            if n >= 30 {
                self.clear_notice(notice::LIGHT_CAP);
            }
            adopted_text += &if n >= 30 {
                format!("; light load re-learned from {n} windows")
            } else {
                "; light load re-learns from the next windows".to_owned()
            };
        }
        self.effects.push(Effect::References);
        self.refs_dirty = false;
        let text = present::not_dust_event(&adopted_text);
        self.event(now, EventKind::NotDust, text.clone(), json!({"decided_by": "user", "key_id": key_id}));
        self.close_case(CaseOutcome::Dismissed, now, text);
        self.reset_evidence(now);
        // Not dust adopts whatever the new firmware does.
        self.clear_notice(notice::BIOS_CHANGE);
        self.refresh(now);
        present::recompute(&mut self.state, now);
        Ok(())
    }

    /// "Later": hides the suspicion actions for 3 days; the badge stays.
    pub fn snooze_suspicion(&mut self, now: &Moment) -> Result<(), String> {
        if self.state.status != Status::Suspicion {
            return Err("there is no suspicion to snooze".to_owned());
        }
        let until = now.plus(SNOOZE_S);
        self.state.snooze.suspicion_actions_until = Some(until.stamp());
        self.event(now, EventKind::Snooze, present::snooze_event(&until.label()), Value::Null);
        present::recompute(&mut self.state, now);
        Ok(())
    }

    /// "Fan fixed": R_fan is re-learned from the next check.
    pub fn fan_fixed(&mut self, now: &Moment) -> Result<(), String> {
        if self.state.fan.status == FanStatus::Ok {
            return Err("the fan has no warning".to_owned());
        }
        let fan = &mut self.state.fan;
        fan.status = FanStatus::Ok;
        fan.first_evidence = None;
        fan.relearn = true;
        fan.accepted = false;
        fan.relearned = false;
        fan.ramp = false;
        fan.ramp_accepted = false;
        fan.reset_at = Some(now.stamp());
        self.event(
            now,
            EventKind::FanFixed,
            "Fan marked fixed: the next cooling check re-learns its speed".to_owned(),
            Value::Null,
        );
        self.refresh(now);
        present::recompute(&mut self.state, now);
        Ok(())
    }

    /// "Accept fan as normal": the slow speed becomes R_fan for the fan rules. A fan that was
    /// only slow to ramp up keeps its R_fan: what she accepts is the late ramp, and a fan that
    /// later slows at full speed must still be flagged against its normal speed.
    pub fn accept_fan(&mut self, now: &Moment) -> Result<(), String> {
        if self.state.fan.status == FanStatus::Ok {
            return Err("the fan has no warning".to_owned());
        }
        let fan = &mut self.state.fan;
        let stopped = fan.status == FanStatus::Stopped;
        let ramp = std::mem::take(&mut fan.ramp);
        fan.status = FanStatus::Ok;
        fan.first_evidence = None;
        fan.reset_at = Some(now.stamp());
        if ramp {
            fan.ramp_accepted = true;
        } else if !stopped && let Some(f) = fan.last_fan {
            fan.r_fan = Some(f);
            fan.accepted = true;
            fan.relearned = false;
        }
        let text = match (stopped, ramp, fan.r_fan) {
            (false, true, r) => format!(
                "Fan accepted as normal: slow to speed up, normal speed {} rpm (your decision)",
                r.map_or("?".to_owned(), |r| r.to_string())
            ),
            (false, false, Some(f)) => {
                format!("Fan accepted as normal at {f} rpm (your decision)")
            }
            _ => "Fan warning accepted (your decision)".to_owned(),
        };
        self.event(now, EventKind::FanAccepted, text, json!({"decided_by": "user"}));
        present::recompute(&mut self.state, now);
        Ok(())
    }

    pub fn ack_fault(&mut self, now: &Moment) -> Result<(), String> {
        if self.state.fault.status == FaultStatus::Ok {
            return Err("there is no cooling fault".to_owned());
        }
        let t = self.state.fault.t_max;
        self.state.fault.status = FaultStatus::Ok;
        self.event(
            now,
            EventKind::FaultAck,
            "Cooling fault acknowledged".to_owned(),
            json!({ "t_max": t }),
        );
        present::recompute(&mut self.state, now);
        Ok(())
    }

    /// The first-run question (§13.2): when was it last cleaned? None is "Not sure".
    pub fn setup_answer(&mut self, now: &Moment, last_cleaned: Option<Date>) -> Result<(), String> {
        let today = self.today(now);
        let text = match last_cleaned {
            Some(d) if d > today => return Err(format!("{} is in the future", d.label())),
            Some(d) => {
                let until = d.add_days(references::KNOWN_CLEAN_DAYS);
                if until >= today {
                    self.state.known_clean_until = Some(until.key());
                }
                format!("Last cleaned {}: known clean until {}", d.label(), until.label())
            }
            None => "Last cleaned: not sure. References are learned but marked unverified".to_owned(),
        };
        self.state.setup_needed = false;
        self.event(now, EventKind::Setup, text, json!({"last_cleaned": last_cleaned.map(|d| d.key())}));
        present::recompute(&mut self.state, now);
        Ok(())
    }

    /// A history line from outside the rules: a check started, or a check refused or aborted,
    /// which reaches no verdict and so never reaches `check_result`.
    pub fn log_event(&mut self, now: &Moment, kind: EventKind, text: String, data: Value) {
        self.event(now, kind, text, data);
    }

    /// The check stopped because the fan sat at 0 rpm under load (§10.5): that raises FAN too.
    pub fn fan_stopped_in_check(&mut self, now: &Moment) {
        self.raise_fan(FanStatus::Stopped, "check", present::fan_stopped_text(), now);
        present::recompute(&mut self.state, now);
    }

    /// A finished cooling check (§8.3, §10.8).
    pub fn check_result(&mut self, now: &Moment, c: CheckOutcome) {
        let today = self.today(now);
        self.advance_to(today, now);
        // The steady window is what a stored reference keeps; `shown` is the check as it was
        // compared with R_active, for last_check and the history.
        let m = &c.metrics;
        let valid = c.verdict != CheckVerdict::Inconclusive;
        let view = self.view(&c.key_id);
        let shown = c.against(view.active.as_ref());
        let check_ref = CheckRef {
            t: m.t_ss,
            duty: m.duty,
            fan: m.fan,
            f: m.f,
            work: m.work,
            djc: m.djc,
            ths: m.ths,
            floor: m.floor,
            t_cool: m.t_cool,
            t_idle: m.t_idle,
            source: "check".to_owned(),
            bios: self.machine.info.bios_version.clone(),
            cpu: c.cpu.clone(),
            generator: c.generator,
            ..CheckRef::default()
        };
        let base = if view.seed {
            Some(references::seed_ref(&self.machine.info.board_name))
        } else {
            view.best.clone()
        };
        let day = c.at.date();
        let in_window = self.in_window(day);
        let t_matched = c.matched.as_ref().and_then(|x| x.t_ss);
        // A first check with nothing to compare against is judged Borderline so that it counts
        // as valid. Whether it then records the first reference is worked out on a copy now, so
        // the history line says "Recorded" only when it did.
        let recorded = valid
            && c.comparable
            && c.purpose == CheckPurpose::Reference
            && matches!(self.state.status, Status::Clean | Status::Learning)
            && c.verdict == CheckVerdict::Borderline
            && view.active.is_none()
            && {
                let mut k = self.refs.keys.get(&c.key_id).cloned().unwrap_or_default();
                let a = references::adopt_reference_check(
                    &mut k,
                    check_ref.clone(),
                    t_matched,
                    day,
                    base.as_ref(),
                    in_window,
                );
                matches!(a, Adopted::Yes(_))
            };
        let label = match (c.post_clean, c.verdict) {
            (Some(PostClean::Restored), _) => "Restored",
            (Some(PostClean::Improved), _) => "Improved",
            (Some(PostClean::NotImproved), _) => "Not improved",
            (_, CheckVerdict::Borderline) if recorded => "Recorded",
            (_, CheckVerdict::Like) => "Like the reference",
            (_, CheckVerdict::Borderline) => "Borderline",
            (_, CheckVerdict::Worse) => "Worse",
            (_, CheckVerdict::Inconclusive) => "Inconclusive",
        };
        self.state.last_check = Some(LastCheck {
            at: c.at.stamp(),
            dir: c.dir.clone(),
            purpose: c.purpose,
            verdict: c.verdict,
            post_clean: c.post_clean,
            t_ss: shown.t_ss,
            duty: shown.duty,
            fan: shown.fan,
            label: label.to_owned(),
        });
        if let Some(case) = &mut self.state.case {
            case.checks.push(Self::case_check(&c));
        }
        let against = view.active.as_ref().and_then(|a| a.t.zip(a.duty));
        let mut text = format!(
            "Cooling check: {label} · {}",
            present::check_summary(shown.t_ss, shown.duty, against)
        );
        for line in &c.diagnosis {
            text += &format!(" · {line}");
        }
        self.event(
            now,
            EventKind::CheckResult,
            text,
            json!({"dir": c.dir, "purpose": c.purpose, "verdict": c.verdict,
                   "post_clean": c.post_clean, "diagnosis": c.diagnosis}),
        );
        // After "Fan fixed" this check measures the new fan: its speed becomes R_fan, so it
        // cannot also be slow against the old one. Until a valid check has done that, the
        // flag compares with the old fan and is not raised.
        let relearn_fan = valid && self.state.fan.relearn && m.fan.is_some();
        if relearn_fan && let Some(f) = m.fan {
            let k = self.refs.keys.entry(c.key_id.clone()).or_default();
            if let Some(a) = &mut k.active {
                a.fan = Some(f);
            } else if let Some(mut s) = view.active.clone() {
                s.fan = Some(f);
                s.label = format!("{} (fan {})", s.label, c.at.date().label());
                k.active = Some(s);
            }
            let fan = &mut self.state.fan;
            fan.relearn = false;
            fan.relearned = true;
            fan.accepted = false;
            fan.r_fan = Some(f.round() as u32);
            self.effects.push(Effect::References);
            self.event(
                now,
                EventKind::Reference,
                format!("Fan speed re-learned from the check: {f:.0} rpm"),
                json!({"kind": "fan", "r_fan": f}),
            );
        } else if c.fan_flag && !self.state.fan.relearn {
            // The flag compared with R_active's fan (§10.8), or the one she accepted.
            let over = self.judge_ctx().r_fan_override;
            let r = over.or_else(|| view.active.as_ref().and_then(|a| a.fan));
            let st = &mut self.state.fan;
            st.last_fan = shown.fan.map(|f| f.round() as u32);
            // The check measured a slow steady speed, not a late ramp.
            st.ramp = false;
            if st.status == FanStatus::Ok && over.is_none() {
                st.r_fan = r.map(|f| f.round() as u32);
            }
            self.raise_fan(FanStatus::Slow, "check", present::fan_check_text(r, shown.fan), now);
        }
        if !valid || (!c.comparable && c.purpose != CheckPurpose::Verify) {
            self.refresh(now);
            present::recompute(&mut self.state, now);
            return;
        }
        if c.comparable {
            // The new firmware has now been measured.
            self.clear_notice(notice::BIOS_CHANGE);
        }
        match (c.purpose, self.state.status) {
            (CheckPurpose::Confirm, Status::Watch | Status::Suspicion) => match c.verdict {
                CheckVerdict::Like => {
                    let first = self.first_label();
                    let text = present::cleared_by_check_event(&first);
                    self.event(now, EventKind::ClearedByCheck, text.clone(), json!({"dir": c.dir}));
                    self.close_case(CaseOutcome::ClearedByCheck, now, text);
                    self.reset_evidence(now);
                }
                CheckVerdict::Worse => self.route_a(&c, now),
                _ => {}
            },
            (CheckPurpose::Before, Status::Warning) => {
                if c.verdict == CheckVerdict::Like {
                    self.notice(notice::LIKE_IN_WARNING, notice::like_in_warning(), now);
                }
            }
            (CheckPurpose::Verify, Status::Verifying) => {
                if let Some(v) = &mut self.state.verification {
                    v.check_dir = Some(c.dir.clone());
                }
                match c.post_clean {
                    Some(PostClean::Restored) => {
                        let k = self.refs.keys.entry(c.key_id.clone()).or_default();
                        if references::adopt_restored(k, check_ref, t_matched, day, base.as_ref()) {
                            self.clear_notice(notice::REFERENCE_HOT);
                        }
                        self.effects.push(Effect::References);
                        if let Some(v) = &mut self.state.verification {
                            v.verdict = Some(PostClean::Restored);
                        }
                        let text = format!("Verified clean {}", day.label());
                        self.event(now, EventKind::Verification, text.clone(), json!({"verdict": "restored", "dir": c.dir}));
                        self.close_case(CaseOutcome::VerifiedClean, now, text);
                        // The check outweighs builds judged earlier today, as a check that
                        // clears a case does.
                        self.reset_evidence(now);
                    }
                    Some(PostClean::Improved) => {
                        let excess = c
                            .against(view.best.as_ref())
                            .t_ss
                            .zip(view.best.as_ref().and_then(|b| b.t))
                            .map(|(t, b)| t - b);
                        let best_day = view.best.as_ref().map(ref_day).unwrap_or_default();
                        let text = format!(
                            "Improved, {} °C above the clean of {best_day}: dust left in the fins, or paste or heatsink seating",
                            excess.map_or("?".to_owned(), present::c)
                        );
                        self.event(now, EventKind::Verification, text.clone(), json!({"verdict": "improved", "dir": c.dir}));
                        self.close_case(CaseOutcome::Improved, now, text);
                        self.reset_evidence(now);
                    }
                    Some(PostClean::NotImproved) => {
                        let label = self
                            .state
                            .verification
                            .as_ref()
                            .map(|v| v.label.clone())
                            .unwrap_or_default();
                        let against = view.best.as_ref().and_then(|b| b.t.zip(b.duty));
                        let p = c.against(view.best.as_ref());
                        self.not_improved(
                            present::not_improved_check_event(
                                &label,
                                &present::check_summary(p.t_ss, p.duty, against),
                            ),
                            json!({"dir": c.dir, "t_ss": p.t_ss, "duty": p.duty}),
                            now,
                        );
                    }
                    _ => {}
                }
            }
            (CheckPurpose::Reference, Status::Clean | Status::Learning) => match c.verdict {
                CheckVerdict::Worse => {
                    let summary = present::check_summary(shown.t_ss, shown.duty, against);
                    self.open_case_by_check(&c, summary, now);
                }
                _ => {
                    let k = self.refs.keys.entry(c.key_id.clone()).or_default();
                    match references::adopt_reference_check(
                        k,
                        check_ref,
                        t_matched,
                        day,
                        base.as_ref(),
                        in_window,
                    ) {
                        Adopted::Yes(label) => {
                            self.effects.push(Effect::References);
                            self.clear_notice(notice::REFERENCE_HOT);
                            self.event(
                                now,
                                EventKind::Reference,
                                format!("Reference {label}: {}", present::check_summary(m.t_ss, m.duty, None)),
                                json!({"key_id": c.key_id, "kind": "active", "dir": c.dir}),
                            );
                        }
                        Adopted::TooHot => {
                            // Named after the base it was bounded by: the seed's clean on a Duo,
                            // a generic machine's own first reference elsewhere.
                            let since = base.as_ref().map(ref_day).unwrap_or_default();
                            self.notice(notice::REFERENCE_HOT, notice::reference_hot(&since), now)
                        }
                        Adopted::No => {}
                    }
                }
            },
            _ => {}
        }
        self.refresh(now);
        present::recompute(&mut self.state, now);
    }

    fn case_check(c: &CheckOutcome) -> CaseCheck {
        CaseCheck {
            at: c.at.stamp(),
            dir: c.dir.clone(),
            purpose: c.purpose,
            verdict: Some(c.verdict),
            t_ss: c.metrics.t_ss,
            duty: c.metrics.duty,
            fan: c.metrics.fan,
            key_id: c.key_id.clone(),
            anyway: !c.comparable,
        }
    }

    /// Route (a): a check came out Worse in WATCH or SUSPICION, unless the case was itself opened
    /// by a check on the same local day.
    fn route_a(&mut self, c: &CheckOutcome, now: &Moment) {
        let opened_by_check_today = self.state.case.as_ref().is_some_and(|case| {
            case.first_evidence.as_ref().is_some_and(|f| {
                f.class == "check" && stamp_date(&f.at) == Some(c.at.date())
            })
        });
        if !opened_by_check_today {
            self.warn("a", present::warning_event("a", 0, 0, 0), now);
        }
    }

    /// A reference check from CLEAN or LEARNING came out Worse: SUSPICION, dated at the check.
    fn open_case_by_check(&mut self, c: &CheckOutcome, summary: String, now: &Moment) {
        let at = c.at.stamp();
        let earlier = self
            .closed
            .iter()
            .rev()
            .find(|x| {
                matches!(x.outcome, CaseOutcome::Lapsed | CaseOutcome::ClearedByCheck)
                    && x.closed >= now.date().add_days(-60)
            })
            .map(|x| Sighting {
                case_id: x.id.clone(),
                day: x.closed.key(),
                label: x.first_day.clone().unwrap_or_else(|| x.closed.label()),
            });
        let case = Case {
            id: case_id(&at),
            opened: now.stamp(),
            first_evidence: Some(FirstEvidence {
                label: at.label(),
                at: at.clone(),
                class: "check".to_owned(),
                summary: summary.clone(),
                record_id: None,
                key_id: c.key_id.clone(),
                top: Vec::new(),
            }),
            earlier_sighting: earlier.clone(),
            suspected_at: Some(now.stamp()),
            checks: vec![Self::case_check(c)],
            ..Case::default()
        };
        self.state.case = Some(case);
        self.state.status = Status::Suspicion;
        self.event(
            now,
            EventKind::FirstEvidence,
            present::first_evidence_event(&summary, earlier.as_ref().map(|e| e.label.as_str())),
            json!({"class": "check", "dir": c.dir}),
        );
        self.event(now, EventKind::Suspicion, present::suspicion_by_check(), json!({"dir": c.dir}));
    }

    // -----------------------------------------------------------------------------------------
    // The derived parts of state.json.

    /// Recomputes the counts, the 30-day unjudged tally, drift, learning progress and the key
    /// block. Called at episode closes, hot and cold windows, day settles, actions and hourly,
    /// never per tick, so state.json changes only then.
    fn refresh(&mut self, now: &Moment) {
        let today = self.day_of(now);
        let mut c = Counts::default();
        for i in 0..21 {
            let d = today.add_days(-i);
            let Some(x) = self.days.get(&d) else { continue };
            let (ev, _, q) = self.day_flags(d);
            c.days += x.observed as u32;
            c.qualifying_days += q as u32;
            c.evidence_days += ev as u32;
            c.voting_plateaus += x.voting;
            c.hot_plateaus += x.hot;
            c.light_windows += x.light_keys.values().map(|l| l.q).sum::<u32>();
            c.hot_windows += x.light_hot;
        }
        self.state.counts = c;

        let mut u = UnjudgedCounts::default();
        for i in 0..30 {
            let Some(x) = self.days.get(&today.add_days(-i)) else { continue };
            for (reason, n) in &x.unjudged {
                let slot = match reason.as_str() {
                    "too_short" => &mut u.too_short,
                    "unsettled" => &mut u.unsettled,
                    "dips" => &mut u.dips,
                    "below_pl1" => &mut u.below_pl1,
                    "power_delivery" => &mut u.power_delivery,
                    "slope" => &mut u.slope,
                    "no_reference" => &mut u.no_reference,
                    _ => &mut u.void,
                };
                *slot += n;
            }
        }
        self.state.unjudged_30d = u;

        let Some(key) = self.shown_key().cloned() else { return };
        // Drift: Theil-Sen of the daily plateau medians over 30 days, per week (§5.3).
        let since = today.add_days(-29);
        let mut by_day: BTreeMap<Date, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
        for p in self
            .plateaus
            .iter()
            .filter(|p| p.voting && p.day >= since && p.key_id == key.id)
        {
            let e = by_day.entry(p.day).or_default();
            e.0.push(p.t);
            e.1.push(p.duty);
        }
        let (mut t_points, mut duty_points) = (Vec::new(), Vec::new());
        for (d, (ts, duties)) in &by_day {
            if let (Some(t), Some(duty)) = (median(ts), median(duties)) {
                t_points.push((d.days() as f64, t));
                duty_points.push((d.days() as f64, duty));
            }
        }
        self.state.drift = (t_points.len() >= 3)
            .then(|| {
                Some(Drift {
                    days: 30,
                    t_c_per_week: theil_sen(&t_points)? * 7.0,
                    duty_pp_per_week: theil_sen(&duty_points)? * 7.0 * 100.0,
                })
            })
            .flatten();

        let view = self.view(&key.id);
        let k = self.refs.keys.get(&key.id);
        let passive = k.and_then(|k| k.passive.as_ref());
        let light = k.and_then(|k| k.light.as_ref());
        self.state.learning = Learning {
            voting: passive.map_or(0, |p| p.n),
            needed: references::PASSIVE_N as u32,
            days: passive.map_or(0, |p| p.days),
            needed_days: references::PASSIVE_DAYS as u32,
            light_windows: light.map_or(0, |l| l.n),
            light_needed: references::LIGHT_N,
        };
        self.state.key = KeyInfo {
            id: key.id.clone(),
            referenced: view.referenced(),
            label: key.key.label(Some(key.tcc_c)),
            reference: view.label(),
            on_battery: self.key.as_ref().is_some_and(|k| !k.key.ac),
        };
        self.state.chassis.tcc_c = Some(key.tcc_c);
        // While FAN is up, r_fan stays the speed it was raised against: the badge reads
        // "FAN SLOW 6100/6800 RPM" whatever key she switches to.
        let fan = &mut self.state.fan;
        if !fan.accepted && !fan.relearned && fan.status == FanStatus::Ok {
            fan.r_fan = view.r_fan().map(|f| f.round() as u32);
        }
        if self.state.case.is_none() {
            self.state.status = self.idle_status();
        }
    }
}

/// Detector and engine together, as the service runs them on every row.
pub struct Watch {
    pub detector: Detector,
    pub engine: Engine,
}

impl Watch {
    pub fn new(engine: Engine) -> Watch {
        Watch {
            detector: Detector::default(),
            engine,
        }
    }

    /// One row: detector, then rules, then the per-row work. `top` supplies the top consumers
    /// for a closing plateau or window (the service snapshots /proc; tests pass nothing). The
    /// detector's outputs are returned so the service can react to an opening plateau.
    pub fn feed(
        &mut self,
        row: &Row,
        key: &KeyCtx,
        mut top: impl FnMut(&Output) -> Vec<(String, f64)>,
    ) -> Vec<Output> {
        self.detector.tuning = self.engine.tuning(key);
        let outs = self.detector.feed(row, &key.id);
        for o in &outs {
            let t = top(o);
            self.engine.handle(o.clone(), t, &row.at);
        }
        self.engine.tick(row, key);
        outs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::references::seed_ref;

    const SEED_KEY: &str = "ac|performance|performance|28|35|8|0|100|open";

    fn plateau(t: f64, duty: f64, fan: f64) -> PlateauRecord {
        PlateauRecord {
            key_id: SEED_KEY.into(),
            t_ss: Some(t),
            duty: Some(duty),
            fan_med: Some(fan),
            ..PlateauRecord::default()
        }
    }

    fn duo_ctx() -> JudgeCtx {
        JudgeCtx {
            has_fan: true,
            has_throttle: true,
            fan_auto_max: Some(7000.0),
            r_fan_override: None,
            ramp_accepted: false,
        }
    }

    #[test]
    fn the_seed_thresholds() {
        let d = duty_lines(DUO_SEED.duty);
        assert_eq!((d.hot, d.only, d.clean, d.like), (0.03, 0.08, 0.02, 0.025));
        // An accepted high R_duty: the additive caps keep the duty arm alive.
        let d = duty_lines(0.06);
        assert!((d.hot - 0.09).abs() < 1e-12 && (d.only - 0.11).abs() < 1e-12);
    }

    #[test]
    fn plateau_verdicts_with_the_duo_numbers() {
        let refs = References::default();
        let seed = KeyView::of(&refs, SEED_KEY, Chassis::Duo, "UX482EG");
        let j = duo_ctx();
        let v = |t, duty| judge_plateau(&plateau(t, duty, 6800.0), &seed, &j).verdict;
        // Seed phase: ≥ 90 °C with ≥ 3 %, or ≥ 10 %; clean ≤ 88.5 °C and ≤ 4 %.
        assert_eq!(v(90.0, 0.03), PlateauVerdict::Hot);
        assert_eq!(v(89.5, 0.05), PlateauVerdict::Neutral);
        assert_eq!(v(86.0, 0.10), PlateauVerdict::Hot);
        assert_eq!(v(88.5, 0.04), PlateauVerdict::Clean);
        assert_eq!(v(86.0, 0.0093), PlateauVerdict::Clean);

        // Learned R_passive equal to the seed: ≥ 89 °C with ≥ 3 %, or ≥ 8 %; clean ≤ 87.5 / 2 %.
        let mut learned = seed.clone();
        learned.passive = Some(references::Passive {
            t: 86.0,
            duty: 0.0093,
            fan: Some(6800.0),
        });
        let v = |t, duty| judge_plateau(&plateau(t, duty, 6800.0), &learned, &j);
        assert_eq!(v(89.0, 0.03).verdict, PlateauVerdict::Hot);
        assert_eq!(v(89.0, 0.03).basis, Basis::Passive);
        assert_eq!(v(86.0, 0.08).verdict, PlateauVerdict::Hot);
        assert_eq!(v(87.5, 0.02).verdict, PlateauVerdict::Clean);
        assert_eq!(v(88.0, 0.02).verdict, PlateauVerdict::Neutral);

        // fan_slow: under R_fan − 400 on a plateau that is not clean.
        let p = judge_plateau(&plateau(89.0, 0.03, 6300.0), &seed, &j);
        assert!(p.fan_slow);
        let p = judge_plateau(&plateau(86.0, 0.005, 6300.0), &seed, &j);
        assert!(!p.fan_slow, "a clean build never flags the fan");

        // No reference: not counted, unless saturated at full fan.
        let other = KeyView::default();
        let p = judge_plateau(&plateau(92.0, 0.2, 6900.0), &other, &j);
        assert_eq!((p.voting, p.verdict), (false, PlateauVerdict::Neutral));
        let p = judge_plateau(&plateau(92.0, 0.35, 6900.0), &other, &j);
        assert_eq!((p.voting, p.verdict, p.basis), (true, PlateauVerdict::Hot, Basis::Saturation));
    }

    #[test]
    fn light_verdicts() {
        let mut view = KeyView::default();
        let j = duo_ctx();
        let w = |hot_ms, cold_ms| LightRecord {
            seconds: 60.0,
            hot_ms,
            cold_ms,
            fan_p90: Some(4600.0),
            ..LightRecord::default()
        };
        // Learning: no vote, and a hot window is still neutral.
        assert_eq!(judge_light(&w(24_000, 0), &view, &j), (LightVerdict::Neutral, false));
        view.light_p90 = Some(0.002);
        assert_eq!(judge_light(&w(24_000, 0), &view, &j), (LightVerdict::Hot, true));
        assert_eq!(judge_light(&w(2_400, 0), &view, &j), (LightVerdict::Neutral, true));
        // L = max(5 %, 10 × p90).
        view.light_p90 = Some(0.01);
        assert_eq!(judge_light(&w(3_600, 0), &view, &j).0, LightVerdict::Neutral);
        assert_eq!(judge_light(&w(6_000, 0), &view, &j).0, LightVerdict::Hot);
        assert_eq!(judge_light(&w(0, 900), &view, &j).0, LightVerdict::Cold);
        assert_eq!(judge_light(&w(0, 300), &view, &j).0, LightVerdict::Neutral);
    }

    #[test]
    fn check_verdicts_against_the_seed() {
        let seed = seed_ref("UX482EG");
        let m = |t: f64, duty: f64, fan: f64| CheckMetrics {
            t_ss: Some(t),
            duty: Some(duty),
            fan: Some(fan),
            ..CheckMetrics::default()
        };
        // Worse: ≥ 89 °C with ≥ 3 %, or ≥ 8 %. Like: ≤ 88 °C, ≤ 2.5 %, fan ≥ 6400 at T ≥ 85.
        assert_eq!(judge_check(&m(89.0, 0.03, 6800.0), &seed, false), CheckVerdict::Worse);
        assert_eq!(judge_check(&m(86.0, 0.08, 6800.0), &seed, false), CheckVerdict::Worse);
        assert_eq!(judge_check(&m(88.0, 0.025, 6400.0), &seed, false), CheckVerdict::Like);
        assert_eq!(judge_check(&m(87.0, 0.01, 6300.0), &seed, false), CheckVerdict::Borderline);
        assert_eq!(judge_check(&m(84.5, 0.01, 6100.0), &seed, false), CheckVerdict::Like);
        assert_eq!(judge_check(&m(88.5, 0.01, 6800.0), &seed, false), CheckVerdict::Borderline);
        assert_eq!(
            judge_check(&CheckMetrics::default(), &seed, false),
            CheckVerdict::Inconclusive
        );

        let before = RBefore {
            t_ss: Some(91.0),
            duty: Some(0.09),
            ..RBefore::default()
        };
        let post = |p: CheckMetrics, b: Option<&RBefore>| judge_post_clean(&p, &p, &seed, b, false);
        assert_eq!(post(m(86.5, 0.012, 6800.0), Some(&before)), PostClean::Restored);
        assert_eq!(post(m(89.0, 0.035, 6800.0), Some(&before)), PostClean::Improved);
        assert_eq!(post(m(90.5, 0.08, 6800.0), Some(&before)), PostClean::NotImproved);
        assert_eq!(post(m(90.5, 0.05, 6800.0), None), PostClean::Improved);
        // B is compared with P's steady window: a cooler matched window does not make it Improved.
        assert_eq!(
            judge_post_clean(&m(88.5, 0.08, 6800.0), &m(90.5, 0.08, 6800.0), &seed, Some(&before), false),
            PostClean::NotImproved
        );
    }

    #[test]
    fn work_and_frequency_decide_together() {
        let own = CheckRef {
            t: Some(84.0),
            duty: Some(0.008),
            fan: Some(6900.0),
            f: Some(3700.0),
            work: Some(300.0),
            ..CheckRef::default()
        };
        let m = |work: Option<f64>, f: Option<f64>| CheckMetrics {
            t_ss: Some(85.5),
            duty: Some(0.01),
            fan: Some(6900.0),
            f,
            work,
            ..CheckMetrics::default()
        };
        // Either one at the reference's level is enough.
        assert_eq!(judge_check(&m(Some(295.0), Some(3550.0)), &own, true), CheckVerdict::Like);
        assert_eq!(judge_check(&m(Some(280.0), Some(3650.0)), &own, true), CheckVerdict::Like);
        assert_eq!(judge_check(&m(Some(280.0), Some(3550.0)), &own, true), CheckVerdict::Borderline);
        // Work dropped for foreign load: the frequency decides, and 130 MHz down is not like.
        assert_eq!(judge_check(&m(None, Some(3570.0)), &own, true), CheckVerdict::Borderline);
        assert_eq!(judge_check(&m(None, Some(3650.0)), &own, true), CheckVerdict::Like);
        // Neither one known: the clause stands aside.
        assert_eq!(judge_check(&m(None, None), &own, true), CheckVerdict::Like);
    }
}
