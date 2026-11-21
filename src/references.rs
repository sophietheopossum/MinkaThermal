//! References (§6): the Duo seed, and per config key R_active, R_best, R_passive and the light
//! reference, with the rules for learning and replacing them.
//!
//! The seed is compiled in and never stored, so no file edit or migration can drop it: a key the
//! seed applies to falls back to it whenever references.json holds nothing better.

use crate::clock::Date;
use crate::episodes::{median, percentile};
use crate::model::{CheckRef, KeyRefs, LightRef, PassiveRef, PassiveSample, PassiveState, References};
use crate::profile::{Chassis, ConfigKey, DUO_SEED};
use std::collections::BTreeSet;

/// R_passive needs this many voting episodes on this many days (§6.2).
pub const PASSIVE_N: usize = 8;
pub const PASSIVE_DAYS: usize = 3;
/// R_duty is learned as min(median duty, 3 %): builds with more steady throttle than that are
/// not what a clean machine does at 28 W.
pub const PASSIVE_DUTY_CAP: f64 = 0.03;
/// R_passive is suspect when its T is this far above R_active's.
pub const SUSPECT_ABOVE_ACTIVE: f64 = 3.0;
/// After 30 days of being suspect, a notice asks for a check.
pub const SUSPECT_NOTICE_DAYS: i64 = 30;
/// Re-learning after a clean may not exceed the previous verified values by more than this.
pub const RELEARN_CAP_T: f64 = 1.5;
pub const RELEARN_CAP_DUTY: f64 = 0.01;
/// The light reference: p90 of duty_hot over this many qualifying windows, floored and capped.
pub const LIGHT_N: u32 = 300;
pub const LIGHT_FLOOR: f64 = 0.002;
pub const LIGHT_CAP: f64 = 0.02;
/// A known-clean window lasts this long after a clean.
pub const KNOWN_CLEAN_DAYS: i64 = 30;
/// An own check may replace the seed only within these bounds (§6.2a, §6.2b).
pub const ACTIVE_BOUND_T: f64 = 3.0;
pub const ACTIVE_BOUND_DUTY: f64 = 0.03;
/// Samples kept while learning; a suspect R_passive keeps learning on the latest ones.
const SAMPLES_MAX: usize = 64;

/// The firmware and CPU the seed was measured with.
pub const SEED_BIOS: &str = "UX482EG.311";
pub const SEED_CPU: &str = "11th Gen Intel(R) Core(TM) i7-1165G7";

/// Whether the compiled-in Duo seed is this key's reference (§4). A Duo machine under any other
/// key learns as generic mode does.
pub fn seed_applies(chassis: Chassis, key_id: &str) -> bool {
    chassis == Chassis::Duo && ConfigKey::parse_id(key_id).is_some_and(|k| k.is_seed_key())
}

/// The seed as a check-shaped reference. A UX482EA has the same chassis and uses it, noted as
/// measured on the UX482EG.
pub fn seed_ref(board: &str) -> CheckRef {
    let s = DUO_SEED;
    let mut label = format!("duo-seed {}", s.date_label);
    if !board.is_empty() && board != s.board {
        label += &format!(" (measured on {})", s.board);
    }
    CheckRef {
        t: Some(s.t_ss),
        duty: Some(s.duty),
        fan: Some(s.fan as f64),
        f: Some(s.f as f64),
        work: None,
        djc: Some(s.d_jc),
        ths: Some(s.t_hs),
        floor: Some(s.floor),
        t_cool: Some(s.t_cool_s),
        t_idle: Some(s.t_idle),
        source: "seed".to_owned(),
        label,
        bios: SEED_BIOS.to_owned(),
        cpu: SEED_CPU.to_owned(),
        // The C burn, not generator v1: work and frequency do not compare.
        generator: None,
        provisional: ["djc", "ths", "floor", "t_cool"].map(str::to_owned).to_vec(),
        accepted: false,
    }
}

/// Learned R_passive values in use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Passive {
    pub t: f64,
    pub duty: f64,
    pub fan: Option<f64>,
}

/// The references in force for one key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyView {
    pub active: Option<CheckRef>,
    pub best: Option<CheckRef>,
    pub passive: Option<Passive>,
    pub passive_label: Option<String>,
    pub light_p90: Option<f64>,
    pub light_label: Option<String>,
    pub seed: bool,
}

