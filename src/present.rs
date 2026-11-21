//! Everything she reads: the pill and MinkaMon blocks of state.json (§11.4), and the wording of
//! summaries, events and notices. The rules decide; this module only words and ranks, so the
//! consumers never need rules of their own. They map the tone to a token and nothing else.

use crate::clock::{Date, Moment};
use crate::model::{
    CheckPhase, CheckVerdict, FanStatus, FaultStatus, Mon, Pill, PostClean, State, Status, Tone,
};

/// A check result stays on MinkaMon this long.
pub const RESULT_SHOW_S: i64 = 24 * 3600;

/// Why Not dust does not apply; the service refuses with it and the CLI says it before asking.
pub const NOT_DUST_NO_CASE: &str =
    "Not dust applies to an open dust case (watch, suspicion or warning)";

/// °C with at most one decimal and none for a whole number: "86", "90.4".
pub fn c(t: f64) -> String {
    let r = (t * 10.0).round() / 10.0;
    if r.fract() == 0.0 {
        format!("{r:.0}")
    } else {
        format!("{r:.1}")
    }
}

/// A fraction as a percentage with one decimal: 0.068 → "6.8 %".
pub fn pct(fraction: f64) -> String {
    format!("{:.1} %", fraction * 100.0)
}

fn mostly(top: &[String]) -> String {
    top.first()
        .map(|comm| format!(", mostly {comm}"))
        .unwrap_or_default()
}

/// "build 90.4 °C, throttled 6.8 % (clean 86 °C, 0.9 %), mostly rustc". Without a reference
/// (a saturation vote while learning) the fan stands in for the comparison.
pub fn build_summary(
    t: f64,
    duty: f64,
    against: Option<(f64, f64)>,
    fan: Option<f64>,
    top: &[String],
) -> String {
    match against {
        Some((rt, rd)) => format!(
            "build {} °C, throttled {} (clean {} °C, {}){}",
            c(t),
            pct(duty),
            c(rt),
            pct(rd),
            mostly(top)
        ),
        None => format!(
            "build {} °C, throttled {} with the fan at {} rpm (no reference yet){}",
            c(t),
            pct(duty),
            fan.map_or("?".to_owned(), |f| format!("{f:.0}")),
            mostly(top)
        ),
    }
}

/// "light load 1.2 threads, throttled 38.0 % at 91 °C (clean 0.2 %), mostly bch-reconcile".
pub fn light_summary(
    threads: f64,
    duty_hot: f64,
    t_max: Option<f64>,
    p90: Option<f64>,
    top: &[String],
) -> String {
    format!(
        "light load {threads:.1} threads, throttled {}{}{}{}",
        pct(duty_hot),
        t_max.map(|t| format!(" at {} °C", c(t))).unwrap_or_default(),
        p90.map(|p| format!(" (clean {})", pct(p)))
            .unwrap_or_else(|| " (no light reference yet)".to_owned()),
        mostly(top)
    )
}

/// "check 90.1 °C, throttled 7.2 % (clean 86 °C, 0.9 %)".
pub fn check_summary(t: Option<f64>, duty: Option<f64>, against: Option<(f64, f64)>) -> String {
    let mut s = format!(
        "check {} °C, throttled {}",
        t.map_or("?".to_owned(), c),
        duty.map_or("?".to_owned(), pct)
    );
    if let Some((rt, rd)) = against {
        s += &format!(" (clean {} °C, {})", c(rt), pct(rd));
    }
    s
}

pub fn first_evidence_event(summary: &str, earlier: Option<&str>) -> String {
    match earlier {
        Some(d) => format!("First evidence: {summary}; earlier sighting {d}"),
        None => format!("First evidence: {summary}"),
    }
}

pub fn suspicion_event(evidence: u32, qualifying: u32) -> String {
    format!("Possible dust: signs on {evidence} of {qualifying} qualifying days in 14")
}

pub fn suspicion_by_check() -> String {
    "Possible dust: the cooling check came out worse than the reference; run it again on another day"
        .to_owned()
}

