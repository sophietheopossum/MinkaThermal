//! One row per tick (§3.2, §3.3).
//!
//! The work is split in two. `Sampler` does the I/O: it decides which sensors are due this tick
//! and reads them into a `Raw`. `Deriver` is pure: it turns a `Raw` into a `Row` (deltas, rolling
//! means, suspend and stall detection, counter rebasing, the config key). Tests and replays drive
//! the `Deriver` with synthetic readings on a `Timeline`, so every rule above it can be tested
//! without sensors or load.

use crate::clock::Moment;
use crate::profile::ConfigKey;
use crate::sensors::{BatStatus, ConfigRead, Context, CpuTimes, PowerRead, Sensors, XeRead};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

/// BOOTTIME − MONOTONIC grew by more than this: the machine was suspended.
pub const SUSPEND_NS: i64 = 2_000_000_000;
/// A MONOTONIC gap this much longer than the scheduled interval is a stall (3 s at 1 Hz).
pub const STALL_SLACK_S: f64 = 2.0;
/// Rows kept for backdating episode onsets and for the dip guard.
pub const RING_S: f64 = 900.0;
/// Frequency is read only at this busy fraction or above, where the plateau rules use it.
pub const FREQ_BUSY: f64 = 0.80;
pub const POWER_EVERY_S: f64 = 5.0;
pub const FAN_SLOW_EVERY_S: f64 = 5.0;
pub const POLL_EVERY_S: f64 = 60.0;
/// The /proc/*/comm walk that finds the bch-reconcile threads and thermald (§3.2): every
/// 10 min and at episode edges. It costs a few ms, as much as the rest of an idle minute.
pub const WALK_EVERY_S: f64 = 600.0;
/// After a read error, discovery runs again, but no more often than this: one sensor that keeps
/// failing must not turn every tick into a sysfs walk.
pub const REDISCOVER_MIN_S: f64 = 10.0;

/// Why a row does not continue the previous one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Gap {
    #[default]
    None,
    /// The first row after start.
    First,
    Suspend,
    Stall,
}

impl Gap {
    /// Suspend and stall void open episodes and windows; the first row only starts them.
    pub fn voids(&self) -> bool {
        matches!(self, Gap::Suspend | Gap::Stall)
    }
}

/// Classifies the step from `prev` to `now` against the interval the loop scheduled.
pub fn gap_between(prev: &Moment, now: &Moment, expected_s: f64) -> Gap {
    if now.asleep_ns() - prev.asleep_ns() > SUSPEND_NS {
        return Gap::Suspend;
    }
    let dt = (now.mono_ns - prev.mono_ns) as f64 / 1e9;
    if dt > expected_s + STALL_SLACK_S {
        Gap::Stall
    } else {
        Gap::None
    }
}

/// How often the loop ticks (§3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cadence {
    /// Full rows.
    pub row_ms: u64,
    /// The extra `x86_pkg_temp` reads, during a check only.
    pub t4_ms: Option<u64>,
}

/// 1 Hz on AC; 0.2 Hz on battery, back to 1 Hz while T ≥ 85 °C so the alarms can time 5 s; full
/// rows at 1 Hz plus T4 at 4 Hz during a check. An unknown AC state counts as AC.
pub fn cadence(ac: Option<bool>, t: Option<f64>, check_running: bool) -> Cadence {
    if check_running {
        return Cadence {
            row_ms: 1000,
            t4_ms: Some(250),
        };
    }
    let row_ms = if ac == Some(false) && !t.is_some_and(|t| t >= 85.0) {
        5000
    } else {
        1000
    };
    Cadence {
        row_ms,
        t4_ms: None,
    }
}

/// Whether the fan is due (§3.2): every tick when B10 ≥ 0.3, T ≥ 75 °C, or a plateau or check is
/// open; otherwise every 5 s. The asus fan read is an EC call over WMI, 131 µs, so it is spared
/// at idle.
pub fn fan_due(
    now_s: f64,
    last_s: Option<f64>,
    b10: Option<f64>,
    t: Option<f64>,
    detail: bool,
) -> bool {
    let fast = detail || b10.is_some_and(|b| b >= 0.3) || t.is_some_and(|t| t >= 75.0);
    due(now_s, last_s, if fast { 0.0 } else { FAN_SLOW_EVERY_S })
}

/// Due when `every` seconds have passed, with half a second of slack for tick jitter.
fn due(now_s: f64, last_s: Option<f64>, every: f64) -> bool {
    last_s.is_none_or(|l| now_s - l >= every - 0.5)
}

/// The busy fraction between two /proc/stat readings; None when a counter went backwards or no
/// time passed.
pub fn busy_fraction(prev: &CpuTimes, cur: &CpuTimes) -> Option<f64> {
    let total = cur.total.checked_sub(prev.total)?;
    let idle = cur.idle.checked_sub(prev.idle)?;
    if total == 0 || idle > total {
        return None;
    }
    Some(1.0 - idle as f64 / total as f64)
}

/// A cumulative counter turned into per-row deltas. A reading lower than the last one is rebased:
/// that row gets no delta and the new value becomes the base.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counter {
    base: Option<u64>,
}

impl Counter {
    pub fn delta(&mut self, v: Option<u64>) -> Option<u64> {
        let Some(v) = v else {
            // A missed read breaks the chain: the next delta would span two rows.
            self.base = None;
            return None;
        };
        let d = self.base.and_then(|b| v.checked_sub(b));
        self.base = Some(v);
        d
    }

    pub fn rebase(&mut self) {
        self.base = None;
    }
}

/// A Δt-weighted rolling mean over the last `window_s` seconds. Each value covers the interval
/// (end − dt, end]; a value only partly inside the window counts for the part inside. Rows of
/// different lengths (1 s on AC, 5 s on battery, a late tick) therefore weigh by the time they
/// cover, never by count.
#[derive(Clone, Debug)]
pub struct Rolling {
    window_s: f64,
    items: VecDeque<(f64, f64, Option<f64>)>,
}

