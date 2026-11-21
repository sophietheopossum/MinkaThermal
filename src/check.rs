//! The cooling check (§10): a five-minute measurement she starts herself. The service owns it,
//! so it outlives the window, and drives it through `CheckRun` once a row and four times a
//! second for `x86_pkg_temp`.
//!
//! It waits for the gates (power, settings, dGPU, an idle and settled machine), then runs 30 s
//! idle, 180 s of load from a `minka-thermal burn` child and 90 s of recovery. At the end it
//! measures the steady and matched windows, decides whether the run counts, and judges it against
//! the references (§10.6, §10.8); the engine applies the result.
//!
//! The arithmetic (`gates`, `measure`, `invalid`, `judge`) is pure and tested with synthetic rows.
//! The I/O is the burn child, an nvidia-smi query, /proc scans and the check directory.

use crate::burn::GENERATOR_VERSION;
use crate::clock::Moment;
use crate::episodes::{COLD_BELOW_TCC, NO_COUNTER_BELOW_TCC, median, theil_sen};
use crate::model::{
    CheckPhase, CheckPurpose, CheckRef, CheckState, CheckVerdict, Gate, PostClean, RBefore,
    References, Status,
};
use crate::note;
use crate::present::check as text;
use crate::profile::{Chassis, ConfigKey, DUO};
use crate::references::KeyView;
use crate::rules::{
    ALARM_READING_S, CheckMetrics, CheckOutcome, FAN_SLOW_MARGIN, KeyCtx, judge_check,
    judge_post_clean,
};
use crate::sampler::{Gap, ProcSnapshot, Row, proc_snapshot};
use crate::sensors::{BatStatus, Context, XeRead};
use crate::service::{CheckRun, CheckStep};
use crate::store;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

/// The gates wait this long after Start, then the check is refused (§10.2).
pub const GATE_WAIT_S: f64 = 300.0;
pub const IDLE_S: f64 = 30.0;
pub const LOAD_S: f64 = 180.0;
pub const RECOVERY_S: f64 = 90.0;
/// Settled: B10 under 10 % for 30 s, the fan down to its lowest EC stage, and a 60-s Theil-Sen
/// slope of T no steeper than −0.02 °C/s, meaning the heatsink has stopped cooling. There is no
/// absolute temperature gate, so a summer room cannot block a check.
pub const SETTLE_B10: f64 = 0.10;
pub const SETTLE_QUIET_S: f64 = 30.0;
pub const SETTLE_FAN: u32 = 5000;
pub const SLOPE_S: f64 = 60.0;
pub const SLOPE_MIN: f64 = -0.02;
/// Rows land on whole seconds, so 60 s of history can come out a little short.
const SLOPE_SLACK_S: f64 = 2.0;
pub const BATTERY_START: u8 = 50;
pub const BATTERY_ABORT: u8 = 30;
/// nvidia-smi `utilization.gpu` must read under this.
pub const DGPU_BUSY_PCT: f64 = 10.0;
const DGPU_TIMEOUT_S: f64 = 10.0;
/// A busy dGPU is asked again this often while the gates wait.
const DGPU_RETRY_S: f64 = 30.0;
/// The busiest processes shown while the gates wait: one /proc scan this often.
const BUSIEST_EVERY_S: f64 = 5.0;
/// state.json takes a gate change at most this often while the check waits. A settle line the
/// machine hovers on (fan near 5000, the slope near −0.02) could otherwise rewrite it every
/// second for 5 minutes; the window sees every change at once through the live events.
const GATES_WRITE_S: f64 = 30.0;
const BUSIEST_N: usize = 3;
/// Aborts (§10.5): T at 99 °C for 3 s, or the fan at 0 rpm for 10 s while T is above 80 °C.
pub const ABORT_T: f64 = 99.0;
pub const ABORT_T_S: f64 = 3.0;
pub const FAN_ZERO_T: f64 = 80.0;
pub const FAN_ZERO_S: f64 = 10.0;
/// Windows in seconds from the load start: steady for own references, matched for the seed,
/// whose own window was 74.3-118.9 s, and the early turbo transient.
pub const STEADY: (f64, f64) = (75.0, 178.0);
pub const MATCHED: (f64, f64) = (75.0, 118.0);
pub const EARLY: (f64, f64) = (15.0, 60.0);
/// Post-load windows in seconds from the load end: the heatsink and the floor.
pub const HEATSINK: (f64, f64) = (2.0, 6.0);
pub const FLOOR: (f64, f64) = (45.0, 60.0);
/// T_end is the median over the last seconds of the load.
const T_END_S: f64 = 2.0;
/// Validity (§10.5).
pub const BUSY_MIN: f64 = 0.95;
pub const PROCHOT_MAX: f64 = 0.10;
pub const COLD_PROCHOT_MAX: f64 = 0.02;
pub const FOREIGN_MAX: f64 = 0.05;
/// t_fan5000: the fan back on its lowest EC stage.
const FAN_LOW_STAGE: u32 = 5000;
/// A work rate needs at least this much burn output inside its window.
const WORK_MIN_S: f64 = 10.0;
/// Diagnosis margins (§10.8).
const DIAG_DJC: f64 = 5.0;
const DIAG_THS_NEAR: f64 = 3.0;
const DIAG_THS: f64 = 5.0;
const DIAG_IDLE: f64 = 3.0;
const DIAG_IDLE_SHARE: f64 = 0.7;
const EPS: f64 = 1e-6;

/// What a check is for, from the state when it starts (§10.1).
pub fn purpose_for(status: Status) -> CheckPurpose {
    match status {
        Status::Watch | Status::Suspicion => CheckPurpose::Confirm,
        Status::Warning => CheckPurpose::Before,
        Status::Verifying => CheckPurpose::Verify,
        _ => CheckPurpose::Reference,
    }
}

/// A command line the check runs: the load child (`<the service's exe> burn --threads N`) and the
/// dGPU query. Tests run something harmless instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// The file the running process was started from, even after a rebuild or an upgrade has
/// replaced the path it was started by.
pub const SELF_EXE: &str = "/proc/self/exe";

impl Cmd {
    /// This service's own binary, run as the hidden `burn` verb. A path such as
    /// target/release/minka-thermal can hold a newer build than the one running, whose generator
    /// would not be the version the check records (§10.4); /proc/self/exe is always this one.
    pub fn burn(threads: usize) -> Cmd {
        Cmd {
            program: PathBuf::from(SELF_EXE),
            args: vec!["burn".into(), "--threads".into(), threads.to_string().into()],
        }
    }

    /// One `utilization.gpu` reading per GPU, a bare number per line.
    pub fn gpu_query(nvidia_smi: &Path) -> Cmd {
        Cmd {
            program: nvidia_smi.to_owned(),
            args: vec![
                "--query-gpu=utilization.gpu".into(),
                "--format=csv,noheader,nounits".into(),
            ],
        }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.args);
        c
    }
}

// ---------------------------------------------------------------------------------------------
// The reference key (§10.2).

/// What the check compares with, fixed when it is accepted.
#[derive(Clone, Debug, Default)]
pub struct Refs {
    pub refs: References,
    pub chassis: Chassis,
    pub board: String,
    /// R_before of the case being verified.
    pub before: Option<RBefore>,
    /// "Accept fan as normal" or a re-learned fan speed: it replaces R_fan.
    pub r_fan_override: Option<f64>,
}

/// How the settings compare with the reference's.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyMatch {
    /// The key has an R_active, whose provenance is given: the check is comparable.
    Matches(Option<String>),
    /// No key has a reference yet: this check records one.
    Unreferenced,
    /// Another key holds the reference; the list says what differs.
    Differs(Vec<String>),
}

impl Refs {
    pub fn view(&self, key_id: &str) -> KeyView {
        KeyView::of(&self.refs, key_id, self.chassis, &self.board)
    }

    /// Whether this key, once on AC, in the Performance profile and with the lid open (which have
    /// gates of their own, and which she can fix while the check waits), is a key with a
    /// reference.
    pub fn key_match(&self, key: &ConfigKey) -> KeyMatch {
        let k = normalised(key);
        if let Some(a) = self.view(&k.id()).active {
            return KeyMatch::Matches(Some(a.label).filter(|l| !l.is_empty()));
        }
        if self.chassis == Chassis::Duo {
            return KeyMatch::Differs(seed_diff(&k));
        }
        let closest = self
            .refs
            .keys
            .iter()
            .filter(|(_, r)| r.active.is_some())
            .filter_map(|(id, _)| ConfigKey::parse_id(id))
            .map(|r| key_diff(&k, &r))
            .min_by_key(Vec::len);
        match closest {
            Some(d) => KeyMatch::Differs(d),
            None => KeyMatch::Unreferenced,
        }
    }
}

/// The key as it will be once the AC, profile and lid gates pass. power-profiles-daemon moves EPP
/// with the profile, so a profile switch fixes both. A closed lid is never something to run
/// anyway with: closing it aborts the load (§10.5), so the check waits for it to open instead.
fn normalised(key: &ConfigKey) -> ConfigKey {
    let mut k = key.clone();
    k.ac = true;
    if k.lid_open == Some(false) {
        k.lid_open = Some(true);
    }
    if k.profile.as_deref().is_some_and(|p| p != "performance") {
        k.profile = Some("performance".to_owned());
        if k.epp.is_some() {
            k.epp = Some("performance".to_owned());
        }
    }
    k
}

fn watts(w: Option<u32>) -> String {
    w.map_or("unknown".to_owned(), |w| format!("{w} W"))
}

fn lid(open: Option<bool>) -> &'static str {
    match open {
        Some(true) => "open",
        Some(false) => "closed",
        None => "unknown",
    }
}

fn turbo(no_turbo: Option<bool>) -> &'static str {
    match no_turbo {
        Some(true) => "off",
        Some(false) => "on",
        None => "unknown",
    }
}

fn opt<T: ToString>(v: &Option<T>) -> String {
    v.as_ref().map_or("unknown".to_owned(), T::to_string)
}

/// What keeps a Duo key from being the seed's (§4), worded for the gate.
fn seed_diff(k: &ConfigKey) -> Vec<String> {
    let mut d = Vec::new();
    if k.profile.as_deref() != Some("performance") {
        d.push(text::differs(&format!("profile {}", opt(&k.profile)), "performance"));
    }
    if k.epp.as_deref() != Some("performance") {
        d.push(text::differs(&format!("EPP {}", opt(&k.epp)), "performance"));
    }
    if !k.pl1_w.is_some_and(|w| (26..=30).contains(&w)) {
        d.push(text::differs(&format!("PL1 {}", watts(k.pl1_w)), "26-30 W"));
    }
    if !k.pl2_w.is_some_and(|w| w >= 33) {
        d.push(text::differs(&format!("PL2 {}", watts(k.pl2_w)), "≥ 33 W"));
    }
    if k.tcc_offset != Some(8) {
        d.push(text::differs(&format!("TCC offset {}", opt(&k.tcc_offset)), "8"));
    }
    if k.no_turbo != Some(false) {
        d.push(text::differs(&format!("turbo {}", turbo(k.no_turbo)), "on"));
    }
    if k.max_perf_pct != Some(100) {
        d.push(text::differs(&format!("max perf {} %", opt(&k.max_perf_pct)), "100 %"));
    }
    if k.lid_open != Some(true) {
        d.push(text::differs(&format!("lid {}", lid(k.lid_open)), "open"));
    }
    d
}

/// What differs between a key and a referenced key, worded for the gate.
fn key_diff(k: &ConfigKey, r: &ConfigKey) -> Vec<String> {
    let mut d = Vec::new();
    if k.ac != r.ac {
        d.push(text::differs("AC", "battery"));
    }
    if k.profile != r.profile {
        d.push(text::differs(&format!("profile {}", opt(&k.profile)), &opt(&r.profile)));
    }
    if k.epp != r.epp {
        d.push(text::differs(&format!("EPP {}", opt(&k.epp)), &opt(&r.epp)));
    }
    if k.pl1_w != r.pl1_w {
        d.push(text::differs(&format!("PL1 {}", watts(k.pl1_w)), &watts(r.pl1_w)));
    }
    if k.pl2_w != r.pl2_w {
        d.push(text::differs(&format!("PL2 {}", watts(k.pl2_w)), &watts(r.pl2_w)));
    }
    if k.tcc_offset != r.tcc_offset {
        d.push(text::differs(&format!("TCC offset {}", opt(&k.tcc_offset)), &opt(&r.tcc_offset)));
    }
    if k.no_turbo != r.no_turbo {
        d.push(text::differs(&format!("turbo {}", turbo(k.no_turbo)), turbo(r.no_turbo)));
    }
    if k.max_perf_pct != r.max_perf_pct {
        d.push(text::differs(
            &format!("max perf {} %", opt(&k.max_perf_pct)),
            &format!("{} %", opt(&r.max_perf_pct)),
        ));
    }
    if k.lid_open != r.lid_open {
        d.push(text::differs(&format!("lid {}", lid(k.lid_open)), lid(r.lid_open)));
    }
    d
}

// ---------------------------------------------------------------------------------------------
// Gates (§10.2).

/// The dGPU as the gate sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dgpu {
    /// Not `active`: it is never woken to be asked.
    Suspended,
    /// Active, and there is no nvidia-smi to ask.
    NoTool,
    /// The query is running.
    Asking,
    /// nvidia-smi failed or timed out. A broken tool does not block the check.
    NoReading,
    /// `utilization.gpu`, percent.
    Busy(f64),
}

/// The settle readings at one row.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SettleNow {
    /// How long B10 has stayed under 10 %.
    pub quiet_s: f64,
    pub b10: Option<f64>,
    pub fan: Option<u32>,
    /// The 60-s Theil-Sen slope of T, °C/s, once 60 s of rows are in.
    pub slope: Option<f64>,
    /// Seconds of continuous rows behind the slope.
    pub covered_s: f64,
}

/// Rows since the last gap, for the settle gate. It starts from the sampler's ring, so a machine
/// that has been idle for a minute can start at once.
#[derive(Clone, Debug, Default)]
struct Settle {
    temps: VecDeque<(f64, f64)>,
    since: Option<f64>,
    quiet_since: Option<f64>,
    last: f64,
    b10: Option<f64>,
    fan: Option<u32>,
}

impl Settle {
    fn push(&mut self, row: &Row) {
        let end = row.at.mono_s();
        if row.gap.voids() || self.since.is_none() {
            self.temps.clear();
            self.quiet_since = None;
            self.since = Some(end);
        }
        match row.b10 {
            Some(b) if b < SETTLE_B10 => {
                self.quiet_since.get_or_insert(end - row.dt);
            }
            _ => self.quiet_since = None,
        }
        if let Some(t) = row.t {
            self.temps.push_back((end, t));
        }
        while self.temps.front().is_some_and(|&(s, _)| s < end - SLOPE_S) {
            self.temps.pop_front();
        }
        self.last = end;
        self.b10 = row.b10;
        self.fan = row.fan;
    }

    fn now(&self) -> SettleNow {
        let covered = self.since.map_or(0.0, |s| (self.last - s).min(SLOPE_S));
        let points: Vec<(f64, f64)> = self.temps.iter().copied().collect();
        SettleNow {
            quiet_s: self.quiet_since.map_or(0.0, |q| self.last - q),
            b10: self.b10,
            fan: self.fan,
            slope: (covered >= SLOPE_S - SLOPE_SLACK_S)
                .then(|| theil_sen(&points))
                .flatten(),
            covered_s: covered,
        }
    }
}

fn gate(id: &str, ok: bool, text: String) -> Gate {
    Gate {
        id: id.to_owned(),
        ok,
        text,
    }
}