/// The WARNING event, by route (§8).
pub fn warning_event(route: &str, evidence: u32, qualifying: u32, e2: u32) -> String {
    match route {
        "a" => "Needs a clean: the cooling check came out worse than the reference".to_owned(),
        "b" => format!(
            "Needs a clean: signs on {evidence} of {qualifying} days in 21, light load on {e2}"
        ),
        "c" => format!("Needs a clean: light-load throttling on {e2} days in 14"),
        _ => format!(
            "Needs a clean: signs on {evidence} of {qualifying} days in 21, after three weeks of suspicion"
        ),
    }
}

/// `case_status` is the status before the clean when a case was open.
pub fn cleaned_event(date: Date, case_status: Option<Status>, known_clean_until: Date) -> String {
    if let Some(status) = case_status {
        let what = if status == Status::Warning {
            "the warning is cleared"
        } else {
            "the case is now being verified"
        };
        format!(
            "Cleaned {}: {what}. A cooling check (about 5 min) verifies it; otherwise the builds of the next two weeks will",
            date.label()
        )
    } else {
        format!(
            "Cleaned {} (no open case): known clean until {}",
            date.label(),
            known_clean_until.label()
        )
    }
}

pub fn verified_by_builds(clean: u32, date: &str) -> String {
    format!("Verified by builds: {clean} clean and no hot builds since the clean of {date}")
}

pub fn unverified(voting: u32, hot: u32, clean: u32, date: &str) -> String {
    format!(
        "Not verified: {voting} comparable builds in the two weeks after the clean of {date} ({hot} hot, {clean} clean)"
    )
}

/// Passive Not improved: what the builds and light load since the clean showed.
pub fn not_improved_event(date: &str, hot: u32, e2_days: u32) -> String {
    let mut parts = Vec::new();
    if hot > 0 {
        parts.push(format!("{hot} hot builds"));
    }
    if e2_days > 0 {
        parts.push(format!("light-load throttling on {e2_days} days"));
    }
    format!("Still hot after cleaning {date}: {} since", parts.join(", "))
}

/// Not improved by a verify check: the check decided, not builds.
pub fn not_improved_check_event(date: &str, check: &str) -> String {
    format!("Still hot after cleaning {date}: cooling {check}")
}

pub fn lapse_event(qualifying: u32, first: &str) -> String {
    format!("No signs for 14 days ({qualifying} with comparable load): the case of {first} closed")
}

pub fn cleared_by_check_event(first: &str) -> String {
    format!("A cooling check measured like the reference: the case of {first} closed")
}

pub fn not_dust_event(adopted: &str) -> String {
    format!(
        "Not dust: today's cooling accepted as normal (your decision); {adopted} until the next clean"
    )
}

pub fn snooze_event(until: &str) -> String {
    format!("Suspicion actions hidden until {until}")
}

pub fn fan_slow_text(r_fan: Option<f64>, fan: Option<f64>) -> String {
    format!(
        "Fan slower than normal ({} → {} rpm): wear, cleaning may not fix it",
        r_fan.map_or("?".to_owned(), |r| format!("{r:.0}")),
        fan.map_or("?".to_owned(), |f| format!("{f:.0}"))
    )
}

/// FAN raised by `fan_low` alone: the fan was still on a low stage two minutes into builds, even
/// if it reached its normal speed later.
pub fn fan_ramp_text(r_fan: Option<f64>, at_settle: f64) -> String {
    format!(
        "Fan slow to speed up ({at_settle:.0} rpm two minutes into a build, normal {} rpm): wear, cleaning may not fix it",
        r_fan.map_or("?".to_owned(), |r| format!("{r:.0}"))
    )
}

pub fn fan_check_text(r_fan: Option<f64>, fan: Option<f64>) -> String {
    format!(
        "Fan slower than reference ({} → {} rpm): wear, cleaning will not fix it",
        r_fan.map_or("?".to_owned(), |r| format!("{r:.0}")),
        fan.map_or("?".to_owned(), |f| format!("{f:.0}"))
    )
}

pub fn fan_stopped_text() -> String {
    "Fan stopped under load".to_owned()
}