impl Rolling {
    pub fn new(window_s: f64) -> Rolling {
        Rolling {
            window_s,
            items: VecDeque::new(),
        }
    }

    /// Adds the interval ending at `end_s` and lasting `dt_s`. A None value covers time without
    /// counting towards the mean.
    pub fn push(&mut self, end_s: f64, dt_s: f64, value: Option<f64>) {
        self.items.push_back((end_s, dt_s.max(0.0), value));
        let start = end_s - self.window_s;
        while self.items.front().is_some_and(|&(end, _, _)| end <= start) {
            self.items.pop_front();
        }
    }

    pub fn mean(&self, now_s: f64) -> Option<f64> {
        let start = now_s - self.window_s;
        let (mut sum, mut weight) = (0.0, 0.0);
        for &(end, dt, value) in &self.items {
            let Some(v) = value else { continue };
            let overlap = end.min(now_s) - (end - dt).max(start);
            if overlap > 0.0 {
                sum += v * overlap;
                weight += overlap;
            }
        }
        (weight > 0.0).then(|| sum / weight)
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}

/// The readings of one tick. Fields left at their defaults were not due this tick.
#[derive(Clone, Debug, Default)]
pub struct Raw {
    pub at: Moment,
    pub cpu: Option<CpuTimes>,
    pub t: Option<f64>,
    pub pkg_thr_ms: Option<u64>,
    pub pkg_thr_count: Option<u64>,
    /// Set when the fan was read this tick; `fan` is then the reading.
    pub fan_read: bool,
    pub fan: Option<u32>,
    /// Per-CPU MHz, only at b ≥ 0.80.
    pub freqs: Vec<u32>,
    pub xe: Option<XeRead>,
    pub xe_act: Option<u32>,
    pub power: Option<PowerRead>,
    pub config: Option<ConfigRead>,
    /// Only while a plateau or check is open.
    pub core_t: Vec<Option<f64>>,
    pub core_thr_ms: Vec<Option<u64>>,
    pub context: Option<Context>,
    /// Cumulative bch-reconcile CPU ticks, when polled.
    pub reconcile_ticks: Option<u64>,
}

/// One tick, as the episode and rule code sees it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Row {
    pub at: Moment,
    /// Measured MONOTONIC seconds since the previous row (0 for the first). Every rate divides
    /// by this, never by a count of rows.
    pub dt: f64,
    pub gap: Gap,
    /// Busy fraction over `dt`; None on the first row, after a gap or a counter going back.
    pub b: Option<f64>,
    pub b5: Option<f64>,
    pub b10: Option<f64>,
    /// Package temperature, °C.
    pub t: Option<f64>,
    /// Package throttle time during `dt`, ms.
    pub thr_ms: Option<u64>,
    pub thr_count: Option<u64>,
    /// The latest fan reading (it may be up to 5 s old at idle); `fan_read` marks a fresh one.
    pub fan: Option<u32>,
    pub fan_read: bool,
    /// Median frequency across CPUs, MHz, when b ≥ 0.80.
    pub f_med: Option<u32>,
    pub xe: Option<XeRead>,
    pub xe_act: Option<u32>,
    pub ac: Option<bool>,
    pub deficit: bool,
    pub bat_status: Option<BatStatus>,
    pub bat_capacity: Option<u8>,
    /// Bumped whenever the config key changes; `Deriver::key` holds the key itself.
    pub key_seq: u32,
    pub lid_open: Option<bool>,
    /// A `Processor` or `intel_powerclamp` cooling device above 0.
    pub cooling_active: bool,
    pub thermald: bool,
    /// The dGPU's runtime_status is `active`: the check asks nvidia-smi how busy it is.
    pub dgpu_active: bool,
    pub config_read: bool,
    pub core_t: Vec<Option<f64>>,
    pub core_thr_ms: Vec<Option<u64>>,
    pub context: Option<Context>,
    /// bch-reconcile's share of the busy CPU time since the previous poll.
    pub reconcile_share: Option<f64>,
}

/// The pure half of the sampler.
#[derive(Clone, Debug)]
pub struct Deriver {
    prev: Option<Moment>,
    cpu: Option<CpuTimes>,
    /// Busy ticks summed over continuous rows, the denominator of shares.
    busy_ticks: u64,
    pkg_ms: Counter,
    pkg_count: Counter,
    core_ms: Vec<Counter>,
    b5: Rolling,
    b10: Rolling,
    fan: Option<u32>,
    power: Option<PowerRead>,
    config: Option<ConfigRead>,
    key: Option<ConfigKey>,
    key_seq: u32,
    /// (reconcile ticks, busy ticks) at the previous reconcile poll.
    reconcile: Option<(u64, u64)>,
}

impl Default for Deriver {
    fn default() -> Self {
        Deriver {
            prev: None,
            cpu: None,
            busy_ticks: 0,
            pkg_ms: Counter::default(),
            pkg_count: Counter::default(),
            core_ms: Vec::new(),
            b5: Rolling::new(5.0),
            b10: Rolling::new(10.0),
            fan: None,
            power: None,
            config: None,
            key: None,
            key_seq: 0,
            reconcile: None,
        }
    }
}

impl Deriver {
    pub fn gap(&self, at: &Moment, expected_s: f64) -> Gap {
        self.prev
            .map_or(Gap::First, |p| gap_between(&p, at, expected_s))
    }

    /// The busy fraction this reading would give, without consuming it.
    pub fn preview_busy(&self, cpu: Option<CpuTimes>) -> Option<f64> {
        busy_fraction(self.cpu.as_ref()?, cpu.as_ref()?)
    }

    pub fn key(&self) -> Option<&ConfigKey> {
        self.key.as_ref()
    }

    pub fn config(&self) -> Option<&ConfigRead> {
        self.config.as_ref()
    }