/// Every gate at one row, in the order the Check page lists them.
pub fn gates(
    row: &Row,
    key: &ConfigKey,
    key_match: &KeyMatch,
    anyway: bool,
    dgpu: Dgpu,
    settle: &SettleNow,
    has_fan: bool,
) -> Vec<Gate> {
    let mut out = vec![gate("ac", row.ac == Some(true), text::ac(row.ac))];
    out.push(match (row.bat_status, row.bat_capacity) {
        (None, None) => gate("battery", true, text::no_battery()),
        (Some(BatStatus::Discharging), c) if row.ac != Some(true) => {
            gate("battery", false, text::battery_draining(c))
        }
        (Some(BatStatus::Discharging), _) => gate("battery", false, text::battery_discharging()),
        (_, Some(c)) if c < BATTERY_START => {
            gate("battery", false, text::battery_low(c, BATTERY_START))
        }
        (_, c) => gate("battery", true, text::battery(c)),
    });
    let profile = key.profile.as_deref();
    out.push(gate(
        "profile",
        profile.is_none_or(|p| p == "performance"),
        text::profile(profile),
    ));
    out.push(gate("lid", row.lid_open != Some(false), text::lid(row.lid_open)));
    out.push(match key_match {
        KeyMatch::Matches(label) => gate("key", true, text::key_matches(label.as_deref())),
        KeyMatch::Unreferenced => gate("key", true, text::key_unreferenced()),
        KeyMatch::Differs(d) if anyway => gate("key", true, text::key_anyway(d)),
        KeyMatch::Differs(d) => gate("key", false, text::key_differs(d)),
    });
    out.push(match dgpu {
        Dgpu::Suspended => gate("dgpu", true, text::dgpu_suspended()),
        Dgpu::NoTool => gate("dgpu", true, text::dgpu_no_tool()),
        Dgpu::Asking => gate("dgpu", false, text::dgpu_asking()),
        Dgpu::NoReading => gate("dgpu", true, text::dgpu_no_reading()),
        Dgpu::Busy(u) => {
            let ok = u < DGPU_BUSY_PCT;
            gate("dgpu", ok, text::dgpu_busy(u, ok))
        }
    });
    let mut failing = Vec::new();
    if settle.quiet_s < SETTLE_QUIET_S - EPS {
        if settle.b10.is_some_and(|b| b < SETTLE_B10) {
            failing.push(text::quiet(settle.quiet_s, SETTLE_QUIET_S));
        } else {
            failing.push(text::b10(settle.b10));
        }
    }
    if has_fan && let Some(f) = settle.fan.filter(|&f| f > SETTLE_FAN) {
        failing.push(text::fan(f, SETTLE_FAN));
    }
    match settle.slope {
        Some(s) if s < SLOPE_MIN => failing.push(text::slope(s, SLOPE_MIN)),
        Some(_) => {}
        None => failing.push(text::slope_measuring(settle.covered_s, SLOPE_S)),
    }
    out.push(if failing.is_empty() {
        gate("settle", true, text::settled())
    } else {
        gate("settle", false, failing.join(", "))
    });
    out
}

/// Clock ticks per second, for /proc CPU times.
fn clk_tck() -> f64 {
    // SAFETY: sysconf only reads a system constant.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as f64 } else { 100.0 }
}

/// The busiest processes between two /proc snapshots, as shares of the whole machine: what keeps
/// it from settling ("rustc 62 %, floorp 9 %").
pub fn busiest(
    before: &ProcSnapshot,
    after: &ProcSnapshot,
    dt_s: f64,
    nproc: usize,
    hz: f64,
) -> Vec<(String, f64)> {
    let capacity = dt_s * hz * nproc.max(1) as f64;
    if capacity <= 0.0 {
        return Vec::new();
    }
    let mut by_comm: HashMap<&str, u64> = HashMap::new();
    for (pid, (comm, ticks)) in &after.procs {
        let d = match before.procs.get(pid) {
            Some((_, prev)) => ticks.saturating_sub(*prev),
            // New since the last scan: its whole CPU time fell in between.
            None => *ticks,
        };
        if d > 0 {
            *by_comm.entry(comm).or_default() += d;
        }
    }
    let mut top: Vec<(String, f64)> = by_comm
        .into_iter()
        .map(|(c, d)| (c.to_owned(), (d as f64 / capacity).min(1.0)))
        .collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(BUSIEST_N);
    top
}

/// `nvidia-smi --query-gpu=utilization.gpu` output: the busiest GPU, percent.
pub fn parse_utilization(out: &str) -> Option<f64> {
    out.lines()
        .filter_map(|l| l.trim().trim_end_matches('%').trim().parse::<f64>().ok())
        .reduce(f64::max)
}

/// Queries killed at their timeout that had not exited yet. An nvidia-smi stuck in the driver
/// can take a while to die even from SIGKILL, and waiting for it would stall the service's only
/// thread; it is reaped here once it has gone.
static ORPHANS: std::sync::Mutex<Vec<Child>> = std::sync::Mutex::new(Vec::new());

/// Kills `child` and reaps it if it has already gone, else leaves it to `reap_orphans`.
fn abandon(mut child: Child) {
    let _ = child.kill();
    if !matches!(child.try_wait(), Ok(Some(_)))
        && let Ok(mut o) = ORPHANS.lock()
    {
        o.push(child);
    }
}

/// Reaps abandoned queries that have exited. The service calls it every row; it never blocks.
pub fn reap_orphans() {
    if let Ok(mut o) = ORPHANS.lock() {
        o.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
    }
}

/// The dGPU gate's one nvidia-smi query, run without blocking the loop and asked again only
/// while it reads busy.
#[derive(Debug, Default)]
struct GpuQuery {
    tool: Option<Cmd>,
    running: Option<(Child, f64)>,
    last: Option<(f64, Option<f64>)>,
}

impl GpuQuery {
    fn poll(&mut self, active: bool, now: f64) -> Dgpu {
        reap_orphans();
        if !active {
            self.stop();
            return Dgpu::Suspended;
        }
        let Some(tool) = self.tool.clone() else {
            return Dgpu::NoTool;
        };
        if let Some((child, started)) = &mut self.running {
            match child.try_wait() {
                Ok(Some(_)) => {
                    let mut out = String::new();
                    if let Some(mut so) = child.stdout.take() {
                        let _ = so.read_to_string(&mut out);
                    }
                    self.last = Some((now, parse_utilization(&out)));
                    self.running = None;
                }
                Ok(None) if now - *started >= DGPU_TIMEOUT_S => {
                    if let Some((child, _)) = self.running.take() {
                        abandon(child);
                    }
                    self.last = Some((now, None));
                }
                Ok(None) => {}
                Err(_) => {
                    self.last = Some((now, None));
                    self.running = None;
                }
            }
        }
        let ask = self.last.is_none_or(|(at, v)| {
            v.is_some_and(|u| u >= DGPU_BUSY_PCT) && now - at >= DGPU_RETRY_S
        });
        if ask && self.running.is_none() {
            let spawned = tool
                .command()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            match spawned {
                Ok(child) => self.running = Some((child, now)),
                Err(_) => self.last = Some((now, None)),
            }
        }
        match self.last {
            None => Dgpu::Asking,
            Some((_, None)) => Dgpu::NoReading,
            Some((_, Some(u))) => Dgpu::Busy(u),
        }
    }

    fn stop(&mut self) {
        if let Some((child, _)) = self.running.take() {
            abandon(child);
        }
        self.last = None;
    }
}

impl Drop for GpuQuery {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------------
// The burn child (§10.1).

/// The load generator: its own process group, killed as a group.
#[derive(Debug)]
struct Burn {
    child: Child,
    pid: i32,
    /// None once read to the end.
    out: Option<ChildStdout>,
    buf: Vec<u8>,
    killed: bool,
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Starts the load in a process group of its own with stdin null, stdout piped to the service
/// and stderr to the journal. It dies with the thread that started it (PR_SET_PDEATHSIG), and
/// SIGTERM and SIGINT, which the service keeps blocked for its signalfd, are unblocked again so
/// that a `systemctl stop` reaches it.
fn spawn_load(cmd: &Cmd) -> io::Result<Burn> {
    // SAFETY: getpid has no preconditions.
    let parent = unsafe { libc::getpid() };
    let mut c = cmd.command();
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0);
    // SAFETY: the closure runs in the forked child before exec and calls only async-signal-safe
    // functions (prctl, getppid, sigemptyset, sigaddset, sigprocmask) on data on its own stack.
    unsafe {
        c.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            // The service died between fork and prctl: nothing would ever kill this load.
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            Ok(())
        });
    }
    let mut child = c.spawn()?;
    let mut burn = Burn {
        pid: child.id() as i32,
        out: child.stdout.take(),
        child,
        buf: Vec::new(),
        killed: false,
    };
    if let Some(fd) = burn.fd()
        && let Err(e) = set_nonblocking(fd)
    {
        burn.kill();
        return Err(e);
    }
    Ok(burn)
}

impl Burn {
    fn fd(&self) -> Option<RawFd> {
        self.out.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Reads what the pipe holds, to the end after a hang-up, and returns the complete lines.
    fn read(&mut self) -> Vec<String> {
        let Some(out) = &mut self.out else {
            return Vec::new();
        };
        let mut chunk = [0u8; 4096];
        loop {
            match out.read(&mut chunk) {
                Ok(0) => {
                    self.out = None;
                    break;
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.out = None;
                    break;
                }
            }
        }
        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            lines.push(String::from_utf8_lossy(&line).trim().to_owned());
        }
        // A burn line is under 300 bytes; anything this long without a newline is not one.
        if self.buf.len() > 64 * 1024 {
            self.buf.clear();
        }
        lines
    }

    /// The child has exited on its own. It is only peeked at, not reaped: while it is an unreaped
    /// zombie its pid, and so its group id, cannot be reused, so `kill` can still signal whatever
    /// is left in the group and then reap it.
    fn exited(&mut self) -> bool {
        if self.killed {
            return false;
        }
        // SAFETY: an all-zero siginfo is valid; waitid writes at most one into it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waits on our own child without blocking or reaping it.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        // SAFETY: si_pid is set (or left zero) by a successful waitid.
        r == 0 && unsafe { info.si_pid() } != 0
    }

    /// SIGKILL to the whole group, then reap the child.
    fn kill(&mut self) {
        if self.killed {
            return;
        }
        self.killed = true;
        // SAFETY: the group this child leads (process_group(0)); its id stays reserved while the
        // child is unreaped or any member of the group lives.
        unsafe { libc::killpg(self.pid, libc::SIGKILL) };
        let _ = self.child.wait();
        self.out = None;
    }
}

impl Drop for Burn {
    fn drop(&mut self) {
        self.kill();
    }
}

/// One burn line: (its MONOTONIC s, total rounds over all threads, thread count).
pub fn parse_burn_line(line: &str) -> Option<(f64, u64, usize)> {
    let v: Value = serde_json::from_str(line).ok()?;
    let t = v.get("t")?.as_f64()?;
    let rounds = v.get("rounds")?.as_array()?;
    let total = rounds.iter().map(Value::as_u64).sum::<Option<u64>>()?;
    Some((t, total, rounds.len()))
}

/// (utime + stime ticks, nice) from a /proc/<pid>/stat.
fn proc_ticks_nice(path: &Path) -> Option<(u64, i64)> {
    let text = fs::read_to_string(path).ok()?;
    let close = text.rfind(')')?;
    let f: Vec<&str> = text.get(close + 1..)?.split_ascii_whitespace().collect();
    // After the comm, state is field 3: utime 14, stime 15, nice 19.
    let ticks = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    Some((ticks, f.get(16)?.parse().ok()?))
}

/// This process's nice, which the burn child inherits.
fn own_nice() -> Option<i64> {
    // SAFETY: errno is cleared first because -1 is a valid nice; both calls touch only this
    // thread's errno and the process's priority.
    unsafe {
        *libc::__errno_location() = 0;
        let n = libc::getpriority(libc::PRIO_PROCESS, 0);
        (n != -1 || *libc::__errno_location() == 0).then_some(n as i64)
    }
}

// ---------------------------------------------------------------------------------------------
// Measurement (§10.5).

/// One row of the check, as samples.csv keeps it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    /// MONOTONIC s at the end of the row, and the row's length.
    pub end_s: f64,
    pub dt: f64,
    pub phase: CheckPhase,
    pub b: Option<f64>,
    pub t: Option<f64>,
    pub t4_mean: Option<f64>,
    pub t4_max: Option<f64>,
    pub core: Vec<Option<f64>>,
    pub fan: Option<u32>,
    pub f_med: Option<u32>,
    pub thr_ms: Option<u64>,
    pub core_thr_ms: Vec<Option<u64>>,
    pub xe: Option<XeRead>,
    pub xe_act: Option<u32>,
    pub ac: Option<bool>,
    pub deficit: bool,
    /// Cumulative burn rounds over all threads, from its latest line.
    pub rounds: Option<u64>,
    /// Cumulative CPU time of the burn process.
    pub burn_cpu_s: Option<f64>,
}

impl Sample {
    fn mid(&self) -> f64 {
        self.end_s - self.dt / 2.0
    }
}

/// When the load ran, MONOTONIC s.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Span {
    pub load_start: f64,
    pub load_end: f64,
}

/// What the measurement needs to know about the machine.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeasureCtx {
    pub tcc_c: f64,
    pub has_throttle: bool,
    /// Burn threads, and CPUs.
    pub threads: usize,
    pub nproc: usize,
    /// t_cool is the fan leaving its load stage, which only the Duo's EC stages define.
    pub duo: bool,
}

/// One window of the load.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Window {
    /// Seconds from the load start.
    pub from_s: f64,
    pub to_s: f64,
    pub seconds: f64,
    /// Median coretemp (the seed's method), and the 4 Hz x86_pkg_temp median beside it.
    pub t_ss: Option<f64>,
    pub t_ss4: Option<f64>,
    /// Thermal throttle time / seconds, attributed by the §5.1 rule on T4 maxima.
    pub duty: Option<f64>,
    pub cold_frac: Option<f64>,
    pub prochot_frac: Option<f64>,
    /// Share of the seconds with xe `reason_prochot` set.
    pub prochot_seconds: f64,
    pub fan_ss: Option<f64>,
    pub f_ss: Option<f64>,
    pub busy: Option<f64>,
    pub deficit: f64,
    /// Share of the seconds with xe `reason_pl1` set.
    pub xe_pl1: f64,
    /// Million rounds per thread-second, and rounds per MHz-second per thread. None when other
    /// processes took more than 5 % of the CPU.
    pub work: Option<f64>,
    pub work_per_mhz: Option<f64>,
    /// The burn's share of the machine, and everything else's.
    pub burn_share: Option<f64>,
    pub foreign: Option<f64>,
}

/// Everything a check measures; result.json keeps it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Measured {
    pub steady: Window,
    pub matched: Window,
    pub t_idle: Option<f64>,
    pub fan_idle: Option<f64>,
    pub sen2: Option<f64>,
    pub sen4: Option<f64>,
    pub t_end: Option<f64>,
    pub t_hs: Option<f64>,
    pub d_jc: Option<f64>,
    pub floor: Option<f64>,
    pub t_cool: Option<f64>,
    pub t_fan5000: Option<f64>,
    pub duty_early: Option<f64>,
    pub t_first_throttle: Option<f64>,
    pub t_fan6500: Option<f64>,
}

/// T4 readings taken in [from, to].
fn readings(t4: &[(f64, f64)], from: f64, to: f64) -> Vec<f64> {
    t4.iter()
        .filter(|(at, _)| *at >= from - EPS && *at <= to + EPS)
        .map(|(_, v)| *v)
        .collect()
}

/// T4 readings taken in (from, to]: the interval a row covers, which the previous row's last
/// reading does not belong to.
fn readings_after(t4: &[(f64, f64)], from: f64, to: f64) -> Vec<f64> {
    t4.iter()
        .filter(|(at, _)| *at > from + EPS && *at <= to + EPS)
        .map(|(_, v)| *v)
        .collect()
}

fn ratio(part: f64, whole: f64) -> f64 {
    if whole > 0.0 { part / whole } else { 0.0 }
}

/// The highest temperature over the three seconds up to `end`: the 4 Hz T4 readings when there
/// are any, else the rows' coretemp.
fn max3(samples: &[Sample], t4: &[(f64, f64)], end: f64) -> Option<f64> {
    let fast = readings_after(t4, end - 3.0, end);
    if !fast.is_empty() {
        return fast.into_iter().reduce(f64::max);
    }
    samples
        .iter()
        .filter(|s| s.end_s > end - 3.0 + EPS && s.end_s <= end + EPS)
        .filter_map(|s| s.t)
        .reduce(f64::max)
}