impl KeyView {
    pub fn of(refs: &References, key_id: &str, chassis: Chassis, board: &str) -> KeyView {
        let seed = seed_applies(chassis, key_id).then(|| seed_ref(board));
        let k = refs.keys.get(key_id);
        let passive = k.and_then(|k| k.passive.as_ref()).and_then(|p| {
            (p.state == PassiveState::Learned)
                .then_some(())
                .and(p.t.zip(p.duty))
                .map(|(t, duty)| {
                    (
                        Passive {
                            t,
                            duty,
                            fan: p.fan,
                        },
                        p.label.clone(),
                    )
                })
        });
        KeyView {
            active: k.and_then(|k| k.active.clone()).or_else(|| seed.clone()),
            best: k.and_then(|k| k.best.clone()).or_else(|| seed.clone()),
            passive_label: passive.as_ref().map(|(_, l)| l.clone()),
            passive: passive.map(|(p, _)| p),
            light_p90: k.and_then(|k| k.light.as_ref()).and_then(|l| l.p90),
            light_label: k
                .and_then(|k| k.light.as_ref())
                .filter(|l| l.p90.is_some())
                .map(|l| l.label.clone()),
            seed: seed.is_some(),
        }
    }

    /// Evidence counts only under a key with a reference (§4). The light reference is one of
    /// them (§6.2): its windows vote as soon as it is learned, so a key that has only that one
    /// is referenced too, and is shown so.
    pub fn referenced(&self) -> bool {
        self.active.is_some() || self.passive.is_some() || self.light_p90.is_some()
    }

    /// R_fan for the fan rules: the learned one from her builds, else R_active's.
    pub fn r_fan(&self) -> Option<f64> {
        self.passive
            .and_then(|p| p.fan)
            .or_else(|| self.active.as_ref().and_then(|a| a.fan))
    }

    /// The provenance shown beside the key: R_active's, else the learned R_passive's, else
    /// the light reference's.
    pub fn label(&self) -> Option<String> {
        self.active
            .as_ref()
            .map(|a| a.label.clone())
            .or_else(|| self.passive_label.clone())
            .or_else(|| {
                self.light_label.as_ref().map(|l| {
                    if l.is_empty() {
                        "light load".to_owned()
                    } else {
                        format!("light load {l}")
                    }
                })
            })
    }
}

/// Something learning produced, for the event log and notices.
#[derive(Clone, Debug, PartialEq)]
pub enum Learned {
    /// R_passive learned (or re-learned): its values and label.
    Passive(Passive, String),
    /// Learned, but more than 3 °C above R_active: not used, learning continues.
    Suspect(Passive),
    /// The light reference learned; `capped` when the p90 was above 2 %.
    Light { p90: f64, raw: f64, capped: bool },
}

fn span_label(samples: &[PassiveSample]) -> String {
    let days: Vec<Date> = samples
        .iter()
        .filter_map(|s| Date::parse_key(&s.day))
        .collect();
    match (days.iter().min(), days.iter().max()) {
        (Some(a), Some(b)) if a != b => format!("learned {}-{}", a.label(), b.label()),
        (Some(a), _) => format!("learned {}", a.label()),
        _ => "learned".to_owned(),
    }
}

