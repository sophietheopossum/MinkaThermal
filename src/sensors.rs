//! Sensor discovery and reads (§3.1, §3.2, the discovery half of §6.3).
//!
//! Everything is found by name, never by number: hwmon, zone and card numbers move between boots.
//! Each file is opened once and re-read with `read_at(buf, 0)`, which re-runs the sysfs `show`
//! without a path walk. A read that fails at the I/O level marks the set stale, and the sampler
//! discovers everything again. The root is a parameter so tests can point discovery at a fake
//! sysfs/procfs tree; the service passes "/".
//!
//! This module only reads. When to read is the sampler's business.

use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// One sysfs or procfs file, held open.
#[derive(Debug)]
pub struct Src {
    pub path: PathBuf,
    file: File,
}

impl Src {
    fn open(path: PathBuf) -> Option<Src> {
        let file = File::open(&path).ok()?;
        Some(Src { path, file })
    }

    fn read<'b>(&self, buf: &'b mut [u8]) -> io::Result<&'b str> {
        let n = self.file.read_at(buf, 0)?;
        std::str::from_utf8(&buf[..n])
            .map(str::trim)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// A fan input, with the highest speed it has shown since discovery.
#[derive(Debug)]
pub struct Fan {
    pub src: Src,
    pub max_seen: Cell<u32>,
}

/// The xe iGPU's throttle files under `device/tile0/gt0/freq0`.
#[derive(Debug)]
pub struct XeFiles {
    pub card: PathBuf,
    pub status: Option<Src>,
    pub pl1: Option<Src>,
    pub prochot: Option<Src>,
    pub thermal: Option<Src>,
    pub act_freq: Option<Src>,
}

/// The power-limit files of one RAPL domain.
#[derive(Debug)]
pub struct RaplFiles {
    /// `intel-rapl-mmio:0` whenever it exists: the MSR domain `intel-rapl:0` reads 60/60 W on her
    /// machine while the package obeys the MMIO 28/35 W (§23).
    pub domain: String,
    pub pl1: Option<Src>,
    pub pl2: Option<Src>,
    pub pl1_max: Option<Src>,
}

/// Cumulative CPU time from the "cpu " line of /proc/stat, in USER_HZ ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTimes {
    pub total: u64,
    /// idle + iowait: a CPU waiting on I/O is not busy.
    pub idle: u64,
}

impl CpuTimes {
    pub fn busy(&self) -> u64 {
        self.total.saturating_sub(self.idle)
    }

    pub fn parse(stat: &str) -> Option<CpuTimes> {
        let line = stat.lines().find(|l| l.starts_with("cpu "))?;
        let v: Vec<u64> = line
            .split_ascii_whitespace()
            .skip(1)
            .map(|f| f.parse().ok())
            .collect::<Option<_>>()?;
        if v.len() < 5 {
            return None;
        }
        // user nice system idle iowait irq softirq steal; guest and guest_nice are already
        // inside user and nice, so they are left out.
        let total = v.iter().take(8).sum();
        Some(CpuTimes {
            total,
            idle: v[3] + v[4],
        })
    }
}

/// The xe throttle state. The reasons are read only while `status` is set, and are false otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct XeRead {
    pub status: bool,
    pub pl1: bool,
    pub prochot: bool,
    pub thermal: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatStatus {
    Charging,
    Discharging,
    NotCharging,
    Full,
    #[default]
    Unknown,
}

impl BatStatus {
    fn parse(s: &str) -> BatStatus {
        match s {
            "Charging" => BatStatus::Charging,
            "Discharging" => BatStatus::Discharging,
            "Not charging" => BatStatus::NotCharging,
            "Full" => BatStatus::Full,
            _ => BatStatus::Unknown,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PowerRead {
    /// Mains online. Without a mains supply, or when its `online` read fails, the battery
    /// status stands in; a machine with neither is a desktop, always on mains. None only when
    /// every read failed, and the sampler then keeps the last known state.
    pub ac: Option<bool>,
    pub bat_status: Option<BatStatus>,
    pub capacity: Option<u8>,
    /// Read only while discharging (§3.2). In the other states it is the charge rate or
    /// nothing, and ACPI answers ENODEV for it whenever the firmware reports the rate unknown,
    /// as some do when full.
    pub power_now_uw: Option<u64>,
}

impl PowerRead {
    /// The charger is not keeping up: on AC, yet the battery is discharging (§3.2). The spec's
    /// "or power_now > 0" adds nothing, since power_now is read only while discharging.
    pub fn deficit(&self) -> bool {
        self.ac == Some(true) && self.bat_status == Some(BatStatus::Discharging)
    }
}

/// Mains state as the battery reports it; None when the status says nothing either way.
fn ac_from_battery(status: BatStatus) -> Option<bool> {
    match status {
        BatStatus::Discharging => Some(false),
        BatStatus::Charging | BatStatus::NotCharging | BatStatus::Full => Some(true),
        BatStatus::Unknown => None,
    }
}

/// Config-key fields whose file exists but gave no value this time: an I/O error, a value that
/// did not parse, or the lid's "unsupported" after a failed _LID evaluation (the kernel prints
/// that instead of returning an error). The sampler keeps their last good value, so one failed
/// read does not change the key and void an open episode. A field whose file is missing is
/// absent, not unread: losing the file is a real change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unread {
    pub platform_profile: bool,
    pub epp: bool,
    pub pl1: bool,
    pub pl2: bool,
    pub tcc_offset: bool,
    pub no_turbo: bool,
    pub max_perf_pct: bool,
    pub lid: bool,
}

fn unread<T>(src: Option<&Src>, value: &Option<T>) -> bool {
    src.is_some() && value.is_none()
}

/// The slow-changing settings that make up the config key (§4), read every 60 s.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigRead {
    pub platform_profile: Option<String>,
    pub epp: Option<String>,
    pub pl_domain: Option<String>,
    pub pl1_uw: Option<u64>,
    pub pl2_uw: Option<u64>,
    pub pl1_max_uw: Option<u64>,
    pub tcc_offset: Option<i64>,
    pub no_turbo: Option<bool>,
    pub max_perf_pct: Option<u32>,
    /// The highest `cur_state` among the `Processor` and `intel_powerclamp` cooling devices.
    /// The `TCC Offset` device is left out: it always reads the offset.
    pub cooling_max: Option<u32>,
    pub dgpu_runtime: Option<String>,
    /// Filled by the sampler from its /proc scan.
    pub thermald: bool,
    pub lid_open: Option<bool>,
    #[serde(skip)]
    pub unread: Unread,
}

impl ConfigRead {
    /// Fills the key fields that could not be read this time from the previous reading.
    pub fn carry_unread(&mut self, prev: &ConfigRead) {
        fn keep<T: Clone>(unread: bool, cur: &mut Option<T>, prev: &Option<T>) {
            if unread && cur.is_none() {
                cur.clone_from(prev);
            }
        }
        let u = self.unread;
        keep(
            u.platform_profile,
            &mut self.platform_profile,
            &prev.platform_profile,
        );
        keep(u.epp, &mut self.epp, &prev.epp);
        keep(u.pl1, &mut self.pl1_uw, &prev.pl1_uw);
        keep(u.pl2, &mut self.pl2_uw, &prev.pl2_uw);
        keep(u.tcc_offset, &mut self.tcc_offset, &prev.tcc_offset);
        keep(u.no_turbo, &mut self.no_turbo, &prev.no_turbo);
        keep(u.max_perf_pct, &mut self.max_perf_pct, &prev.max_perf_pct);
        keep(u.lid, &mut self.lid_open, &prev.lid_open);
    }
}

/// Context-only zones (§3.2), every 60 s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Context {
    pub sen2: Option<f64>,
    pub sen4: Option<f64>,
    pub acpitz: Option<f64>,
}