pub fn fault_text(t: f64, tcc: f64) -> String {
    format!(
        "Cooling fault: {} °C for 5 s, 6 °C past the {} °C throttle point: TCC is not acting",
        c(t),
        c(tcc)
    )
}

pub fn config_change_event(old: Option<&str>, new: &str) -> String {
    match old {
        Some(o) => format!("Settings changed: {o} → {new}"),
        None => format!("Settings changed: {new}"),
    }
}

/// Notice ids and texts (§4, §6.2, §7, §8.4). A notice is logged at most once per id per day.
pub mod notice {
    use super::{c, pct};

    pub const POWER_CAPPED: &str = "power_capped";
    pub const COOLING_ACTIVE: &str = "cooling_active";
    pub const COLD_THROTTLE: &str = "cold_throttle";
    pub const STARVATION: &str = "starvation";
    pub const PASSIVE_SUSPECT: &str = "passive_suspect";
    pub const LIGHT_CAP: &str = "light_cap";
    pub const REFERENCE_HOT: &str = "reference_hot";
    pub const BIOS_CHANGE: &str = "bios_change";
    pub const LIKE_IN_WARNING: &str = "like_in_warning";
    pub const STATE_UNREADABLE: &str = "state_unreadable";
    pub const REFERENCES_UNREADABLE: &str = "references_unreadable";

    /// state.json did not parse and the service started from a fresh state.
    pub fn state_unreadable(kept_as: &str) -> String {
        format!(
            "state.json could not be read and was kept as {kept_as}: an open case or warning in it was lost; History still has its events"
        )
    }

    /// references.json did not parse and the service started without learned references.
    pub fn references_unreadable(kept_as: &str) -> String {
        format!(
            "references.json could not be read and was kept as {kept_as}: learned and accepted references were lost and learn again"
        )
    }

    pub fn power_capped(pl1_w: Option<u32>) -> String {
        format!(
            "power limit looks capped (PL1 {} W): not dust",
            pl1_w.map_or("?".to_owned(), |w| w.to_string())
        )
    }

    pub fn cooling_active() -> String {
        "a cooling device or thermald was throttling during a build: those minutes are not judged"
            .to_owned()
    }

    pub fn cold_throttle() -> String {
        "throttling while cool: charger or EC limit".to_owned()
    }

    pub fn starvation() -> String {
        "not enough comparable load to judge; run a check?".to_owned()
    }

    pub fn passive_suspect(t: f64, active_t: Option<f64>) -> String {
        format!(
            "builds run hotter than the reference check ({} °C against {}): long-build heat soak or a different load; run a check",
            c(t),
            active_t.map_or("?".to_owned(), |a| format!("{} °C", c(a)))
        )
    }

    pub fn light_cap(raw: f64) -> String {
        format!(
            "light-load throttling while learning was {}, so the light reference is capped at 2 %: check the vents and the profile",
            pct(raw)
        )
    }

    /// `since` is the date of the reference it was bounded by; empty when its label has none.
    pub fn reference_hot(since: &str) -> String {
        let clean = if since.is_empty() {
            "clean reference".to_owned()
        } else {
            format!("{since} clean")
        };
        format!("reference ran hotter than the {clean}: check the profile, PL and vents")
    }

    pub fn bios_change(old: &str, new: &str) -> String {
        let short = |v: &str| v.rsplit('.').next().unwrap_or(v).to_owned();
        format!(
            "firmware changed ({} → {}): if builds now read differently, run a check; Not dust adopts the new behaviour",
            short(old),
            short(new)
        )
    }

    pub fn like_in_warning() -> String {
        "measured like the reference today; if you have not cleaned, consider Not dust".to_owned()
    }
}

/// The cooling check (§10): gates, refusals, aborts, validity, notes and diagnosis lines.
pub mod check {
    use super::{c, pct};
    use crate::model::CheckPurpose;

    pub const PROMPT: &str = "hard surface · vents clear · lid open · usual room";
    pub const WAITING: &str = "the load starts by itself once the machine is idle (up to 5 min)";