    pub fn power(&self) -> Option<&PowerRead> {
        self.power.as_ref()
    }

    pub fn busy_ticks(&self) -> u64 {
        self.busy_ticks
    }

    /// Forgets every baseline, so the next readings start fresh (after a suspend or stall).
    fn rebase(&mut self) {
        self.cpu = None;
        self.pkg_ms.rebase();
        self.pkg_count.rebase();
        self.core_ms.clear();
        self.b5.clear();
        self.b10.clear();
        self.reconcile = None;
    }

    pub fn derive(&mut self, raw: Raw, expected_s: f64) -> Row {
        let gap = self.gap(&raw.at, expected_s);
        let dt = self
            .prev
            .map_or(0.0, |p| (raw.at.mono_ns - p.mono_ns) as f64 / 1e9);
        if gap != Gap::None {
            self.rebase();
        }
        self.prev = Some(raw.at);
        let end = raw.at.mono_s();

        let mut b = None;
        match (self.cpu, raw.cpu) {
            (Some(prev), Some(cur)) => {
                b = busy_fraction(&prev, &cur);
                if b.is_some() {
                    self.busy_ticks += cur.busy().saturating_sub(prev.busy());
                }
                self.cpu = Some(cur);
            }
            (None, cur) => self.cpu = cur,
            (Some(_), None) => self.cpu = None,
        }
        self.b5.push(end, dt, b);
        self.b10.push(end, dt, b);

        if raw.fan_read {
            self.fan = raw.fan;
        }
        let core_thr_ms = if raw.core_thr_ms.is_empty() {
            // Not read this tick: the next read starts a new chain instead of spanning the pause.
            self.core_ms.clear();
            Vec::new()
        } else {
            self.core_ms
                .resize(raw.core_thr_ms.len(), Counter::default());
            self.core_ms
                .iter_mut()
                .zip(&raw.core_thr_ms)
                .map(|(c, v)| c.delta(*v))
                .collect()
        };

        // A read that failed keeps the last good value: one flicker would otherwise change the
        // key twice and void the open plateau, and a failed AC read would count battery time as
        // AC. A file that is gone for good does change the key (see `Unread`).
        let mut key_inputs = false;
        if let Some(mut p) = raw.power {
            if p.ac.is_none() {
                p.ac = self.power.as_ref().and_then(|q| q.ac);
            }
            self.power = Some(p);
            key_inputs = true;
        }
        let config_read = raw.config.is_some();
        if let Some(mut c) = raw.config {
            if let Some(prev) = &self.config {
                c.carry_unread(prev);
            }
            self.config = Some(c);
            key_inputs = true;
        }
        if key_inputs && let Some(cfg) = &self.config {
            let key = ConfigKey::new(self.power.as_ref().and_then(|p| p.ac), cfg);
            if self.key.as_ref() != Some(&key) {
                self.key = Some(key);
                self.key_seq += 1;
            }
        }

        let reconcile_share = raw.reconcile_ticks.and_then(|ticks| {
            let share = self.reconcile.and_then(|(r0, b0)| {
                let busy = self.busy_ticks.checked_sub(b0).filter(|&d| d > 0)?;
                Some((ticks.checked_sub(r0)? as f64 / busy as f64).min(1.0))
            });
            self.reconcile = Some((ticks, self.busy_ticks));
            share
        });

        let power = self.power.as_ref();
        let config = self.config.as_ref();
        let mut freqs = raw.freqs;
        Row {
            at: raw.at,
            dt,
            gap,
            b,
            b5: self.b5.mean(end),
            b10: self.b10.mean(end),
            t: raw.t,
            thr_ms: self.pkg_ms.delta(raw.pkg_thr_ms),
            thr_count: self.pkg_count.delta(raw.pkg_thr_count),
            fan: self.fan,
            fan_read: raw.fan_read,
            f_med: median(&mut freqs),
            xe: raw.xe,
            xe_act: raw.xe_act,
            ac: power.and_then(|p| p.ac),
            deficit: power.is_some_and(PowerRead::deficit),
            bat_status: power.and_then(|p| p.bat_status),
            bat_capacity: power.and_then(|p| p.capacity),
            key_seq: self.key_seq,
            lid_open: config.and_then(|c| c.lid_open),
            cooling_active: config.and_then(|c| c.cooling_max).is_some_and(|s| s > 0),
            thermald: config.is_some_and(|c| c.thermald),
            dgpu_active: config.and_then(|c| c.dgpu_runtime.as_deref()) == Some("active"),
            config_read,
            core_t: raw.core_t,
            core_thr_ms,
            context: raw.context,
            reconcile_share,
        }
    }
}

/// The median of a few integers; the mean of the middle two for an even count.
pub fn median(values: &mut [u32]) -> Option<u32> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        ((values[n / 2 - 1] as u64 + values[n / 2] as u64) / 2) as u32
    })
}

/// The last 900 s of rows. MONOTONIC stands still during a suspend, so rows from before one can
/// still be inside the window: anything scanning back stops at a row whose gap `voids()`.
#[derive(Clone, Debug, Default)]
pub struct Ring {
    rows: VecDeque<Row>,
}

impl Ring {
    pub fn push(&mut self, row: Row) {
        let newest = row.at.mono_s();
        self.rows.push_back(row);
        while self
            .rows
            .front()
            .is_some_and(|r| r.at.mono_s() < newest - RING_S)
        {
            self.rows.pop_front();
        }
    }

    pub fn rows(&self) -> &VecDeque<Row> {
        &self.rows
    }

    pub fn last(&self) -> Option<&Row> {
        self.rows.back()
    }
}

/// CPU time per process from /proc/*/stat, for "mostly rustc".
#[derive(Clone, Debug, Default)]
pub struct ProcSnapshot {
    pub procs: HashMap<i32, (String, u64)>,
}