/// Feeds one voting episode into R_passive learning (§6.2). Learning runs while the key has no
/// learned R_passive, while it is suspect, and while it re-learns after a clean. With `hold` (a
/// clean being verified) a finished result waits in `held` and the old values stay in use.
pub fn learn_passive(
    k: &mut KeyRefs,
    sample: PassiveSample,
    active_t: Option<f64>,
    verified: bool,
    hold: bool,
) -> Option<Learned> {
    let p = k.passive.get_or_insert_with(PassiveRef::default);
    if p.held.is_some() || (p.state == PassiveState::Learned && !p.relearning) {
        return None;
    }
    p.samples.push(sample);
    if p.samples.len() > SAMPLES_MAX {
        p.samples.remove(0);
    }
    let days: BTreeSet<&str> = p.samples.iter().map(|s| s.day.as_str()).collect();
    p.n = p.samples.len() as u32;
    p.days = days.len() as u32;
    if p.samples.len() < PASSIVE_N || days.len() < PASSIVE_DAYS {
        return None;
    }
    let ts: Vec<f64> = p.samples.iter().map(|s| s.t).collect();
    let duties: Vec<f64> = p.samples.iter().map(|s| s.duty).collect();
    let fans: Vec<f64> = p.samples.iter().filter_map(|s| s.fan).collect();
    let mut t = median(&ts)?;
    let mut duty = median(&duties)?.min(PASSIVE_DUTY_CAP);
    if let Some(c) = p.cap_t {
        t = t.min(c);
    }
    if let Some(c) = p.cap_duty {
        duty = duty.min(c);
    }
    let values = Passive {
        t,
        duty,
        fan: median(&fans),
    };
    if active_t.is_some_and(|a| t > a + SUSPECT_ABOVE_ACTIVE) {
        // Not used. While re-learning, the old values stay in use.
        if !p.relearning {
            p.state = PassiveState::Suspect;
            p.t = Some(t);
            p.duty = Some(duty);
            p.fan = values.fan;
        }
        let first = p.suspect_since.is_none();
        if first {
            p.suspect_since = p.samples.last().map(|s| s.day.clone());
        }
        return first.then_some(Learned::Suspect(values));
    }
    let label = span_label(&p.samples);
    let learned = PassiveRef {
        state: PassiveState::Learned,
        n: p.n,
        days: p.days,
        t: Some(t),
        duty: Some(duty),
        fan: values.fan,
        accepted: false,
        verified,
        label: label.clone(),
        relearning: false,
        cap_t: None,
        cap_duty: None,
        suspect_since: None,
        samples: std::mem::take(&mut p.samples),
        held: None,
    };
    if hold {
        p.held = Some(Box::new(learned));
    } else {
        *p = learned;
    }
    Some(Learned::Passive(values, label))
}

/// The p90 of `n` windows of which only `values` were non-zero.
fn light_p90(n: u32, values: &[f64]) -> Option<f64> {
    let zeros = (n as usize).saturating_sub(values.len());
    let mut all = vec![0.0; zeros];
    all.extend_from_slice(values);
    percentile(&all, 0.9)
}

/// Feeds one qualifying light window into the light reference (§6.2). `hold` as for
/// `learn_passive`.
pub fn learn_light(
    k: &mut KeyRefs,
    duty_hot: f64,
    verified: bool,
    day: Date,
    hold: bool,
) -> Option<Learned> {
    let l = k.light.get_or_insert_with(LightRef::default);
    if l.held.is_some() || (l.p90.is_some() && !l.relearning) {
        return None;
    }
    l.n += 1;
    if duty_hot > 0.0 {
        l.values.push(duty_hot);
    }
    if l.n < LIGHT_N {
        return None;
    }
    let raw = light_p90(l.n, &l.values)?;
    let capped = raw > LIGHT_CAP;
    let p90 = raw.clamp(LIGHT_FLOOR, LIGHT_CAP);
    let learned = LightRef {
        n: l.n,
        p90: Some(p90),
        accepted: false,
        verified,
        capped,
        relearning: false,
        label: format!("learned to {}", day.label()),
        values: l.values.clone(),
        held: None,
    };
    if hold {
        l.held = Some(Box::new(learned));
    } else {
        *l = learned;
    }
    Some(Learned::Light { p90, raw, capped })
}

/// The end of a verification: references re-learned meanwhile come into use (`promote`), or are
/// dropped because the clean did not work. Returns whether anything changed.
pub fn settle_held(k: &mut KeyRefs, promote: bool) -> bool {
    let mut changed = false;
    if let Some(p) = &mut k.passive
        && let Some(h) = p.held.take()
    {
        if promote {
            *p = *h;
        }
        changed = true;
    }
    if let Some(l) = &mut k.light
        && let Some(h) = l.held.take()
    {
        if promote {
            *l = *h;
        }
        changed = true;
    }
    changed
}

/// A clean (§9.1, §6.2): accepted references are dropped, a new known-clean window starts with
/// no reference checks in it, and R_passive and the light reference re-learn within the caps.
/// The old values stay in use until the new ones are learned.
pub fn on_cleaned(k: &mut KeyRefs) {
    if k.active.as_ref().is_some_and(|a| a.accepted) {
        k.active = k.verified.clone();
    }
    k.clean_checks.clear();
    if let Some(p) = &mut k.passive {
        p.held = None;
        let keep_caps = (p.cap_t, p.cap_duty);
        p.samples.clear();
        p.n = 0;
        p.days = 0;
        p.suspect_since = None;
        if p.accepted || p.state != PassiveState::Learned {
            // Nothing verified to keep using: learn from scratch, within the caps an accepted
            // value carried over from the verified one it replaced.
            *p = PassiveRef {
                cap_t: keep_caps.0,
                cap_duty: keep_caps.1,
                ..PassiveRef::default()
            };
        } else {
            p.relearning = true;
            p.cap_t = p.t.map(|t| t + RELEARN_CAP_T);
            p.cap_duty = p.duty.map(|d| d + RELEARN_CAP_DUTY);
        }
    }
    if let Some(l) = &mut k.light {
        if l.accepted || l.p90.is_none() {
            *l = LightRef::default();
        } else {
            l.relearning = true;
            l.n = 0;
            l.values.clear();
            l.held = None;
        }
    }
}