    pub fn purpose(p: CheckPurpose) -> &'static str {
        match p {
            CheckPurpose::Confirm => "confirm the dust case",
            CheckPurpose::Before => "measure before cleaning",
            CheckPurpose::Verify => "verify the clean",
            CheckPurpose::Reference => "record a reference",
        }
    }

    pub fn started_event(p: CheckPurpose) -> String {
        format!("Cooling check started ({})", purpose(p))
    }

    pub fn refused_event(reason: &str) -> String {
        format!("Cooling check refused: {reason}")
    }

    pub fn aborted_event(reason: &str) -> String {
        format!("Cooling check aborted: {reason}")
    }

    // Gates (§10.2).

    pub fn ac(ac: Option<bool>) -> String {
        match ac {
            Some(true) => "AC online".to_owned(),
            Some(false) => "on battery: plug in the charger".to_owned(),
            None => "AC state unknown".to_owned(),
        }
    }

    pub fn no_battery() -> String {
        "no battery".to_owned()
    }

    pub fn battery(capacity: Option<u8>) -> String {
        capacity.map_or("battery".to_owned(), |c| format!("battery {c} %"))
    }

    pub fn battery_low(capacity: u8, needed: u8) -> String {
        format!("battery {capacity} % (needs {needed} %)")
    }

    /// Discharging while on AC.
    pub fn battery_discharging() -> String {
        "battery discharging: the charger is not keeping up".to_owned()
    }

    /// Discharging with the charger unplugged.
    pub fn battery_draining(capacity: Option<u8>) -> String {
        match capacity {
            Some(c) => format!("battery {c} %, discharging"),
            None => "battery discharging".to_owned(),
        }
    }

    pub fn profile(profile: Option<&str>) -> String {
        match profile {
            None => "no platform profile".to_owned(),
            Some("performance") => "profile performance".to_owned(),
            Some(p) => format!("profile {p}: switch to the Performance profile"),
        }
    }

    pub fn lid(open: Option<bool>) -> String {
        match open {
            Some(true) => "lid open".to_owned(),
            Some(false) => "lid closed: open the lid".to_owned(),
            None => "lid not reported".to_owned(),
        }
    }

    /// One setting that differs from the reference key: "PL1 25 W (reference 26-30 W)".
    pub fn differs(now: &str, reference: &str) -> String {
        format!("{now} (reference {reference})")
    }

    pub fn key_matches(reference: Option<&str>) -> String {
        match reference {
            Some(r) => format!("settings as the reference ({r})"),
            None => "settings as the reference".to_owned(),
        }
    }

    pub fn key_unreferenced() -> String {
        "no reference yet: this check records one".to_owned()
    }

    pub fn key_differs(diff: &[String]) -> String {
        format!(
            "{}: not comparable with the clean reference; Run anyway compares only with the before-clean values",
            diff.join(", ")
        )
    }

    pub fn key_anyway(diff: &[String]) -> String {
        format!(
            "{}: run anyway, not comparable with the clean reference",
            diff.join(", ")
        )
    }

    pub fn dgpu_suspended() -> String {
        "dGPU suspended".to_owned()
    }

    pub fn dgpu_no_tool() -> String {
        "dGPU active (no nvidia-smi to ask)".to_owned()
    }

    pub fn dgpu_asking() -> String {
        "dGPU active: asking nvidia-smi".to_owned()
    }

    pub fn dgpu_no_reading() -> String {
        "dGPU active: nvidia-smi gave no reading".to_owned()
    }

    pub fn dgpu_busy(percent: f64, ok: bool) -> String {
        if ok {
            format!("dGPU {percent:.0} % busy")
        } else {
            format!("dGPU {percent:.0} % busy (needs < 10 %)")
        }
    }

    pub fn settled() -> String {
        "idle and settled".to_owned()
    }

    /// The settle parts still failing, as the Check page shows them:
    /// "B10 14 % (< 10), fan 5200 (≤ 5000), slope −0.05 °C/s (≥ −0.02)".
    pub fn b10(b10: Option<f64>) -> String {
        match b10 {
            Some(b) => format!("B10 {:.0} % (< 10)", b * 100.0),
            None => "B10 not measured yet".to_owned(),
        }
    }

    pub fn quiet(quiet_s: f64, needed_s: f64) -> String {
        format!("idle {quiet_s:.0}/{needed_s:.0} s")
    }

    pub fn fan(rpm: u32, max: u32) -> String {
        format!("fan {rpm} (≤ {max})")
    }

    pub fn slope(slope: f64, min: f64) -> String {
        format!("slope {} °C/s (≥ {})", signed(slope, 2), signed(min, 2))
    }

    pub fn slope_measuring(covered_s: f64, needed_s: f64) -> String {
        format!("measuring the slope ({covered_s:.0}/{needed_s:.0} s)")
    }

    /// A minus sign rather than a hyphen, as the spec writes them, and none on a rounded zero.
    pub fn signed(v: f64, decimals: usize) -> String {
        let s = format!("{:.*}", decimals, v.abs());
        let zero = s.chars().all(|ch| ch == '0' || ch == '.');
        if v < 0.0 && !zero { format!("−{s}") } else { s }
    }

    /// Refused after the 5-minute wait: "too busy" when the machine never settled.
    pub fn refused(settle_failed: bool, failing: &[String]) -> String {
        let list = failing.join("; ");
        if settle_failed {
            format!("too busy after 5 min: {list}")
        } else {
            format!("gates still failing after 5 min: {list}")
        }
    }

    // Aborts (§10.5).

    pub const CANCELLED: &str = "cancelled";
    pub const AC_LOST: &str = "AC lost";
    pub const SUSPENDED: &str = "the machine was suspended";
    pub const LID_CLOSED: &str = "lid closed";
    pub const LOAD_STOPPED: &str = "the load generator stopped";

    pub fn battery_dropped(capacity: u8) -> String {
        format!("battery {capacity} % (below 30 %)")
    }

    pub fn key_changed(label: &str) -> String {
        format!("settings changed: {label}")
    }

    pub fn too_hot(t: f64) -> String {
        format!("{} °C for 3 s: TCC is not holding", c(t))
    }

    pub fn fan_stopped(t: f64) -> String {
        format!("fan stopped at {} °C", c(t))
    }

    pub fn load_failed(error: &str) -> String {
        format!("the load generator did not start: {error}")
    }

    // Validity (§10.5): any one makes the check Inconclusive.

    pub fn no_temperatures() -> String {
        "no temperature readings in the steady window".to_owned()
    }

    pub fn starved(nice: i64) -> String {
        format!("load generator starved (nice {nice})")
    }

    pub fn not_busy(busy: f64) -> String {
        format!("busy {} over the steady window (needs 95 %)", pct(busy))
    }

    pub fn prochot(share: f64) -> String {
        format!("iGPU PROCHOT in {} of the steady seconds", pct(share))
    }

    pub fn cold_prochot(fraction: f64) -> String {
        format!(
            "cold or PROCHOT throttle {} of the steady time: charger, EC or VR limit",
            pct(fraction)
        )
    }

    pub fn pl_changed() -> String {
        "the power limit changed during the load".to_owned()
    }

    pub fn not_comparable() -> String {
        "not comparable with the clean reference (run anyway)".to_owned()
    }

    pub fn nothing_to_compare() -> String {
        "nothing to compare with: no clean reference and no before-clean values".to_owned()
    }

    // Notes: reported, never disqualifying.

    pub fn foreign(share: f64) -> String {
        format!(
            "other processes used {} of the CPU: work not compared",
            pct(share)
        )
    }

    pub fn deficit(share: f64) -> String {
        format!("charger deficit in {} of the steady window", pct(share))
    }

    // Diagnosis lines (§10.8).

    pub const DIAG_FAN: &str = "fan slower than reference: dust inside the fan, or wear";
    pub const DIAG_CONTACT: &str = "heat is not reaching the heatsink: paste or heatsink contact";
    pub const DIAG_FINS: &str = "heatsink still hot: fins or vents still blocked";
    pub const DIAG_ROOM: &str =
        "everything is uniformly warmer than the reference: the room may be warmer";
}