/// Parses /proc/<pid>/stat into (comm, utime + stime). The comm can hold spaces and parentheses,
/// so the fields are counted from the last ')'.
pub fn parse_stat(text: &str) -> Option<(String, u64)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_owned();
    let mut fields = text.get(close + 1..)?.split_ascii_whitespace();
    // After the comm: state is field 3, utime field 14, stime field 15.
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some((comm, utime + stime))
}

fn pids(proc: &Path) -> impl Iterator<Item = (i32, PathBuf)> {
    fs::read_dir(proc).into_iter().flatten().filter_map(|e| {
        let e = e.ok()?;
        let pid = e.file_name().to_str()?.parse().ok()?;
        Some((pid, e.path()))
    })
}

/// A scan of /proc/*/stat (5-15 ms): at episode edges and at the end of a hot light window only.
pub fn proc_snapshot(proc: &Path) -> ProcSnapshot {
    ProcSnapshot {
        procs: pids(proc)
            .filter_map(|(pid, dir)| {
                Some((
                    pid,
                    parse_stat(&fs::read_to_string(dir.join("stat")).ok()?)?,
                ))
            })
            .collect(),
    }
}

/// The top `n` consumers between two snapshots, as (comm, share of the CPU time consumed by all
/// processes), summed by name. A process new in `after` counts with its whole CPU time; one that
/// exited in between is not seen, which undercounts short-lived compiler processes.
pub fn top_consumers(before: &ProcSnapshot, after: &ProcSnapshot, n: usize) -> Vec<(String, f64)> {
    let mut by_comm: HashMap<&str, u64> = HashMap::new();
    let mut total = 0u64;
    for (pid, (comm, ticks)) in &after.procs {
        let d = match before.procs.get(pid) {
            Some((_, prev)) if prev <= ticks => ticks - prev,
            _ => *ticks,
        };
        if d > 0 {
            *by_comm.entry(comm).or_default() += d;
            total += d;
        }
    }
    let mut top: Vec<(String, f64)> = by_comm
        .into_iter()
        .map(|(c, d)| (c.to_owned(), d as f64 / total as f64))
        .collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(n);
    top
}

/// Thread-group leaders and their names, from /proc/*/comm. Every kernel thread is its own leader,
/// so bch-reconcile shows up here.
pub fn scan_comms(proc: &Path) -> Vec<(i32, String)> {
    pids(proc)
        .filter_map(|(pid, dir)| {
            Some((
                pid,
                fs::read_to_string(dir.join("comm")).ok()?.trim().to_owned(),
            ))
        })
        .collect()
}

/// Whether `pid` still exists under the name `comm`.
fn comm_is(proc: &Path, pid: i32, comm: &str) -> bool {
    fs::read_to_string(proc.join(pid.to_string()).join("comm")).is_ok_and(|c| c.trim() == comm)
}

/// thermald running as its systemd unit: the unit's cgroup has a process. One small read, so
/// the minute poll sees it start without walking /proc.
fn thermald_unit_running(root: &Path) -> bool {
    fs::read_to_string(root.join("sys/fs/cgroup/system.slice/thermald.service/cgroup.procs"))
        .is_ok_and(|p| !p.trim().is_empty())
}

/// bch-reconcile's cumulative CPU time over the threads seen so far.
#[derive(Clone, Debug, Default)]
struct Reconcile {
    last: HashMap<i32, u64>,
    total: u64,
}

impl Reconcile {
    fn set_pids(&mut self, pids: &[i32]) {
        self.last.retain(|pid, _| pids.contains(pid));
        for &pid in pids {
            self.last.entry(pid).or_insert(u64::MAX);
        }
    }

    /// Adds the time each known thread used since the last read. A thread seen for the first
    /// time contributes from now on.
    fn read(&mut self, proc: &Path) -> u64 {
        let mut gone = Vec::new();
        for (pid, last) in &mut self.last {
            match fs::read_to_string(proc.join(pid.to_string()).join("stat"))
                .ok()
                .and_then(|t| parse_stat(&t))
            {
                Some((_, ticks)) => {
                    if *last != u64::MAX && ticks >= *last {
                        self.total += ticks - *last;
                    }
                    *last = ticks;
                }
                None => gone.push(*pid),
            }
        }
        for pid in gone {
            self.last.remove(&pid);
        }
        self.total
    }
}

/// Cumulative bch-reconcile and busy ticks at one instant; two marks give the share between them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileMark {
    pub reconcile: u64,
    pub busy: u64,
}

impl ReconcileMark {
    pub fn share_since(&self, earlier: &ReconcileMark) -> Option<f64> {
        let busy = self.busy.checked_sub(earlier.busy).filter(|&d| d > 0)?;
        Some((self.reconcile.checked_sub(earlier.reconcile)? as f64 / busy as f64).min(1.0))
    }
}

/// The I/O half: owns the sensors, decides what is due and keeps the ring.
#[derive(Debug)]
pub struct Sampler {
    pub sensors: Sensors,
    pub deriver: Deriver,
    pub ring: Ring,
    /// Set while a plateau or check is open: core temperatures, core throttle and a 1 Hz fan.
    pub detail: bool,
    /// The config poll is wanted on the next row, and with `walk` the /proc walk too.
    edge: bool,
    walk: bool,
    discovered_s: f64,
    last_fan_s: Option<f64>,
    last_power_s: Option<f64>,
    last_poll_s: Option<f64>,
    last_walk_s: Option<f64>,
    /// The thermald the last walk found, checked again at each poll between walks.
    thermald_pid: Option<i32>,
    reconcile: Reconcile,
}

impl Sampler {
    pub fn new(root: &Path) -> Sampler {
        Sampler {
            sensors: Sensors::discover(root),
            deriver: Deriver::default(),
            ring: Ring::default(),
            detail: false,
            edge: false,
            walk: false,
            discovered_s: f64::NEG_INFINITY,
            last_fan_s: None,
            last_power_s: None,
            last_poll_s: None,
            last_walk_s: None,
            thermald_pid: None,
            reconcile: Reconcile::default(),
        }
    }