/// The values "Not dust" adopts (§9.2).
#[derive(Clone, Debug, PartialEq)]
pub struct Accepted {
    pub t: f64,
    pub duty: f64,
    pub fan: Option<f64>,
    pub f: Option<f64>,
    pub work: Option<f64>,
    pub generator: Option<u32>,
}

/// Not dust: R_active and R_passive become `a`, uncapped, until the next clean. R_best stays.
pub fn accept(k: &mut KeyRefs, a: &Accepted, date: Date, bios: &str, cpu: &str) {
    let label = format!("accepted {}", date.label());
    k.active = Some(CheckRef {
        t: Some(a.t),
        duty: Some(a.duty),
        fan: a.fan,
        f: a.f,
        work: a.work,
        source: "accepted".to_owned(),
        label: label.clone(),
        bios: bios.to_owned(),
        cpu: cpu.to_owned(),
        generator: a.generator,
        accepted: true,
        ..CheckRef::default()
    });
    // A verified R_passive leaves its caps behind for the re-learning after the next clean.
    let caps = match &k.passive {
        Some(p) if p.state == PassiveState::Learned && !p.accepted => (
            p.t.map(|t| t + RELEARN_CAP_T),
            p.duty.map(|d| d + RELEARN_CAP_DUTY),
        ),
        Some(p) => (p.cap_t, p.cap_duty),
        None => (None, None),
    };
    k.passive = Some(PassiveRef {
        state: PassiveState::Learned,
        t: Some(a.t),
        duty: Some(a.duty),
        fan: a.fan,
        accepted: true,
        label,
        cap_t: caps.0,
        cap_duty: caps.1,
        ..PassiveRef::default()
    });
}

/// Not dust re-learns the light reference from the last 7 days: `n` windows with the non-zero
/// `values`. Too few windows leave the reference re-learning from the next ones.
pub fn accept_light(k: &mut KeyRefs, n: u32, values: &[f64], date: Date) {
    let l = k.light.get_or_insert_with(LightRef::default);
    if n >= 30
        && let Some(p90) = light_p90(n, values)
    {
        *l = LightRef {
            n,
            p90: Some(p90.max(LIGHT_FLOOR)),
            accepted: true,
            label: format!("accepted {}", date.label()),
            values: values.to_vec(),
            ..LightRef::default()
        };
    } else {
        l.relearning = l.p90.is_some();
        l.accepted = true;
        l.n = 0;
        l.values.clear();
        l.held = None;
    }
}

/// What an own reference check did to R_active (§6.2a).
#[derive(Clone, Debug, PartialEq)]
pub enum Adopted {
    /// R_active is now this check (or the mean of two), with this label.
    Yes(String),
    /// Hotter than the bound: the seed (or the earlier reference) is kept.
    TooHot,
    /// Outside a known-clean window with a reference already in place, or a third check.
    No,
}

/// A check's T_ss as compared with `r`. The seed's load ended at 119 s, so against it the check
/// counts over the matched window (`t_matched`); a stored check keeps the steady window's.
fn t_against(c: &CheckRef, t_matched: Option<f64>, r: &CheckRef) -> Option<f64> {
    if r.source == "seed" { t_matched.or(c.t) } else { c.t }
}

/// R_best is the best verified clean reference: the lower T_ss wins.
fn update_best(k: &mut KeyRefs, c: &CheckRef, t_matched: Option<f64>, base: Option<&CheckRef>) {
    let better = match k.best.as_ref().or(base) {
        Some(b) => b.t.is_none_or(|bt| t_against(c, t_matched, b).is_some_and(|ct| ct < bt)),
        None => true,
    };
    if better {
        k.best = Some(c.clone());
    }
}