fn window(
    samples: &[Sample],
    t4: &[(f64, f64)],
    burn: &[(f64, u64)],
    span: Span,
    ctx: &MeasureCtx,
    (a, b): (f64, f64),
) -> Window {
    let (lo, hi) = (span.load_start + a, span.load_start + b);
    let rows: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.phase == CheckPhase::Load && s.mid() >= lo - EPS && s.mid() <= hi + EPS)
        .collect();
    let seconds: f64 = rows.iter().map(|s| s.dt).sum();
    let temps: Vec<f64> = rows.iter().filter_map(|s| s.t).collect();

    let (mut thermal, mut cold, mut prochot, mut counted) = (0u64, 0u64, 0u64, 0.0);
    for s in &rows {
        let Some(thr) = s.thr_ms else { continue };
        counted += s.dt;
        if s.xe.is_some_and(|x| x.prochot) {
            prochot += thr;
        } else if max3(samples, t4, s.end_s).is_some_and(|m| m < ctx.tcc_c - COLD_BELOW_TCC) {
            cold += thr;
        } else {
            thermal += thr;
        }
    }
    let (duty, cold_frac, prochot_frac) = if counted > 0.0 {
        let ms = counted * 1000.0;
        (
            Some(thermal as f64 / ms),
            Some(cold as f64 / ms),
            Some(prochot as f64 / ms),
        )
    } else if !ctx.has_throttle && seconds > 0.0 {
        // No throttle counters: duty is the share of seconds at TCC − 2 (§6.3).
        let hot: f64 = rows
            .iter()
            .filter(|s| s.t.is_some_and(|t| t >= ctx.tcc_c - NO_COUNTER_BELOW_TCC))
            .map(|s| s.dt)
            .sum();
        (Some(hot / seconds), None, None)
    } else {
        (None, None, None)
    };

    let share = |f: &dyn Fn(&Sample) -> bool| -> f64 {
        ratio(rows.iter().filter(|s| f(s)).map(|s| s.dt).sum(), seconds)
    };
    let (busy_sum, busy_s) = rows
        .iter()
        .filter_map(|s| s.b.map(|b| (b * s.dt, s.dt)))
        .fold((0.0, 0.0), |a, x| (a.0 + x.0, a.1 + x.1));
    let busy = (busy_s > 0.0).then(|| busy_sum / busy_s);
    let fans: Vec<f64> = rows.iter().filter_map(|s| s.fan.map(f64::from)).collect();
    let fs: Vec<f64> = rows.iter().filter_map(|s| s.f_med.map(f64::from)).collect();
    let f_ss = median(&fs);

    let cpu: Vec<(f64, f64)> = rows
        .iter()
        .filter_map(|s| s.burn_cpu_s.map(|c| (s.end_s, c)))
        .collect();
    let burn_share = match (cpu.first(), cpu.last()) {
        (Some(f), Some(l)) if l.0 - f.0 > 1.0 => {
            Some(((l.1 - f.1) / ((l.0 - f.0) * ctx.nproc.max(1) as f64)).clamp(0.0, 1.0))
        }
        _ => None,
    };
    let foreign = busy.zip(burn_share).map(|(b, s)| (b - s).max(0.0));
    let lines: Vec<&(f64, u64)> = burn.iter().filter(|(t, _)| *t >= lo - EPS && *t <= hi + EPS).collect();
    let mut work = match (lines.first(), lines.last()) {
        (Some(f), Some(l)) if l.0 - f.0 >= WORK_MIN_S && l.1 >= f.1 => Some(
            (l.1 - f.1) as f64 / 1e6 / ((l.0 - f.0) * ctx.threads.max(1) as f64),
        ),
        _ => None,
    };
    if foreign.is_some_and(|f| f > FOREIGN_MAX) {
        work = None;
    }
    Window {
        from_s: a,
        to_s: b,
        seconds,
        t_ss: median(&temps),
        t_ss4: median(&readings(t4, lo, hi)),
        duty,
        cold_frac,
        prochot_frac,
        prochot_seconds: share(&|s| s.xe.is_some_and(|x| x.prochot)),
        fan_ss: median(&fans),
        f_ss,
        busy,
        deficit: share(&|s| s.deficit),
        xe_pl1: share(&|s| s.xe.is_some_and(|x| x.pl1)),
        work_per_mhz: work.zip(f_ss).filter(|(_, f)| *f > 0.0).map(|(w, f)| w * 1e6 / f),
        work,
        burn_share,
        foreign,
    }
}

/// The §10.5 metrics from a finished run.
pub fn measure(
    samples: &[Sample],
    t4: &[(f64, f64)],
    burn: &[(f64, u64)],
    span: Span,
    ctx: &MeasureCtx,
) -> Measured {
    let (ls, le) = (span.load_start, span.load_end);
    let idle: Vec<&Sample> = samples.iter().filter(|s| s.phase == CheckPhase::Idle).collect();
    let load: Vec<&Sample> = samples.iter().filter(|s| s.phase == CheckPhase::Load).collect();
    let recovery: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.phase == CheckPhase::Recovery)
        .collect();
    let idle_t: Vec<f64> = idle.iter().filter_map(|s| s.t).collect();
    let idle_fan: Vec<f64> = idle.iter().filter_map(|s| s.fan.map(f64::from)).collect();

    // Post-load: from the 4 Hz T4 when there are readings, else from the rows' coretemp.
    let after = |a: f64, b: f64| -> Option<f64> {
        median(&readings(t4, le + a, le + b)).or_else(|| {
            let t: Vec<f64> = samples
                .iter()
                .filter(|s| s.end_s >= le + a - EPS && s.end_s <= le + b + EPS)
                .filter_map(|s| s.t)
                .collect();
            median(&t)
        })
    };
    let t_end = median(&readings(t4, le - T_END_S, le))
        .or_else(|| load.iter().rev().find_map(|s| s.t));
    let t_hs = after(HEATSINK.0, HEATSINK.1);
    let first_after = |f: &dyn Fn(&Sample) -> bool| -> Option<f64> {
        recovery.iter().find(|s| f(s)).map(|s| s.end_s - le)
    };
    let early: Vec<&&Sample> = load
        .iter()
        .filter(|s| s.mid() >= ls + EARLY.0 - EPS && s.mid() <= ls + EARLY.1 + EPS)
        .collect();
    let (early_ms, early_s) = early
        .iter()
        .filter_map(|s| s.thr_ms.map(|m| (m as f64, s.dt)))
        .fold((0.0, 0.0), |a, x| (a.0 + x.0, a.1 + x.1));
    Measured {
        steady: window(samples, t4, burn, span, ctx, STEADY),
        matched: window(samples, t4, burn, span, ctx, MATCHED),
        t_idle: median(&idle_t),
        fan_idle: median(&idle_fan),
        sen2: None,
        sen4: None,
        d_jc: t_end.zip(t_hs).map(|(e, h)| e - h),
        t_end,
        t_hs,
        floor: after(FLOOR.0, FLOOR.1),
        t_cool: if ctx.duo {
            first_after(&|s| s.fan.is_some_and(|f| f <= DUO.ec_stages[1]))
        } else {
            None
        },
        t_fan5000: first_after(&|s| s.fan.is_some_and(|f| f <= FAN_LOW_STAGE)),
        duty_early: (early_s > 0.0).then(|| early_ms / (early_s * 1000.0)),
        t_first_throttle: load
            .iter()
            .find(|s| s.thr_ms.is_some_and(|m| m > 0))
            .map(|s| s.end_s - ls),
        t_fan6500: load
            .iter()
            .find(|s| s.fan.is_some_and(|f| f >= DUO.settle_fan))
            .map(|s| s.end_s - ls),
    }
}

/// Why a finished run does not count (§10.5). `nice` is the burn's; a starved generator explains
/// a machine that was not fully busy.
pub fn invalid(steady: &Window, nice: Option<i64>, pl_changed: bool) -> Vec<String> {
    let mut r = Vec::new();
    if steady.t_ss.is_none() {
        r.push(text::no_temperatures());
    }
    let busy = steady.busy.unwrap_or(0.0);
    if busy < BUSY_MIN {
        r.push(match nice {
            Some(n) if n > 0 => text::starved(n),
            _ => text::not_busy(busy),
        });
    }
    if steady.prochot_seconds >= PROCHOT_MAX {
        r.push(text::prochot(steady.prochot_seconds));
    }
    let lost = steady.cold_frac.unwrap_or(0.0) + steady.prochot_frac.unwrap_or(0.0);
    if lost > COLD_PROCHOT_MAX {
        r.push(text::cold_prochot(lost));
    }
    if pl_changed {
        r.push(text::pl_changed());
    }
    r
}

/// What the judgement needs besides the measurements.
#[derive(Clone, Copy, Debug, Default)]
pub struct JudgeIn<'a> {
    pub purpose: CheckPurpose,
    /// False after "Run anyway" on a key mismatch: compared only with R_before.
    pub comparable: bool,
    pub active: Option<&'a CheckRef>,
    pub best: Option<&'a CheckRef>,
    pub before: Option<&'a RBefore>,
    /// "Accept fan as normal" or a re-learned fan speed: it replaces every reference's R_fan.
    pub r_fan_override: Option<f64>,
    pub cpu: &'a str,
    pub has_fan: bool,
}

/// The verdicts of one check.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Judged {
    pub verdict: CheckVerdict,
    pub post_clean: Option<PostClean>,
    /// Why it is Inconclusive.
    pub inconclusive: Vec<String>,
    pub fan_flag: bool,
    pub diagnosis: Vec<String>,
    /// Reported, never disqualifying: foreign CPU, charger deficit.
    pub notes: Vec<String>,
    /// "matched" against the seed, "steady" against own references.
    pub window: &'static str,
}

/// The seed's numbers came from 74.3-118.9 s of load, so it compares with the matched window;
/// own references were measured like this check, over the steady one.
pub fn window_for<'a>(m: &'a Measured, r: &CheckRef) -> (&'a Window, &'static str) {
    if r.source == "seed" {
        (&m.matched, "matched")
    } else {
        (&m.steady, "steady")
    }
}

pub fn metrics_of(w: &Window, m: &Measured) -> CheckMetrics {
    CheckMetrics {
        t_ss: w.t_ss,
        duty: w.duty,
        fan: w.fan_ss,
        f: w.f_ss,
        work: w.work,
        djc: m.d_jc,
        ths: m.t_hs,
        floor: m.floor,
        t_cool: m.t_cool,
        t_idle: m.t_idle,
    }
}

/// Work and frequency compare only on the same CPU model and generator.
fn same_rig(r: &CheckRef, cpu: &str) -> bool {
    r.cpu == cpu && r.generator == Some(GENERATOR_VERSION)
}

/// R_before with the numbers a comparison needs. One made from light-load windows alone has
/// none, and compares with nothing.
fn usable(before: Option<&RBefore>) -> Option<&RBefore> {
    before.filter(|b| b.t_ss.is_some() && b.duty.is_some())
}

/// Improved or Not improved against R_before alone: `judge_post_clean` with a reference that has
/// no numbers, which can never come out Restored. `m` is the steady window.
fn against_before(m: &CheckMetrics, before: &RBefore) -> PostClean {
    judge_post_clean(m, m, &CheckRef::default(), Some(before), false)
}

/// The reference as the check compares with it: an accepted or re-learned fan speed is R_fan
/// for every fan clause (§8.5), the Like rule's included, so a fan she accepted as normal cannot
/// keep a clean machine from coming out Restored.
fn with_fan(r: &CheckRef, r_fan_override: Option<f64>) -> CheckRef {
    let mut r = r.clone();
    if r_fan_override.is_some() {
        r.fan = r_fan_override;
    }
    r
}

/// The diagnosis lines (§10.8), against R_active (with its fan already the effective R_fan). The
/// seed's post-load values are provisional.
pub fn diagnose(m: &Measured, w: &Window, r: &CheckRef, has_fan: bool) -> Vec<String> {
    let mut d = Vec::new();
    if has_fan && w.fan_ss.zip(r.fan).is_some_and(|(f, rf)| f < rf - FAN_SLOW_MARGIN) {
        d.push(text::DIAG_FAN.to_owned());
    }
    let djc_high = m.d_jc.zip(r.djc).is_some_and(|(x, rx)| x >= rx + DIAG_DJC);
    if djc_high && m.t_hs.zip(r.ths).is_some_and(|(x, rx)| (x - rx).abs() <= DIAG_THS_NEAR) {
        d.push(text::DIAG_CONTACT.to_owned());
    }
    if !djc_high && m.d_jc.is_some() && m.t_hs.zip(r.ths).is_some_and(|(x, rx)| x >= rx + DIAG_THS)
    {
        d.push(text::DIAG_FINS.to_owned());
    }
    if let (Some(ti), Some(rti), Some(t), Some(rt)) = (m.t_idle, r.t_idle, w.t_ss, r.t) {
        let (d_idle, d_ss) = (ti - rti, t - rt);
        if d_idle >= DIAG_IDLE && d_idle >= DIAG_IDLE_SHARE * d_ss {
            d.push(text::DIAG_ROOM.to_owned());
        }
    }
    d
}