    fn proc(&self) -> PathBuf {
        self.sensors.root.join("proc")
    }

    /// Discovers every sensor again (after a resume, a stall or a read error).
    pub fn rediscover(&mut self, now_s: f64) {
        self.sensors = Sensors::discover(&self.sensors.root.clone());
        self.discovered_s = now_s;
    }

    /// Asks for the config, lid and bch-reconcile reads on the next row, and the /proc walk
    /// with them: an episode is opening or closing (§3.2).
    pub fn request_edge(&mut self) {
        self.edge = true;
        self.walk = true;
    }

    /// Asks for the config and lid reads alone on the next row: a check's gates and phases,
    /// and its 5-s lid watch, which need no walk.
    pub fn request_config(&mut self) {
        self.edge = true;
    }

    /// Reads one row now. `expected_s` is the interval the loop slept for, which sets the stall line.
    pub fn read(&mut self, expected_s: f64) -> &Row {
        self.read_at(Moment::now(), expected_s)
    }

    /// Reads one row stamped `at`. The clocks are a parameter so tests can drive a fake tree.
    pub fn read_at(&mut self, at: Moment, expected_s: f64) -> &Row {
        let now = at.mono_s();
        let gap = self.deriver.gap(&at, expected_s);
        if gap.voids() || (self.sensors.failed() && now - self.discovered_s >= REDISCOVER_MIN_S) {
            self.rediscover(now);
        }
        if gap != Gap::None {
            self.last_fan_s = None;
            self.last_power_s = None;
            self.last_poll_s = None;
            self.last_walk_s = None;
        }

        let s = &self.sensors;
        let mut raw = Raw {
            at,
            cpu: s.cpu_times(),
            t: s.temp_c(),
            pkg_thr_ms: s.pkg_throttle_ms(),
            pkg_thr_count: s.pkg_throttle_count(),
            xe: s.xe(),
            xe_act: s.xe_act_mhz(),
            ..Raw::default()
        };
        if gap == Gap::None
            && self
                .deriver
                .preview_busy(raw.cpu)
                .is_some_and(|b| b >= FREQ_BUSY)
        {
            raw.freqs = s.freqs_mhz();
        }
        let b10 = self.ring.last().and_then(|r| r.b10);
        if fan_due(now, self.last_fan_s, b10, raw.t, self.detail) {
            raw.fan = s.fan_rpm();
            raw.fan_read = true;
            self.last_fan_s = Some(now);
        }
        if due(now, self.last_power_s, POWER_EVERY_S) {
            raw.power = Some(s.power());
            self.last_power_s = Some(now);
        }
        if self.detail {
            raw.core_t = s.core_temps_c();
            raw.core_thr_ms = s.core_throttle_ms();
        }
        if self.edge || due(now, self.last_poll_s, POLL_EVERY_S) {
            let mut config = s.config();
            raw.context = Some(s.context());
            let proc = self.proc();
            if self.walk || due(now, self.last_walk_s, WALK_EVERY_S) {
                let comms = scan_comms(&proc);
                self.thermald_pid = comms.iter().find(|(_, c)| c == "thermald").map(|(p, _)| *p);
                let recon: Vec<i32> = comms
                    .iter()
                    .filter(|(_, c)| c.starts_with("bch-reconcile"))
                    .map(|(p, _)| *p)
                    .collect();
                self.reconcile.set_pids(&recon);
                self.last_walk_s = Some(now);
                self.walk = false;
            } else {
                self.thermald_pid = self.thermald_pid.filter(|&p| comm_is(&proc, p, "thermald"));
            }
            config.thermald =
                self.thermald_pid.is_some() || thermald_unit_running(&self.sensors.root);
            // The known threads' own stat files: a handful of reads, every poll.
            raw.reconcile_ticks = Some(self.reconcile.read(&proc));
            raw.config = Some(config);
            self.last_poll_s = Some(now);
            self.edge = false;
        }
        let row = self.deriver.derive(raw, expected_s);
        self.ring.push(row);
        self.ring.last().expect("just pushed")
    }

    /// `x86_pkg_temp` alone, for the 4 Hz check reads.
    pub fn read_t4(&self) -> Option<f64> {
        self.sensors.t4_c()
    }

    /// bch-reconcile and busy ticks now, for an episode edge.
    pub fn reconcile_mark(&mut self) -> ReconcileMark {
        let proc = self.proc();
        ReconcileMark {
            reconcile: self.reconcile.read(&proc),
            busy: self.deriver.busy_ticks(),
        }
    }

    /// A /proc/*/stat snapshot for `top_consumers`.
    pub fn snapshot(&self) -> ProcSnapshot {
        proc_snapshot(&self.proc())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Date, Timeline};
    use crate::sensors::{Unread, fake};
    use crate::testutil::{put, test_dir};

    fn timeline() -> Timeline {
        Timeline::starting(Date::new(2026, 11, 20).unwrap(), 16, 50, 0, 0)
    }

    fn cpu(busy: u64, idle: u64) -> Option<CpuTimes> {
        Some(CpuTimes {
            total: busy + idle,
            idle,
        })
    }

    #[test]
    fn rolling_means_weigh_by_time() {
        let mut r = Rolling::new(10.0);
        // 8 s at 1.0 then one 4-s row at 0.0: 6 of the window's 10 s are at 1.0.
        let mut t = 0.0;
        for _ in 0..8 {
            t += 1.0;
            r.push(t, 1.0, Some(1.0));
        }
        t += 4.0;
        r.push(t, 4.0, Some(0.0));
        let m = r.mean(t).unwrap();
        assert!((m - 0.6).abs() < 1e-9, "{m}");
        // A count-weighted mean would say 8/9; the window must not.
        assert!((m - 8.0 / 9.0).abs() > 0.1);

        // A row with no value covers time without dragging the mean.
        t += 1.0;
        r.push(t, 1.0, None);
        let m = r.mean(t).unwrap();
        // Window (3, 13]: 1.0 over (3, 8], 0.0 over (8, 12]: 5/9.
        assert!((m - 5.0 / 9.0).abs() < 1e-9, "{m}");

        let mut r = Rolling::new(5.0);
        assert_eq!(r.mean(1.0), None);
        r.push(10.0, 1.0, None);
        assert_eq!(r.mean(10.0), None);
    }