/// Read once at start.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StaticInfo {
    pub sys_vendor: String,
    pub board_name: String,
    pub product_name: String,
    pub bios_version: String,
    pub kernel: String,
    /// /proc/cpuinfo's model name without the " @ 2.80GHz" suffix.
    pub cpu_model: String,
    pub nproc: usize,
    pub boot_id: String,
}

impl StaticInfo {
    pub fn read(root: &Path) -> StaticInfo {
        let dmi = |f: &str| read_trim(&root.join("sys/class/dmi/id").join(f)).unwrap_or_default();
        let cpu_model = fs::read_to_string(root.join("proc/cpuinfo"))
            .ok()
            .and_then(|text| {
                text.lines()
                    .find(|l| l.starts_with("model name"))
                    .and_then(|l| l.split_once(':'))
                    .map(|(_, v)| v.trim().to_owned())
            })
            .map(|m| match m.split_once(" @ ") {
                Some((head, _)) => head.trim().to_owned(),
                None => m,
            })
            .unwrap_or_default();
        StaticInfo {
            sys_vendor: dmi("sys_vendor"),
            board_name: dmi("board_name"),
            product_name: dmi("product_name"),
            bios_version: dmi("bios_version"),
            kernel: read_trim(&root.join("proc/sys/kernel/osrelease")).unwrap_or_default(),
            cpu_model,
            nproc: online_cpus(root),
            boot_id: boot_id(root),
        }
    }
}

/// The current boot's id, which tells a live service from a pid reused after a reboot.
pub fn boot_id(root: &Path) -> String {
    read_trim(&root.join("proc/sys/kernel/random/boot_id")).unwrap_or_default()
}

fn online_cpus(root: &Path) -> usize {
    let ranges = read_trim(&root.join("sys/devices/system/cpu/online")).unwrap_or_default();
    let n: usize = ranges
        .split(',')
        .filter_map(|r| {
            let (a, b) = r.split_once('-').unwrap_or((r, r));
            (b.trim().parse::<usize>().ok()? + 1).checked_sub(a.trim().parse().ok()?)
        })
        .sum();
    if n > 0 {
        n
    } else {
        cpu_dirs(root).len().max(1)
    }
}

/// The TCC point (§3.2, §6.3): temp1_crit − offset, else temp1_crit, else temp1_max, else 95 °C.
pub fn tcc_point(crit_c: Option<f64>, max_c: Option<f64>, offset: Option<i64>) -> f64 {
    match (crit_c, offset) {
        (Some(c), Some(o)) => c - o as f64,
        (Some(c), None) => c,
        (None, _) => max_c.unwrap_or(95.0),
    }
}

/// Everything discovered under one root.
#[derive(Debug)]
pub struct Sensors {
    pub root: PathBuf,
    pub proc_stat: Option<Src>,
    /// Package temperature: coretemp temp1, else k10temp/zenpower Tctl.
    pub temp: Option<Src>,
    /// The hwmon driver `temp` came from.
    pub temp_driver: Option<String>,
    pub temp_crit_c: Option<f64>,
    pub temp_max_c: Option<f64>,
    /// coretemp temp2.. (the cores), in index order.
    pub core_temps: Vec<Src>,
    /// zone `x86_pkg_temp`, a live MSR read: 4 Hz during a check only.
    pub t4: Option<Src>,
    pub pkg_thr_ms: Option<Src>,
    pub pkg_thr_count: Option<Src>,
    /// One per physical core (the first CPU of each core id).
    pub core_thr_ms: Vec<Src>,
    pub fans: Vec<Fan>,
    /// One per CPU: cpuinfo_avg_freq, else scaling_cur_freq (kHz).
    pub freqs: Vec<Src>,
    pub xe: Option<XeFiles>,
    pub ac: Option<Src>,
    pub bat_status: Option<Src>,
    pub bat_capacity: Option<Src>,
    pub bat_power_now: Option<Src>,
    pub platform_profile: Option<Src>,
    pub epp: Option<Src>,
    pub rapl: Option<RaplFiles>,
    pub tcc_offset: Option<Src>,
    pub no_turbo: Option<Src>,
    pub max_perf_pct: Option<Src>,
    pub cooling: Vec<(String, Src)>,
    pub dgpu: Option<Src>,
    pub lid: Option<Src>,
    pub sen2: Option<Src>,
    pub sen4: Option<Src>,
    pub acpitz: Option<Src>,
    pub nproc: usize,
    failed: Cell<bool>,
}

impl Sensors {
    pub fn discover(root: &Path) -> Sensors {
        let sys = root.join("sys");
        let mut s = Sensors {
            root: root.to_path_buf(),
            proc_stat: Src::open(root.join("proc/stat")),
            temp: None,
            temp_driver: None,
            temp_crit_c: None,
            temp_max_c: None,
            core_temps: Vec::new(),
            t4: None,
            pkg_thr_ms: None,
            pkg_thr_count: None,
            core_thr_ms: Vec::new(),
            fans: Vec::new(),
            freqs: Vec::new(),
            xe: None,
            ac: None,
            bat_status: None,
            bat_capacity: None,
            bat_power_now: None,
            platform_profile: Src::open(sys.join("firmware/acpi/platform_profile")),
            epp: None,
            rapl: None,
            tcc_offset: None,
            no_turbo: Src::open(sys.join("devices/system/cpu/intel_pstate/no_turbo")),
            max_perf_pct: Src::open(sys.join("devices/system/cpu/intel_pstate/max_perf_pct")),
            cooling: Vec::new(),
            dgpu: None,
            lid: None,
            sen2: None,
            sen4: None,
            acpitz: None,
            nproc: online_cpus(root),
            failed: Cell::new(false),
        };
        s.discover_hwmon(&sys);
        s.discover_thermal(&sys);
        s.discover_cpus(root);
        s.discover_drm(&sys);
        s.discover_power(&sys);
        s.rapl = discover_rapl(&sys);
        s.tcc_offset = discover_tcc_offset(&sys);
        s.lid = first_entry(&root.join("proc/acpi/button/lid"))
            .and_then(|d| Src::open(d.join("state")));
        s
    }