// ---------------------------------------------------------------------------------------------
// The pill and MinkaMon blocks.

fn first_date(s: &State) -> String {
    s.case
        .as_ref()
        .and_then(|c| c.first_evidence.as_ref())
        .map(|f| f.at.date_label())
        .unwrap_or_default()
}

fn first_label(s: &State) -> String {
    s.case
        .as_ref()
        .and_then(|c| c.first_evidence.as_ref())
        .map(|f| f.label.clone())
        .unwrap_or_default()
}

/// The clean date after a passive or checked Not improved: WARNING re-latched in the same case.
fn not_improved(s: &State) -> Option<String> {
    let v = s.verification.as_ref()?;
    (s.status == Status::Warning && v.verdict == Some(PostClean::NotImproved))
        .then(|| v.label.clone())
}

fn rpm(v: Option<u32>) -> String {
    v.map_or("?".to_owned(), |r| r.to_string())
}

/// Rebuilds both blocks from the state. Priorities (§11.4): the pill shows fault, then dust,
/// then fan; MinkaMon shows fault, dust, fan, a running check, verifying, suspicion, and a check
/// result for 24 h.
pub fn recompute(s: &mut State, now: &Moment) {
    s.pill = pill(s);
    s.mon = mon(s, now);
}

fn pill(s: &State) -> Pill {
    let make = |kind: &str, tone: Tone, label: String| Pill {
        show: true,
        kind: kind.to_owned(),
        tone,
        label,
        page: "status".to_owned(),
    };
    if s.fault.status == FaultStatus::Fault {
        return make("fault", Tone::Red, "Cooling fault".to_owned());
    }
    if s.status == Status::Warning {
        return match not_improved(s) {
            Some(d) => make(
                "dust_not_improved",
                Tone::Amber,
                format!("Still hot after cleaning {d}"),
            ),
            None => make(
                "dust_warning",
                Tone::Amber,
                format!("Needs a clean since {}", first_date(s)),
            ),
        };
    }
    match s.fan.status {
        FanStatus::Slow => make(
            "fan",
            Tone::Amber,
            format!("Fan slow · {} rpm", rpm(s.fan.last_fan)),
        ),
        FanStatus::Stopped => make("fan", Tone::Amber, "Fan stopped".to_owned()),
        FanStatus::Ok => Pill::default(),
    }
}