/// The verdict against R_active (§10.6), the post-clean outcome of a verify check against
/// R_best (§10.8), the fan flag (§10.7) and the diagnosis lines.
pub fn judge(m: &Measured, j: &JudgeIn, invalid: Vec<String>) -> Judged {
    let mut out = Judged {
        inconclusive: invalid,
        window: "steady",
        ..Judged::default()
    };
    if let Some(f) = m.steady.foreign.filter(|f| *f > FOREIGN_MAX) {
        out.notes.push(text::foreign(f));
    }
    if m.steady.deficit > 0.0 {
        out.notes.push(text::deficit(m.steady.deficit));
    }
    if !out.inconclusive.is_empty() {
        out.verdict = CheckVerdict::Inconclusive;
        return out;
    }
    let steady = metrics_of(&m.steady, m);
    if !j.comparable {
        // Run anyway: the clean reference was measured under other settings.
        match (j.purpose, usable(j.before)) {
            (CheckPurpose::Verify, Some(b)) => {
                out.verdict = CheckVerdict::Borderline;
                out.post_clean = Some(against_before(&steady, b));
            }
            (CheckPurpose::Verify, _) => {
                out.verdict = CheckVerdict::Inconclusive;
                out.inconclusive.push(text::nothing_to_compare());
            }
            _ => {
                out.verdict = CheckVerdict::Inconclusive;
                out.inconclusive.push(text::not_comparable());
            }
        }
        return out;
    }
    let active = j.active.map(|a| with_fan(a, j.r_fan_override));
    match &active {
        Some(a) => {
            let (w, name) = window_for(m, a);
            out.window = name;
            out.verdict = judge_check(&metrics_of(w, m), a, same_rig(a, j.cpu));
            out.fan_flag = j.has_fan
                && w.fan_ss
                    .zip(a.fan)
                    .is_some_and(|(f, rf)| f < rf - FAN_SLOW_MARGIN)
                && w.t_ss.zip(a.t).is_some_and(|(t, rt)| t >= rt - 1.0);
            if out.verdict == CheckVerdict::Inconclusive {
                out.inconclusive.push(text::nothing_to_compare());
            }
        }
        // Nothing recorded yet: a valid run that the engine adopts as the first reference.
        None => out.verdict = CheckVerdict::Borderline,
    }
    if j.purpose == CheckPurpose::Verify && out.verdict != CheckVerdict::Inconclusive {
        let best = j.best.or(j.active).map(|b| with_fan(b, j.r_fan_override));
        out.post_clean = match (&best, usable(j.before)) {
            // Restored over R_best's own window; Improved against B over the steady one.
            (Some(best), before) => {
                let (w, _) = window_for(m, best);
                Some(judge_post_clean(
                    &metrics_of(w, m),
                    &steady,
                    best,
                    before,
                    same_rig(best, j.cpu),
                ))
            }
            (None, Some(b)) => Some(against_before(&steady, b)),
            (None, None) => {
                out.verdict = CheckVerdict::Inconclusive;
                out.inconclusive.push(text::nothing_to_compare());
                None
            }
        };
        if matches!(out.post_clean, Some(PostClean::Improved | PostClean::NotImproved))
            && let Some(a) = &active
        {
            let (w, _) = window_for(m, a);
            out.diagnosis = diagnose(m, w, a, j.has_fan);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The check directory (§11.5).

/// `checks/YYYY-MM-DD_HHMM` from the local start; a second check in the same minute gets `-2`.
/// Returns the path relative to the store and the absolute one.
pub fn make_dir(checks: &Path, now: &Moment) -> io::Result<(String, PathBuf)> {
    fs::create_dir_all(checks)?;
    let c = now.civil();
    let base = format!("{}_{:02}{:02}", c.date.key(), c.hour, c.minute);
    for n in 1..100 {
        let name = if n == 1 { base.clone() } else { format!("{base}-{n}") };
        let dir = checks.join(&name);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok((format!("checks/{name}"), dir)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{} already has 99 checks for this minute", checks.display()),
    ))
}

pub const CSV_HEADER: &str = "t_s,phase,b,T,T4_mean,T4_max,core0,core1,core2,core3,fan,f_med,\
pkg_thr_ms,core_thr_ms0,core_thr_ms1,core_thr_ms2,core_thr_ms3,xe_status,xe_reasons,xe_act,ac,\
deficit,rounds,burn_cpu_s";

fn num(v: Option<f64>, decimals: i32) -> Value {
    let scale = 10f64.powi(decimals);
    v.filter(|x| x.is_finite())
        .map_or(Value::Null, |x| json!((x * scale).round() / scale))
}

/// One sample as named columns, `t_s` counted from the idle phase's start. samples.csv and
/// `check.rows` both come from this, so they cannot drift apart.
fn columns(s: &Sample, origin: f64) -> Vec<(&'static str, Value)> {
    let core = |i: usize| num(s.core.get(i).copied().flatten(), 1);
    let core_thr = |i: usize| {
        s.core_thr_ms
            .get(i)
            .copied()
            .flatten()
            .map_or(Value::Null, |v| json!(v))
    };
    let reasons = s.xe.map(|x| {
        let mut r = Vec::new();
        if x.pl1 {
            r.push("pl1");
        }
        if x.prochot {
            r.push("prochot");
        }
        if x.thermal {
            r.push("thermal");
        }
        r.join("+")
    });
    let int = |v: Option<u32>| v.map_or(Value::Null, |v| json!(v));
    vec![
        ("t_s", num(Some(s.end_s - origin), 3)),
        ("phase", serde_json::to_value(s.phase).unwrap_or_default()),
        ("b", num(s.b, 4)),
        ("T", num(s.t, 1)),
        ("T4_mean", num(s.t4_mean, 2)),
        ("T4_max", num(s.t4_max, 1)),
        ("core0", core(0)),
        ("core1", core(1)),
        ("core2", core(2)),
        ("core3", core(3)),
        ("fan", int(s.fan)),
        ("f_med", int(s.f_med)),
        ("pkg_thr_ms", s.thr_ms.map_or(Value::Null, |v| json!(v))),
        ("core_thr_ms0", core_thr(0)),
        ("core_thr_ms1", core_thr(1)),
        ("core_thr_ms2", core_thr(2)),
        ("core_thr_ms3", core_thr(3)),
        ("xe_status", s.xe.map_or(Value::Null, |x| json!(x.status))),
        // None set is an empty cell, as in the CSV; xe_status tells it from no xe reading.
        (
            "xe_reasons",
            reasons.filter(|r| !r.is_empty()).map_or(Value::Null, Value::String),
        ),
        ("xe_act", int(s.xe_act)),
        ("ac", s.ac.map_or(Value::Null, Value::Bool)),
        ("deficit", json!(s.deficit)),
        ("rounds", s.rounds.map_or(Value::Null, |v| json!(v))),
        ("burn_cpu_s", num(s.burn_cpu_s, 2)),
    ]
}

fn csv_line(cols: &[(&'static str, Value)]) -> String {
    let mut line = cols
        .iter()
        .map(|(_, v)| match v {
            Value::Null => String::new(),
            Value::Bool(b) => u8::from(*b).to_string(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(",");
    line.push('\n');
    line
}

fn row_object(cols: Vec<(&'static str, Value)>) -> Value {
    Value::Object(cols.into_iter().map(|(k, v)| (k.to_owned(), v)).collect::<Map<_, _>>())
}

/// The columns `csv_line` writes as 1/0.
const BOOL_COLUMNS: [&str; 3] = ["xe_status", "ac", "deficit"];

/// A samples.csv read back as `check.rows` objects, for the last check after a restart. The
/// values come back with the JSON types the running check gives them: whole numbers as integers
/// (a float column is always written with its decimal point), and the flags as booleans.
pub fn read_samples_csv(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next()?.split(',').collect();
    let rows = lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let obj: Map<String, Value> = header
                .iter()
                .zip(l.split(','))
                .map(|(k, v)| {
                    let value = if v.is_empty() {
                        Value::Null
                    } else if BOOL_COLUMNS.contains(k) && (v == "0" || v == "1") {
                        Value::Bool(v == "1")
                    } else if let Ok(n) = v.parse::<u64>() {
                        json!(n)
                    } else if let Ok(n) = v.parse::<i64>() {
                        json!(n)
                    } else if let Ok(n) = v.parse::<f64>() {
                        json!(n)
                    } else {
                        Value::String(v.to_owned())
                    };
                    ((*k).to_owned(), value)
                })
                .collect();
            Value::Object(obj)
        })
        .collect();
    Some(Value::Array(rows))
}

// ---------------------------------------------------------------------------------------------
// The running check.

/// What the machine is, for the check.
#[derive(Clone, Debug, Default)]
pub struct CheckMachine {
    pub nproc: usize,
    /// Burn threads: one per CPU.
    pub threads: usize,
    pub cpu: String,
    pub bios: String,
    pub kernel: String,
    pub board: String,
    pub chassis: Chassis,
    pub has_fan: bool,
    pub has_throttle: bool,
}

/// Everything fixed when the check is accepted.
#[derive(Clone, Debug)]
pub struct Setup {
    pub purpose: CheckPurpose,
    pub anyway: bool,
    /// Relative to the store ("checks/2026-12-05_1102"), and absolute.
    pub rel_dir: String,
    pub dir: PathBuf,
    /// The sysfs/procfs root ("/" outside tests), for the burn's CPU time and the /proc scans.
    pub root: PathBuf,
    pub load: Cmd,
    /// The dGPU query, when nvidia-smi exists.
    pub gpu_query: Option<Cmd>,
    /// The dGPU's `power/runtime_status`, read afresh before every query.
    pub dgpu_status: Option<PathBuf>,
    pub machine: CheckMachine,
    pub refs: Refs,
}

pub struct Check {
    setup: Setup,
    state: CheckState,
    started: Moment,
    /// MONOTONIC s: the current phase's start, and the idle phase's (samples.csv's origin).
    phase_start: f64,
    idle_start: f64,
    settle: Settle,
    gpu: GpuQuery,
    busiest: Vec<(String, f64)>,
    snap: Option<(f64, ProcSnapshot)>,
    /// Which gates passed at the last state.json write, and when (MONOTONIC s).
    pattern: Vec<bool>,
    gates_written: f64,
    key_seq: u32,
    key_at_load: Option<KeyCtx>,
    pl_at_load: (Option<u32>, Option<u32>),
    pl_changed: bool,
    comparable: bool,
    samples: Vec<Sample>,
    t4: Vec<(f64, f64)>,
    burn: Option<Burn>,
    /// (MONOTONIC s, total rounds) from each burn line.
    burn_lines: Vec<(f64, u64)>,
    burn_cpu_s: Option<f64>,
    burn_nice: Option<i64>,
    service_nice: Option<i64>,
    load_start: Option<f64>,
    load_end: Option<f64>,
    hot_since: Option<f64>,
    fan_zero_since: Option<f64>,
    fan_stopped: bool,
    idle_context: Option<Context>,
    csv: Option<File>,
    /// A write to the check directory failed and was logged; later ones stay quiet.
    io_failed: bool,
    result: Value,
}

impl Check {
    /// A check in the gating phase. `history` is the sampler's ring, so the settle gate can pass
    /// at once on a machine that has been idle.
    pub fn new<'a>(setup: Setup, history: impl IntoIterator<Item = &'a Row>, now: &Moment) -> Check {
        let mut settle = Settle::default();
        for row in history {
            settle.push(row);
        }
        let state = CheckState {
            purpose: setup.purpose,
            state: CheckPhase::Gating,
            started_at: now.stamp(),
            phase_started_at: now.stamp(),
            phase_s: GATE_WAIT_S as u32,
            anyway: setup.anyway,
            dir: setup.rel_dir.clone(),
            gates: Vec::new(),
            reason: None,
        };
        Check {
            gpu: GpuQuery {
                tool: setup.gpu_query.clone(),
                running: None,
                last: None,
            },
            setup,
            state,
            started: *now,
            phase_start: now.mono_s(),
            idle_start: now.mono_s(),
            settle,
            busiest: Vec::new(),
            snap: None,
            pattern: Vec::new(),
            gates_written: f64::NEG_INFINITY,
            key_seq: 0,
            key_at_load: None,
            pl_at_load: (None, None),
            pl_changed: false,
            comparable: true,
            samples: Vec::new(),
            t4: Vec::new(),
            burn: None,
            burn_lines: Vec::new(),
            burn_cpu_s: None,
            burn_nice: None,
            service_nice: None,
            load_start: None,
            load_end: None,
            hot_since: None,
            fan_zero_since: None,
            fan_stopped: false,
            idle_context: None,
            csv: None,
            io_failed: false,
            result: Value::Null,
        }
    }

    fn running(&self) -> bool {
        matches!(
            self.state.state,
            CheckPhase::Gating | CheckPhase::Idle | CheckPhase::Load | CheckPhase::Recovery
        )
    }

    fn io_error(&mut self, what: &Path, e: io::Error) {
        if !self.io_failed {
            self.io_failed = true;
            note(format_args!("{}: {e}", what.display()));
        }
    }

    fn payload(&self, live: Value) -> Value {
        json!({ "state": self.state, "live": live })
    }

    fn set_phase(&mut self, phase: CheckPhase, secs: f64, now: &Moment) {
        self.state.state = phase;
        self.state.phase_started_at = now.stamp();
        self.state.phase_s = secs as u32;
        self.phase_start = now.mono_s();
    }

    fn refresh_busiest(&mut self, t: f64) {
        if self.snap.as_ref().is_some_and(|(at, _)| t - at < BUSIEST_EVERY_S) {
            return;
        }
        let snap = proc_snapshot(&self.setup.root.join("proc"));
        if let Some((at, before)) = &self.snap {
            self.busiest = busiest(before, &snap, t - at, self.setup.machine.nproc, clk_tck());
        }
        self.snap = Some((t, snap));
    }

    fn gate_row(&mut self, row: &Row, key: &KeyCtx, now: &Moment) -> CheckStep {
        let t = now.mono_s();
        // A suspend ends the wait too (§10.5). Her Start was for the machine as it stood: hours
        // later it may be on her lap, and MONOTONIC stood still meanwhile, so the 5-min limit
        // would not have run out either.
        if row.gap == Gap::Suspend {
            self.end(CheckPhase::Aborted, text::SUSPENDED.to_owned(), now);
            return CheckStep::Finished(None);
        }
        self.settle.push(row);
        // The row's runtime_status can be a minute old, and a dGPU that has suspended since must
        // not be woken by the query (§10.2). The attribute itself never resumes the device, so it
        // is read again here; an unreadable one counts as suspended.
        let active = match &self.setup.dgpu_status {
            Some(path) => fs::read_to_string(path).is_ok_and(|s| s.trim() == "active"),
            None => row.dgpu_active,
        };
        let dgpu = self.gpu.poll(active, t);
        self.refresh_busiest(t);
        let key_match = self.setup.refs.key_match(&key.key);
        let settle = self.settle.now();
        let gates = gates(
            row,
            &key.key,
            &key_match,
            self.setup.anyway,
            dgpu,
            &settle,
            self.setup.machine.has_fan,
        );
        let waited = t - self.phase_start;
        let live = json!({
            "waited_s": num(Some(waited), 1),
            "gates": gates,
            "b10": num(settle.b10, 3),
            "quiet_s": num(Some(settle.quiet_s), 1),
            "fan": settle.fan,
            "slope": num(settle.slope, 4),
            "busiest": self.busiest,
            "prompt": text::PROMPT,
            "waiting": text::WAITING,
        });
        if gates.iter().all(|g| g.ok) {
            self.comparable = !matches!(key_match, KeyMatch::Differs(_));
            self.state.anyway = !self.comparable;
            self.state.gates = gates;
            self.start_idle(row, now);
            return CheckStep::Event(self.payload(live));
        }
        if waited >= GATE_WAIT_S - EPS {
            let failing: Vec<String> = gates.iter().filter(|g| !g.ok).map(|g| g.text.clone()).collect();
            let settle_failed = gates.iter().any(|g| g.id == "settle" && !g.ok);
            self.state.gates = gates;
            self.end(CheckPhase::Refused, text::refused(settle_failed, &failing), now);
            return CheckStep::Finished(None);
        }
        let pattern: Vec<bool> = gates.iter().map(|g| g.ok).collect();
        if pattern != self.pattern && t - self.gates_written >= GATES_WRITE_S - EPS {
            // state.json keeps the gates as they stood at the last write; the values move every
            // second and go to the window as live events only. A change held back here is
            // written once the interval has passed, if it still stands.
            self.pattern = pattern;
            self.gates_written = t;
            self.state.gates = gates;
            return CheckStep::Event(self.payload(live));
        }
        CheckStep::Live(self.payload(live))
    }

    fn start_idle(&mut self, row: &Row, now: &Moment) {
        self.set_phase(CheckPhase::Idle, IDLE_S, now);
        self.idle_start = now.mono_s();
        self.key_seq = row.key_seq;
        self.gpu.stop();
        let path = self.setup.dir.join("samples.csv");
        let opened = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| {
                f.write_all(format!("{CSV_HEADER}\n").as_bytes())?;
                Ok(f)
            });
        match opened {
            Ok(f) => self.csv = Some(f),
            Err(e) => self.io_error(&path, e),
        }
    }

    /// The burn's lines, and its CPU time and nice from /proc.
    fn read_burn(&mut self) {
        let Some(b) = &mut self.burn else { return };
        for line in b.read() {
            if let Some((t, total, _)) = parse_burn_line(&line) {
                self.burn_lines.push((t, total));
            }
        }
        if b.killed {
            return;
        }
        let stat = self.setup.root.join(format!("proc/{}/stat", b.pid));
        if let Some((ticks, nice)) = proc_ticks_nice(&stat) {
            self.burn_cpu_s = Some(ticks as f64 / clk_tck());
            self.burn_nice = Some(nice);
        }
    }

    fn sample(&self, row: &Row) -> Sample {
        let end = row.at.mono_s();
        let t4 = readings_after(&self.t4, end - row.dt.max(0.25), end);
        let loaded = self.load_start.is_some();
        Sample {
            end_s: end,
            dt: row.dt,
            phase: self.state.state,
            b: row.b,
            t: row.t,
            t4_mean: (!t4.is_empty()).then(|| t4.iter().sum::<f64>() / t4.len() as f64),
            t4_max: t4.iter().copied().reduce(f64::max),
            core: row.core_t.clone(),
            fan: row.fan,
            f_med: row.f_med,
            thr_ms: row.thr_ms,
            core_thr_ms: row.core_thr_ms.clone(),
            xe: row.xe,
            xe_act: row.xe_act,
            ac: row.ac,
            deficit: row.deficit,
            rounds: if loaded { self.burn_lines.last().map(|l| l.1) } else { None },
            burn_cpu_s: if loaded { self.burn_cpu_s } else { None },
        }
    }

    /// §10.5: cancel and the service stopping come through `abort`; these come from the rows.
    fn abort_reason(&mut self, row: &Row, key: &KeyCtx, t: f64) -> Option<String> {
        if row.gap == Gap::Suspend {
            return Some(text::SUSPENDED.to_owned());
        }
        if row.lid_open == Some(false) {
            return Some(text::LID_CLOSED.to_owned());
        }
        if row.ac == Some(false) {
            return Some(text::AC_LOST.to_owned());
        }
        if let Some(c) = row.bat_capacity.filter(|&c| c < BATTERY_ABORT) {
            return Some(text::battery_dropped(c));
        }
        if row.key_seq != self.key_seq {
            return Some(text::key_changed(&key.key.label(Some(key.tcc_c))));
        }
        match row.t {
            Some(x) if x >= ABORT_T => {
                let since = *self.hot_since.get_or_insert(t - row.dt.min(ALARM_READING_S));
                if t - since >= ABORT_T_S - EPS {
                    return Some(text::too_hot(x));
                }
            }
            _ => self.hot_since = None,
        }
        match row.t {
            Some(x) if self.setup.machine.has_fan && row.fan == Some(0) && x > FAN_ZERO_T => {
                let since = *self.fan_zero_since.get_or_insert(t - row.dt.min(ALARM_READING_S));
                if t - since >= FAN_ZERO_S - EPS {
                    self.fan_stopped = true;
                    return Some(text::fan_stopped(x));
                }
            }
            _ => self.fan_zero_since = None,
        }
        None
    }

    fn run_row(&mut self, row: &Row, key: &KeyCtx, now: &Moment) -> CheckStep {
        let t = now.mono_s();
        self.read_burn();
        let sample = self.sample(row);
        let cols = columns(&sample, self.idle_start);
        if let Some(f) = &mut self.csv
            && let Err(e) = f.write_all(csv_line(&cols).as_bytes())
        {
            let path = self.setup.dir.join("samples.csv");
            self.csv = None;
            self.io_error(&path, e);
        }
        self.samples.push(sample);
        if self.state.state == CheckPhase::Idle && row.context.is_some() {
            self.idle_context = row.context;
        }
        if let Some(reason) = self.abort_reason(row, key, t) {
            self.end(CheckPhase::Aborted, reason, now);
            return CheckStep::Finished(None);
        }
        if self.state.state == CheckPhase::Load && (key.key.pl1_w, key.key.pl2_w) != self.pl_at_load {
            self.pl_changed = true;
        }
        let elapsed = t - self.phase_start;
        match self.state.state {
            CheckPhase::Idle if elapsed >= IDLE_S - EPS => {
                match spawn_load(&self.setup.load) {
                    Ok(b) => self.burn = Some(b),
                    Err(e) => {
                        self.end(CheckPhase::Aborted, text::load_failed(&e.to_string()), now);
                        return CheckStep::Finished(None);
                    }
                }
                self.load_start = Some(t);
                self.key_at_load = Some(key.clone());
                self.pl_at_load = (key.key.pl1_w, key.key.pl2_w);
                self.service_nice = own_nice();
                self.set_phase(CheckPhase::Load, LOAD_S, now);
                self.write_meta(row, key, now);
                CheckStep::Event(self.payload(self.live(row, t)))
            }
            CheckPhase::Load if elapsed >= LOAD_S - EPS => {
                self.read_burn();
                if let Some(b) = &mut self.burn {
                    b.kill();
                }
                self.load_end = Some(t);
                self.set_phase(CheckPhase::Recovery, RECOVERY_S, now);
                CheckStep::Event(self.payload(self.live(row, t)))
            }
            CheckPhase::Load if self.burn.as_mut().is_some_and(Burn::exited) => {
                self.end(CheckPhase::Aborted, text::LOAD_STOPPED.to_owned(), now);
                CheckStep::Finished(None)
            }
            CheckPhase::Recovery if elapsed >= RECOVERY_S - EPS => {
                let outcome = self.finish(now);
                CheckStep::Finished(Some(Box::new(outcome)))
            }
            _ => CheckStep::Live(self.payload(self.live(row, t))),
        }
    }

    /// The per-second values for the window's Now grid.
    fn live(&self, row: &Row, t: f64) -> Value {
        let load: Vec<&Sample> = self
            .samples
            .iter()
            .filter(|s| s.phase == CheckPhase::Load)
            .collect();
        let (ms, secs) = load
            .iter()
            .filter_map(|s| s.thr_ms.map(|m| (m as f64, s.dt)))
            .fold((0.0, 0.0), |a, x| (a.0 + x.0, a.1 + x.1));
        let work = match self.burn_lines.as_slice() {
            [.., a, b] if b.0 > a.0 => Some(
                (b.1.saturating_sub(a.1)) as f64
                    / 1e6
                    / ((b.0 - a.0) * self.setup.machine.threads.max(1) as f64),
            ),
            _ => None,
        };
        let foreign = match self.samples.as_slice() {
            [.., a, b] if b.dt > 0.0 => a.burn_cpu_s.zip(b.burn_cpu_s).zip(b.b).map(|((x, y), busy)| {
                (busy - (y - x) / (b.dt * self.setup.machine.nproc.max(1) as f64)).max(0.0)
            }),
            _ => None,
        };
        json!({
            "t_s": num(Some(t - self.idle_start), 1),
            "phase_elapsed_s": num(Some(t - self.phase_start), 1),
            "total_s": IDLE_S + LOAD_S + RECOVERY_S,
            "T": row.t,
            "T4_max": self.samples.last().and_then(|s| s.t4_max),
            "fan": row.fan,
            "f": row.f_med,
            "b": num(row.b, 3),
            "duty": num((secs > 0.0).then(|| ms / (secs * 1000.0)), 4),
            "work": num(work, 1),
            "foreign": num(foreign, 3),
            "deficit": row.deficit,
        })
    }

    fn write_json(&mut self, name: &str, value: &Value) {
        let path = self.setup.dir.join(name);
        let mut bytes = serde_json::to_vec_pretty(value).unwrap_or_default();
        bytes.push(b'\n');
        if let Err(e) = store::atomic_write(&path, &bytes) {
            self.io_error(&path, e);
        }
    }

    /// meta.json, when the load starts (§11.5).
    fn write_meta(&mut self, row: &Row, key: &KeyCtx, now: &Moment) {
        let m = &self.setup.machine;
        let meta = json!({
            "purpose": self.setup.purpose,
            "started_at": self.started.stamp(),
            "load_started_at": now.stamp(),
            "comparable": self.comparable,
            "anyway": !self.comparable,
            "key_id": key.id,
            "key_label": key.key.label(Some(key.tcc_c)),
            "pl1_w": key.key.pl1_w,
            "pl2_w": key.key.pl2_w,
            "tcc_offset": key.key.tcc_offset,
            "tcc_c": key.tcc_c,
            "power": {"ac": row.ac, "battery": row.bat_status, "capacity": row.bat_capacity, "deficit": row.deficit},
            "chassis": m.chassis,
            "board": m.board,
            "bios": m.bios,
            "kernel": m.kernel,
            "cpu": m.cpu,
            "nproc": m.nproc,
            "threads": m.threads,
            "generator": GENERATOR_VERSION,
            "nice": self.service_nice,
            "top": self.busiest,
            "idle_context": self.idle_context,
        });
        self.write_json("meta.json", &meta);
    }

    /// Stops everything and records how it ended (refused or aborted).
    fn end(&mut self, phase: CheckPhase, reason: String, now: &Moment) {
        if let Some(b) = &mut self.burn {
            b.kill();
        }
        self.gpu.stop();
        self.state.state = phase;
        self.state.reason = Some(reason.clone());
        self.state.phase_started_at = now.stamp();
        self.state.phase_s = 0;
        self.result = json!({
            "state": phase,
            "reason": reason,
            "purpose": self.setup.purpose,
            "started_at": self.started.stamp(),
            "ended_at": now.stamp(),
            "dir": self.setup.rel_dir,
            "gates": self.state.gates,
        });
        let result = self.result.clone();
        self.write_json("result.json", &result);
        self.close_csv();
    }

    fn close_csv(&mut self) {
        if let Some(f) = self.csv.take()
            && let Err(e) = f.sync_data()
        {
            let path = self.setup.dir.join("samples.csv");
            self.io_error(&path, e);
        }
    }

    /// The end of the recovery: measure, judge, and write result.json.
    fn finish(&mut self, now: &Moment) -> CheckOutcome {
        let m = self.setup.machine.clone();
        let key = self.key_at_load.clone().unwrap_or_else(|| KeyCtx {
            seq: 0,
            key: ConfigKey::default(),
            id: String::new(),
            tcc_c: 0.0,
            pl1_max_w: None,
        });
        let span = Span {
            load_start: self.load_start.unwrap_or(self.phase_start),
            load_end: self.load_end.unwrap_or(self.phase_start),
        };
        let ctx = MeasureCtx {
            tcc_c: key.tcc_c,
            has_throttle: m.has_throttle,
            threads: m.threads,
            nproc: m.nproc,
            duo: m.chassis == Chassis::Duo,
        };
        let mut measured = measure(&self.samples, &self.t4, &self.burn_lines, span, &ctx);
        measured.sen2 = self.idle_context.and_then(|c| c.sen2);
        measured.sen4 = self.idle_context.and_then(|c| c.sen4);
        let view = self.setup.refs.view(&key.id);
        let reasons = invalid(
            &measured.steady,
            self.burn_nice.or(self.service_nice),
            self.pl_changed,
        );
        let j = judge(
            &measured,
            &JudgeIn {
                purpose: self.setup.purpose,
                comparable: self.comparable,
                active: view.active.as_ref(),
                best: view.best.as_ref(),
                before: self.setup.refs.before.as_ref(),
                r_fan_override: self.setup.refs.r_fan_override,
                cpu: &m.cpu,
                has_fan: m.has_fan,
            },
            reasons,
        );
        let this = if j.window == "matched" { &measured.matched } else { &measured.steady };
        // The fan the check was compared with: an accepted or re-learned one where she has one.
        let over = self.setup.refs.r_fan_override;
        let column = |label: String, r: &CheckRef| {
            json!({"label": label, "t_ss": r.t, "duty": r.duty, "fan": r.fan, "f": r.f,
                   "work": r.work, "t_cool": r.t_cool, "t_hs": r.ths, "d_jc": r.djc})
        };
        let comparison = json!({
            "reference": view.active.as_ref().map(|a| column(a.label.clone(), &with_fan(a, over))),
            "best": view.best.as_ref().map(|b| column(b.label.clone(), &with_fan(b, over))),
            "before": self.setup.refs.before.as_ref().map(|b| json!({
                "label": format!("before cleaning ({})", b.source),
                "t_ss": b.t_ss, "duty": b.duty, "fan": b.fan,
            })),
            "check": {
                "label": self.started.date().label(), "window": j.window,
                "t_ss": this.t_ss, "duty": this.duty, "fan": this.fan_ss, "f": this.f_ss,
                "work": this.work, "t_cool": measured.t_cool, "t_hs": measured.t_hs,
                "d_jc": measured.d_jc,
            },
        });
        self.state.state = CheckPhase::Done;
        self.state.reason = None;
        self.state.phase_started_at = now.stamp();
        self.state.phase_s = 0;
        self.result = json!({
            "state": CheckPhase::Done,
            "reason": null,
            "purpose": self.setup.purpose,
            "started_at": self.started.stamp(),
            "ended_at": now.stamp(),
            "dir": self.setup.rel_dir,
            "key_id": key.id,
            "key_label": key.key.label(Some(key.tcc_c)),
            "comparable": self.comparable,
            "anyway": !self.comparable,
            "verdict": j.verdict,
            "post_clean": j.post_clean,
            "window": j.window,
            "inconclusive": j.inconclusive,
            "notes": j.notes,
            "diagnosis": j.diagnosis,
            "fan_flag": j.fan_flag,
            "load": {
                "start_s": num(Some(span.load_start - self.idle_start), 3),
                "end_s": num(Some(span.load_end - self.idle_start), 3),
                "threads": m.threads,
                "generator": GENERATOR_VERSION,
                "nice": self.burn_nice.or(self.service_nice),
            },
            "measured": measured,
            "comparison": comparison,
        });
        let result = self.result.clone();
        self.write_json("result.json", &result);
        self.close_csv();
        CheckOutcome {
            at: self.started,
            dir: self.setup.rel_dir.clone(),
            purpose: self.setup.purpose,
            verdict: j.verdict,
            post_clean: j.post_clean,
            comparable: self.comparable,
            key_id: key.id.clone(),
            metrics: metrics_of(&measured.steady, &measured),
            matched: Some(metrics_of(&measured.matched, &measured)),
            generator: Some(GENERATOR_VERSION),
            cpu: m.cpu.clone(),
            fan_flag: j.fan_flag,
            diagnosis: j.diagnosis,
        }
    }
}

impl CheckRun for Check {
    fn burn_fd(&self) -> Option<RawFd> {
        self.burn.as_ref().and_then(Burn::fd)
    }

    fn burn_ready(&mut self, now: &Moment) -> CheckStep {
        self.read_burn();
        let gone = self.burn.as_ref().is_some_and(|b| b.out.is_none() && !b.killed);
        if gone && self.state.state == CheckPhase::Load {
            self.end(CheckPhase::Aborted, text::LOAD_STOPPED.to_owned(), now);
            return CheckStep::Finished(None);
        }
        CheckStep::Running
    }

    fn t4(&mut self, t4: Option<f64>, now: &Moment) {
        if self.state.state != CheckPhase::Gating
            && self.running()
            && let Some(v) = t4
        {
            self.t4.push((now.mono_s(), v));
        }
    }

    fn row(&mut self, row: &Row, key: &KeyCtx, now: &Moment) -> CheckStep {
        match self.state.state {
            CheckPhase::Gating => self.gate_row(row, key, now),
            CheckPhase::Idle | CheckPhase::Load | CheckPhase::Recovery => {
                self.run_row(row, key, now)
            }
            _ => CheckStep::Running,
        }
    }

    fn abort(&mut self, reason: &str, now: &Moment) {
        if self.running() {
            self.end(CheckPhase::Aborted, reason.to_owned(), now);
        }
    }

    fn state(&self) -> &CheckState {
        &self.state
    }

    fn rows(&self) -> Value {
        Value::Array(
            self.samples
                .iter()
                .map(|s| row_object(columns(s, self.idle_start)))
                .collect(),
        )
    }

    fn fan_stopped(&self) -> bool {
        self.fan_stopped
    }

    fn result(&self) -> Value {
        self.result.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Date, Timeline};
    use crate::model::KeyRefs;
    use crate::references::seed_ref;
    use crate::testutil::{become_subreaper, family, read_pid, reap, reaped, sh, sleeper, test_dir};
    use std::time::Duration;

    const SEED_KEY: &str = "ac|performance|performance|28|35|8|0|100|open";
    const CPU: &str = "11th Gen Intel(R) Core(TM) i7-1165G7";
    const TCC: f64 = 92.0;

    fn her_key() -> ConfigKey {
        ConfigKey::parse_id(SEED_KEY).unwrap()
    }

    fn duo_refs() -> Refs {
        Refs {
            chassis: Chassis::Duo,
            board: "UX482EG".into(),
            ..Refs::default()
        }
    }

    /// An idle row under her normal key.
    fn base_row() -> Row {
        Row {
            dt: 1.0,
            b: Some(0.02),
            b5: Some(0.02),
            b10: Some(0.02),
            t: Some(55.0),
            thr_ms: Some(0),
            fan: Some(3500),
            xe: Some(XeRead::default()),
            ac: Some(true),
            bat_status: Some(BatStatus::NotCharging),
            bat_capacity: Some(100),
            key_seq: 1,
            lid_open: Some(true),
            ..Row::default()
        }
    }

    fn settled() -> SettleNow {
        SettleNow {
            quiet_s: 60.0,
            b10: Some(0.02),
            fan: Some(3500),
            slope: Some(-0.001),
            covered_s: 60.0,
        }
    }

    #[test]
    fn purposes_follow_the_status() {
        assert_eq!(purpose_for(Status::Watch), CheckPurpose::Confirm);
        assert_eq!(purpose_for(Status::Suspicion), CheckPurpose::Confirm);
        assert_eq!(purpose_for(Status::Warning), CheckPurpose::Before);
        assert_eq!(purpose_for(Status::Verifying), CheckPurpose::Verify);
        assert_eq!(purpose_for(Status::Clean), CheckPurpose::Reference);
        assert_eq!(purpose_for(Status::Learning), CheckPurpose::Reference);
    }

    #[test]
    fn settings_against_the_reference_key() {
        let refs = duo_refs();
        assert_eq!(
            refs.key_match(&her_key()),
            KeyMatch::Matches(Some("duo-seed 20/11/2026".into()))
        );
        // Balanced, with EPP following the profile: the profile gate waits for her switch.
        let mut balanced = her_key();
        balanced.profile = Some("balanced".into());
        balanced.epp = Some("balance_performance".into());
        assert!(matches!(refs.key_match(&balanced), KeyMatch::Matches(_)));
        // On battery: the AC gate waits.
        let mut battery = her_key();
        battery.ac = false;
        assert!(matches!(refs.key_match(&battery), KeyMatch::Matches(_)));
        // A closed lid: the lid gate waits for it to open, as the AC gate waits for the charger.
        let mut closed = her_key();
        closed.lid_open = Some(false);
        assert!(matches!(refs.key_match(&closed), KeyMatch::Matches(_)));
        // What waiting cannot fix.
        let mut k = her_key();
        k.pl1_w = Some(25);
        k.lid_open = Some(false);
        assert_eq!(
            refs.key_match(&k),
            KeyMatch::Differs(vec!["PL1 25 W (reference 26-30 W)".into()])
        );
        let mut unknown = her_key();
        unknown.lid_open = None;
        assert_eq!(
            refs.key_match(&unknown),
            KeyMatch::Differs(vec!["lid unknown (reference open)".into()])
        );

        // Generic: nothing yet, then the key with an own reference.
        let mut g = Refs::default();
        assert_eq!(g.key_match(&her_key()), KeyMatch::Unreferenced);
        g.refs.keys.insert(
            SEED_KEY.into(),
            KeyRefs {
                active: Some(CheckRef {
                    t: Some(80.0),
                    duty: Some(0.01),
                    label: "check 1/12/2026".into(),
                    ..CheckRef::default()
                }),
                ..KeyRefs::default()
            },
        );
        assert_eq!(
            g.key_match(&her_key()),
            KeyMatch::Matches(Some("check 1/12/2026".into()))
        );
        let mut low = her_key();
        low.pl1_w = Some(15);
        assert_eq!(
            g.key_match(&low),
            KeyMatch::Differs(vec!["PL1 15 W (reference 28 W)".into()])
        );
    }

    fn failing(g: &[Gate]) -> Vec<String> {
        g.iter().filter(|g| !g.ok).map(|g| g.text.clone()).collect()
    }

    #[test]
    fn the_gates() {
        let seed = KeyMatch::Matches(Some("duo-seed 20/11/2026".into()));
        let all = |row: &Row, key: &ConfigKey, m: &KeyMatch, anyway, dgpu, s: &SettleNow| {
            gates(row, key, m, anyway, dgpu, s, true)
        };
        let g = all(&base_row(), &her_key(), &seed, false, Dgpu::Suspended, &settled());
        let ids: Vec<&str> = g.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["ac", "battery", "profile", "lid", "key", "dgpu", "settle"]);
        assert!(failing(&g).is_empty(), "{g:?}");
        assert_eq!(g[1].text, "battery 100 %");
        assert_eq!(g[3].text, "lid open");
        assert_eq!(g[4].text, "settings as the reference (duo-seed 20/11/2026)");

        // Power.
        let mut r = base_row();
        r.ac = Some(false);
        r.bat_status = Some(BatStatus::Discharging);
        r.bat_capacity = Some(42);
        let g = all(&r, &her_key(), &seed, false, Dgpu::Suspended, &settled());
        assert_eq!(
            failing(&g),
            ["on battery: plug in the charger", "battery 42 %, discharging"]
        );
        // On AC and still discharging: the charger is too weak for what runs.
        r.ac = Some(true);
        r.bat_capacity = Some(80);
        let g = all(&r, &her_key(), &seed, false, Dgpu::Suspended, &settled());
        assert_eq!(failing(&g), ["battery discharging: the charger is not keeping up"]);
        r.bat_capacity = Some(42);
        r.bat_status = Some(BatStatus::Charging);
        let g = all(&r, &her_key(), &seed, false, Dgpu::Suspended, &settled());
        assert_eq!(failing(&g), ["battery 42 % (needs 50 %)"]);
        r.bat_status = None;
        r.bat_capacity = None;
        let g = all(&r, &her_key(), &seed, false, Dgpu::Suspended, &settled());
        assert_eq!((g[1].ok, g[1].text.as_str()), (true, "no battery"));

        // The profile and the key.
        let mut balanced = her_key();
        balanced.profile = Some("balanced".into());
        let g = all(&base_row(), &balanced, &seed, false, Dgpu::Suspended, &settled());
        assert_eq!(failing(&g), ["profile balanced: switch to the Performance profile"]);
        let mut closed = base_row();
        closed.lid_open = Some(false);
        let g = all(&closed, &her_key(), &seed, false, Dgpu::Suspended, &settled());
        assert_eq!(failing(&g), ["lid closed: open the lid"]);
        closed.lid_open = None;
        let g = all(&closed, &her_key(), &seed, false, Dgpu::Suspended, &settled());
        assert_eq!((g[3].ok, g[3].text.as_str()), (true, "lid not reported"));
        let differs = KeyMatch::Differs(vec!["PL1 25 W (reference 26-30 W)".into()]);
        let g = all(&base_row(), &her_key(), &differs, false, Dgpu::Suspended, &settled());
        assert_eq!(
            failing(&g),
            ["PL1 25 W (reference 26-30 W): not comparable with the clean reference; Run anyway compares only with the before-clean values"]
        );
        let g = all(&base_row(), &her_key(), &differs, true, Dgpu::Suspended, &settled());
        assert!(failing(&g).is_empty());
        assert_eq!(
            g[4].text,
            "PL1 25 W (reference 26-30 W): run anyway, not comparable with the clean reference"
        );

        // The dGPU: a busy one or a pending answer holds the check; no tool or no answer does not.
        for (d, ok, text) in [
            (Dgpu::Busy(37.0), false, "dGPU 37 % busy (needs < 10 %)"),
            (Dgpu::Busy(3.0), true, "dGPU 3 % busy"),
            (Dgpu::Asking, false, "dGPU active: asking nvidia-smi"),
            (Dgpu::NoTool, true, "dGPU active (no nvidia-smi to ask)"),
            (Dgpu::NoReading, true, "dGPU active: nvidia-smi gave no reading"),
        ] {
            let g = all(&base_row(), &her_key(), &seed, false, d, &settled());
            assert_eq!((g[5].ok, g[5].text.as_str()), (ok, text));
        }

        // Settle, in the Check page's words.
        let busy = SettleNow {
            quiet_s: 0.0,
            b10: Some(0.14),
            fan: Some(5200),
            slope: Some(-0.05),
            covered_s: 60.0,
        };
        let g = all(&base_row(), &her_key(), &seed, false, Dgpu::Suspended, &busy);
        assert_eq!(
            failing(&g),
            ["B10 14 % (< 10), fan 5200 (≤ 5000), slope −0.05 °C/s (≥ −0.02)"]
        );
        let settling = SettleNow {
            quiet_s: 12.0,
            slope: None,
            covered_s: 40.0,
            ..settled()
        };
        let g = all(&base_row(), &her_key(), &seed, false, Dgpu::Suspended, &settling);
        assert_eq!(failing(&g), ["idle 12/30 s, measuring the slope (40/60 s)"]);
        // A machine with no fan sensor has no fan line.
        let g = gates(&base_row(), &her_key(), &seed, false, Dgpu::Suspended, &busy, false);
        assert_eq!(failing(&g), ["B10 14 % (< 10), slope −0.05 °C/s (≥ −0.02)"]);
    }

    #[test]
    fn settle_follows_quiet_time_and_the_slope() {
        let mut tl = Timeline::starting(Date::new(2026, 12, 5).unwrap(), 11, 0, 0, 0);
        let mut s = Settle::default();
        // Busy, then quiet and cooling 0.05 °C/s, then flat.
        for i in 0..20 {
            let at = tl.advance(1.0);
            s.push(&Row {
                at,
                b10: Some(0.5),
                t: Some(80.0 - i as f64 * 0.05),
                ..base_row()
            });
        }
        assert_eq!(s.now().quiet_s, 0.0);
        assert_eq!(s.now().slope, None, "under 60 s of rows");
        for i in 0..45 {
            let at = tl.advance(1.0);
            s.push(&Row {
                at,
                t: Some(79.0 - i as f64 * 0.05),
                ..base_row()
            });
        }
        let now = s.now();
        assert_eq!(now.quiet_s, 45.0);
        assert!((now.slope.unwrap() + 0.05).abs() < 1e-9, "{:?}", now.slope);
        for _ in 0..60 {
            let at = tl.advance(1.0);
            s.push(&Row {
                at,
                t: Some(60.0),
                ..base_row()
            });
        }
        assert_eq!(s.now().slope, Some(0.0));
        // A suspend starts the history again.
        let at = tl.suspend(600.0);
        s.push(&Row {
            at,
            gap: Gap::Suspend,
            b10: None,
            ..base_row()
        });
        let now = s.now();
        assert_eq!((now.quiet_s, now.slope, now.covered_s), (0.0, None, 0.0));
    }

    #[test]
    fn small_parsers() {
        assert_eq!(parse_utilization("37\n"), Some(37.0));
        assert_eq!(parse_utilization("3 %\n12\n"), Some(12.0));
        assert_eq!(parse_utilization("NVIDIA-SMI has failed\n"), None);
        assert_eq!(
            parse_burn_line(r#"{"t":12.5,"rounds":[1048576,2097152]}"#),
            Some((12.5, 3_145_728, 2))
        );
        assert_eq!(parse_burn_line("12345"), None);
        assert_eq!(parse_burn_line(r#"{"t":1,"rounds":[1,-2]}"#), None);
    }

    #[test]
    fn busiest_processes_as_shares_of_the_machine() {
        let snap = |v: &[(i32, &str, u64)]| ProcSnapshot {
            procs: v.iter().map(|(p, c, t)| (*p, ((*c).to_owned(), *t))).collect(),
        };
        let before = snap(&[(1, "rustc", 1000), (2, "floorp", 500), (3, "rustc", 10)]);
        let after = snap(&[(1, "rustc", 1400), (2, "floorp", 572), (3, "rustc", 106), (4, "cc", 8)]);
        // 5 s of 8 CPUs at 100 ticks a second is 4000 ticks.
        let top = busiest(&before, &after, 5.0, 8, 100.0);
        assert_eq!(top[0], ("rustc".to_owned(), 0.124));
        assert_eq!(top[1], ("floorp".to_owned(), 0.018));
        assert_eq!(top[2], ("cc".to_owned(), 0.002));
    }

    // -----------------------------------------------------------------------------------------
    // Measurement over synthetic rows.

    /// The idle phase starts at this MONOTONIC second; the load at +30 s and its end at +210 s.
    const T0: f64 = 1000.0;

    struct Synth {
        samples: Vec<Sample>,
        t4: Vec<(f64, f64)>,
        burn: Vec<(f64, u64)>,
    }

    fn span() -> Span {
        Span {
            load_start: T0 + 30.0,
            load_end: T0 + 210.0,
        }
    }

    fn ctx() -> MeasureCtx {
        MeasureCtx {
            tcc_c: TCC,
            has_throttle: true,
            threads: 8,
            nproc: 8,
            duo: true,
        }
    }

    /// A run shaped like the seed's: idle at 60 °C; 60 s of turbo transient at 91 °C with 38 %
    /// throttle and the fan on its 6100 stage; then `t` °C with `ms` of throttle every 10th
    /// second (T4 4 °C higher in those seconds); recovery at 56, 52, then 49 °C. T4 at 4 Hz,
    /// a burn line a second at 300 M rounds per thread-second, and the burn on 98 % of the CPUs.
    fn synth(t: f64, ms: u64, fan: u32) -> Synth {
        let (ls, le) = (span().load_start, span().load_end);
        let mut out = Synth {
            samples: Vec::new(),
            t4: Vec::new(),
            burn: Vec::new(),
        };
        for k in 1..=300u32 {
            let end = T0 + k as f64;
            let mut s = Sample {
                end_s: end,
                dt: 1.0,
                ac: Some(true),
                xe: Some(XeRead::default()),
                b: Some(0.02),
                thr_ms: Some(0),
                ..Sample::default()
            };
            let t4;
            if end <= ls {
                s.phase = CheckPhase::Idle;
                s.t = Some(60.0);
                s.fan = Some(4600);
                t4 = 60.0;
            } else if end <= le {
                let rel = end - ls;
                s.phase = CheckPhase::Load;
                s.b = Some(0.99);
                s.f_med = Some(3700);
                s.burn_cpu_s = Some(8.0 * 0.98 * rel);
                if rel <= 60.0 {
                    s.t = Some(91.0);
                    s.thr_ms = Some(380);
                    s.fan = Some(6100);
                    t4 = 91.0;
                } else {
                    let thr = if k % 10 == 5 { ms } else { 0 };
                    s.t = Some(t);
                    s.thr_ms = Some(thr);
                    s.fan = Some(if rel < 62.0 { 6100 } else { fan });
                    t4 = if thr > 0 { t + 4.0 } else { t };
                }
                out.burn.push((end, 8 * 300_000_000 * rel as u64));
            } else {
                let rel = end - le;
                s.phase = CheckPhase::Recovery;
                s.t = Some(if rel <= 10.0 {
                    56.0
                } else if rel < 45.0 {
                    52.0
                } else {
                    49.0
                });
                s.fan = Some(if rel < 15.0 {
                    6800
                } else if rel < 30.0 {
                    6100
                } else {
                    5000
                });
                t4 = s.t.unwrap();
            }
            for q in 0..4 {
                out.t4.push((end - 0.75 + 0.25 * q as f64, t4));
            }
            out.samples.push(s);
        }
        out
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-6)
    }

    #[test]
    fn windows_and_metrics() {
        let s = synth(86.0, 93, 6800);
        let m = measure(&s.samples, &s.t4, &s.burn, span(), &ctx());

        // Steady: load seconds 75-178, rows whose middle falls inside: 103 of them.
        let w = &m.steady;
        assert_eq!((w.from_s, w.to_s, w.seconds), (75.0, 178.0, 103.0));
        assert_eq!((w.t_ss, w.t_ss4), (Some(86.0), Some(86.0)));
        // Throttle in 10 of those seconds, every ms of it thermal (T4 reached 90 °C there).
        assert!(close(w.duty, 930.0 / 103_000.0), "{:?}", w.duty);
        assert_eq!((w.cold_frac, w.prochot_frac), (Some(0.0), Some(0.0)));
        assert_eq!((w.fan_ss, w.f_ss), (Some(6800.0), Some(3700.0)));
        assert!(close(w.busy, 0.99));
        assert!(close(w.work, 300.0), "{:?}", w.work);
        assert!(close(w.work_per_mhz, 300e6 / 3700.0));
        assert!(close(w.burn_share, 0.98) && close(w.foreign, 0.01), "{w:?}");
        // Matched: 75-118 s, the seed's own window: 43 rows, 4 of them throttled.
        let mw = &m.matched;
        assert_eq!(mw.seconds, 43.0);
        assert!(close(mw.duty, 372.0 / 43_000.0), "{:?}", mw.duty);

        assert_eq!((m.t_idle, m.fan_idle), (Some(60.0), Some(4600.0)));
        // Post-load, from T4: the last 2 s of load, then +2-6 s and +45-60 s.
        assert_eq!((m.t_end, m.t_hs, m.d_jc, m.floor), (Some(86.0), Some(56.0), Some(30.0), Some(49.0)));
        // The fan leaves its load stage at 15 s and is back on 5000 at 30 s.
        assert_eq!((m.t_cool, m.t_fan5000), (Some(15.0), Some(30.0)));
        // The turbo transient: throttle from the first second, 38 % over 15-60 s.
        assert!(close(m.duty_early, 0.38));
        assert_eq!((m.t_first_throttle, m.t_fan6500), (Some(1.0), Some(62.0)));

        // t_cool is the Duo EC's stage; a generic machine has none.
        let generic = MeasureCtx { duo: false, ..ctx() };
        assert_eq!(measure(&s.samples, &s.t4, &s.burn, span(), &generic).t_cool, None);
    }

    #[test]
    fn throttle_attribution_and_foreign_load() {
        let mut s = synth(86.0, 93, 6800);
        let ls = span().load_start;
        for x in s.samples.iter_mut().filter(|x| x.phase == CheckPhase::Load) {
            let rel = x.end_s - ls;
            if (100.0..110.0).contains(&rel) {
                // PROCHOT from the charger or VR: its throttle is not the cooling's.
                x.xe = Some(XeRead { status: true, prochot: true, ..XeRead::default() });
                x.thr_ms = Some(50);
            }
            if (120.0..130.0).contains(&rel) {
                // Throttle at 80 °C, far under TCC − 5: cold.
                x.t = Some(80.0);
                x.thr_ms = Some(20);
            }
            // Another process on 7 % of the machine.
            x.b = Some(1.0);
            x.burn_cpu_s = x.burn_cpu_s.map(|c| c / 0.98 * 0.93);
        }
        for r in s.t4.iter_mut() {
            if (ls + 119.0..ls + 130.0).contains(&r.0) {
                r.1 = 80.0;
            }
        }
        let m = measure(&s.samples, &s.t4, &s.burn, span(), &ctx());
        let w = &m.steady;
        assert!(close(w.prochot_frac, 500.0 / 103_000.0), "{:?}", w.prochot_frac);
        assert!(close(w.cold_frac, 200.0 / 103_000.0), "{:?}", w.cold_frac);
        assert!((w.prochot_seconds - 10.0 / 103.0).abs() < 1e-9);
        // The rows at 105 and 125 s are no longer thermal.
        assert!(close(w.duty, 744.0 / 103_000.0), "{:?}", w.duty);
        assert!(close(w.foreign, 0.07));
        assert_eq!((w.work, w.work_per_mhz), (None, None), "work is not compared");
        let j = judge(&m, &JudgeIn { comparable: true, ..JudgeIn::default() }, invalid(w, Some(0), false));
        assert!(j.notes.iter().any(|n| n == "other processes used 7.0 % of the CPU: work not compared"));
        // 0.7 % PROCHOT throttle in 9.7 % of the seconds, under both lines: still valid.
        assert!(j.inconclusive.is_empty(), "{:?}", j.inconclusive);

        // Without throttle counters, duty is the share of seconds at TCC − 2.
        let mut hot = synth(91.0, 0, 6800);
        for x in &mut hot.samples {
            x.thr_ms = None;
        }
        let no_counter = MeasureCtx { has_throttle: false, ..ctx() };
        let m = measure(&hot.samples, &hot.t4, &hot.burn, span(), &no_counter);
        assert_eq!(m.steady.duty, Some(1.0));
    }

    #[test]
    fn validity() {
        let ok = Window {
            t_ss: Some(86.0),
            busy: Some(0.99),
            cold_frac: Some(0.0),
            prochot_frac: Some(0.0),
            ..Window::default()
        };
        assert!(invalid(&ok, Some(0), false).is_empty());
        let not_busy = Window { busy: Some(0.91), ..ok.clone() };
        assert_eq!(invalid(&not_busy, Some(0), false), ["busy 91.0 % over the steady window (needs 95 %)"]);
        assert_eq!(invalid(&not_busy, Some(5), false), ["load generator starved (nice 5)"]);
        let prochot = Window { prochot_seconds: 0.12, ..ok.clone() };
        assert_eq!(invalid(&prochot, None, false), ["iGPU PROCHOT in 12.0 % of the steady seconds"]);
        let lost = Window { cold_frac: Some(0.015), prochot_frac: Some(0.01), ..ok.clone() };
        assert_eq!(
            invalid(&lost, None, false),
            ["cold or PROCHOT throttle 2.5 % of the steady time: charger, EC or VR limit"]
        );
        assert_eq!(invalid(&ok, None, true), ["the power limit changed during the load"]);
        let empty = Window::default();
        assert_eq!(
            invalid(&empty, None, false),
            ["no temperature readings in the steady window", "busy 0.0 % over the steady window (needs 95 %)"]
        );
        // Any reason makes it Inconclusive, whatever the numbers say.
        let m = measured(86.0, 0.009, 6800.0);
        let seed = seed_ref("UX482EG");
        let j = judge(
            &m,
            &JudgeIn { comparable: true, active: Some(&seed), ..JudgeIn::default() },
            invalid(&prochot, None, false),
        );
        assert_eq!(j.verdict, CheckVerdict::Inconclusive);
        assert_eq!(j.inconclusive, ["iGPU PROCHOT in 12.0 % of the steady seconds"]);
    }

    // -----------------------------------------------------------------------------------------
    // Verdicts.

    /// A measurement with the same numbers in both windows.
    fn measured(t: f64, duty: f64, fan: f64) -> Measured {
        let w = Window {
            t_ss: Some(t),
            duty: Some(duty),
            fan_ss: Some(fan),
            f_ss: Some(3700.0),
            busy: Some(0.99),
            cold_frac: Some(0.0),
            prochot_frac: Some(0.0),
            work: Some(300.0),
            ..Window::default()
        };
        Measured {
            steady: w.clone(),
            matched: w,
            t_idle: Some(62.0),
            t_end: Some(t),
            t_hs: Some(56.0),
            d_jc: Some(t - 56.0),
            floor: Some(49.0),
            ..Measured::default()
        }
    }

    fn against<'a>(r: &'a CheckRef, purpose: CheckPurpose) -> JudgeIn<'a> {
        JudgeIn {
            purpose,
            comparable: true,
            active: Some(r),
            best: Some(r),
            cpu: CPU,
            has_fan: true,
            ..JudgeIn::default()
        }
    }

    #[test]
    fn verdicts_against_the_seed() {
        let seed = seed_ref("UX482EG");
        let v = |t, duty, fan| {
            judge(&measured(t, duty, fan), &against(&seed, CheckPurpose::Reference), Vec::new())
        };
        // §10.6 against the Duo seed: Worse at ≥ 89 °C with ≥ 3 %, or ≥ 8 %; Like at ≤ 88 °C,
        // ≤ 2.5 % and fan ≥ 6400 once T_ss ≥ 85.
        assert_eq!(v(86.5, 0.012, 6800.0).verdict, CheckVerdict::Like);
        assert_eq!(v(86.5, 0.012, 6800.0).window, "matched");
        assert_eq!(v(89.0, 0.03, 6800.0).verdict, CheckVerdict::Worse);
        assert_eq!(v(86.0, 0.08, 6800.0).verdict, CheckVerdict::Worse);
        assert_eq!(v(88.5, 0.01, 6800.0).verdict, CheckVerdict::Borderline);
        assert_eq!(v(87.0, 0.01, 6300.0).verdict, CheckVerdict::Borderline);
        // A cool room: the EC can sit on 6100 with a clean machine, and nothing is flagged.
        let cool = v(84.0, 0.005, 6100.0);
        assert_eq!((cool.verdict, cool.fan_flag), (CheckVerdict::Like, false));
        // §10.7: slow fan at the reference's temperature.
        let slow = v(86.0, 0.01, 6300.0);
        assert!(slow.fan_flag);
        // Under R_T − 1 the fan speed is the EC's choice, not a fault.
        let below = v(84.5, 0.01, 6300.0);
        assert_eq!((below.verdict, below.fan_flag), (CheckVerdict::Like, false));
        // The matched window is the one compared against the seed.
        let mut m = measured(86.0, 0.01, 6800.0);
        m.steady.t_ss = Some(95.0);
        let j = judge(&m, &against(&seed, CheckPurpose::Reference), Vec::new());
        assert_eq!(j.verdict, CheckVerdict::Like);
    }

    #[test]
    fn verdicts_against_an_own_reference() {
        let own = CheckRef {
            t: Some(84.0),
            duty: Some(0.008),
            fan: Some(6900.0),
            f: Some(3700.0),
            work: Some(300.0),
            source: "check".into(),
            cpu: CPU.into(),
            generator: Some(GENERATOR_VERSION),
            ..CheckRef::default()
        };
        let j = |m: &Measured| judge(m, &against(&own, CheckPurpose::Reference), Vec::new());
        let mut m = measured(85.0, 0.01, 6900.0);
        // Against an own reference the steady window counts, not the matched one.
        m.matched.t_ss = Some(95.0);
        assert_eq!((j(&m).verdict, j(&m).window), (CheckVerdict::Like, "steady"));
        // Same CPU and generator: 150 MHz down with duty at D_hot is Worse.
        let mut slow = measured(85.0, 0.03, 6900.0);
        slow.steady.f_ss = Some(3540.0);
        assert_eq!(j(&slow).verdict, CheckVerdict::Worse);
        // Less work and lower frequency at a like temperature: Borderline, not Like.
        let mut weak = measured(85.0, 0.01, 6900.0);
        weak.steady.work = Some(280.0);
        weak.steady.f_ss = Some(3550.0);
        assert_eq!(j(&weak).verdict, CheckVerdict::Borderline);
        // Another generator: work and frequency do not compare.
        let other = CheckRef { generator: Some(GENERATOR_VERSION + 1), ..own.clone() };
        let jo = judge(&weak, &against(&other, CheckPurpose::Reference), Vec::new());
        assert_eq!(jo.verdict, CheckVerdict::Like);
        // No reference at all yet (generic): a valid run, recorded as the first one.
        let first = judge(
            &measured(80.0, 0.01, 4000.0),
            &JudgeIn { comparable: true, ..JudgeIn::default() },
            Vec::new(),
        );
        assert_eq!((first.verdict, first.post_clean), (CheckVerdict::Borderline, None));
    }

    #[test]
    fn verify_outcomes() {
        let seed = seed_ref("UX482EG");
        let before = RBefore {
            source: "check".into(),
            t_ss: Some(91.0),
            duty: Some(0.09),
            fan: Some(6800.0),
            ..RBefore::default()
        };
        let verify = |m: &Measured, b: Option<&RBefore>| {
            let mut j = against(&seed, CheckPurpose::Verify);
            j.before = b;
            judge(m, &j, Vec::new())
        };
        let restored = verify(&measured(86.5, 0.012, 6800.0), Some(&before));
        assert_eq!(restored.post_clean, Some(PostClean::Restored));
        assert!(restored.diagnosis.is_empty());
        let improved = verify(&measured(89.0, 0.035, 6800.0), Some(&before));
        assert_eq!(improved.post_clean, Some(PostClean::Improved));
        let not = verify(&measured(90.5, 0.08, 6800.0), Some(&before));
        assert_eq!((not.verdict, not.post_clean), (CheckVerdict::Worse, Some(PostClean::NotImproved)));
        // With no R_before: within 5 °C and 6 pp of R_best is Improved.
        assert_eq!(verify(&measured(90.5, 0.05, 6800.0), None).post_clean, Some(PostClean::Improved));
        assert_eq!(verify(&measured(91.5, 0.05, 6800.0), None).post_clean, Some(PostClean::NotImproved));
        // An invalid run stays Inconclusive with no post-clean verdict.
        let mut j = against(&seed, CheckPurpose::Verify);
        j.before = Some(&before);
        let inc = judge(&measured(86.5, 0.012, 6800.0), &j, vec!["busy".into()]);
        assert_eq!((inc.verdict, inc.post_clean), (CheckVerdict::Inconclusive, None));

        // Restored is judged over the seed's matched window, Improved against a before-check over
        // the steady one: a cooler first 118 s does not make a soaked machine look improved.
        let mut soaked = measured(88.5, 0.028, 6800.0);
        soaked.steady.t_ss = Some(90.0);
        soaked.steady.duty = Some(0.06);
        assert_eq!(verify(&soaked, Some(&before)).post_clean, Some(PostClean::NotImproved));
        soaked.steady.t_ss = Some(88.9);
        assert_eq!(verify(&soaked, Some(&before)).post_clean, Some(PostClean::Improved));
    }

    #[test]
    fn an_accepted_fan_is_r_fan_for_the_check() {
        let seed = seed_ref("UX482EG");
        // A clean machine on a worn fan she accepted at 6100 rpm.
        let m = measured(86.0, 0.01, 6100.0);
        let verify = |over: Option<f64>| {
            let mut j = against(&seed, CheckPurpose::Verify);
            j.r_fan_override = over;
            judge(&m, &j, Vec::new())
        };
        let before = verify(None);
        assert_eq!((before.verdict, before.fan_flag), (CheckVerdict::Borderline, true));
        assert_ne!(before.post_clean, Some(PostClean::Restored));
        let accepted = verify(Some(6100.0));
        assert_eq!(accepted.verdict, CheckVerdict::Like);
        assert_eq!(accepted.post_clean, Some(PostClean::Restored));
        assert!(!accepted.fan_flag && accepted.diagnosis.is_empty());
        // A confirm check can clear a case again, too.
        let mut j = against(&seed, CheckPurpose::Confirm);
        j.r_fan_override = Some(6100.0);
        assert_eq!(judge(&m, &j, Vec::new()).verdict, CheckVerdict::Like);
    }

    #[test]
    fn a_before_with_no_numbers_compares_with_nothing() {
        // Generic, no reference anywhere: WARNING came from light-load windows alone, so
        // R_before holds only their rate.
        let light_only = RBefore {
            source: "case".into(),
            e2_per_day: Some(12.0),
            ..RBefore::default()
        };
        let j = JudgeIn {
            purpose: CheckPurpose::Verify,
            comparable: true,
            before: Some(&light_only),
            cpu: CPU,
            has_fan: true,
            ..JudgeIn::default()
        };
        let out = judge(&measured(80.0, 0.01, 4000.0), &j, Vec::new());
        assert_eq!((out.verdict, out.post_clean), (CheckVerdict::Inconclusive, None));
        assert_eq!(out.inconclusive, [text::nothing_to_compare()]);
        // With numbers it compares, as before.
        let numbers = RBefore {
            t_ss: Some(90.0),
            duty: Some(0.09),
            ..light_only.clone()
        };
        let j = JudgeIn { before: Some(&numbers), ..j };
        let out = judge(&measured(80.0, 0.01, 4000.0), &j, Vec::new());
        assert_eq!(out.post_clean, Some(PostClean::Improved));
    }

    #[test]
    fn diagnosis_lines() {
        let seed = seed_ref("UX482EG");
        let before = RBefore {
            t_ss: Some(92.0),
            duty: Some(0.12),
            ..RBefore::default()
        };
        let diag = |m: &Measured| {
            let mut j = against(&seed, CheckPurpose::Verify);
            j.before = Some(&before);
            judge(m, &j, Vec::new())
        };
        // A slow fan, and heat stuck in the die: T_hs as the reference's, D_jc 6 °C higher.
        let mut m = measured(91.0, 0.09, 6300.0);
        m.t_hs = Some(57.0);
        m.d_jc = Some(36.0);
        let j = diag(&m);
        assert_eq!(j.post_clean, Some(PostClean::NotImproved));
        assert_eq!(j.diagnosis, [text::DIAG_FAN, text::DIAG_CONTACT]);
        // A hot heatsink with a normal D_jc.
        let mut m = measured(91.0, 0.09, 6800.0);
        m.t_hs = Some(62.0);
        m.d_jc = Some(29.0);
        assert_eq!(diag(&m).diagnosis, [text::DIAG_FINS]);
        // Everything 4 °C warmer, idle included: Improved, with the room as a note.
        let mut m = measured(90.0, 0.05, 6800.0);
        m.t_idle = Some(66.0);
        m.t_hs = Some(58.0);
        m.d_jc = Some(32.0);
        let j = diag(&m);
        assert_eq!(j.post_clean, Some(PostClean::Improved));
        assert_eq!(j.diagnosis, [text::DIAG_ROOM]);
        // A Restored check has no diagnosis, nor does a confirm check.
        let mut m = measured(86.0, 0.009, 6800.0);
        m.t_hs = Some(62.0);
        assert_eq!(diag(&m).post_clean, Some(PostClean::Restored));
        assert!(diag(&m).diagnosis.is_empty());
        let confirm = judge(&measured(91.0, 0.09, 6300.0), &against(&seed, CheckPurpose::Confirm), Vec::new());
        assert!(confirm.diagnosis.is_empty());
    }

    #[test]
    fn run_anyway_compares_only_with_before() {
        let seed = seed_ref("UX482EG");
        let before = RBefore {
            t_ss: Some(91.0),
            duty: Some(0.09),
            ..RBefore::default()
        };
        let anyway = |purpose, b: Option<&RBefore>, t, duty| {
            let mut j = against(&seed, purpose);
            j.comparable = false;
            j.before = b;
            judge(&measured(t, duty, 6800.0), &j, Vec::new())
        };
        let j = anyway(CheckPurpose::Verify, Some(&before), 86.0, 0.01);
        assert_eq!((j.verdict, j.post_clean), (CheckVerdict::Borderline, Some(PostClean::Improved)));
        let j = anyway(CheckPurpose::Verify, Some(&before), 90.0, 0.08);
        assert_eq!(j.post_clean, Some(PostClean::NotImproved));
        let j = anyway(CheckPurpose::Verify, None, 86.0, 0.01);
        assert_eq!(j.verdict, CheckVerdict::Inconclusive);
        assert_eq!(j.inconclusive, [text::nothing_to_compare()]);
        for purpose in [CheckPurpose::Confirm, CheckPurpose::Before, CheckPurpose::Reference] {
            let j = anyway(purpose, Some(&before), 86.0, 0.01);
            assert_eq!(j.verdict, CheckVerdict::Inconclusive);
            assert_eq!(j.inconclusive, [text::not_comparable()]);
        }
    }

    // -----------------------------------------------------------------------------------------
    // The running check, with harmless children standing in for the burn.

    /// A check under test on a synthetic timeline, after a minute of idle rows.
    struct Rig {
        tl: Timeline,
        check: Check,
        key: KeyCtx,
        row: Row,
        /// What each 4 Hz T4 read returns.
        t4: Option<f64>,
        dir: PathBuf,
    }

    fn rig(name: &str, load: Cmd, purpose: CheckPurpose) -> Rig {
        let dir = test_dir(name);
        let mut tl = Timeline::starting(Date::new(2026, 12, 5).unwrap(), 11, 2, 0, 0);
        let row = base_row();
        let history: Vec<Row> = (0..61)
            .map(|_| Row {
                at: tl.advance(1.0),
                ..row.clone()
            })
            .collect();
        let now = tl.now();
        let (rel_dir, abs) = make_dir(&dir.join("store/checks"), &now).unwrap();
        let setup = Setup {
            purpose,
            anyway: false,
            rel_dir,
            dir: abs,
            root: dir.join("root"),
            load,
            gpu_query: None,
            dgpu_status: None,
            machine: CheckMachine {
                nproc: 8,
                threads: 8,
                cpu: CPU.into(),
                chassis: Chassis::Duo,
                has_fan: true,
                has_throttle: true,
                ..CheckMachine::default()
            },
            refs: duo_refs(),
        };
        let check = Check::new(setup, &history, &now);
        Rig {
            tl,
            check,
            key: KeyCtx::new(1, her_key(), TCC, Some(28)),
            row,
            t4: None,
            dir,
        }
    }

    impl Rig {
        /// One second: three T4 reads, then T4 and the row together.
        fn step(&mut self, edit: impl FnOnce(&mut Row)) -> CheckStep {
            for _ in 0..3 {
                let at = self.tl.advance(0.25);
                self.check.t4(self.t4, &at);
            }
            let at = self.tl.advance(0.25);
            self.check.t4(self.t4, &at);
            let mut row = Row {
                at,
                ..self.row.clone()
            };
            edit(&mut row);
            let key = self.key.clone();
            self.check.row(&row, &key, &at)
        }

        fn phase(&self) -> CheckPhase {
            self.check.state.state
        }

        /// Steps until `phase`, at most `max` rows.
        fn run_to(&mut self, phase: CheckPhase, max: usize) {
            for _ in 0..max {
                if self.phase() == phase {
                    return;
                }
                self.step(|_| {});
            }
            assert_eq!(self.phase(), phase);
        }

        fn burn_pid(&self) -> i32 {
            self.check.burn.as_ref().expect("a load child").pid
        }

        fn result(&self) -> Value {
            let text = fs::read_to_string(self.check.setup.dir.join("result.json")).unwrap();
            serde_json::from_str(&text).unwrap()
        }
    }

    #[test]
    fn a_whole_check_like_the_reference() {
        let mut r = rig("check-whole", sleeper(), CheckPurpose::Reference);
        // The ring already holds a quiet, settled minute: the gates pass at the first row.
        assert!(matches!(r.step(|_| {}), CheckStep::Event(_)));
        assert_eq!(r.phase(), CheckPhase::Idle);
        assert!(r.check.state.gates.iter().all(|g| g.ok));
        r.row.t = Some(60.0);
        r.t4 = Some(60.0);
        for _ in 0..29 {
            assert!(matches!(r.step(|_| {}), CheckStep::Live(_)));
        }
        assert!(r.check.burn.is_none());
        // Load: a busy machine at 86 °C, 93 ms of throttle every 10 s with T4 at 90 °C.
        r.row = Row {
            b: Some(0.99),
            b10: Some(0.99),
            t: Some(86.0),
            fan: Some(6800),
            f_med: Some(3700),
            ..base_row()
        };
        assert!(matches!(r.step(|_| {}), CheckStep::Event(_)), "idle → load");
        assert_eq!(r.phase(), CheckPhase::Load);
        let child = r.burn_pid();
        for i in 1..=180 {
            r.t4 = Some(if i % 10 == 5 { 90.0 } else { 86.0 });
            let thr = if i % 10 == 5 { 93 } else { 0 };
            r.step(|row| row.thr_ms = Some(thr));
        }
        assert_eq!(r.phase(), CheckPhase::Recovery);
        assert!(reaped(child), "the load is killed at the end of the load phase");
        r.row = Row {
            t: Some(52.0),
            fan: Some(5000),
            ..base_row()
        };
        r.t4 = Some(52.0);
        let mut outcome = None;
        for _ in 0..90 {
            if let CheckStep::Finished(o) = r.step(|_| {}) {
                outcome = o;
                break;
            }
        }
        let o = *outcome.expect("finished with a result");
        assert_eq!(r.phase(), CheckPhase::Done);
        assert_eq!((o.purpose, o.verdict, o.post_clean), (CheckPurpose::Reference, CheckVerdict::Like, None));
        assert!(o.comparable && !o.fan_flag && o.diagnosis.is_empty());
        assert_eq!((o.key_id.as_str(), o.generator), (SEED_KEY, Some(GENERATOR_VERSION)));
        assert_eq!((o.metrics.t_ss, o.metrics.fan, o.metrics.t_idle), (Some(86.0), Some(6800.0), Some(60.0)));
        assert!(close(o.metrics.duty, 930.0 / 103_000.0), "{:?}", o.metrics.duty);
        assert_eq!((o.metrics.ths, o.metrics.djc), (Some(52.0), Some(34.0)));
        assert_eq!(o.dir, r.check.state.dir);

        // The directory: every row, the meta and the result.
        let dir = r.check.setup.dir.clone();
        let csv = fs::read_to_string(dir.join("samples.csv")).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], CSV_HEADER);
        assert_eq!(lines.len(), 1 + 30 + 180 + 90);
        assert!(lines[31].starts_with("31.0,load,0.99,86.0,86.0,86.0,"), "{}", lines[31]);
        let meta: Value = serde_json::from_str(&fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!((meta["threads"].clone(), meta["generator"].clone()), (json!(8), json!(1)));
        assert_eq!(meta["key_id"], SEED_KEY);
        let res = r.result();
        assert_eq!((res["state"].clone(), res["verdict"].clone(), res["window"].clone()), (json!("done"), json!("like"), json!("matched")));
        assert_eq!(res["comparison"]["reference"]["label"], "duo-seed 20/11/2026");
        assert_eq!(r.check.rows().as_array().unwrap().len(), 300);
        // After a restart the rows come from samples.csv, with the same JSON types.
        assert_eq!(read_samples_csv(&dir.join("samples.csv")).unwrap(), r.check.rows());
    }

    #[test]
    fn a_machine_that_never_settles_is_refused_after_5_minutes() {
        let mut r = rig("check-refused", sleeper(), CheckPurpose::Reference);
        r.row.b10 = Some(0.4);
        assert!(matches!(r.step(|_| {}), CheckStep::Event(_)), "the first gates are written");
        let mut steps = 1;
        loop {
            match r.step(|_| {}) {
                CheckStep::Live(_) => steps += 1,
                CheckStep::Finished(None) => break,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(steps, 299, "refused at 300 s");
        assert_eq!(r.phase(), CheckPhase::Refused);
        let reason = r.check.state.reason.clone().unwrap();
        assert_eq!(reason, "too busy after 5 min: B10 40 % (< 10)");
        assert!(r.check.burn.is_none());
        assert_eq!(r.result()["state"], "refused");
    }

    #[test]
    fn a_suspend_while_waiting_at_the_gates_ends_the_check() {
        let mut r = rig("check-gating-suspend", sleeper(), CheckPurpose::Reference);
        r.row.b10 = Some(0.4);
        assert!(matches!(r.step(|_| {}), CheckStep::Event(_)));
        assert_eq!(r.phase(), CheckPhase::Gating);
        // The lid closes and the machine sleeps for hours, then wakes idle and settled: the
        // load must not start by itself on a machine that may now be on her lap.
        r.tl.suspend(8.0 * 3600.0);
        r.row.b10 = Some(0.02);
        assert!(matches!(r.step(|row| row.gap = Gap::Suspend), CheckStep::Finished(None)));
        assert_eq!(r.phase(), CheckPhase::Aborted);
        assert_eq!(r.check.state.reason.as_deref(), Some("the machine was suspended"));
        assert!(r.check.burn.is_none(), "no load was started");
    }

    #[test]
    fn gate_changes_reach_state_json_at_most_every_30_s() {
        let mut r = rig("check-gate-writes", sleeper(), CheckPurpose::Reference);
        // Charging below 50 % holds the check; meanwhile the fan hovers on the settle line.
        r.row.bat_capacity = Some(40);
        r.row.bat_status = Some(BatStatus::Charging);
        let mut writes = Vec::new();
        for i in 0..120 {
            let fan = if i % 2 == 0 { 4900 } else { 5100 };
            if let CheckStep::Event(_) = r.step(|row| row.fan = Some(fan)) {
                writes.push(i);
            }
        }
        // The first gates, then a change only once 30 s have passed since the last write, and
        // only if it still differs from what was written (at 30 s the fan was back under 5000).
        assert_eq!(writes, [0, 31, 62, 93]);
        assert_eq!(r.phase(), CheckPhase::Gating);
        // Passing every gate is a phase change, written at once.
        r.row.bat_capacity = Some(80);
        assert!(matches!(r.step(|row| row.fan = Some(4900)), CheckStep::Event(_)));
        assert_eq!(r.phase(), CheckPhase::Idle);
    }

    #[test]
    fn the_dgpu_status_is_read_afresh_before_asking() {
        let mut r = rig("check-dgpu-fresh", sleeper(), CheckPurpose::Reference);
        let status = r.dir.join("runtime_status");
        fs::write(&status, "active\n").unwrap();
        r.check.setup.dgpu_status = Some(status.clone());
        r.check.gpu.tool = Some(sh("echo 37"));
        // The row still says suspended from a minute ago: the file says active, so it is asked.
        assert!(matches!(r.step(|row| row.dgpu_active = false), CheckStep::Event(_)));
        assert_eq!(r.phase(), CheckPhase::Gating);
        assert!(r.check.gpu.running.is_some() || r.check.gpu.last.is_some());
        r.check.gpu.stop();
        // Suspended since, though the row still says active: never asked, and the gate passes.
        fs::write(&status, "suspended\n").unwrap();
        r.check.gpu.last = Some((0.0, Some(37.0)));
        r.step(|row| row.dgpu_active = true);
        assert!(r.check.gpu.running.is_none(), "no query to wake it");
        assert_eq!(r.phase(), CheckPhase::Idle);
        let dgpu = r.check.state.gates.iter().find(|g| g.id == "dgpu").unwrap();
        assert_eq!(dgpu.text, "dGPU suspended");
    }

    #[test]
    fn aborts_from_the_rows() {
        type Edit = fn(&mut Row);
        let cases: [(&str, Edit, &str); 5] = [
            ("ac", |r| r.ac = Some(false), "AC lost"),
            ("battery", |r| r.bat_capacity = Some(25), "battery 25 % (below 30 %)"),
            ("suspend", |r| r.gap = Gap::Suspend, "the machine was suspended"),
            ("lid", |r| r.lid_open = Some(false), "lid closed"),
            (
                "key",
                |r| r.key_seq = 2,
                "settings changed: AC · performance · PL1 28 W · PL2 35 W · TCC 92 °C · lid open",
            ),
        ];
        for (name, edit, reason) in cases {
            let mut r = rig(&format!("check-abort-{name}"), sleeper(), CheckPurpose::Reference);
            r.step(|_| {});
            assert_eq!(r.phase(), CheckPhase::Idle);
            assert!(matches!(r.step(edit), CheckStep::Finished(None)), "{name}");
            assert_eq!(r.phase(), CheckPhase::Aborted);
            assert_eq!(r.check.state.reason.as_deref(), Some(reason));
            assert_eq!(r.result()["reason"], reason);
        }

        // 99 °C for 3 s.
        let mut r = rig("check-abort-hot", sleeper(), CheckPurpose::Reference);
        r.step(|_| {});
        r.row.t = Some(99.0);
        assert!(matches!(r.step(|_| {}), CheckStep::Live(_)));
        assert!(matches!(r.step(|_| {}), CheckStep::Live(_)));
        assert!(matches!(r.step(|_| {}), CheckStep::Finished(None)));
        assert_eq!(r.check.state.reason.as_deref(), Some("99 °C for 3 s: TCC is not holding"));
        assert!(!r.check.fan_stopped());

        // The fan at 0 rpm for 10 s while T > 80 °C; a pause at idle temperature is the EC's.
        let mut r = rig("check-abort-fan", sleeper(), CheckPurpose::Reference);
        r.step(|_| {});
        r.row.fan = Some(0);
        for _ in 0..12 {
            r.step(|_| {});
        }
        assert_eq!(r.phase(), CheckPhase::Idle, "0 rpm at 55 °C");
        r.row.t = Some(85.0);
        for _ in 0..9 {
            assert!(matches!(r.step(|_| {}), CheckStep::Live(_)));
        }
        assert!(matches!(r.step(|_| {}), CheckStep::Finished(None)));
        assert_eq!(r.check.state.reason.as_deref(), Some("fan stopped at 85 °C"));
        assert!(r.check.fan_stopped(), "this raises FAN too");
    }

    #[test]
    fn an_abort_in_the_load_kills_the_whole_group() {
        become_subreaper();
        let dir = test_dir("check-group");
        let (cmd, pid_file) = family(&dir);
        let mut r = rig("check-group-rig", cmd, CheckPurpose::Reference);
        r.run_to(CheckPhase::Load, 40);
        let child = r.burn_pid();
        let grandchild = read_pid(&pid_file);
        assert!(matches!(r.step(|row| row.ac = Some(false)), CheckStep::Finished(None)));
        assert!(reaped(child), "the load child is killed and reaped");
        assert_eq!(reap(grandchild), Some(libc::SIGKILL), "and so is everything in its group");
        assert_eq!(r.check.burn_fd(), None);
    }

    #[test]
    fn cancel_and_drop_kill_the_group() {
        become_subreaper();
        let dir = test_dir("check-cancel");
        let (cmd, pid_file) = family(&dir);
        let mut r = rig("check-cancel-rig", cmd, CheckPurpose::Verify);
        r.run_to(CheckPhase::Load, 40);
        let (child, grandchild) = (r.burn_pid(), read_pid(&pid_file));
        let now = r.tl.now();
        r.check.abort(text::CANCELLED, &now);
        assert_eq!((r.phase(), r.check.state.reason.as_deref()), (CheckPhase::Aborted, Some("cancelled")));
        assert!(reaped(child));
        assert_eq!(reap(grandchild), Some(libc::SIGKILL));
        // A second abort (the service stopping right after) changes nothing.
        r.check.abort("service stop", &now);
        assert_eq!(r.check.state.reason.as_deref(), Some("cancelled"));

        // Dropped while loading (the service going away some other way): the group still dies.
        let dir = test_dir("check-drop");
        let (cmd, pid_file) = family(&dir);
        let mut r = rig("check-drop-rig", cmd, CheckPurpose::Reference);
        r.run_to(CheckPhase::Load, 40);
        let (child, grandchild) = (r.burn_pid(), read_pid(&pid_file));
        drop(r);
        assert!(reaped(child));
        assert_eq!(reap(grandchild), Some(libc::SIGKILL));
    }

    fn wait_readable(fd: RawFd) {
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the call.
        let n = unsafe { libc::poll(&mut p, 1, 5000) };
        assert_eq!(n, 1, "the pipe never became readable");
    }

    #[test]
    fn the_burn_pipe() {
        // Lines from the child are read as they come.
        let mut r = rig(
            "check-pipe",
            sh(r#"echo '{"t":5.5,"rounds":[1048576,2097152]}'; exec /usr/bin/sleep 30"#),
            CheckPurpose::Reference,
        );
        r.run_to(CheckPhase::Load, 40);
        let child = r.burn_pid();
        wait_readable(r.check.burn_fd().unwrap());
        let now = r.tl.now();
        assert!(matches!(r.check.burn_ready(&now), CheckStep::Running));
        assert_eq!(r.check.burn_lines, [(5.5, 3_145_728)]);
        r.check.abort(text::CANCELLED, &now);
        assert!(reaped(child));

        // A load that stops on its own ends the check: there is nothing left to measure.
        let mut r = rig("check-pipe-eof", sh("exit 0"), CheckPurpose::Reference);
        r.run_to(CheckPhase::Load, 40);
        wait_readable(r.check.burn_fd().unwrap());
        let now = r.tl.now();
        assert!(matches!(r.check.burn_ready(&now), CheckStep::Finished(None)));
        assert_eq!(r.check.state.reason.as_deref(), Some(text::LOAD_STOPPED));
        assert_eq!(r.check.burn_fd(), None);

        // The same seen from the rows, before any pipe read: the exit is peeked at, and the
        // child is reaped only after its group has been signalled.
        let mut r = rig("check-pipe-exit", sh("exit 0"), CheckPurpose::Reference);
        r.run_to(CheckPhase::Load, 40);
        let child = r.burn_pid();
        wait_readable(r.check.burn_fd().unwrap());
        let mut step = r.step(|_| {});
        for _ in 0..250 {
            if !matches!(step, CheckStep::Live(_)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            step = r.step(|_| {});
        }
        assert!(matches!(step, CheckStep::Finished(None)), "{step:?}");
        assert_eq!(r.check.state.reason.as_deref(), Some(text::LOAD_STOPPED));
        assert!(reaped(child));

        // One that cannot start at all.
        let missing = Cmd {
            program: r.dir.join("no-such-burn"),
            args: Vec::new(),
        };
        let mut r = rig("check-pipe-missing", missing, CheckPurpose::Reference);
        r.run_to(CheckPhase::Aborted, 40);
        assert!(r.check.state.reason.as_deref().unwrap().starts_with("the load generator did not start: "));
    }

    #[test]
    fn the_dgpu_query() {
        let reading = |q: &mut GpuQuery, active: bool, t: f64| {
            let mut d = q.poll(active, t);
            for _ in 0..250 {
                if d != Dgpu::Asking {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
                d = q.poll(active, t);
            }
            d
        };
        let mut q = GpuQuery {
            tool: Some(sh("echo 37")),
            running: None,
            last: None,
        };
        assert_eq!(q.poll(false, 0.0), Dgpu::Suspended, "a suspended dGPU is never asked");
        assert!(q.running.is_none());
        assert_eq!(reading(&mut q, true, 0.0), Dgpu::Busy(37.0));
        // Busy: asked again after 30 s, not before.
        q.tool = Some(sh("echo 4"));
        assert_eq!(q.poll(true, 20.0), Dgpu::Busy(37.0));
        assert!(q.running.is_none());
        let _ = q.poll(true, 40.0);
        let mut d = Dgpu::Busy(37.0);
        for _ in 0..250 {
            d = q.poll(true, 40.0);
            if d != Dgpu::Busy(37.0) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(d, Dgpu::Busy(4.0));
        // A tool that hangs is killed after 10 s and counts as no reading.
        let mut q = GpuQuery {
            tool: Some(sleeper()),
            running: None,
            last: None,
        };
        assert_eq!(q.poll(true, 0.0), Dgpu::Asking);
        let pid = q.running.as_ref().unwrap().0.id() as i32;
        assert_eq!(q.poll(true, 10.0), Dgpu::NoReading);
        assert!(q.running.is_none(), "given up on at once");
        // Killed but not waited for, which an nvidia-smi stuck in the driver would make the
        // service do for as long as it took: it is reaped by a later row once it has died.
        let mut gone = false;
        for _ in 0..250 {
            reap_orphans();
            if reaped(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone, "the abandoned query is reaped");
        let mut none = GpuQuery::default();
        assert_eq!(none.poll(true, 0.0), Dgpu::NoTool);
    }

    #[test]
    fn the_load_is_this_very_binary() {
        let c = Cmd::burn(8);
        assert_eq!(c.program, Path::new("/proc/self/exe"));
        assert_eq!(c.args, ["burn", "--threads", "8"]);
    }

    #[test]
    fn check_directories() {
        let dir = test_dir("check-dirs");
        let now = Timeline::starting(Date::new(2026, 12, 5).unwrap(), 11, 2, 30, 3600).now();
        let (a, pa) = make_dir(&dir.join("checks"), &now).unwrap();
        let (b, _) = make_dir(&dir.join("checks"), &now).unwrap();
        // Named by the local time.
        assert_eq!((a.as_str(), b.as_str()), ("checks/2026-12-05_1102", "checks/2026-12-05_1102-2"));
        assert!(pa.is_dir());
    }
}
