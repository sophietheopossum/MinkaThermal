//! The store's file formats (§11.3-§11.5): state.json, records, daily lines, cases, references
//! and events.
//!
//! Readers are lenient: every struct takes `#[serde(default)]`, unknown fields are ignored, and
//! enums read an unknown word as a harmless variant. A window or CLI one version behind can still
//! read what a newer service wrote.

use crate::clock::Stamp;
use crate::profile::{Chassis, ChassisSetting};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// serde_json writes a NaN or infinite f64 as `null` without complaint, then refuses to read
/// `null` back into a plain f64. Every plain f64 field reads `null` as NaN instead, so one bad
/// number from a later bug costs that number, not the whole state.json or the record's line.
/// (An `Option<f64>` needs none of this: NaN comes back as None.)
mod lenient {
    use serde::{Deserialize, Deserializer};

    pub fn number<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        Ok(Option::<f64>::deserialize(d)?.unwrap_or(f64::NAN))
    }

    pub fn numbers<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<f64>, D::Error> {
        Ok(Vec::<Option<f64>>::deserialize(d)?
            .into_iter()
            .map(|v| v.unwrap_or(f64::NAN))
            .collect())
    }

    /// `[[comm, share], …]`.
    pub fn shares<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<(String, f64)>, D::Error> {
        Ok(Vec::<(String, Option<f64>)>::deserialize(d)?
            .into_iter()
            .map(|(comm, v)| (comm, v.unwrap_or(f64::NAN)))
            .collect())
    }
}

// ---------------------------------------------------------------------------------------------
// state.json

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Learning,
    Clean,
    Watch,
    Suspicion,
    Warning,
    Verifying,
    /// Written by a newer service; shown as is, never acted on.
    #[serde(other)]
    Unknown,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Learning => "learning",
            Status::Clean => "clean",
            Status::Watch => "watch",
            Status::Suspicion => "suspicion",
            Status::Warning => "warning",
            Status::Verifying => "verifying",
            Status::Unknown => "unknown",
        }
    }

    /// An open dust case that Not dust can close (§9.2): WATCH, SUSPICION or WARNING, Not
    /// improved included. VERIFYING also has a case, but its answer is the verification.
    pub fn dust_case_open(self) -> bool {
        matches!(self, Status::Watch | Status::Suspicion | Status::Warning)
    }
}

/// Consumers map tones to their own tokens (§11.4): amber → WARN_AMBER, red → RED,
/// purple → PURPLE, green → OK_GREEN, neutral → TEXT_MUTED.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Amber,
    Red,
    Purple,
    Green,
    // serde's catch-all has to be the last variant.
    #[default]
    #[serde(other)]
    Neutral,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub schema: u32,
    pub rules_version: u32,
    pub version: String,
    pub updated_at: Stamp,
    /// When this service process started; `status` shows it as "since".
    pub started_at: Option<Stamp>,
    pub pid: u32,
    pub boot_id: String,
    pub chassis: ChassisInfo,
    pub key: KeyInfo,
    /// The active evidence classes. "E2" only while light-load evidence is on (off since
    /// 21/11/2026), so it no longer says whether the throttle counter exists.
    pub classes: Vec<String>,
    /// Whether the package throttle counter exists; `replay` judges plateaus by it. Read it with
    /// `has_throttle_counter`.
    pub throttle_counter: Option<bool>,
    pub status: Status,
    pub setup_needed: bool,
    /// Day key (`yyyy-mm-dd`) on which the known-clean window ends.
    pub known_clean_until: Option<String>,
    /// Day key from which days count as evidence: the last Cleaned, a check that cleared a case,
    /// or Not dust. Days before it were judged against what has since been refuted or replaced.
    pub evidence_from: Option<String>,
    /// The moment of that reset. Today's records from before it were judged against what was
    /// refuted, so a restart must not count them again.
    pub evidence_reset_at: Option<Stamp>,
    pub case: Option<Case>,
    pub verification: Option<Verification>,
    pub check: Option<CheckState>,
    pub last_check: Option<LastCheck>,
    pub fan: FanState,
    pub fault: FaultState,
    pub notices: Vec<Notice>,
    pub snooze: Snooze,
    pub learning: Learning,
    pub last_plateau: Option<LastPlateau>,
    pub counts: Counts,
    pub unjudged_30d: UnjudgedCounts,
    pub drift: Option<Drift>,
    pub pill: Pill,
    pub mon: Mon,
}