    #[test]
    fn busy_and_rolling_through_the_deriver() {
        let mut tl = timeline();
        let mut d = Deriver::default();
        let row = d.derive(
            Raw {
                at: tl.now(),
                cpu: cpu(0, 0),
                ..Raw::default()
            },
            1.0,
        );
        assert_eq!(row.gap, Gap::First);
        assert_eq!(row.b, None);
        // 8 CPUs × 100 ticks a second: 600 busy, 200 idle is b = 0.75.
        let mut busy = 0;
        let mut idle = 0;
        let mut last = None;
        for _ in 0..12 {
            busy += 600;
            idle += 200;
            last = Some(d.derive(
                Raw {
                    at: tl.advance(1.0),
                    cpu: cpu(busy, idle),
                    ..Raw::default()
                },
                1.0,
            ));
        }
        let row = last.unwrap();
        assert_eq!(row.gap, Gap::None);
        assert_eq!(row.dt, 1.0);
        assert_eq!(row.b, Some(0.75));
        assert_eq!(row.b10, Some(0.75));
        assert_eq!(d.busy_ticks(), 12 * 600);
    }

    #[test]
    fn suspend_and_stall() {
        let prev = timeline().now();
        let mut tl = timeline();
        // A late tick is not a stall: 1 s scheduled, 2.9 s measured.
        assert_eq!(gap_between(&prev, &tl.advance(2.9), 1.0), Gap::None);
        let mut tl2 = timeline();
        assert_eq!(gap_between(&prev, &tl2.advance(3.5), 1.0), Gap::Stall);
        // On battery the loop sleeps 5 s: 6 s is normal there.
        let mut tl3 = timeline();
        assert_eq!(gap_between(&prev, &tl3.advance(6.0), 5.0), Gap::None);
        // A suspend leaves MONOTONIC almost where it was and moves BOOTTIME.
        let mut tl4 = timeline();
        tl4.suspend(3600.0);
        assert_eq!(gap_between(&prev, &tl4.advance(1.0), 1.0), Gap::Suspend);
        // A short blip of BOOTTIME is not a suspend.
        let mut tl5 = timeline();
        tl5.suspend(1.5);
        assert_eq!(gap_between(&prev, &tl5.advance(1.0), 1.0), Gap::None);

        // Through the deriver: both rebase the counters and clear the rolling means.
        let mut tl = timeline();
        let mut d = Deriver::default();
        let raw = |at, busy, ms| Raw {
            at,
            cpu: cpu(busy, 100),
            pkg_thr_ms: Some(ms),
            ..Raw::default()
        };
        d.derive(raw(tl.now(), 100, 10), 1.0);
        let r = d.derive(raw(tl.advance(1.0), 200, 15), 1.0);
        assert_eq!((r.thr_ms, r.b10.is_some()), (Some(5), true));
        tl.suspend(600.0);
        let r = d.derive(raw(tl.advance(1.0), 300, 20), 1.0);
        assert_eq!(r.gap, Gap::Suspend);
        assert!(r.gap.voids());
        assert_eq!((r.b, r.b10, r.thr_ms), (None, None, None));
        let r = d.derive(raw(tl.advance(1.0), 400, 27), 1.0);
        assert_eq!((r.gap, r.thr_ms), (Gap::None, Some(7)));
        assert_eq!(r.b, Some(1.0));
        let r = d.derive(raw(tl.advance(4.0), 500, 30), 1.0);
        assert_eq!((r.gap, r.thr_ms, r.b), (Gap::Stall, None, None));
        assert_eq!(r.dt, 4.0);
    }

    #[test]
    fn counters_that_go_back_are_rebased() {
        let mut c = Counter::default();
        assert_eq!(c.delta(Some(100)), None);
        assert_eq!(c.delta(Some(130)), Some(30));
        assert_eq!(c.delta(Some(5)), None, "a reset counter gives no delta");
        assert_eq!(c.delta(Some(9)), Some(4));
        assert_eq!(c.delta(None), None);
        assert_eq!(c.delta(Some(20)), None, "a missed read breaks the chain");
        assert_eq!(c.delta(Some(21)), Some(1));

        // /proc/stat going back (iowait can) gives no busy value for that row only.
        let mut tl = timeline();
        let mut d = Deriver::default();
        d.derive(
            Raw {
                at: tl.now(),
                cpu: cpu(1000, 1000),
                ..Raw::default()
            },
            1.0,
        );
        let r = d.derive(
            Raw {
                at: tl.advance(1.0),
                cpu: cpu(1100, 990),
                ..Raw::default()
            },
            1.0,
        );
        assert_eq!(r.b, None);
        let r = d.derive(
            Raw {
                at: tl.advance(1.0),
                cpu: cpu(1200, 1090),
                ..Raw::default()
            },
            1.0,
        );
        assert_eq!(r.b, Some(0.5));
    }