    fn discover_hwmon(&mut self, sys: &Path) {
        let mut coretemp = None;
        let mut amd = None;
        let mut asus_fan = None;
        let mut fans = Vec::new();
        for (_, dir) in entries(&sys.join("class/hwmon")) {
            let Some(name) = read_trim(&dir.join("name")) else {
                continue;
            };
            match name.as_str() {
                "coretemp" if coretemp.is_none() => coretemp = Some(dir.clone()),
                "k10temp" | "zenpower" if amd.is_none() => amd = Some((name.clone(), dir.clone())),
                _ => {}
            }
            for (file, path) in entries(&dir) {
                if indexed(&file, "fan", "_input").is_some() {
                    if name == "asus" && file == "fan1_input" {
                        asus_fan = Some(path.clone());
                    }
                    fans.push(path);
                }
            }
        }
        if let Some(dir) = coretemp {
            let mut temps: Vec<(u32, PathBuf)> = entries(&dir)
                .into_iter()
                .filter_map(|(f, p)| Some((indexed(&f, "temp", "_input")?, p)))
                .collect();
            temps.sort_by_key(|(i, _)| *i);
            self.temp = Src::open(dir.join("temp1_input"));
            self.temp_driver = Some("coretemp".to_owned());
            self.temp_crit_c = read_milli(&dir.join("temp1_crit"));
            self.temp_max_c = read_milli(&dir.join("temp1_max"));
            self.core_temps = temps
                .into_iter()
                .filter(|(i, _)| *i >= 2)
                .filter_map(|(_, p)| Src::open(p))
                .collect();
        } else if let Some((driver, dir)) = amd {
            // Tctl is the control temperature, the one the firmware throttles on.
            let index = entries(&dir)
                .into_iter()
                .filter_map(|(f, p)| Some((indexed(&f, "temp", "_label")?, p)))
                .find(|(_, p)| read_trim(p).as_deref() == Some("Tctl"))
                .map_or(1, |(i, _)| i);
            self.temp = Src::open(dir.join(format!("temp{index}_input")));
            self.temp_driver = Some(driver);
            self.temp_crit_c = read_milli(&dir.join(format!("temp{index}_crit")));
            self.temp_max_c = read_milli(&dir.join(format!("temp{index}_max")));
        }
        // The asus EC fan is the Duo's; elsewhere every fan input is a candidate, and reads
        // report the one with the highest speed seen (§6.3).
        let fans = match asus_fan {
            Some(p) => vec![p],
            None => fans,
        };
        self.fans = fans
            .into_iter()
            .filter_map(Src::open)
            .map(|src| Fan {
                src,
                max_seen: Cell::new(0),
            })
            .collect();
    }

    fn discover_thermal(&mut self, sys: &Path) {
        for (name, dir) in entries(&sys.join("class/thermal")) {
            if name.starts_with("thermal_zone") {
                let slot = match read_trim(&dir.join("type")).as_deref() {
                    Some("x86_pkg_temp") => &mut self.t4,
                    Some("SEN2") => &mut self.sen2,
                    Some("SEN4") => &mut self.sen4,
                    Some("acpitz") => &mut self.acpitz,
                    _ => continue,
                };
                if slot.is_none() {
                    *slot = Src::open(dir.join("temp"));
                }
            } else if name.starts_with("cooling_device") {
                let Some(kind) = read_trim(&dir.join("type")) else {
                    continue;
                };
                if (kind == "Processor" || kind == "intel_powerclamp")
                    && let Some(src) = Src::open(dir.join("cur_state"))
                {
                    self.cooling.push((kind, src));
                }
            }
        }
    }

    fn discover_cpus(&mut self, root: &Path) {
        let mut cores_seen = Vec::new();
        for (_, dir) in cpu_dirs(root) {
            let freq = Src::open(dir.join("cpufreq/cpuinfo_avg_freq"))
                .or_else(|| Src::open(dir.join("cpufreq/scaling_cur_freq")));
            if let Some(src) = freq {
                self.freqs.push(src);
            }
            if self.epp.is_none() {
                self.epp = Src::open(dir.join("cpufreq/energy_performance_preference"));
            }
            let throttle = dir.join("thermal_throttle");
            if self.pkg_thr_ms.is_none() {
                self.pkg_thr_ms = Src::open(throttle.join("package_throttle_total_time_ms"));
                self.pkg_thr_count = Src::open(throttle.join("package_throttle_count"));
            }
            // Hyper-threads share their core's counter, so one CPU per physical core.
            let id = match (
                read_trim(&dir.join("topology/physical_package_id")),
                read_trim(&dir.join("topology/core_id")),
            ) {
                (Some(p), Some(c)) => format!("{p}/{c}"),
                _ => dir.display().to_string(),
            };
            if !cores_seen.contains(&id)
                && let Some(src) = Src::open(throttle.join("core_throttle_total_time_ms"))
            {
                self.core_thr_ms.push(src);
                cores_seen.push(id);
            }
        }
    }

    fn discover_drm(&mut self, sys: &Path) {
        for (name, dir) in entries(&sys.join("class/drm")) {
            if indexed(&name, "card", "").is_none() {
                continue;
            }
            let driver = fs::read_link(dir.join("device/driver"))
                .ok()
                .and_then(|t| t.file_name().map(|n| n.to_string_lossy().into_owned()));
            match driver.as_deref() {
                Some("xe") if self.xe.is_none() => {
                    let freq = dir.join("device/tile0/gt0/freq0");
                    let thr = freq.join("throttle");
                    self.xe = Some(XeFiles {
                        card: dir.clone(),
                        status: Src::open(thr.join("status")),
                        pl1: Src::open(thr.join("reason_pl1")),
                        prochot: Src::open(thr.join("reason_prochot")),
                        thermal: Src::open(thr.join("reason_thermal")),
                        act_freq: Src::open(freq.join("act_freq")),
                    });
                }
                // runtime_status is a plain attribute: reading it never wakes a suspended dGPU.
                Some("nvidia" | "nouveau") if self.dgpu.is_none() => {
                    self.dgpu = Src::open(dir.join("device/power/runtime_status"));
                }
                _ => {}
            }
        }
    }

    fn discover_power(&mut self, sys: &Path) {
        for (_, dir) in entries(&sys.join("class/power_supply")) {
            match read_trim(&dir.join("type")).as_deref() {
                Some("Mains") if self.ac.is_none() => self.ac = Src::open(dir.join("online")),
                // HID pens and mice report batteries too, with scope Device.
                Some("Battery")
                    if self.bat_status.is_none()
                        && read_trim(&dir.join("scope")).as_deref() != Some("Device") =>
                {
                    self.bat_status = Src::open(dir.join("status"));
                    self.bat_capacity = Src::open(dir.join("capacity"));
                    self.bat_power_now = Src::open(dir.join("power_now"));
                }
                _ => {}
            }
        }
    }

    /// Whether a read failed at the I/O level since the last discovery.
    pub fn failed(&self) -> bool {
        self.failed.get()
    }

    fn text(&self, src: Option<&Src>) -> Option<String> {
        let mut buf = [0u8; 256];
        match src?.read(&mut buf) {
            Ok(s) => Some(s.to_owned()),
            Err(_) => {
                self.failed.set(true);
                None
            }
        }
    }

    fn int(&self, src: Option<&Src>) -> Option<i64> {
        let mut buf = [0u8; 64];
        match src?.read(&mut buf) {
            Ok(s) => s.parse().ok(),
            Err(_) => {
                self.failed.set(true);
                None
            }
        }
    }

    fn uint(&self, src: Option<&Src>) -> Option<u64> {
        self.int(src).and_then(|v| u64::try_from(v).ok())
    }

    fn milli(&self, src: Option<&Src>) -> Option<f64> {
        self.int(src).map(|v| v as f64 / 1000.0)
    }

    pub fn cpu_times(&self) -> Option<CpuTimes> {
        // The "cpu " line comes first; the rest of /proc/stat is not needed.
        let mut buf = [0u8; 1024];
        match self.proc_stat.as_ref()?.read(&mut buf) {
            Ok(s) => CpuTimes::parse(s),
            Err(_) => {
                self.failed.set(true);
                None
            }
        }
    }

    /// Package temperature, °C (coretemp's 1 s driver cache, the seed's method).
    pub fn temp_c(&self) -> Option<f64> {
        self.milli(self.temp.as_ref())
    }

    pub fn t4_c(&self) -> Option<f64> {
        self.milli(self.t4.as_ref())
    }