fn mon(s: &State, now: &Moment) -> Mon {
    let alarm = |kind: &str, tone: Tone, badge: String, short: &str| Mon {
        show: true,
        kind: kind.to_owned(),
        tone,
        triangle: true,
        pulse: true,
        badge,
        badge_short: short.to_owned(),
        actions: Vec::new(),
        page: "status".to_owned(),
    };
    let note = |kind: &str, tone: Tone, badge: String, short: String, actions: &[&str], page: &str| Mon {
        show: true,
        kind: kind.to_owned(),
        tone,
        triangle: false,
        pulse: false,
        badge,
        badge_short: short,
        actions: actions.iter().map(|a| (*a).to_owned()).collect(),
        page: page.to_owned(),
    };
    if s.fault.status == FaultStatus::Fault {
        let t = s.fault.t_max.map(|t| format!(" {} °C", c(t))).unwrap_or_default();
        return alarm("fault", Tone::Red, format!("TCC NOT ACTING{t}"), "TCC NOT ACTING");
    }
    if s.status == Status::Warning {
        return match not_improved(s) {
            Some(d) => alarm(
                "dust_not_improved",
                Tone::Amber,
                format!("STILL HOT AFTER CLEANING {d}"),
                "STILL HOT",
            ),
            None => alarm(
                "dust_warning",
                Tone::Amber,
                format!("NEEDS A CLEAN · FIRST {}", first_label(s)),
                "NEEDS A CLEAN",
            ),
        };
    }
    match s.fan.status {
        FanStatus::Slow => {
            return alarm(
                "fan",
                Tone::Amber,
                format!(
                    "FAN SLOW {}/{} RPM",
                    rpm(s.fan.last_fan),
                    rpm(s.fan.r_fan)
                ),
                "FAN SLOW",
            );
        }
        FanStatus::Stopped => {
            return alarm("fan", Tone::Amber, "FAN STOPPED".to_owned(), "FAN STOPPED");
        }
        FanStatus::Ok => {}
    }
    if let Some(ch) = &s.check {
        let phase = match ch.state {
            CheckPhase::Gating => Some("SETTLING"),
            CheckPhase::Idle => Some("IDLE"),
            CheckPhase::Load => Some("LOAD"),
            CheckPhase::Recovery => Some("RECOVERY"),
            _ => None,
        };
        if let Some(p) = phase {
            return note(
                "check",
                Tone::Purple,
                format!("COOLING CHECK · {p}"),
                "COOLING CHECK".to_owned(),
                &[],
                "check",
            );
        }
    }
    if s.status == Status::Verifying
        && let Some(v) = &s.verification
    {
        return note(
            "verifying",
            Tone::Purple,
            format!("CLEANED {} · VERIFY COOLING", v.label),
            "VERIFY COOLING".to_owned(),
            &["check"],
            "check",
        );
    }
    if s.status == Status::Suspicion {
        let snoozed = s
            .snooze
            .suspicion_actions_until
            .as_ref()
            .and_then(|u| u.unix())
            .is_some_and(|u| now.unix_s() < u);
        let actions: &[&str] = if snoozed { &[] } else { &["check", "later"] };
        return note(
            "suspicion",
            Tone::Neutral,
            format!("DUST? SINCE {}", first_date(s)),
            "DUST?".to_owned(),
            actions,
            "status",
        );
    }
    if let Some(lc) = &s.last_check
        && lc.at.unix().is_some_and(|at| now.unix_s() - at < RESULT_SHOW_S)
    {
        let good = lc.post_clean == Some(PostClean::Restored)
            || (lc.post_clean.is_none() && lc.verdict == CheckVerdict::Like);
        let label = lc.label.to_uppercase();
        return note(
            "result",
            if good { Tone::Green } else { Tone::Neutral },
            format!("CHECK {}: {label}", lc.at.date_label()),
            format!("CHECK: {label}"),
            &[],
            "check",
        );
    }
    Mon::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Date, Stamp, Timeline};
    use crate::model::tests::WARNING_EXAMPLE;
    use crate::model::{CheckState, FirstEvidence, LastCheck, Verification};

    fn now() -> Moment {
        Timeline::starting(Date::new(2026, 12, 5).unwrap(), 10, 31, 2, 0).now()
    }

    #[test]
    fn numbers() {
        assert_eq!(c(86.0), "86");
        assert_eq!(c(90.44), "90.4");
        assert_eq!(c(89.96), "90");
        assert_eq!(pct(0.0093), "0.9 %");
        assert_eq!(pct(0.068), "6.8 %");
        assert_eq!(
            build_summary(90.4, 0.068, Some((86.0, 0.0093)), Some(6900.0), &["rustc".into()]),
            "build 90.4 °C, throttled 6.8 % (clean 86 °C, 0.9 %), mostly rustc"
        );
        assert_eq!(
            notice::bios_change("UX482EG.311", "UX482EG.312"),
            "firmware changed (311 → 312): if builds now read differently, run a check; Not dust adopts the new behaviour"
        );
    }

    #[test]
    fn the_spec_example_blocks() {
        let mut s: State = serde_json::from_str(WARNING_EXAMPLE).unwrap();
        let (pill, mon) = (s.pill.clone(), s.mon.clone());
        s.pill = Pill::default();
        s.mon = Mon::default();
        recompute(&mut s, &now());
        assert_eq!(s.pill, pill);
        assert_eq!(s.mon, mon);
    }

    #[test]
    fn priorities() {
        let mut s = State::new();
        let first = FirstEvidence {
            at: Stamp::at(Date::new(2026, 12, 3).unwrap().days() * 86_400 + 14 * 3600 + 300, 0),
            label: "3/12/2026 14:05".into(),
            ..FirstEvidence::default()
        };
        s.case = Some(crate::model::Case {
            first_evidence: Some(first),
            ..crate::model::Case::default()
        });

        // Learning, clean and watch show nothing.
        for st in [Status::Learning, Status::Clean, Status::Watch] {
            s.status = st;
            recompute(&mut s, &now());
            assert!(!s.pill.show && !s.mon.show, "{st:?}");
        }

        s.status = Status::Suspicion;
        recompute(&mut s, &now());
        assert!(!s.pill.show);
        assert_eq!(
            (s.mon.kind.as_str(), s.mon.badge.as_str(), s.mon.tone),
            ("suspicion", "DUST? SINCE 3/12/2026", Tone::Neutral)
        );
        assert_eq!(s.mon.actions, vec!["check", "later"]);
        assert!(!s.mon.triangle);
        // Later: the actions go for 3 days, the badge stays.
        s.snooze.suspicion_actions_until = Some(now().plus(3.0 * 86_400.0).stamp());
        recompute(&mut s, &now());
        assert!(s.mon.show && s.mon.actions.is_empty());

        // Verifying: no pill at all, a purple note on MinkaMon.
        s.status = Status::Verifying;
        s.verification = Some(Verification {
            label: "5/12/2026".into(),
            ..Verification::default()
        });
        recompute(&mut s, &now());
        assert!(!s.pill.show);
        assert_eq!(s.mon.badge, "CLEANED 5/12/2026 · VERIFY COOLING");
        assert_eq!((s.mon.tone, s.mon.actions.clone()), (Tone::Purple, vec!["check".to_owned()]));

        // A running check outranks verifying.
        s.check = Some(CheckState {
            state: CheckPhase::Load,
            ..CheckState::default()
        });
        recompute(&mut s, &now());
        assert_eq!(s.mon.badge, "COOLING CHECK · LOAD");

        // Not improved: WARNING again, with its own wording.
        s.check = None;
        s.status = Status::Warning;
        s.verification.as_mut().unwrap().verdict = Some(PostClean::NotImproved);
        recompute(&mut s, &now());
        assert_eq!(
            (s.pill.kind.as_str(), s.pill.label.as_str()),
            ("dust_not_improved", "Still hot after cleaning 5/12/2026")
        );
        assert_eq!(s.mon.badge, "STILL HOT AFTER CLEANING 5/12/2026");
        assert!(s.mon.triangle && s.mon.pulse);

        // Fan under dust in the pill; fault over everything, in red.
        s.fan.status = FanStatus::Slow;
        s.fan.last_fan = Some(6100);
        s.fan.r_fan = Some(6800);
        recompute(&mut s, &now());
        assert_eq!(s.pill.kind, "dust_not_improved");
        s.status = Status::Clean;
        s.case = None;
        s.verification = None;
        recompute(&mut s, &now());
        assert_eq!(
            (s.pill.label.as_str(), s.pill.tone),
            ("Fan slow · 6100 rpm", Tone::Amber)
        );
        assert_eq!(s.mon.badge, "FAN SLOW 6100/6800 RPM");
        s.fault.status = FaultStatus::Fault;
        s.fault.t_max = Some(99.0);
        recompute(&mut s, &now());
        assert_eq!(
            (s.pill.kind.as_str(), s.pill.tone, s.pill.label.as_str()),
            ("fault", Tone::Red, "Cooling fault")
        );
        assert_eq!(s.mon.badge, "TCC NOT ACTING 99 °C");

        // A check result shows for 24 h, green when restored.
        s.fault.status = FaultStatus::Ok;
        s.fan.status = FanStatus::Ok;
        s.last_check = Some(LastCheck {
            at: now().plus(-3600.0).stamp(),
            verdict: CheckVerdict::Like,
            post_clean: Some(PostClean::Restored),
            label: "Restored".into(),
            ..LastCheck::default()
        });
        recompute(&mut s, &now());
        assert_eq!(
            (s.mon.badge.as_str(), s.mon.tone),
            ("CHECK 5/12/2026: RESTORED", Tone::Green)
        );
        recompute(&mut s, &now().plus(86_400.0));
        assert!(!s.mon.show);
    }
}