    #[test]
    fn key_changes_bump_the_sequence() {
        let mut tl = timeline();
        let mut d = Deriver::default();
        let cfg = ConfigRead {
            platform_profile: Some("performance".into()),
            pl1_uw: Some(28_000_000),
            ..ConfigRead::default()
        };
        let power = |ac| PowerRead {
            ac: Some(ac),
            ..PowerRead::default()
        };
        let r = d.derive(
            Raw {
                at: tl.now(),
                power: Some(power(true)),
                config: Some(cfg.clone()),
                ..Raw::default()
            },
            1.0,
        );
        assert_eq!(r.key_seq, 1);
        assert!(r.config_read);
        // The same readings again: no change.
        let r = d.derive(
            Raw {
                at: tl.advance(1.0),
                power: Some(power(true)),
                config: Some(cfg.clone()),
                ..Raw::default()
            },
            1.0,
        );
        assert_eq!(r.key_seq, 1);
        // Unplugged: the key changes with the 5-s power read, not only at the 60-s poll.
        let r = d.derive(
            Raw {
                at: tl.advance(1.0),
                power: Some(power(false)),
                ..Raw::default()
            },
            1.0,
        );
        assert_eq!(r.key_seq, 2);
        assert_eq!(r.ac, Some(false));
        assert!(!r.config_read);
        assert_eq!(d.key().unwrap().id(), "bat|performance|-|28|-|-|-|-|-");

        // A failed AC read keeps the last state instead of reading as AC.
        let r = d.derive(
            Raw {
                at: tl.advance(5.0),
                power: Some(PowerRead::default()),
                ..Raw::default()
            },
            5.0,
        );
        assert_eq!((r.key_seq, r.ac), (2, Some(false)));

        // A lid that answers "unsupported" once keeps its last state: no key change, so no
        // void:config for the open episode. A real change still counts.
        let open = ConfigRead {
            lid_open: Some(true),
            ..cfg.clone()
        };
        let flicker = ConfigRead {
            lid_open: None,
            unread: Unread {
                lid: true,
                ..Unread::default()
            },
            ..cfg.clone()
        };
        let closed = ConfigRead {
            lid_open: Some(false),
            ..cfg.clone()
        };
        let mut seqs = Vec::new();
        for c in [open, flicker, closed] {
            let r = d.derive(
                Raw {
                    at: tl.advance(60.0),
                    power: Some(power(true)),
                    config: Some(c),
                    ..Raw::default()
                },
                60.0,
            );
            seqs.push((r.key_seq, r.lid_open));
        }
        assert_eq!(
            seqs,
            vec![(3, Some(true)), (3, Some(true)), (4, Some(false))]
        );
    }

    #[test]
    fn cadence_rules() {
        assert_eq!(
            cadence(Some(true), Some(60.0), false),
            Cadence {
                row_ms: 1000,
                t4_ms: None
            }
        );
        assert_eq!(cadence(None, None, false).row_ms, 1000);
        assert_eq!(cadence(Some(false), Some(60.0), false).row_ms, 5000);
        assert_eq!(cadence(Some(false), Some(85.0), false).row_ms, 1000);
        assert_eq!(
            cadence(Some(true), Some(60.0), true),
            Cadence {
                row_ms: 1000,
                t4_ms: Some(250)
            }
        );
    }

    #[test]
    fn fan_schedule() {
        assert!(fan_due(10.0, None, None, None, false));
        assert!(!fan_due(11.0, Some(10.0), Some(0.1), Some(60.0), false));
        assert!(fan_due(15.0, Some(10.0), Some(0.1), Some(60.0), false));
        assert!(fan_due(11.0, Some(10.0), Some(0.3), Some(60.0), false));
        assert!(fan_due(11.0, Some(10.0), Some(0.1), Some(75.0), false));
        assert!(fan_due(11.0, Some(10.0), None, None, true));
    }

    #[test]
    fn stat_and_consumers() {
        let (comm, ticks) =
            parse_stat("42 (Web Content (x)) S 1 2 3 4 5 6 7 8 9 10 250 50 0 0 20 0").unwrap();
        assert_eq!(comm, "Web Content (x)");
        assert_eq!(ticks, 300);
        assert_eq!(parse_stat("garbage"), None);

        let snap = |v: &[(i32, &str, u64)]| ProcSnapshot {
            procs: v.iter().map(|&(p, c, t)| (p, (c.to_owned(), t))).collect(),
        };
        let before = snap(&[(1, "rustc", 100), (2, "floorp", 1000), (3, "cargo", 50)]);
        let after = snap(&[
            (1, "rustc", 800),
            (2, "floorp", 1100),
            (3, "cargo", 50),
            (4, "rustc", 100),
        ]);
        let top = top_consumers(&before, &after, 3);
        assert_eq!(top.len(), 2);
        // rustc: 700 from pid 1 plus all 100 of the new pid 4, against floorp's 100.
        assert_eq!(top[0].0, "rustc");
        assert!((top[0].1 - 8.0 / 9.0).abs() < 1e-9);
        assert_eq!(top[1].0, "floorp");

        let m0 = ReconcileMark {
            reconcile: 10,
            busy: 1000,
        };
        let m1 = ReconcileMark {
            reconcile: 110,
            busy: 1400,
        };
        assert_eq!(m1.share_since(&m0), Some(0.25));
        assert_eq!(m0.share_since(&m0), None);
    }

    #[test]
    fn median_values() {
        assert_eq!(median(&mut []), None);
        assert_eq!(median(&mut [4100, 3900, 4000]), Some(4000));
        assert_eq!(median(&mut [4100, 3900, 4000, 3800]), Some(3950));
    }

    #[test]
    fn ring_keeps_900_seconds() {
        let mut tl = timeline();
        let mut ring = Ring::default();
        for _ in 0..1000 {
            ring.push(Row {
                at: tl.advance(1.0),
                ..Row::default()
            });
        }
        assert_eq!(ring.rows().len(), 901);
        let span = ring.last().unwrap().at.mono_s() - ring.rows().front().unwrap().at.mono_s();
        assert_eq!(span, 900.0);
    }