    pub fn core_temps_c(&self) -> Vec<Option<f64>> {
        self.core_temps
            .iter()
            .map(|s| self.milli(Some(s)))
            .collect()
    }

    pub fn pkg_throttle_ms(&self) -> Option<u64> {
        self.uint(self.pkg_thr_ms.as_ref())
    }

    pub fn pkg_throttle_count(&self) -> Option<u64> {
        self.uint(self.pkg_thr_count.as_ref())
    }

    pub fn core_throttle_ms(&self) -> Vec<Option<u64>> {
        self.core_thr_ms
            .iter()
            .map(|s| self.uint(Some(s)))
            .collect()
    }

    /// The fan with the highest speed seen so far (the only one on the Duo).
    pub fn fan_rpm(&self) -> Option<u32> {
        let mut best: Option<(u32, u32)> = None;
        for fan in &self.fans {
            let Some(rpm) = self
                .uint(Some(&fan.src))
                .and_then(|v| u32::try_from(v).ok())
            else {
                continue;
            };
            fan.max_seen.set(fan.max_seen.get().max(rpm));
            if best.is_none_or(|(max, _)| fan.max_seen.get() > max) {
                best = Some((fan.max_seen.get(), rpm));
            }
        }
        best.map(|(_, rpm)| rpm)
    }

    /// Per-CPU frequency, MHz.
    pub fn freqs_mhz(&self) -> Vec<u32> {
        self.freqs
            .iter()
            .filter_map(|s| self.uint(Some(s)))
            .map(|khz| (khz / 1000) as u32)
            .collect()
    }

    pub fn xe(&self) -> Option<XeRead> {
        let xe = self.xe.as_ref()?;
        let status = self.int(xe.status.as_ref())? != 0;
        let flag = |s: &Option<Src>| status && self.int(s.as_ref()).is_some_and(|v| v != 0);
        Some(XeRead {
            status,
            pl1: flag(&xe.pl1),
            prochot: flag(&xe.prochot),
            thermal: flag(&xe.thermal),
        })
    }

    pub fn xe_act_mhz(&self) -> Option<u32> {
        let xe = self.xe.as_ref()?;
        self.uint(xe.act_freq.as_ref()).map(|v| v as u32)
    }

    /// power_now without marking the set stale: ACPI answers ENODEV for it while the rate is
    /// unknown, which is the firmware's state, not a vanished device.
    fn power_now(&self) -> Option<u64> {
        let mut buf = [0u8; 64];
        self.bat_power_now
            .as_ref()?
            .read(&mut buf)
            .ok()?
            .parse()
            .ok()
    }

    pub fn power(&self) -> PowerRead {
        let online = self.int(self.ac.as_ref()).map(|v| v != 0);
        let bat_status = self
            .text(self.bat_status.as_ref())
            .map(|s| BatStatus::parse(&s));
        let capacity = self
            .uint(self.bat_capacity.as_ref())
            .map(|v| v.min(100) as u8);
        let power_now_uw = if bat_status == Some(BatStatus::Discharging) {
            self.power_now()
        } else {
            None
        };
        // A failed `online` (ACPI _PSR can fail) must not read as AC while she is on battery,
        // so the battery status answers first.
        let from_battery = bat_status.and_then(ac_from_battery);
        let ac = match (self.ac.is_some(), bat_status) {
            (true, _) => online.or(from_battery),
            (false, Some(_)) => from_battery.or(Some(true)),
            (false, None) => Some(true),
        };
        PowerRead {
            ac,
            bat_status,
            capacity,
            power_now_uw,
        }
    }

    /// The config-key reads plus the cooling devices, dGPU and lid. `thermald` is left false for
    /// the sampler to fill.
    pub fn config(&self) -> ConfigRead {
        let rapl = self.rapl.as_ref();
        let pl1_src = rapl.and_then(|r| r.pl1.as_ref());
        let pl2_src = rapl.and_then(|r| r.pl2.as_ref());
        let mut c = ConfigRead {
            platform_profile: self.text(self.platform_profile.as_ref()),
            epp: self.text(self.epp.as_ref()),
            pl_domain: rapl.map(|r| r.domain.clone()),
            pl1_uw: self.uint(pl1_src),
            pl2_uw: self.uint(pl2_src),
            pl1_max_uw: rapl.and_then(|r| self.uint(r.pl1_max.as_ref())),
            tcc_offset: self.int(self.tcc_offset.as_ref()),
            no_turbo: self.int(self.no_turbo.as_ref()).map(|v| v != 0),
            max_perf_pct: self.uint(self.max_perf_pct.as_ref()).map(|v| v as u32),
            cooling_max: self
                .cooling
                .iter()
                .filter_map(|(_, s)| self.uint(Some(s)))
                .max()
                .map(|v| v as u32),
            dgpu_runtime: self.text(self.dgpu.as_ref()),
            thermald: false,
            lid_open: self.lid_open(),
            unread: Unread::default(),
        };
        c.unread = Unread {
            platform_profile: unread(self.platform_profile.as_ref(), &c.platform_profile),
            epp: unread(self.epp.as_ref(), &c.epp),
            pl1: unread(pl1_src, &c.pl1_uw),
            pl2: unread(pl2_src, &c.pl2_uw),
            tcc_offset: unread(self.tcc_offset.as_ref(), &c.tcc_offset),
            no_turbo: unread(self.no_turbo.as_ref(), &c.no_turbo),
            max_perf_pct: unread(self.max_perf_pct.as_ref(), &c.max_perf_pct),
            lid: unread(self.lid.as_ref(), &c.lid_open),
        };
        c
    }

    /// "state:      open" in /proc/acpi/button/lid/*/state (an ACPI evaluation, 1.7 ms).
    pub fn lid_open(&self) -> Option<bool> {
        let text = self.text(self.lid.as_ref())?;
        let state = text
            .split_once(':')
            .map_or(text.as_str(), |(_, v)| v)
            .trim();
        match state {
            "open" => Some(true),
            "closed" => Some(false),
            _ => None,
        }
    }

    pub fn context(&self) -> Context {
        Context {
            sen2: self.milli(self.sen2.as_ref()),
            sen4: self.milli(self.sen4.as_ref()),
            acpitz: self.milli(self.acpitz.as_ref()),
        }
    }

    /// The TCC point with the offset read now.
    pub fn tcc_c(&self, offset: Option<i64>) -> f64 {
        tcc_point(self.temp_crit_c, self.temp_max_c, offset)
    }