/// Whether a check may become R_active (§6.2a, §6.2b): within 3 °C of the base, and throttling
/// no more than 3 %, or than the base itself where a generic machine's first reference already
/// throttled more. With no base (a generic machine's first reference) there is nothing to bound
/// it by, as there is for one recorded outside a known-clean window.
fn within_bound(c: &CheckRef, t_matched: Option<f64>, base: Option<&CheckRef>) -> bool {
    let Some(b) = base else {
        return c.t.is_some() && c.duty.is_some();
    };
    let t_ok = match (t_against(c, t_matched, b), b.t) {
        (Some(t), Some(bt)) => t <= bt + ACTIVE_BOUND_T,
        (Some(_), None) => true,
        (None, _) => false,
    };
    let cap = b.duty.map_or(ACTIVE_BOUND_DUTY, |d| d.max(ACTIVE_BOUND_DUTY));
    t_ok && c.duty.is_some_and(|d| d <= cap)
}

/// An own reference check (§6.2a). `base` is the bound: the seed on its key, else R_best.
/// `t_matched` is the check's T_ss over the matched window, which is what compares with the seed.
/// Two checks on different days inside the window combine: R_T their mean, R_duty and R_fan the
/// worse of the two (the higher duty, the lower fan).
pub fn adopt_reference_check(
    k: &mut KeyRefs,
    check: CheckRef,
    t_matched: Option<f64>,
    day: Date,
    base: Option<&CheckRef>,
    in_window: bool,
) -> Adopted {
    if !in_window {
        // First run "Not sure": a key with nothing at all takes the check, marked unverified.
        if k.active.is_none() && base.is_none() {
            let mut c = check;
            c.label = format!("check {} (unverified)", day.label());
            k.active = Some(c.clone());
            return Adopted::Yes(c.label);
        }
        return Adopted::No;
    }
    if !within_bound(&check, t_matched, base) {
        return Adopted::TooHot;
    }
    let mut c = check;
    c.label = format!("check {}", day.label());
    // Each check on its own competes for R_best, not their mean.
    update_best(k, &c, t_matched, base);
    let other_day = k
        .clean_checks
        .first()
        .filter(|first| first.label != c.label)
        .cloned();
    match (k.clean_checks.len(), other_day) {
        (0, _) => k.clean_checks.push(c.clone()),
        (1, Some(first)) => {
            k.clean_checks.push(c.clone());
            let fan = match (first.fan, c.fan) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let label = format!(
                "checks {} + {}",
                first.label.trim_start_matches("check "),
                day.label()
            );
            c = CheckRef {
                t: first.t.zip(c.t).map(|(a, b)| (a + b) / 2.0),
                duty: first.duty.zip(c.duty).map(|(a, b)| a.max(b)),
                fan,
                label,
                ..c
            };
        }
        // A second check the same day replaces the first; a third changes nothing.
        (1, None) => k.clean_checks[0] = c.clone(),
        _ => return Adopted::No,
    }
    k.active = Some(c.clone());
    k.verified = Some(c.clone());
    Adopted::Yes(c.label)
}

/// A post-clean Restored check (§6.2b): R_active follows it within the bound, and R_best too
/// when it is better.
pub fn adopt_restored(
    k: &mut KeyRefs,
    check: CheckRef,
    t_matched: Option<f64>,
    day: Date,
    base: Option<&CheckRef>,
) -> bool {
    let mut c = check;
    c.label = format!("restored {}", day.label());
    c.source = "restored".to_owned();
    update_best(k, &c, t_matched, base);
    if !within_bound(&c, t_matched, base) {
        return false;
    }
    k.active = Some(c.clone());
    k.verified = Some(c);
    true
}