impl State {
    /// An empty state stamped with this build's versions.
    pub fn new() -> State {
        State {
            schema: crate::SCHEMA,
            rules_version: crate::RULES_VERSION,
            version: crate::VERSION.to_owned(),
            ..State::default()
        }
    }

    /// Whether the machine has the package throttle counter. A state from before
    /// `throttle_counter` said so through "E2" in `classes`, and one with no classes at all is
    /// taken to have it, as the Duo does.
    pub fn has_throttle_counter(&self) -> bool {
        self.throttle_counter
            .unwrap_or_else(|| self.classes.is_empty() || self.classes.iter().any(|c| c == "E2"))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChassisInfo {
    pub profile: Chassis,
    pub setting: ChassisSetting,
    pub board: String,
    pub product: String,
    pub bios: String,
    pub cpu: String,
    pub tcc_c: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyInfo {
    pub id: String,
    pub referenced: bool,
    pub label: String,
    /// The reference's provenance, e.g. "duo-seed 20/11/2026".
    pub reference: Option<String>,
    /// On battery nothing is judged (§3.3), so this block and the status, learning and drift
    /// keep describing the last AC key; this says the machine is on battery meanwhile.
    pub on_battery: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FirstEvidence {
    pub at: Stamp,
    /// `d/M/yyyy HH:mm`.
    pub label: String,
    /// "E1", "E2" or "check" (FAN and FAULT use their own classes).
    pub class: String,
    pub summary: String,
    pub record_id: Option<String>,
    pub key_id: String,
    pub top: Vec<String>,
}

/// "earlier sighting d/M/yyyy": a lapsed or check-cleared case within 60 days.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sighting {
    pub case_id: String,
    pub day: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CleanedMark {
    pub at: Stamp,
    /// She gave a past date without a time.
    pub date_only: bool,
    pub label: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckPurpose {
    Confirm,
    Before,
    Verify,
    #[default]
    #[serde(other)]
    Reference,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckVerdict {
    Like,
    Borderline,
    Worse,
    #[default]
    #[serde(other)]
    Inconclusive,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostClean {
    Restored,
    Improved,
    NotImproved,
    VerifiedByBuilds,
    Unverified,
    #[default]
    #[serde(other)]
    Unknown,
}

/// A check run while the case was open.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaseCheck {
    pub at: Stamp,
    pub dir: String,
    pub purpose: CheckPurpose,
    pub verdict: Option<CheckVerdict>,
    pub t_ss: Option<f64>,
    pub duty: Option<f64>,
    pub fan: Option<f64>,
    /// The key it ran under: its numbers only stand for that key's references.
    pub key_id: String,
    /// "Run anyway" on a key mismatch: not comparable with the clean reference.
    pub anyway: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Case {
    pub id: String,
    pub opened: Stamp,
    pub first_evidence: Option<FirstEvidence>,
    pub earlier_sighting: Option<Sighting>,
    pub suspected_at: Option<Stamp>,
    pub warning_since: Option<Stamp>,
    /// The route to WARNING (§8): "a", "b", "c" or "d".
    pub route: Option<String>,
    pub evidence_days: Vec<String>,
    pub e2_days: Vec<String>,
    pub qualifying_days: Vec<String>,
    pub checks: Vec<CaseCheck>,
    pub cleaned: Vec<CleanedMark>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RBefore {
    /// "check", "plateaus_7d" or "case".
    pub source: String,
    pub t_ss: Option<f64>,
    pub duty: Option<f64>,
    pub fan: Option<f64>,
    pub e2_per_day: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PassiveVerification {
    pub voting: u32,
    pub hot: u32,
    pub clean: u32,
    pub e2_days: u32,
    /// Day keys.
    pub from: String,
    pub deadline: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Verification {
    pub cleaned_at: Stamp,
    pub date_only: bool,
    pub label: String,
    pub r_before: Option<RBefore>,
    pub passive: Option<PassiveVerification>,
    pub verdict: Option<PostClean>,
    pub check_dir: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckPhase {
    #[default]
    Gating,
    Idle,
    Load,
    Recovery,
    Done,
    Refused,
    #[serde(other)]
    Aborted,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Gate {
    pub id: String,
    pub ok: bool,
    pub text: String,
}

/// The running check, written at phase changes only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckState {
    pub purpose: CheckPurpose,
    pub state: CheckPhase,
    pub started_at: Stamp,
    pub phase_started_at: Stamp,
    pub phase_s: u32,
    pub anyway: bool,
    /// Relative to the store, e.g. "checks/2026-12-05_1102".
    pub dir: String,
    pub gates: Vec<Gate>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LastCheck {
    pub at: Stamp,
    pub dir: String,
    pub purpose: CheckPurpose,
    pub verdict: CheckVerdict,
    pub post_clean: Option<PostClean>,
    pub t_ss: Option<f64>,
    pub duty: Option<f64>,
    pub fan: Option<f64>,
    pub label: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanStatus {
    Slow,
    Stopped,
    #[default]
    #[serde(other)]
    Ok,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FanState {
    pub status: FanStatus,
    /// The R_fan the fan rules use.
    pub r_fan: Option<u32>,
    pub last_fan: Option<u32>,
    pub first_evidence: Option<FirstEvidence>,
    /// "Accept fan as normal": `r_fan` holds the accepted speed and overrides the references.
    pub accepted: bool,
    /// "Fan fixed": the next valid check sets R_fan.
    pub relearn: bool,
    /// `r_fan` was re-learned by the check after "Fan fixed" and overrides the references, as an
    /// accepted speed does: the learned build R_fan is the old fan's.
    pub relearned: bool,
    /// The last "Fan fixed" or "Accept fan as normal": slow builds before it no longer count.
    pub reset_at: Option<Stamp>,
    /// The open FAN came only from builds whose fan was slow to ramp up (`fan_low`) while its
    /// settled speed was normal. Accepting it then accepts the slow ramp, not a slower R_fan.
    pub ramp: bool,
    /// "Accept fan as normal" on such a FAN: `fan_low` no longer flags a build; R_fan is kept.
    pub ramp_accepted: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultStatus {
    Fault,
    #[default]
    #[serde(other)]
    Ok,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FaultState {
    pub status: FaultStatus,
    pub at: Option<Stamp>,
    pub t_max: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notice {
    pub id: String,
    pub since: Stamp,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snooze {
    pub suspicion_actions_until: Option<Stamp>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Learning {
    pub voting: u32,
    pub needed: u32,
    pub days: u32,
    pub needed_days: u32,
    pub light_windows: u32,
    pub light_needed: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlateauVerdict {
    Hot,
    Clean,
    #[default]
    #[serde(other)]
    Neutral,
}

impl PlateauVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlateauVerdict::Hot => "hot",
            PlateauVerdict::Clean => "clean",
            PlateauVerdict::Neutral => "neutral",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LastPlateau {
    pub label: String,
    #[serde(deserialize_with = "lenient::number")]
    pub t_ss: f64,
    /// A fraction: 0.051 is 5.1 %.
    #[serde(deserialize_with = "lenient::number")]
    pub duty: f64,
    pub fan: Option<u32>,
    pub verdict: PlateauVerdict,
    pub record_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Counts {
    pub days: u32,
    pub qualifying_days: u32,
    pub evidence_days: u32,
    pub voting_plateaus: u32,
    pub hot_plateaus: u32,
    pub light_windows: u32,
    pub hot_windows: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UnjudgedCounts {
    pub too_short: u32,
    pub unsettled: u32,
    pub dips: u32,
    pub below_pl1: u32,
    pub power_delivery: u32,
    pub slope: u32,
    pub no_reference: u32,
    /// Every `void:<cause>` together.
    pub void: u32,
}

impl UnjudgedCounts {
    /// The counts by reason, in a fixed order.
    pub fn entries(&self) -> [(&'static str, u32); 8] {
        [
            ("too_short", self.too_short),
            ("unsettled", self.unsettled),
            ("dips", self.dips),
            ("below_pl1", self.below_pl1),
            ("power_delivery", self.power_delivery),
            ("slope", self.slope),
            ("no_reference", self.no_reference),
            ("void", self.void),
        ]
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Drift {
    pub days: u32,
    #[serde(deserialize_with = "lenient::number")]
    pub t_c_per_week: f64,
    #[serde(deserialize_with = "lenient::number")]
    pub duty_pp_per_week: f64,
}

/// The bar pill (§11.4). MinkaShell reads this block as is; `show` needs a label.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pill {
    pub show: bool,
    /// "dust_warning", "dust_not_improved", "fan" or "fault".
    pub kind: String,
    pub tone: Tone,
    pub label: String,
    /// The window page a click opens.
    pub page: String,
}

/// MinkaMon's triangle and header badge (§11.4, §15).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mon {
    pub show: bool,
    pub kind: String,
    pub tone: Tone,
    pub triangle: bool,
    pub pulse: bool,
    pub badge: String,
    pub badge_short: String,
    /// A fixed vocabulary MinkaMon maps itself: "check" and "later".
    pub actions: Vec<String>,
    pub page: String,
}

// ---------------------------------------------------------------------------------------------
// records.jsonl

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    // Boxed: a plateau record is several times the size of the others.
    Plateau(Box<PlateauRecord>),
    Light(LightRecord),
    Unjudged(UnjudgedRecord),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Early {
    pub t_first_throttle: Option<f64>,
    pub duty_early: Option<f64>,
    pub t_fan6500: Option<f64>,
    /// The 95th percentile of the busy-CPU frequency in the first 10 s, MHz: generic mode learns
    /// `f_allcore_max` from it (§6.3).
    pub f_p95_first10: Option<f64>,
    /// The fan at the settle point, rpm: what a `fan_low` flag saw when fan_med ramped later.
    pub fan_settle: Option<f64>,
}

/// A voting or judged plateau episode (§5.1). Fractions, not percentages, throughout.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlateauRecord {
    pub id: String,
    pub rules_version: u32,
    pub onset: Stamp,
    pub end: Stamp,
    pub key_id: String,
    pub bios: String,
    pub cpu: String,
    #[serde(deserialize_with = "lenient::number")]
    pub duration_s: f64,
    #[serde(deserialize_with = "lenient::number")]
    pub measurable_s: f64,
    pub blocks: u32,
    pub valid_blocks: u32,
    /// Mean busy fraction over the measurable seconds of the valid blocks (for `power_capped`).
    pub busy: Option<f64>,
    pub t_ss: Option<f64>,
    pub t_p90: Option<f64>,
    pub duty: Option<f64>,
    pub cold_frac: Option<f64>,
    pub prochot_frac: Option<f64>,
    pub fan_med: Option<f64>,
    pub f_med: Option<f64>,
    pub core_spread: Option<f64>,
    #[serde(deserialize_with = "lenient::numbers")]
    pub core_throttle_share: Vec<f64>,
    pub deficit_frac: Option<f64>,
    pub reconcile_share: Option<f64>,
    pub igpu_act_med: Option<f64>,
    /// [[comm, share] × 3].
    #[serde(deserialize_with = "lenient::shares")]
    pub top: Vec<(String, f64)>,
    pub early: Early,
    pub flags: Vec<String>,
    pub verdict: PlateauVerdict,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LightVerdict {
    Hot,
    /// Throttled while cool: charger or EC, never dust.
    Cold,
    #[default]
    #[serde(other)]
    Neutral,
}

/// A light-load window (§5.2). Only hot and cold-throttle windows are written.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LightRecord {
    pub id: String,
    pub rules_version: u32,
    pub start: Stamp,
    pub key_id: String,
    /// Measured seconds the window covered (about 60): duty_hot = hot_ms / seconds.
    #[serde(deserialize_with = "lenient::number")]
    pub seconds: f64,
    #[serde(deserialize_with = "lenient::number")]
    pub busy: f64,
    #[serde(deserialize_with = "lenient::number")]
    pub threads: f64,
    pub t_p90: Option<f64>,
    pub t_max: Option<f64>,
    pub fan_p90: Option<f64>,
    pub hot_ms: u64,
    pub cold_ms: u64,
    pub reconcile_share: Option<f64>,
    /// Fraction of the window with a charger deficit.
    #[serde(deserialize_with = "lenient::number")]
    pub deficit: f64,
    #[serde(deserialize_with = "lenient::shares")]
    pub top: Vec<(String, f64)>,
    pub verdict: LightVerdict,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UnjudgedRecord {
    pub rules_version: u32,
    pub onset: Stamp,
    #[serde(deserialize_with = "lenient::number")]
    pub duration_s: f64,
    /// `too_short`, `unsettled`, `dips`, `below_pl1`, `power_delivery`, `slope`, `no_reference`
    /// or `void:<cause>`.
    pub reason: String,
    pub key_id: String,
    /// As on plateau records: a clean machine with no known f_allcore_max produces only
    /// `below_pl1` episodes until it is learned, so they must teach it too.
    pub f_p95_first10: Option<f64>,
}

// ---------------------------------------------------------------------------------------------
// daily.jsonl

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bin {
    #[serde(deserialize_with = "lenient::number")]
    pub min: f64,
    pub thr_ms: u64,
    pub hot_ms: u64,
    pub t_med: Option<f64>,
    pub fan_med: Option<f64>,
}

/// Busy bins: <5, 5-30, 30-60, 60-85 and ≥85 %.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bins {
    pub lt5: Bin,
    #[serde(rename = "5_30")]
    pub b5_30: Bin,
    #[serde(rename = "30_60")]
    pub b30_60: Bin,
    #[serde(rename = "60_85")]
    pub b60_85: Bin,
    pub ge85: Bin,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DailyRecord {
    pub day: String,
    pub rules_version: u32,
    pub bins: Bins,
    pub voting: u32,
    pub hot: u32,
    pub clean: u32,
    pub light_q: u32,
    pub light_hot: u32,
    pub qualifying: bool,
    pub e1: bool,
    pub e2: bool,
    pub unjudged: BTreeMap<String, u32>,
    /// Cold throttle time in qualifying light windows, ms.
    pub light_cold_ms: u64,
    /// Qualifying light windows per config key (voting or learning), so the light reference can
    /// be re-learned per key from the last 7 days (§9.2).
    pub light_keys: BTreeMap<String, u32>,
}

// ---------------------------------------------------------------------------------------------
// cases.jsonl

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseOutcome {
    VerifiedClean,
    VerifiedByBuilds,
    Improved,
    Lapsed,
    ClearedByCheck,
    Dismissed,
    /// Closed at start: it rested on light-load evidence (E2) alone, which a build with it off
    /// (since 21/11/2026) does not count. Not an earlier sighting.
    Withdrawn,
    #[default]
    #[serde(other)]
    Unverified,
}

/// A closed case: one interval of the dust history.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaseRecord {
    pub id: String,
    pub rules_version: u32,
    pub opened: Stamp,
    pub first_evidence: Option<FirstEvidence>,
    pub closed: Stamp,
    pub outcome: CaseOutcome,
    pub cleaned: Vec<CleanedMark>,
    pub check_dirs: Vec<String>,
    pub summary: String,
}

// ---------------------------------------------------------------------------------------------
// references.json

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct References {
    pub schema: u32,
    pub keys: BTreeMap<String, KeyRefs>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyRefs {
    pub active: Option<CheckRef>,
    pub best: Option<CheckRef>,
    pub passive: Option<PassiveRef>,
    pub light: Option<LightRef>,
    /// The last verified R_active: where R_active falls back to when an accepted one is dropped
    /// at the next Cleaned (§6.2c). None means the seed (or nothing, in generic mode).
    pub verified: Option<CheckRef>,
    /// Own reference checks inside the current known-clean window (§6.2a), at most two.
    pub clean_checks: Vec<CheckRef>,
}

/// A check-shaped reference (R_active, R_best). Post-load values may be provisional.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckRef {
    pub t: Option<f64>,
    pub duty: Option<f64>,
    pub fan: Option<f64>,
    pub f: Option<f64>,
    pub work: Option<f64>,
    pub djc: Option<f64>,
    pub ths: Option<f64>,
    pub floor: Option<f64>,
    pub t_cool: Option<f64>,
    /// Median T over the idle phase: the room line of the diagnosis compares it.
    pub t_idle: Option<f64>,
    /// "seed", "check", "restored" or "accepted".
    pub source: String,
    /// Provenance with full dates, e.g. "duo-seed 20/11/2026" or "accepted 3/1/2027".
    pub label: String,
    pub bios: String,
    pub cpu: String,
    pub generator: Option<u32>,
    /// Names of the fields still provisional, e.g. ["ths", "djc", "floor"].
    pub provisional: Vec<String>,
    pub accepted: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassiveState {
    Learned,
    Suspect,
    #[default]
    #[serde(other)]
    Learning,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PassiveRef {
    pub state: PassiveState,
    pub n: u32,
    pub days: u32,
    pub t: Option<f64>,
    pub duty: Option<f64>,
    pub fan: Option<f64>,
    pub accepted: bool,
    /// Learned inside a known-clean window.
    pub verified: bool,
    /// Provenance with full dates, e.g. "learned 21/11/2026-20/12/2026".
    pub label: String,
    /// Re-learning after a clean: `t`/`duty`/`fan` stay in use until the new values are learned.
    pub relearning: bool,
    /// Caps on re-learned values: the previous verified R_passive + 1.5 °C and + 1 pp (§6.2).
    pub cap_t: Option<f64>,
    pub cap_duty: Option<f64>,
    /// Day key on which the values first came out suspect.
    pub suspect_since: Option<String>,
    /// The voting episodes learned from (bounded).
    pub samples: Vec<PassiveSample>,
    /// Re-learned while a clean is being verified: not used until the case closes, and dropped
    /// if the clean comes out Not improved, so a failed clean cannot raise the reference.
    pub held: Option<Box<PassiveRef>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PassiveSample {
    pub day: String,
    #[serde(deserialize_with = "lenient::number")]
    pub t: f64,
    #[serde(deserialize_with = "lenient::number")]
    pub duty: f64,
    pub fan: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LightRef {
    pub n: u32,
    pub p90: Option<f64>,
    pub accepted: bool,
    pub verified: bool,
    /// The learned p90 was above the 2 % cap.
    pub capped: bool,
    /// Re-learning after a clean: `p90` stays in use until `n` reaches 300 again.
    pub relearning: bool,
    pub label: String,
    /// The non-zero duty_hot values among the `n` windows; the rest were 0.
    #[serde(deserialize_with = "lenient::numbers")]
    pub values: Vec<f64>,
    /// Re-learned while a clean is being verified, as `PassiveRef::held`.
    pub held: Option<Box<LightRef>>,
}

// ---------------------------------------------------------------------------------------------
// events.jsonl

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Setup,
    FirstEvidence,
    Suspicion,
    Warning,
    CheckStart,
    CheckResult,
    Cleaned,
    Verification,
    NotImproved,
    Lapse,
    ClearedByCheck,
    NotDust,
    Snooze,
    Fan,
    FanFixed,
    FanAccepted,
    Fault,
    FaultAck,
    Reference,
    ConfigChange,
    BiosChange,
    /// Light-load evidence is off, and a case (or a Not improved) that a build with it on left
    /// behind was decided again at start.
    Withdrawn,
    #[default]
    #[serde(other)]
    Notice,
}

/// One line of the human history: append-only, fsynced, never pruned.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    pub seq: u64,
    pub at: Stamp,
    #[serde(rename = "type")]
    pub kind: EventKind,
    pub case: Option<String>,
    pub text: String,
    pub data: Value,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The §11.3 example, with its "…" placeholders filled in.
    pub(crate) const WARNING_EXAMPLE: &str = r#"{
      "schema": 1, "rules_version": 1, "version": "0.1.0",
      "updated_at": {"utc": "2026-12-05T10:31:02Z", "local": "2026-12-05T10:31:02+00:00"},
      "started_at": {"utc": "2026-12-03T09:12:40Z", "local": "2026-12-03T09:12:40+00:00"},
      "pid": 1432, "boot_id": "6f0c",
      "chassis": {"profile": "duo", "setting": "auto", "board": "UX482EG", "product": "ZenBook UX482EG_UX482EG",
                  "bios": "UX482EG.311", "cpu": "11th Gen Intel(R) Core(TM) i7-1165G7", "tcc_c": 92},
      "key": {"id": "ac|performance|performance|28|35|8|0|100|open", "referenced": true,
              "label": "AC · performance · PL1 28 W · PL2 35 W · TCC 92 °C · lid open", "reference": "duo-seed 20/11/2026"},
      "classes": ["E1", "E2", "check"],
      "status": "warning", "setup_needed": false, "known_clean_until": null,
      "case": {
        "id": "c-20261203-1405", "opened": {"utc": "2026-12-03T14:05:10Z", "local": "2026-12-03T14:05:10+00:00"},
        "first_evidence": {"at": {"utc": "2026-12-03T14:05:10Z", "local": "2026-12-03T14:05:10+00:00"}, "label": "3/12/2026 14:05", "class": "E1",
          "summary": "build 90.4 °C, throttled 6.8 % (clean 86 °C, 0.9 %), mostly rustc",
          "record_id": "p-20261203-1405", "key_id": "ac|performance|performance|28|35|8|0|100|open", "top": ["rustc", "cargo", "floorp"]},
        "earlier_sighting": null,
        "suspected_at": {"utc": "2026-12-04T18:00:00Z", "local": "2026-12-04T18:00:00+00:00"},
        "warning_since": {"utc": "2026-12-05T10:31:02Z", "local": "2026-12-05T10:31:02+00:00"}, "route": "b",
        "evidence_days": ["2026-12-03", "2026-12-04", "2026-12-05"], "e2_days": ["2026-12-04"],
        "qualifying_days": ["2026-12-01"], "checks": [], "cleaned": []
      },
      "verification": null, "check": null, "last_check": null,
      "fan": {"status": "ok", "r_fan": 6800, "last_fan": 6800, "first_evidence": null},
      "fault": {"status": "ok", "at": null, "t_max": null},
      "notices": [],
      "snooze": {"suspicion_actions_until": null},
      "learning": {"voting": 3, "needed": 8, "days": 2, "needed_days": 3, "light_windows": 412, "light_needed": 300},
      "last_plateau": {"label": "5/12/2026 10:22", "t_ss": 89.6, "duty": 0.051, "fan": 6900, "verdict": "hot", "record_id": "p-20261205-1013"},
      "counts": {"days": 21, "qualifying_days": 6, "evidence_days": 4, "voting_plateaus": 9, "hot_plateaus": 5,
                 "light_windows": 2310, "hot_windows": 14},
      "unjudged_30d": {"too_short": 31, "unsettled": 2, "dips": 6, "below_pl1": 0, "power_delivery": 0, "slope": 1, "no_reference": 0, "void": 3},
      "drift": {"days": 30, "t_c_per_week": 0.4, "duty_pp_per_week": 0.6},
      "pill": {"show": true, "kind": "dust_warning", "tone": "amber", "label": "Needs a clean since 3/12/2026", "page": "status"},
      "mon":  {"show": true, "kind": "dust_warning", "tone": "amber", "triangle": true, "pulse": true,
               "badge": "NEEDS A CLEAN · FIRST 3/12/2026 14:05", "badge_short": "NEEDS A CLEAN", "actions": [], "page": "status"}
    }"#;

    #[test]
    fn the_spec_example_round_trips() {
        let s: State = serde_json::from_str(WARNING_EXAMPLE).unwrap();
        assert_eq!(s.status, Status::Warning);
        assert_eq!(s.chassis.profile, Chassis::Duo);
        assert_eq!(s.chassis.tcc_c, Some(92.0));
        assert_eq!(s.pill.tone, Tone::Amber);
        let case = s.case.as_ref().unwrap();
        assert_eq!(case.first_evidence.as_ref().unwrap().top.len(), 3);
        assert_eq!(
            s.last_plateau.as_ref().unwrap().verdict,
            PlateauVerdict::Hot
        );
        let again: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(again, s);
    }

    #[test]
    fn readers_are_lenient() {
        let s: State = serde_json::from_str(
            r#"{"status": "on_fire", "pill": {"tone": "ultraviolet", "show": true}, "future_field": [1, 2],
                "chassis": {"profile": "tower", "setting": "laptop"}, "fan": {"status": "wobbly"}}"#,
        )
        .unwrap();
        assert_eq!(s.status, Status::Unknown);
        assert_eq!(s.pill.tone, Tone::Neutral);
        assert!(s.pill.show);
        assert_eq!(s.chassis.profile, Chassis::Generic);
        assert_eq!(s.chassis.setting, ChassisSetting::Auto);
        assert_eq!(s.fan.status, FanStatus::Ok);
        assert_eq!(s.schema, 0, "a missing schema reads as 0, so it migrates");
        let s: State = serde_json::from_str("{}").unwrap();
        assert_eq!(s, State::default());
    }

    #[test]
    fn the_throttle_counter_outlives_e2_in_the_classes() {
        let with = |classes: &[&str], counter: Option<bool>| State {
            classes: classes.iter().map(|c| (*c).to_owned()).collect(),
            throttle_counter: counter,
            ..State::default()
        };
        // Light-load evidence off (21/11/2026): no E2 in the classes, the counter still there.
        assert!(with(&["E1", "check"], Some(true)).has_throttle_counter());
        assert!(!with(&["E1", "check"], Some(false)).has_throttle_counter());
        // A state from before the field: E2 said it, and no classes at all is the Duo.
        assert!(with(&["E1", "E2", "check"], None).has_throttle_counter());
        assert!(!with(&["E1", "check"], None).has_throttle_counter());
        assert!(with(&[], None).has_throttle_counter());
    }

    #[test]
    fn records_are_tagged_by_type() {
        let p = Record::Plateau(Box::new(PlateauRecord {
            id: "p-20261203-1405".into(),
            rules_version: 1,
            top: vec![("rustc".into(), 0.71), ("cargo".into(), 0.1)],
            verdict: PlateauVerdict::Hot,
            ..PlateauRecord::default()
        }));
        let line = serde_json::to_string(&p).unwrap();
        assert!(
            line.starts_with(r#"{"type":"plateau","id":"p-20261203-1405""#),
            "{line}"
        );
        assert!(
            line.contains(r#""top":[["rustc",0.71],["cargo",0.1]]"#),
            "{line}"
        );
        assert_eq!(serde_json::from_str::<Record>(&line).unwrap(), p);

        let u: Record =
            serde_json::from_str(r#"{"type":"unjudged","reason":"void:config","extra":1}"#)
                .unwrap();
        assert!(
            matches!(u, Record::Unjudged(UnjudgedRecord { ref reason, .. }) if reason == "void:config")
        );

        let d = DailyRecord {
            day: "2026-12-03".into(),
            ..DailyRecord::default()
        };
        let line = serde_json::to_string(&d).unwrap();
        assert!(
            line.contains(r#""5_30":{"#) && line.contains(r#""ge85":{"#),
            "{line}"
        );

        let e: Event =
            serde_json::from_str(r#"{"seq":17,"type":"first_evidence","text":"First evidence"}"#)
                .unwrap();
        assert_eq!((e.seq, e.kind), (17, EventKind::FirstEvidence));
        let e: Event = serde_json::from_str(r#"{"type":"something_new"}"#).unwrap();
        assert_eq!(e.kind, EventKind::Notice);
    }

    #[test]
    fn a_nan_written_as_null_reads_back() {
        let mut s = State::new();
        s.last_plateau = Some(LastPlateau {
            t_ss: f64::NAN,
            duty: 0.05,
            ..LastPlateau::default()
        });
        s.drift = Some(Drift {
            days: 3,
            t_c_per_week: f64::INFINITY,
            duty_pp_per_week: f64::NAN,
        });
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains(r#""t_ss":null"#), "{text}");
        let back: State = serde_json::from_str(&text).unwrap();
        let p = back.last_plateau.unwrap();
        assert!(p.t_ss.is_nan());
        assert_eq!(p.duty, 0.05);
        assert!(back.drift.unwrap().t_c_per_week.is_nan());

        let plateau = Record::Plateau(Box::new(PlateauRecord {
            duration_s: f64::NAN,
            core_throttle_share: vec![0.25, f64::NAN],
            top: vec![("rustc".into(), f64::NAN)],
            ..PlateauRecord::default()
        }));
        let light = Record::Light(LightRecord {
            busy: f64::NAN,
            deficit: f64::NAN,
            top: vec![("floorp".into(), f64::NAN)],
            ..LightRecord::default()
        });
        let unjudged = Record::Unjudged(UnjudgedRecord {
            duration_s: f64::NAN,
            ..UnjudgedRecord::default()
        });
        for record in [plateau, light, unjudged] {
            let line = serde_json::to_string(&record).unwrap();
            let back: Record =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
            match back {
                Record::Plateau(p) => {
                    assert!(p.duration_s.is_nan());
                    assert_eq!(p.core_throttle_share[0], 0.25);
                    assert!(p.core_throttle_share[1].is_nan());
                    assert!(p.top[0].0 == "rustc" && p.top[0].1.is_nan());
                }
                Record::Light(l) => {
                    assert!(l.busy.is_nan() && l.deficit.is_nan() && l.top[0].1.is_nan())
                }
                Record::Unjudged(u) => assert!(u.duration_s.is_nan()),
            }
        }

        let mut day = DailyRecord::default();
        day.bins.ge85.min = f64::NAN;
        let line = serde_json::to_string(&day).unwrap();
        let back: DailyRecord = serde_json::from_str(&line).unwrap();
        assert!(back.bins.ge85.min.is_nan());
        // A missing field still takes the default, not NaN.
        let back: DailyRecord = serde_json::from_str(r#"{"bins":{"ge85":{}}}"#).unwrap();
        assert_eq!(back.bins.ge85.min, 0.0);
    }
}