    #[test]
    fn the_comm_walk_runs_every_10_minutes_and_at_episode_edges() {
        let root = test_dir("sampler-walk");
        fake::duo(&root);
        let cgroup = "sys/fs/cgroup/system.slice/thermald.service/cgroup.procs";
        let mut s = Sampler::new(&root);
        let mut tl = timeline();
        assert!(!s.read_at(tl.now(), 1.0).thermald);
        // thermald started by hand: the minute polls do not walk /proc for it...
        put(&root, "proc/950/comm", "thermald\n");
        let r = s.read_at(tl.advance(60.0), 60.0).clone();
        assert!(r.config_read && !r.thermald);
        // ...the walk 10 min after the last one finds it,
        for _ in 0..8 {
            s.read_at(tl.advance(60.0), 60.0);
        }
        assert!(s.read_at(tl.advance(60.0), 60.0).thermald);
        // and the polls in between see it go by its pid alone.
        fs::remove_dir_all(root.join("proc/950")).unwrap();
        let r = s.read_at(tl.advance(60.0), 60.0).clone();
        assert!(r.config_read && !r.thermald);
        // Started as its unit: the unit's cgroup shows it at the next poll.
        put(&root, cgroup, "951\n");
        assert!(s.read_at(tl.advance(60.0), 60.0).thermald);
        put(&root, cgroup, "");
        // An episode edge walks at once; a check's config read does not.
        put(&root, "proc/952/comm", "thermald\n");
        s.request_config();
        let r = s.read_at(tl.advance(1.0), 1.0).clone();
        assert!(r.config_read && !r.thermald);
        s.request_edge();
        assert!(s.read_at(tl.advance(1.0), 1.0).thermald);
    }

    #[test]
    fn sampler_on_a_fake_tree() {
        let root = test_dir("sampler-fake");
        fake::duo(&root);
        put(&root, "proc/327/comm", "bch-reconcile\n");
        put(
            &root,
            "proc/327/stat",
            "327 (bch-reconcile) S 2 0 0 0 -1 0 0 0 0 0 100 0 0 0 39 19\n",
        );
        put(&root, "proc/900/comm", "rustc\n");
        put(
            &root,
            "proc/900/stat",
            "900 (rustc) R 1 0 0 0 -1 0 0 0 0 0 5000 100 0 0 20 0\n",
        );
        let mut s = Sampler::new(&root);
        let mut tl = timeline();

        let r = s.read_at(tl.now(), 1.0).clone();
        assert_eq!(r.gap, Gap::First);
        assert!(r.fan_read && r.config_read);
        assert_eq!(r.fan, Some(3500));
        assert_eq!(r.t, Some(57.0));
        assert_eq!(r.ac, Some(true));
        assert_eq!(r.key_seq, 1);
        assert_eq!(r.lid_open, Some(true));
        assert!(!r.thermald);
        assert_eq!(r.reconcile_share, None);
        assert_eq!(
            s.deriver.key().unwrap().id(),
            "ac|performance|performance|28|35|8|0|100|open"
        );

        // Busy at 0.90 with bch-reconcile using a tenth of it; the package throttled 40 ms.
        put(&root, "proc/stat", "cpu  1720 0 500 8080 500 0 0 0 0 0\n");
        put(
            &root,
            "sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_total_time_ms",
            "18324\n",
        );
        put(
            &root,
            "proc/327/stat",
            "327 (bch-reconcile) S 2 0 0 0 -1 0 0 0 0 0 172 0 0 0 39 19\n",
        );
        s.request_edge();
        let r = s.read_at(tl.advance(1.0), 1.0).clone();
        assert_eq!(r.gap, Gap::None);
        assert!((r.b.unwrap() - 0.9).abs() < 1e-9);
        assert_eq!(r.thr_ms, Some(40));
        assert_eq!(r.f_med, Some(3350), "read at b >= 0.80");
        assert!(!r.fan_read, "idle-rate fan: every 5 s");
        assert_eq!(r.fan, Some(3500), "the last reading carries");
        assert!(r.config_read, "an edge asks for the config poll");
        assert!((r.reconcile_share.unwrap() - 0.1).abs() < 1e-9);

        let mark = s.reconcile_mark();
        assert_eq!(mark.reconcile, 72);
        assert_eq!(s.snapshot().procs.len(), 2);

        // A failed _LID evaluation at an edge poll leaves the key and the lid state alone.
        put(
            &root,
            "proc/acpi/button/lid/LID/state",
            "state:      unsupported\n",
        );
        s.request_edge();
        let r = s.read_at(tl.advance(1.0), 1.0).clone();
        assert!(r.config_read);
        assert_eq!((r.key_seq, r.lid_open), (1, Some(true)));
        put(
            &root,
            "proc/acpi/button/lid/LID/state",
            "state:      open\n",
        );

        // A failing sensor marks the set stale; the next read after the minimum interval rediscovers.
        let fan = root.join("sys/class/hwmon/hwmon6/fan1_input");
        fs::remove_file(&fan).unwrap();
        fs::create_dir(&fan).unwrap();
        s.rediscover(tl.now().mono_s());
        s.detail = true;
        let r = s.read_at(tl.advance(1.0), 1.0).clone();
        assert!(r.fan_read);
        assert_eq!(r.fan, None);
        assert!(s.sensors.failed());
        assert_eq!(r.core_t.len(), 4);
        assert_eq!(
            r.core_thr_ms,
            vec![None; 4],
            "the first detail row only starts the chain"
        );
        fs::remove_dir(&fan).unwrap();
        fs::write(&fan, "6800\n").unwrap();
        s.read_at(tl.advance(1.0), 1.0);
        assert!(s.sensors.failed(), "too soon to rediscover");
        let r = s
            .read_at(tl.advance(REDISCOVER_MIN_S), 1.0 + REDISCOVER_MIN_S)
            .clone();
        assert!(!s.sensors.failed());
        assert_eq!(r.fan, Some(6800));

        // After a suspend everything is read again, from fresh descriptors.
        tl.suspend(1800.0);
        let r = s.read_at(tl.advance(1.0), 1.0).clone();
        assert_eq!(r.gap, Gap::Suspend);
        assert!(r.fan_read && r.config_read);
        assert_eq!(r.b, None);
    }
}