/// Whether `day` is inside the known-clean window ending on `until` (a day key).
pub fn in_known_clean(until: Option<&str>, day: Date) -> bool {
    until
        .and_then(Date::parse_key)
        .is_some_and(|u| day <= u)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED_KEY: &str = "ac|performance|performance|28|35|8|0|100|open";

    fn day(d: u32) -> Date {
        Date::new(2026, 11, 20).unwrap().add_days(d as i64)
    }

    fn sample(d: u32, t: f64, duty: f64) -> PassiveSample {
        PassiveSample {
            day: day(d).key(),
            t,
            duty,
            fan: Some(6800.0),
        }
    }

    #[test]
    fn the_seed_is_never_dropped() {
        let refs = References::default();
        let v = KeyView::of(&refs, SEED_KEY, Chassis::Duo, "UX482EG");
        assert!(v.seed && v.referenced());
        assert_eq!(v.label().as_deref(), Some("duo-seed 20/11/2026"));
        assert_eq!(v.active.as_ref().unwrap().t, Some(86.0));
        assert_eq!(v.r_fan(), Some(6800.0));
        let v = KeyView::of(&refs, SEED_KEY, Chassis::Duo, "UX482EA");
        assert_eq!(
            v.label().as_deref(),
            Some("duo-seed 20/11/2026 (measured on UX482EG)")
        );
        // Generic mode, or another key: nothing until learned.
        assert!(!KeyView::of(&refs, SEED_KEY, Chassis::Generic, "X").referenced());
        let other = "ac|balanced|balance_performance|15|35|8|0|100|open";
        assert!(!KeyView::of(&refs, other, Chassis::Duo, "UX482EG").referenced());
    }

    #[test]
    fn a_learned_light_reference_is_a_reference() {
        // Its windows vote as soon as it exists, so the key is referenced and shown so.
        let other = "ac|balanced|balance_performance|15|35|8|0|100|open";
        let mut refs = References::default();
        refs.keys.entry(other.into()).or_default().light = Some(LightRef {
            n: 300,
            p90: Some(0.003),
            label: "learned to 3/12/2026".into(),
            ..LightRef::default()
        });
        let v = KeyView::of(&refs, other, Chassis::Duo, "UX482EG");
        assert!(v.referenced());
        assert_eq!(v.label().as_deref(), Some("light load learned to 3/12/2026"));
        // Still learning: no reference yet.
        refs.keys.get_mut(other).unwrap().light.as_mut().unwrap().p90 = None;
        assert!(!KeyView::of(&refs, other, Chassis::Duo, "UX482EG").referenced());
    }

    #[test]
    fn passive_learning_needs_eight_on_three_days() {
        let mut k = KeyRefs::default();
        for i in 0..7 {
            assert_eq!(
                learn_passive(&mut k, sample(i % 3, 86.0 + i as f64 * 0.25, 0.012), Some(86.0), true, false),
                None
            );
        }
        // Eight on two days is not enough.
        let mut k2 = k.clone();
        k2.passive.as_mut().unwrap().samples.retain(|s| s.day != day(2).key());
        for _ in 0..4 {
            learn_passive(&mut k2, sample(0, 86.0, 0.01), Some(86.0), true, false);
        }
        assert_eq!(k2.passive.as_ref().unwrap().state, PassiveState::Learning);

        let got = learn_passive(&mut k, sample(3, 86.5, 0.05), Some(86.0), true, false);
        let Some(Learned::Passive(p, label)) = got else {
            panic!("{got:?}")
        };
        assert_eq!(p.t, 86.625);
        assert_eq!(p.duty, 0.012);
        assert_eq!(label, "learned 20/11/2026-23/11/2026");
        let v = KeyView::of(
            &References {
                keys: [(SEED_KEY.to_owned(), k.clone())].into(),
                ..References::default()
            },
            SEED_KEY,
            Chassis::Duo,
            "UX482EG",
        );
        assert_eq!(v.passive.unwrap().t, 86.625);
        // Frozen once learned.
        assert_eq!(learn_passive(&mut k, sample(4, 95.0, 0.2), Some(86.0), true, false), None);
    }

    #[test]
    fn passive_duty_is_capped_and_hot_learning_is_suspect() {
        let mut k = KeyRefs::default();
        let mut last = None;
        for i in 0..8 {
            last = learn_passive(&mut k, sample(i % 4, 90.0, 0.06), Some(86.0), true, false);
        }
        let Some(Learned::Suspect(p)) = last else {
            panic!("{last:?}")
        };
        assert_eq!(p.duty, PASSIVE_DUTY_CAP);
        let pr = k.passive.as_ref().unwrap();
        assert_eq!(pr.state, PassiveState::Suspect);
        assert_eq!(pr.suspect_since.as_deref(), Some(day(3).key().as_str()));
        // Learning continues; a cooler run brings it under the line.
        for i in 0..64 {
            learn_passive(&mut k, sample(5 + i % 4, 87.0, 0.01), Some(86.0), true, false);
        }
        assert_eq!(k.passive.as_ref().unwrap().state, PassiveState::Learned);
    }

    #[test]
    fn a_clean_relearns_within_caps_and_drops_accepted() {
        let mut k = KeyRefs::default();
        for i in 0..8 {
            learn_passive(&mut k, sample(i % 3, 86.0, 0.01), Some(86.0), true, false);
        }
        on_cleaned(&mut k);
        let p = k.passive.as_ref().unwrap();
        assert!(p.relearning && p.state == PassiveState::Learned);
        assert_eq!((p.cap_t, p.cap_duty), (Some(87.5), Some(0.02)));
        // Hotter builds after the clean re-learn only up to the cap.
        let mut got = None;
        for i in 0..8 {
            got = learn_passive(&mut k, sample(10 + i % 3, 89.0, 0.025), Some(86.0), true, false);
        }
        let Some(Learned::Passive(p, _)) = got else {
            panic!("{got:?}")
        };
        assert_eq!((p.t, p.duty), (87.5, 0.02));

        // Not dust: uncapped, accepted; the next clean drops it and keeps the verified caps.
        accept(
            &mut k,
            &Accepted {
                t: 91.0,
                duty: 0.07,
                fan: Some(6900.0),
                f: None,
                work: None,
                generator: None,
            },
            day(20),
            "UX482EG.311",
            SEED_CPU,
        );
        let refs = References {
            keys: [(SEED_KEY.to_owned(), k.clone())].into(),
            ..References::default()
        };
        let v = KeyView::of(&refs, SEED_KEY, Chassis::Duo, "UX482EG");
        assert_eq!(v.active.as_ref().unwrap().t, Some(91.0));
        assert_eq!(v.label().as_deref(), Some("accepted 10/12/2026"));
        assert_eq!(v.best.as_ref().unwrap().source, "seed", "R_best is untouched");
        on_cleaned(&mut k);
        let refs = References {
            keys: [(SEED_KEY.to_owned(), k.clone())].into(),
            ..References::default()
        };
        let v = KeyView::of(&refs, SEED_KEY, Chassis::Duo, "UX482EG");
        assert_eq!(v.active.as_ref().unwrap().source, "seed");
        assert_eq!(v.passive, None);
        let p = k.passive.as_ref().unwrap();
        assert_eq!(p.cap_t, Some(89.0));
        assert!((p.cap_duty.unwrap() - 0.03).abs() < 1e-12);
    }

    #[test]
    fn values_relearned_while_verifying_wait_for_the_verdict() {
        let mut k = KeyRefs::default();
        for i in 0..8 {
            learn_passive(&mut k, sample(i % 3, 86.0, 0.01), Some(86.0), true, false);
        }
        on_cleaned(&mut k);
        let mut got = None;
        for i in 0..8 {
            got = learn_passive(&mut k, sample(10 + i % 3, 87.0, 0.015), Some(86.0), true, true);
        }
        assert!(matches!(got, Some(Learned::Passive(..))), "{got:?}");
        let p = k.passive.as_ref().unwrap();
        assert_eq!(
            (p.t, p.held.as_ref().unwrap().t),
            (Some(86.0), Some(87.0)),
            "the old values stay in use"
        );
        assert_eq!(
            learn_passive(&mut k, sample(14, 87.0, 0.015), Some(86.0), true, true),
            None,
            "learning stops while a result waits"
        );
        let mut promoted = k.clone();
        assert!(settle_held(&mut promoted, true));
        assert_eq!(promoted.passive.as_ref().unwrap().t, Some(87.0));
        assert!(settle_held(&mut k, false));
        let p = k.passive.as_ref().unwrap();
        assert_eq!((p.t, p.held.is_none()), (Some(86.0), true));
        assert!(!settle_held(&mut k, true), "nothing left to settle");
    }

    #[test]
    fn a_clean_with_no_case_open_relearns_too() {
        let mut k = KeyRefs::default();
        for i in 0..300 {
            learn_light(&mut k, if i % 50 == 0 { 0.01 } else { 0.0 }, false, day(5), false);
        }
        for i in 0..8 {
            learn_passive(&mut k, sample(i % 3, 86.0, 0.01), Some(86.0), false, false);
        }
        on_cleaned(&mut k);
        // Learned outside a known-clean window, so unverified: learned again inside the new one,
        // with the old values in use meanwhile.
        let l = k.light.as_ref().unwrap();
        assert_eq!((l.p90, l.relearning, l.n), (Some(LIGHT_FLOOR), true, 0));
        let p = k.passive.as_ref().unwrap();
        assert_eq!((p.t, p.relearning), (Some(86.0), true));
    }

    #[test]
    fn light_reference_floor_and_cap() {
        let mut k = KeyRefs::default();
        let mut got = None;
        for i in 0..300 {
            got = learn_light(&mut k, if i % 50 == 0 { 0.01 } else { 0.0 }, true, day(5), false);
        }
        // Mostly zeros: p90 0, floored at 0.2 %.
        assert_eq!(
            got,
            Some(Learned::Light {
                p90: LIGHT_FLOOR,
                raw: 0.0,
                capped: false
            })
        );
        let mut k = KeyRefs::default();
        for i in 0..300 {
            got = learn_light(&mut k, if i % 2 == 0 { 0.05 } else { 0.0 }, true, day(5), false);
        }
        assert_eq!(
            got,
            Some(Learned::Light {
                p90: LIGHT_CAP,
                raw: 0.05,
                capped: true
            })
        );
    }

    #[test]
    fn reference_checks() {
        let seed = seed_ref("UX482EG");
        let check = |t: f64, duty: f64, fan: f64| CheckRef {
            t: Some(t),
            duty: Some(duty),
            fan: Some(fan),
            source: "check".into(),
            ..CheckRef::default()
        };
        let mut k = KeyRefs::default();
        assert_eq!(
            adopt_reference_check(&mut k, check(89.5, 0.01, 6800.0), None, day(1), Some(&seed), true),
            Adopted::TooHot
        );
        assert_eq!(
            adopt_reference_check(&mut k, check(86.5, 0.01, 6900.0), None, day(1), Some(&seed), true),
            Adopted::Yes("check 21/11/2026".into())
        );
        assert_eq!(
            adopt_reference_check(&mut k, check(85.5, 0.02, 6800.0), None, day(3), Some(&seed), true),
            Adopted::Yes("checks 21/11/2026 + 23/11/2026".into())
        );
        let a = k.active.as_ref().unwrap();
        assert_eq!((a.t, a.duty, a.fan), (Some(86.0), Some(0.02), Some(6800.0)));
        assert_eq!(k.best.as_ref().unwrap().t, Some(85.5));
        assert_eq!(
            adopt_reference_check(&mut k, check(86.0, 0.01, 6800.0), None, day(4), Some(&seed), true),
            Adopted::No
        );
        // Outside a known-clean window with a reference in place: nothing changes.
        let mut k = KeyRefs::default();
        assert_eq!(
            adopt_reference_check(&mut k, check(86.0, 0.01, 6800.0), None, day(1), Some(&seed), false),
            Adopted::No
        );
        // Generic, first run "Not sure": the first check becomes R_active, unverified.
        assert_eq!(
            adopt_reference_check(&mut k, check(80.0, 0.01, 4000.0), None, day(1), None, false),
            Adopted::Yes("check 21/11/2026 (unverified)".into())
        );

        // Against the seed the bound reads the matched window, as the verdict does: a steady
        // window that soaked to 89.5 °C is within it when the matched one read 87.5.
        let mut k = KeyRefs::default();
        assert_eq!(
            adopt_reference_check(&mut k, check(89.5, 0.01, 6800.0), Some(87.5), day(1), Some(&seed), true),
            Adopted::Yes("check 21/11/2026".into())
        );
        assert_eq!(k.active.as_ref().unwrap().t, Some(89.5), "the stored value stays the steady one");
        assert!(k.best.is_none(), "not better than the seed over the same window");

        // Generic, inside a known-clean window: a thin laptop that throttles 6 % at PL1 has its
        // first reference recorded, and later ones are bounded by it.
        let mut k = KeyRefs::default();
        assert_eq!(
            adopt_reference_check(&mut k, check(90.0, 0.06, 4800.0), None, day(1), None, true),
            Adopted::Yes("check 21/11/2026".into())
        );
        let first = k.best.clone().unwrap();
        assert_eq!(
            adopt_reference_check(&mut k, check(91.0, 0.055, 4800.0), None, day(2), Some(&first), true),
            Adopted::Yes("checks 21/11/2026 + 22/11/2026".into())
        );
        let mut k2 = KeyRefs::default();
        assert_eq!(
            adopt_reference_check(&mut k2, check(91.0, 0.08, 4800.0), None, day(2), Some(&first), true),
            Adopted::TooHot
        );
        assert_eq!(
            adopt_reference_check(&mut k2, check(93.5, 0.05, 4800.0), None, day(2), Some(&first), true),
            Adopted::TooHot
        );

        assert!(in_known_clean(Some("2026-12-20"), Date::new(2026, 12, 20).unwrap()));
        assert!(!in_known_clean(Some("2026-12-20"), Date::new(2026, 12, 21).unwrap()));
        assert!(!in_known_clean(None, day(0)));
    }
}