    /// Evidence classes the discovered signals support (§6.3: missing signals drop their classes).
    /// E1 needs the package temperature and busy time, E2 the package throttle counter as well,
    /// and the check the temperature. E2 is listed only with `light_load_evidence`
    /// (`rules::LIGHT_LOAD_EVIDENCE`, off since 21/11/2026). The fan rules are separate: see
    /// `has_fan`.
    pub fn classes(&self, light_load_evidence: bool) -> Vec<&'static str> {
        let base = self.temp.is_some() && self.proc_stat.is_some();
        let mut out = Vec::new();
        if base {
            out.push("E1");
        }
        if base && self.pkg_thr_ms.is_some() && light_load_evidence {
            out.push("E2");
        }
        if self.temp.is_some() {
            out.push("check");
        }
        out
    }

    pub fn has_fan(&self) -> bool {
        !self.fans.is_empty()
    }

    /// Every discovered file with its current value, for the `sensors` verb.
    pub fn describe(&self) -> Vec<Line> {
        let mut out = Vec::new();
        let mut add = |name: &str, src: Option<&Src>, value: String| {
            if let Some(src) = src {
                out.push(Line {
                    name: name.to_owned(),
                    value,
                    path: src.path.display().to_string(),
                });
            }
        };
        let c = |v: Option<f64>| v.map_or("?".to_owned(), |v| format!("{v:.1} °C"));
        let n = |v: Option<i64>| v.map_or("?".to_owned(), |v| v.to_string());
        let t = |v: Option<String>| v.unwrap_or_else(|| "?".to_owned());

        add(
            "busy/total ticks",
            self.proc_stat.as_ref(),
            self.cpu_times()
                .map_or("?".into(), |c| format!("{}/{}", c.busy(), c.total)),
        );
        add("T", self.temp.as_ref(), c(self.temp_c()));
        for (i, s) in self.core_temps.iter().enumerate() {
            add(&format!("core T {i}"), Some(s), c(self.milli(Some(s))));
        }
        add("T4 x86_pkg_temp", self.t4.as_ref(), c(self.t4_c()));
        add(
            "pkg throttle ms",
            self.pkg_thr_ms.as_ref(),
            n(self.int(self.pkg_thr_ms.as_ref())),
        );
        add(
            "pkg throttle n",
            self.pkg_thr_count.as_ref(),
            n(self.int(self.pkg_thr_count.as_ref())),
        );
        for (i, s) in self.core_thr_ms.iter().enumerate() {
            add(&format!("core throttle {i}"), Some(s), n(self.int(Some(s))));
        }
        for f in &self.fans {
            add(
                "fan",
                Some(&f.src),
                self.int(Some(&f.src))
                    .map_or("?".into(), |v| format!("{v} rpm")),
            );
        }
        for (i, s) in self.freqs.iter().enumerate() {
            let v = self
                .int(Some(s))
                .map_or("?".into(), |v| format!("{} MHz", v / 1000));
            add(&format!("f cpu{i}"), Some(s), v);
        }
        if let Some(xe) = &self.xe {
            add(
                "xe status",
                xe.status.as_ref(),
                n(self.int(xe.status.as_ref())),
            );
            add(
                "xe reason_pl1",
                xe.pl1.as_ref(),
                n(self.int(xe.pl1.as_ref())),
            );
            add(
                "xe reason_prochot",
                xe.prochot.as_ref(),
                n(self.int(xe.prochot.as_ref())),
            );
            add(
                "xe reason_thermal",
                xe.thermal.as_ref(),
                n(self.int(xe.thermal.as_ref())),
            );
            add(
                "xe act_freq",
                xe.act_freq.as_ref(),
                n(self.int(xe.act_freq.as_ref())) + " MHz",
            );
        }
        add("AC online", self.ac.as_ref(), n(self.int(self.ac.as_ref())));
        add(
            "battery status",
            self.bat_status.as_ref(),
            t(self.text(self.bat_status.as_ref())),
        );
        add(
            "battery %",
            self.bat_capacity.as_ref(),
            n(self.int(self.bat_capacity.as_ref())),
        );
        add(
            "battery power_now",
            self.bat_power_now.as_ref(),
            self.power_now().map_or("?".into(), |v| format!("{v} µW")),
        );
        add(
            "platform_profile",
            self.platform_profile.as_ref(),
            t(self.text(self.platform_profile.as_ref())),
        );
        add("EPP", self.epp.as_ref(), t(self.text(self.epp.as_ref())));
        if let Some(r) = &self.rapl {
            let w = |s: &Option<Src>| {
                self.int(s.as_ref()).map_or("?".into(), |v| {
                    format!("{} W ({})", v as f64 / 1e6, r.domain)
                })
            };
            add("PL1", r.pl1.as_ref(), w(&r.pl1));
            add("PL2", r.pl2.as_ref(), w(&r.pl2));
            add("PL1 max", r.pl1_max.as_ref(), w(&r.pl1_max));
        }
        add(
            "TCC offset",
            self.tcc_offset.as_ref(),
            n(self.int(self.tcc_offset.as_ref())),
        );
        add(
            "no_turbo",
            self.no_turbo.as_ref(),
            n(self.int(self.no_turbo.as_ref())),
        );
        add(
            "max_perf_pct",
            self.max_perf_pct.as_ref(),
            n(self.int(self.max_perf_pct.as_ref())),
        );
        for (kind, s) in &self.cooling {
            add(&format!("cooling {kind}"), Some(s), n(self.int(Some(s))));
        }
        add(
            "dGPU runtime",
            self.dgpu.as_ref(),
            t(self.text(self.dgpu.as_ref())),
        );
        add(
            "lid",
            self.lid.as_ref(),
            self.lid_open()
                .map_or("?".into(), |o| if o { "open" } else { "closed" }.into()),
        );
        add(
            "SEN2",
            self.sen2.as_ref(),
            c(self.milli(self.sen2.as_ref())),
        );
        add(
            "SEN4",
            self.sen4.as_ref(),
            c(self.milli(self.sen4.as_ref())),
        );
        add(
            "acpitz",
            self.acpitz.as_ref(),
            c(self.milli(self.acpitz.as_ref())),
        );
        out
    }
}

/// One row of the `sensors` listing.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub name: String,
    pub value: String,
    pub path: String,
}

fn discover_rapl(sys: &Path) -> Option<RaplFiles> {
    let powercap = sys.join("class/powercap");
    // MMIO only when it exists: the MSR domain is a decoy on her machine (§1.5, §23).
    let domain = ["intel-rapl-mmio:0", "intel-rapl:0"]
        .into_iter()
        .find(|d| powercap.join(d).is_dir())?;
    let dir = powercap.join(domain);
    Some(RaplFiles {
        domain: domain.to_owned(),
        pl1: Src::open(dir.join("constraint_0_power_limit_uw")),
        pl2: Src::open(dir.join("constraint_1_power_limit_uw")),
        pl1_max: Src::open(dir.join("constraint_0_max_power_uw")),
    })
}

fn discover_tcc_offset(sys: &Path) -> Option<Src> {
    const FILE: &str = "tcc_offset_degree_celsius";
    let devices = sys.join("bus/pci/devices");
    // 00:04.0 is the processor thermal device on her machine; other Intel laptops may place it
    // elsewhere, so look further if it is not there.
    Src::open(devices.join("0000:00:04.0").join(FILE)).or_else(|| {
        entries(&devices)
            .into_iter()
            .find_map(|(_, d)| Src::open(d.join(FILE)))
    })
}

/// `name<digits><suffix>` → the number, e.g. ("temp12_input", "temp", "_input") → 12.
fn indexed(name: &str, prefix: &str, suffix: &str) -> Option<u32> {
    let mid = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if mid.is_empty() || !mid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    mid.parse().ok()
}

/// A directory's entries, sorted so that `hwmon10` follows `hwmon9`.
fn entries(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = rd
        .filter_map(Result::ok)
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    out.sort_by(|a, b| natural_key(&a.0).cmp(&natural_key(&b.0)));
    out
}

fn natural_key(name: &str) -> (&str, u64, &str) {
    let head = name.trim_end_matches(|c: char| c.is_ascii_digit());
    let num = name[head.len()..].parse().unwrap_or(0);
    (head, num, name)
}

/// The `cpuN` directories, in CPU order.
fn cpu_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    entries(&root.join("sys/devices/system/cpu"))
        .into_iter()
        .filter(|(n, _)| indexed(n, "cpu", "").is_some())
        .collect()
}

fn first_entry(dir: &Path) -> Option<PathBuf> {
    entries(dir).into_iter().next().map(|(_, p)| p)
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

fn read_milli(path: &Path) -> Option<f64> {
    read_trim(path)?
        .parse::<i64>()
        .ok()
        .map(|v| v as f64 / 1000.0)
}

#[cfg(test)]
pub(crate) mod fake {
    //! A fake sysfs/procfs tree shaped like her Zenbook Duo (§23 lists the real paths).

    use crate::testutil::{link, put};
    use std::path::Path;

    pub fn duo(root: &Path) {
        put(
            root,
            "proc/stat",
            "cpu  1000 0 500 8000 500 0 0 0 0 0\ncpu0 1 2 3 4 5 6 7 8 0 0\nintr 1 2 3\n",
        );
        put(
            root,
            "proc/cpuinfo",
            "processor\t: 0\nmodel name\t: 11th Gen Intel(R) Core(TM) i7-1165G7 @ 2.80GHz\n",
        );
        put(root, "proc/sys/kernel/osrelease", "7.3.4-1-cachyos\n");
        put(
            root,
            "proc/sys/kernel/random/boot_id",
            "4d5ab25d-6171-4ce0-ac1b-ec48694c2006\n",
        );
        put(root, "proc/acpi/button/lid/LID/state", "state:      open\n");
        for (f, v) in [
            ("sys_vendor", "ASUSTeK COMPUTER INC."),
            ("board_name", "UX482EG"),
            ("product_name", "ZenBook UX482EG_UX482EG"),
            ("bios_version", "UX482EG.311"),
        ] {
            put(root, &format!("sys/class/dmi/id/{f}"), &format!("{v}\n"));
        }
        // hwmon numbering deliberately differs from hers: discovery goes by name.
        put(root, "sys/class/hwmon/hwmon0/name", "AC0\n");
        put(root, "sys/class/hwmon/hwmon3/name", "nvme\n");
        put(root, "sys/class/hwmon/hwmon3/temp1_input", "38850\n");
        put(root, "sys/class/hwmon/hwmon6/name", "asus\n");
        put(root, "sys/class/hwmon/hwmon6/fan1_input", "3500\n");
        put(root, "sys/class/hwmon/hwmon11/name", "coretemp\n");
        put(root, "sys/class/hwmon/hwmon11/temp1_input", "57000\n");
        put(root, "sys/class/hwmon/hwmon11/temp1_crit", "100000\n");
        put(root, "sys/class/hwmon/hwmon11/temp1_max", "100000\n");
        for (i, t) in [(2, 55000), (3, 46000), (4, 57000), (5, 48000)] {
            put(
                root,
                &format!("sys/class/hwmon/hwmon11/temp{i}_input"),
                &format!("{t}\n"),
            );
        }
        for (i, kind, temp) in [
            (0, "acpitz", 27800),
            (3, "SEN2", 41000),
            (5, "SEN4", 39000),
            (7, "x86_pkg_temp", 58000),
        ] {
            put(
                root,
                &format!("sys/class/thermal/thermal_zone{i}/type"),
                &format!("{kind}\n"),
            );
            put(
                root,
                &format!("sys/class/thermal/thermal_zone{i}/temp"),
                &format!("{temp}\n"),
            );
        }
        for i in 0..8 {
            put(
                root,
                &format!("sys/class/thermal/cooling_device{i}/type"),
                "Processor\n",
            );
            put(
                root,
                &format!("sys/class/thermal/cooling_device{i}/cur_state"),
                "0\n",
            );
        }
        put(
            root,
            "sys/class/thermal/cooling_device8/type",
            "intel_powerclamp\n",
        );
        put(root, "sys/class/thermal/cooling_device8/cur_state", "0\n");
        put(
            root,
            "sys/class/thermal/cooling_device9/type",
            "TCC Offset\n",
        );
        put(root, "sys/class/thermal/cooling_device9/cur_state", "8\n");
        put(root, "sys/devices/system/cpu/online", "0-7\n");
        for cpu in 0..8 {
            let d = format!("sys/devices/system/cpu/cpu{cpu}");
            put(
                root,
                &format!("{d}/topology/core_id"),
                &format!("{}\n", cpu % 4),
            );
            put(root, &format!("{d}/topology/physical_package_id"), "0\n");
            put(
                root,
                &format!("{d}/cpufreq/cpuinfo_avg_freq"),
                &format!("{}\n", 3_000_000 + cpu * 100_000),
            );
            put(root, &format!("{d}/cpufreq/scaling_cur_freq"), "1\n");
            put(
                root,
                &format!("{d}/cpufreq/energy_performance_preference"),
                "performance\n",
            );
            put(
                root,
                &format!("{d}/thermal_throttle/package_throttle_total_time_ms"),
                "18284\n",
            );
            put(
                root,
                &format!("{d}/thermal_throttle/package_throttle_count"),
                "4056\n",
            );
            put(
                root,
                &format!("{d}/thermal_throttle/core_throttle_total_time_ms"),
                &format!("{}\n", 100 + cpu),
            );
        }
        put(
            root,
            "sys/devices/system/cpu/cpufreq/policy0/scaling_driver",
            "intel_pstate\n",
        );
        put(root, "sys/devices/system/cpu/intel_pstate/no_turbo", "0\n");
        put(
            root,
            "sys/devices/system/cpu/intel_pstate/max_perf_pct",
            "100\n",
        );
        put(root, "sys/firmware/acpi/platform_profile", "performance\n");
        // The decoy: the MSR domain says 60/60 W, the package obeys MMIO 28/35 W.
        put(
            root,
            "sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw",
            "60000000\n",
        );
        put(
            root,
            "sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw",
            "60000000\n",
        );
        put(
            root,
            "sys/class/powercap/intel-rapl:0/constraint_0_max_power_uw",
            "28000000\n",
        );
        put(
            root,
            "sys/class/powercap/intel-rapl-mmio:0/constraint_0_power_limit_uw",
            "28000000\n",
        );
        put(
            root,
            "sys/class/powercap/intel-rapl-mmio:0/constraint_1_power_limit_uw",
            "35000000\n",
        );
        put(
            root,
            "sys/class/powercap/intel-rapl-mmio:0/constraint_0_max_power_uw",
            "28000000\n",
        );
        put(
            root,
            "sys/bus/pci/devices/0000:00:04.0/tcc_offset_degree_celsius",
            "8\n",
        );
        put(
            root,
            "sys/bus/pci/devices/0000:00:02.0/tile0/gt0/freq0/throttle/status",
            "0\n",
        );
        put(
            root,
            "sys/bus/pci/devices/0000:00:02.0/tile0/gt0/freq0/throttle/reason_pl1",
            "1\n",
        );
        put(
            root,
            "sys/bus/pci/devices/0000:00:02.0/tile0/gt0/freq0/throttle/reason_prochot",
            "0\n",
        );
        put(
            root,
            "sys/bus/pci/devices/0000:00:02.0/tile0/gt0/freq0/throttle/reason_thermal",
            "0\n",
        );
        put(
            root,
            "sys/bus/pci/devices/0000:00:02.0/tile0/gt0/freq0/act_freq",
            "350\n",
        );
        link(
            root,
            "sys/class/drm/card1/device",
            "../../../bus/pci/devices/0000:00:02.0",
        );
        link(
            root,
            "sys/bus/pci/devices/0000:00:02.0/driver",
            "../../../bus/pci/drivers/xe",
        );
        put(root, "sys/class/drm/card1-eDP-1/status", "connected\n");
        put(
            root,
            "sys/bus/pci/devices/0000:01:00.0/power/runtime_status",
            "suspended\n",
        );
        link(
            root,
            "sys/class/drm/card0/device",
            "../../../bus/pci/devices/0000:01:00.0",
        );
        link(
            root,
            "sys/bus/pci/devices/0000:01:00.0/driver",
            "../../../bus/pci/drivers/nvidia",
        );
        put(root, "sys/class/power_supply/AC0/type", "Mains\n");
        put(root, "sys/class/power_supply/AC0/online", "1\n");
        put(root, "sys/class/power_supply/BAT0/type", "Battery\n");
        put(root, "sys/class/power_supply/BAT0/status", "Not charging\n");
        put(root, "sys/class/power_supply/BAT0/capacity", "100\n");
        put(root, "sys/class/power_supply/BAT0/power_now", "0\n");
        put(
            root,
            "sys/class/power_supply/hid-pen-battery/type",
            "Battery\n",
        );
        put(
            root,
            "sys/class/power_supply/hid-pen-battery/scope",
            "Device\n",
        );
        put(
            root,
            "sys/class/power_supply/hid-pen-battery/status",
            "Discharging\n",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{put, test_dir};

    #[test]
    fn discovers_the_duo_by_name() {
        let root = test_dir("sensors-duo");
        fake::duo(&root);
        let s = Sensors::discover(&root);

        assert_eq!(s.temp_driver.as_deref(), Some("coretemp"));
        assert_eq!(s.temp_c(), Some(57.0));
        assert_eq!(s.temp_crit_c, Some(100.0));
        assert_eq!(
            s.core_temps_c(),
            vec![Some(55.0), Some(46.0), Some(57.0), Some(48.0)]
        );
        assert_eq!(s.t4_c(), Some(58.0));
        assert_eq!(s.fan_rpm(), Some(3500));
        assert_eq!(s.pkg_throttle_ms(), Some(18284));
        assert_eq!(s.pkg_throttle_count(), Some(4056));
        // Four physical cores, not eight hyper-threads.
        assert_eq!(
            s.core_throttle_ms(),
            vec![Some(100), Some(101), Some(102), Some(103)]
        );
        assert_eq!(s.freqs_mhz().len(), 8);
        assert_eq!(s.freqs_mhz()[1], 3100);
        assert_eq!(s.nproc, 8);
        assert_eq!(
            s.cpu_times(),
            Some(CpuTimes {
                total: 10_000,
                idle: 8500
            })
        );

        // status 0: the reasons are not consulted, even though reason_pl1 holds 1.
        assert_eq!(s.xe(), Some(XeRead::default()));
        assert_eq!(s.xe_act_mhz(), Some(350));
        put(
            &root,
            "sys/bus/pci/devices/0000:00:02.0/tile0/gt0/freq0/throttle/status",
            "1\n",
        );
        assert_eq!(
            s.xe(),
            Some(XeRead {
                status: true,
                pl1: true,
                prochot: false,
                thermal: false
            })
        );

        let p = s.power();
        assert_eq!(p.ac, Some(true));
        assert_eq!(p.bat_status, Some(BatStatus::NotCharging));
        assert_eq!(p.capacity, Some(100));
        assert!(!p.deficit());

        let c = s.config();
        assert_eq!(c.platform_profile.as_deref(), Some("performance"));
        assert_eq!(c.epp.as_deref(), Some("performance"));
        assert_eq!(c.tcc_offset, Some(8));
        assert_eq!(c.no_turbo, Some(false));
        assert_eq!(c.max_perf_pct, Some(100));
        assert_eq!(c.cooling_max, Some(0), "TCC Offset's 8 must not count");
        assert_eq!(c.dgpu_runtime.as_deref(), Some("suspended"));
        assert_eq!(c.lid_open, Some(true));
        assert_eq!(s.tcc_c(c.tcc_offset), 92.0);

        let ctx = s.context();
        assert_eq!(
            (ctx.sen2, ctx.sen4, ctx.acpitz),
            (Some(41.0), Some(39.0), Some(27.8))
        );
        assert_eq!(s.classes(true), vec!["E1", "E2", "check"]);
        // Light-load evidence off (21/11/2026): the counter is there, E2 is not listed.
        assert_eq!(s.classes(false), vec!["E1", "check"]);
        assert!(s.pkg_thr_ms.is_some());
        assert!(!s.failed());

        let info = StaticInfo::read(&root);
        assert_eq!(info.cpu_model, "11th Gen Intel(R) Core(TM) i7-1165G7");
        assert_eq!(info.board_name, "UX482EG");
        assert_eq!(info.boot_id, "4d5ab25d-6171-4ce0-ac1b-ec48694c2006");
    }

    #[test]
    fn power_limits_come_from_mmio_not_the_msr_decoy() {
        let root = test_dir("sensors-rapl-decoy");
        fake::duo(&root);
        let c = Sensors::discover(&root).config();
        assert_eq!(c.pl_domain.as_deref(), Some("intel-rapl-mmio:0"));
        assert_eq!(c.pl1_uw, Some(28_000_000));
        assert_eq!(c.pl2_uw, Some(35_000_000));
        assert_eq!(c.pl1_max_uw, Some(28_000_000));

        // Without the MMIO domain, the MSR one is all there is (generic mode).
        fs::remove_dir_all(root.join("sys/class/powercap/intel-rapl-mmio:0")).unwrap();
        let c = Sensors::discover(&root).config();
        assert_eq!(c.pl_domain.as_deref(), Some("intel-rapl:0"));
        assert_eq!(c.pl1_uw, Some(60_000_000));
    }

    #[test]
    fn reads_are_live_and_errors_mark_the_set_stale() {
        let root = test_dir("sensors-live");
        fake::duo(&root);
        let s = Sensors::discover(&root);
        // Rewriting in place keeps the inode, as sysfs does: the open fd sees the new value.
        fs::write(root.join("sys/class/hwmon/hwmon11/temp1_input"), "86000\n").unwrap();
        assert_eq!(s.temp_c(), Some(86.0));
        // Parse errors are not I/O errors.
        fs::write(root.join("sys/class/hwmon/hwmon11/temp1_input"), "N/A\n").unwrap();
        assert_eq!(s.temp_c(), None);
        assert!(!s.failed());

        // A directory opens but cannot be read: the stand-in for EIO from a vanished device.
        let fan = root.join("sys/class/hwmon/hwmon6/fan1_input");
        fs::remove_file(&fan).unwrap();
        fs::create_dir(&fan).unwrap();
        let s = Sensors::discover(&root);
        assert_eq!(s.fan_rpm(), None);
        assert!(s.failed());

        fs::remove_dir(&fan).unwrap();
        fs::write(&fan, "6800\n").unwrap();
        let s = Sensors::discover(&root);
        assert_eq!(s.fan_rpm(), Some(6800));
        assert!(!s.failed());
    }

    #[test]
    fn generic_discovery() {
        let root = test_dir("sensors-generic");
        put(&root, "proc/stat", "cpu  1 2 3 4 5 6 7 8 9 10\n");
        put(&root, "sys/class/hwmon/hwmon0/name", "k10temp\n");
        put(&root, "sys/class/hwmon/hwmon0/temp1_input", "50000\n");
        put(&root, "sys/class/hwmon/hwmon0/temp1_label", "Tccd1\n");
        put(&root, "sys/class/hwmon/hwmon0/temp3_input", "61500\n");
        put(&root, "sys/class/hwmon/hwmon0/temp3_label", "Tctl\n");
        put(&root, "sys/class/hwmon/hwmon1/name", "thinkpad\n");
        put(&root, "sys/class/hwmon/hwmon1/fan1_input", "2000\n");
        put(&root, "sys/class/hwmon/hwmon1/fan2_input", "0\n");
        put(&root, "sys/class/power_supply/AC/type", "Mains\n");
        put(&root, "sys/class/power_supply/AC/online", "0\n");
        put(&root, "sys/class/power_supply/BAT1/type", "Battery\n");
        put(&root, "sys/class/power_supply/BAT1/status", "Discharging\n");
        put(&root, "sys/class/power_supply/BAT1/power_now", "9000000\n");
        let s = Sensors::discover(&root);
        assert_eq!(s.temp_driver.as_deref(), Some("k10temp"));
        assert_eq!(s.temp_c(), Some(61.5));
        assert!(s.core_temps.is_empty());
        // No throttle counters: E2 drops.
        assert_eq!(s.classes(true), vec!["E1", "check"]);
        // No crit, no max: 95 °C.
        assert_eq!(s.tcc_c(None), 95.0);
        assert_eq!(s.fan_rpm(), Some(2000));
        // fan2 spins up past fan1's best: it becomes the fan.
        fs::write(root.join("sys/class/hwmon/hwmon1/fan2_input"), "4000\n").unwrap();
        assert_eq!(s.fan_rpm(), Some(4000));
        fs::write(root.join("sys/class/hwmon/hwmon1/fan2_input"), "1000\n").unwrap();
        assert_eq!(s.fan_rpm(), Some(1000));
        let p = s.power();
        assert_eq!(p.ac, Some(false));
        assert!(!p.deficit(), "on battery is not a deficit");
        assert_eq!(s.config().pl_domain, None);
    }

    #[test]
    fn deficit_rules() {
        let p = |status, power| PowerRead {
            ac: Some(true),
            bat_status: Some(status),
            capacity: Some(80),
            power_now_uw: power,
        };
        assert!(p(BatStatus::Discharging, None).deficit());
        assert!(p(BatStatus::Discharging, Some(4_000_000)).deficit());
        assert!(!p(BatStatus::NotCharging, Some(4_000_000)).deficit());
        assert!(!p(BatStatus::Full, None).deficit());
        assert!(!p(BatStatus::Charging, Some(20_000_000)).deficit());
    }

    /// Replaces a file with a directory: it stays openable but every read fails, the stand-in for
    /// an ACPI evaluation that errors.
    fn break_file(path: &Path) {
        fs::remove_file(path).unwrap();
        fs::create_dir(path).unwrap();
    }

    #[test]
    fn power_now_only_while_discharging() {
        let root = test_dir("sensors-power-now");
        fake::duo(&root);
        let bat = root.join("sys/class/power_supply/BAT0");
        // Full with the rate unknown: ACPI answers ENODEV for power_now. It is not read at all.
        fs::write(bat.join("status"), "Full\n").unwrap();
        break_file(&bat.join("power_now"));
        let s = Sensors::discover(&root);
        let p = s.power();
        assert_eq!((p.ac, p.power_now_uw), (Some(true), None));
        assert!(!p.deficit());
        assert!(
            !s.failed(),
            "a battery that is full must not trigger rediscovery"
        );
        // Discharging on AC is a deficit, and an unreadable rate still does not mark the set stale.
        fs::write(bat.join("status"), "Discharging\n").unwrap();
        let p = s.power();
        assert!(p.deficit());
        assert_eq!(p.power_now_uw, None);
        assert!(!s.failed());
        s.describe();
        assert!(!s.failed(), "the sensors listing reads it quietly too");

        fs::remove_dir(bat.join("power_now")).unwrap();
        fs::write(bat.join("power_now"), "4200000\n").unwrap();
        let s = Sensors::discover(&root);
        assert_eq!(s.power().power_now_uw, Some(4_200_000));
    }

    #[test]
    fn a_failed_ac_read_asks_the_battery() {
        let root = test_dir("sensors-ac-failed");
        fake::duo(&root);
        let bat = root.join("sys/class/power_supply/BAT0");
        break_file(&root.join("sys/class/power_supply/AC0/online"));
        fs::write(bat.join("status"), "Discharging\n").unwrap();
        let s = Sensors::discover(&root);
        let p = s.power();
        assert_eq!(p.ac, Some(false), "on battery, never read as AC");
        assert!(!p.deficit());
        assert!(s.failed());
        fs::write(bat.join("status"), "Not charging\n").unwrap();
        assert_eq!(s.power().ac, Some(true));
        // Nothing to go on: unknown, for the sampler to carry the last state.
        fs::write(bat.join("status"), "Unknown\n").unwrap();
        assert_eq!(s.power().ac, None);
    }

    #[test]
    fn unreadable_key_fields_are_marked_missing_ones_are_not() {
        let root = test_dir("sensors-unread");
        fake::duo(&root);
        let s = Sensors::discover(&root);
        assert_eq!(s.config().unread, Unread::default());

        // A failed _LID evaluation: the kernel prints "unsupported", with no I/O error.
        put(
            &root,
            "proc/acpi/button/lid/LID/state",
            "state:      unsupported\n",
        );
        // The held descriptor would still read the unlinked file: discover the broken one.
        break_file(&root.join("sys/firmware/acpi/platform_profile"));
        let c = Sensors::discover(&root).config();
        assert_eq!((c.lid_open, c.platform_profile.as_deref()), (None, None));
        assert!(c.unread.lid && c.unread.platform_profile);
        assert!(!c.unread.epp && !c.unread.pl1);

        // A file that is not there at all is absent, not unread.
        fs::remove_file(root.join("sys/devices/system/cpu/intel_pstate/no_turbo")).unwrap();
        let c = Sensors::discover(&root).config();
        assert_eq!(c.no_turbo, None);
        assert!(!c.unread.no_turbo);

        // Carrying fills only the unread fields.
        let prev = ConfigRead {
            lid_open: Some(true),
            platform_profile: Some("performance".into()),
            no_turbo: Some(false),
            ..ConfigRead::default()
        };
        let mut c = c;
        c.carry_unread(&prev);
        assert_eq!(c.lid_open, Some(true));
        assert_eq!(c.platform_profile.as_deref(), Some("performance"));
        assert_eq!(c.no_turbo, None, "a missing file is a real change");
    }

    #[test]
    fn tcc_point_rules() {
        assert_eq!(tcc_point(Some(100.0), Some(90.0), Some(8)), 92.0);
        assert_eq!(tcc_point(Some(100.0), None, None), 100.0);
        assert_eq!(tcc_point(None, Some(90.0), Some(8)), 90.0);
        assert_eq!(tcc_point(None, None, None), 95.0);
    }

    #[test]
    fn proc_stat_parse() {
        let c = CpuTimes::parse("cpu  1334712 98484 1043867 18199942 1353859 96562 42523 0 7 7\n")
            .unwrap();
        assert_eq!(c.idle, 18_199_942 + 1_353_859);
        assert_eq!(
            c.total,
            1_334_712 + 98_484 + 1_043_867 + 18_199_942 + 1_353_859 + 96_562 + 42_523
        );
        assert_eq!(CpuTimes::parse("cpu0 1 2 3 4 5\n"), None);
    }
}
