//! The service (§12.1): the only writer of the store.
//!
//! One thread runs `poll()` over the listening socket, its clients, a signalfd (SIGTERM, SIGINT)
//! and, during a check, the burn child's pipe. The timeout is the next CLOCK_MONOTONIC deadline,
//! so the unit's TimerSlackNSec applies (a timerfd would ignore it). Each row goes sampler →
//! episodes → rules → present; state.json is written only when it changed, and every write is
//! broadcast to the clients as a `state` event.
//!
//! Tests drive the same code with a fake sysfs root, a store under target/test-tmp and a clock
//! they advance by hand (`Clock`), calling `step` instead of `run`.

use crate::check::{self, Check, CheckMachine, Cmd, Refs, Setup};
use crate::cli::{EXIT_ERROR, EXIT_OK, EXIT_RUNNING};
use crate::clock::{self, Date, Moment};
use crate::episodes::Output;
use crate::ipc::{self, Conn, IpcError};
use crate::model::{
    CaseRecord, CheckPhase, CheckState, DailyRecord, Event, EventKind, Record, References, State,
};
use crate::paths::Paths;
use crate::profile::{self, ConfigKey};
use crate::rules::{CheckOutcome, Effect, Engine, KeyCtx, Machine, Watch};
use crate::sampler::{self, ProcSnapshot, Row, Sampler, top_consumers};
use crate::sensors::{ConfigRead, StaticInfo};
use crate::store::{self, Store, StoreError, StoreLock};
use crate::{SCHEMA, note, present};
use serde_json::{Value, json};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::SystemTime;

const NS: i64 = 1_000_000_000;
/// The config, lid and chassis poll (§12.1). The sampler times its own config reads; this one
/// covers the settings file and the time zone.
const POLL_NS: i64 = 60 * NS;
/// While a check runs, config and lid are read this often instead of once a minute: the lid
/// closing or a settings change must stop the load within seconds (§10.5), as AC loss does
/// through the 5-s power read, and the lid gate must see it open. An ACPI lid read every 5 s
/// costs nothing against a full load or an idle machine being waited on; the /proc walk is
/// not part of it.
const CHECK_EDGE_NS: i64 = 5 * NS;
/// Clients beyond this are turned away: each one costs a descriptor and two buffers.
const MAX_CLIENTS: usize = 32;
/// "mostly rustc": the top consumers kept for a plateau or a hot window.
const TOP_N: usize = 3;
/// A light window is hot only with T near TCC (§5.2), so a minute that starts more than this
/// below TCC, after a window that was not hot, skips its /proc scan.
const WARM_BELOW_TCC: f64 = 10.0;
/// The engine keeps 90 days of tallies; record archives older than that are not read at start.
const RESTORE_DAYS: i64 = 92;

/// Where the service gets the time. The real one reads the three clocks; tests use a timeline.
pub trait Clock {
    fn now(&mut self) -> Moment;
    /// Follow a change of the local time zone (glibc loads it only once). Called every 60 s.
    fn reload_zone(&mut self) {}
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&mut self) -> Moment {
        Moment::now()
    }

    fn reload_zone(&mut self) {
        clock::reload_zone();
    }
}

/// What a running cooling check (§10) needs from the loop. `check::Check` is the real one;
/// `start_check` builds it.
///
/// While a check runs the loop samples full rows at 1 Hz and calls `t4` at 4 Hz, polls
/// `burn_fd`, and hands every row to `row` before the passive detector sees it (the detector is
/// suspended for the whole check). The burn child inherits the service's blocked SIGTERM and
/// SIGINT, so its `pre_exec` must unblock them along with PR_SET_PDEATHSIG.
pub trait CheckRun {
    /// The burn child's stdout while it runs; None once it has been read to the end. The
    /// service stops polling an fd after it hangs up, even if this still returns it.
    fn burn_fd(&self) -> Option<RawFd>;
    /// `burn_fd` is readable or has hung up. After a hang-up this is the last call for that
    /// fd, so it must read to EOF, not just one chunk.
    fn burn_ready(&mut self, now: &Moment) -> CheckStep;
    /// An `x86_pkg_temp` reading at the 4 Hz cadence.
    fn t4(&mut self, t4: Option<f64>, now: &Moment);
    /// Every row.
    fn row(&mut self, row: &Row, key: &KeyCtx, now: &Moment) -> CheckStep;
    /// Cancel, or the service stopping: kill the burn group now and end as aborted with `reason`.
    fn abort(&mut self, reason: &str, now: &Moment);
    /// The `check` block of state.json.
    fn state(&self) -> &CheckState;
    /// `check.rows`: every row so far.
    fn rows(&self) -> Value;
    /// It ended because the fan stood still under load, which raises FAN too (§10.5).
    fn fan_stopped(&self) -> bool {
        false
    }
    /// result.json's content once it has ended, for the final `check` event.
    fn result(&self) -> Value {
        Value::Null
    }
}

/// What a check asks of the service after a row or a burn read.
#[derive(Debug)]
pub enum CheckStep {
    Running,
    /// The per-second values: broadcast as `check`, state.json untouched.
    Live(Value),
    /// A gate or phase changed: state.json is rewritten and `payload` broadcast as `check`.
    Event(Value),
    /// Over. `Some` carries the verdicts for the rules; `None` is a refusal or an abort.
    Finished(Option<Box<CheckOutcome>>),
}

/// Why the service did not start.
#[derive(Debug)]
pub enum StartError {
    /// Another service owns the store or the socket (exit 4: restarting cannot help).
    Owned(String),
    Failed(String),
}

impl StartError {
    pub fn code(&self) -> i32 {
        match self {
            StartError::Owned(_) => EXIT_RUNNING,
            StartError::Failed(_) => EXIT_ERROR,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            StartError::Owned(m) | StartError::Failed(m) => m,
        }
    }
}

fn failed(what: &Path) -> impl FnOnce(io::Error) -> StartError + '_ {
    move |e| StartError::Failed(format!("{}: {e}", what.display()))
}

/// `minka-thermal service`.
pub fn main(paths: &Paths, root: &Path) -> i32 {
    match Service::start(paths, root, Box::new(SystemClock), true) {
        Ok(mut service) => {
            // The load is this same binary's hidden `burn` verb, one thread per CPU.
            let threads = service.watch.engine.machine.info.nproc.max(1);
            service.load = Some(Cmd::burn(threads));
            service.run()
        }
        Err(e) => {
            note(format_args!("{}", e.message()));
            e.code()
        }
    }
}

/// The /proc snapshots behind "mostly rustc": one at a plateau's onset, and one at the start of
/// each wall-clock minute while light-load throttling is likely, so a hot window has a "before"
/// (§3.2 scans only at its end, which alone would give each process's whole lifetime).
#[derive(Default)]
struct Tops {
    plateau: Option<ProcSnapshot>,
    /// The last two minute-start snapshots, (unix s, snapshot): a window closes a row or two into
    /// the next minute, after that minute's own snapshot has been taken.
    minutes: Vec<(i64, ProcSnapshot)>,
    /// Taken during the current row, shared by everything that needs one then.
    fresh: Option<ProcSnapshot>,
    last_minute: Option<i64>,
    last_window_hot: bool,
}

impl Tops {
    fn fresh(&mut self, sampler: &Sampler) -> &ProcSnapshot {
        self.fresh.get_or_insert_with(|| sampler.snapshot())
    }

    fn for_output(&mut self, out: &Output, sampler: &Sampler) -> Vec<(String, f64)> {
        match out {
            Output::PlateauOpened { .. } => {
                self.plateau = Some(self.fresh(sampler).clone());
                Vec::new()
            }
            Output::Plateau(Record::Plateau(_)) => match self.plateau.take() {
                Some(before) => top_consumers(&before, self.fresh(sampler), TOP_N),
                None => Vec::new(),
            },
            Output::Plateau(_) => {
                self.plateau = None;
                Vec::new()
            }
            Output::Window(w) => {
                self.last_window_hot = w.hot_ms > 0;
                let start = w.start.unix_s();
                let before = self
                    .minutes
                    .iter()
                    .position(|(at, _)| (at - start).abs() <= 5);
                match before {
                    Some(i) if w.hot_ms > 0 => {
                        let (_, before) = self.minutes.remove(i);
                        top_consumers(&before, self.fresh(sampler), TOP_N)
                    }
                    _ => Vec::new(),
                }
            }
        }
    }

    /// After each row: at the first row of a new minute, take that minute's snapshot when it may
    /// be needed.
    fn after_row(&mut self, row: &Row, tcc_c: f64, sampler: &Sampler, windows_possible: bool) {
        let minute = row.at.unix_s().div_euclid(60);
        if self.last_minute != Some(minute) {
            self.last_minute = Some(minute);
            let warm =
                row.t.is_some_and(|t| t >= tcc_c - WARM_BELOW_TCC) || self.last_window_hot;
            if warm && windows_possible {
                let snap = self.fresh(sampler).clone();
                self.minutes.push((row.at.unix_s(), snap));
                if self.minutes.len() > 2 {
                    self.minutes.remove(0);
                }
            } else {
                self.minutes.clear();
            }
        }
        self.fresh = None;
    }
}

/// A method's result: most return the state after the change.
enum Reply {
    State,
    Value(Value),
}

pub struct Service {
    paths: Paths,
    clock: Box<dyn Clock>,
    _lock: StoreLock,
    listener: UnixListener,
    /// (dev, ino) of the socket this service bound; it removes the file at exit only if it is
    /// still that one.
    socket_id: (u64, u64),
    signals: Option<OwnedFd>,
    clients: Vec<Conn>,
    store: Store,
    sampler: Sampler,
    watch: Watch,
    check: Option<Box<dyn CheckRun>>,
    /// The check's load child. `main` sets `<this exe> burn --threads N`; tests set something
    /// harmless, and without one a check is refused.
    pub load: Option<Cmd>,
    /// nvidia-smi's query for the dGPU gate, when it exists under the root.
    gpu_query: Option<Cmd>,
    tops: Tops,
    /// MONOTONIC ns: when the next row is due, and when the last one was read.
    next_row_ns: i64,
    last_read_ns: i64,
    next_t4_ns: Option<i64>,
    /// The running check's burn fd after it hung up: no longer polled (see `step`).
    burn_hung_up: Option<RawFd>,
    next_poll_ns: i64,
    /// While a check runs: when config and lid are next read.
    next_check_edge_ns: i64,
    settings_mtime: Option<SystemTime>,
    last_ac: Option<bool>,
    last_lid: Option<bool>,
    /// A failed state.json write has been logged; the next ones stay quiet until one succeeds.
    state_failing: bool,
    turned_away: bool,
}

impl Service {
    /// Start-up steps 1-5 of §12.1. With `signals`, SIGTERM and SIGINT are blocked in this
    /// thread and read from a signalfd instead.
    pub fn start(
        paths: &Paths,
        root: &Path,
        mut clock: Box<dyn Clock>,
        signals: bool,
    ) -> Result<Service, StartError> {
        // 1. The store directory, mode 0700.
        store::ensure_dir(paths).map_err(failed(&paths.dir))?;
        // 2. Only services take this lock; a second one stops here.
        let lock = match store::try_lock(paths) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return Err(StartError::Owned(format!(
                    "another minka-thermal service owns {}",
                    paths.dir.display()
                )));
            }
            Err(e) => return Err(failed(&paths.lock)(e)),
        };
        // 3. The socket.
        let (listener, socket_id) = bind_socket(&paths.socket)?;
        let signals = if signals {
            Some(
                signal_fd(&[libc::SIGTERM, libc::SIGINT])
                    .map_err(|e| StartError::Failed(format!("signalfd: {e}")))?,
            )
        } else {
            None
        };

        // 4. References and state, then the tallies from the logs.
        let now = clock.now();
        // Files set aside here, as (notice id, notice text), raised once the engine exists.
        let mut unreadable = Vec::new();
        let refs = match store::load_references(paths) {
            Ok(r) => r,
            Err(StoreError::Corrupt(path, why)) => {
                let kept = set_aside(&path, &why, &now, "starting with no learned references")?;
                unreadable.push((
                    present::notice::REFERENCES_UNREADABLE,
                    present::notice::references_unreadable(&kept),
                ));
                References {
                    schema: SCHEMA,
                    ..References::default()
                }
            }
            Err(e) => return Err(StartError::Failed(e.to_string())),
        };
        let mut state = match store::load_state(paths) {
            Ok(s) => s,
            // A crash loop would help nobody: the file is kept beside the store, and the history
            // in events.jsonl and cases.jsonl is untouched.
            Err(StoreError::Corrupt(path, why)) => {
                let kept = set_aside(&path, &why, &now, "starting from a fresh state")?;
                unreadable.push((
                    present::notice::STATE_UNREADABLE,
                    present::notice::state_unreadable(&kept),
                ));
                None
            }
            Err(e) => return Err(StartError::Failed(e.to_string())),
        };
        let aborted = state.as_mut().is_some_and(abort_unfinished_check);
        let mut store = Store::open(paths, &now).map_err(|e| StartError::Failed(e.to_string()))?;
        if aborted {
            let event = store.event(
                &now,
                EventKind::CheckResult,
                state.as_ref().and_then(|s| s.case.as_ref()).map(|c| c.id.as_str()),
                "Cooling check aborted: the service restarted".to_owned(),
                json!({"aborted": "service restart"}),
            );
            if let Err(e) = event {
                note(format_args!("{}: {e}", paths.events.display()));
            }
            if let Some(c) = state.as_ref().and_then(|s| s.check.as_ref()) {
                close_check_dir(&paths.dir, c, &now);
            }
        }
        let daily: Vec<DailyRecord> = store::read_jsonl(&paths.daily).map_err(failed(&paths.daily))?;
        let cases: Vec<CaseRecord> = store::read_jsonl(&paths.cases).map_err(failed(&paths.cases))?;
        let events: Vec<Event> = store::read_jsonl(&paths.events).map_err(failed(&paths.events))?;
        let records = recent_records(paths, &now);

        // 5. Sensors, DMI, chassis; the key and the BIOS comparison come with the first row.
        let sampler = Sampler::new(root);
        let info = StaticInfo::read(root);
        let setting = profile::read_chassis_setting(paths.settings.as_deref());
        let chassis = profile::resolve_chassis(setting, &info);
        let sensors = &sampler.sensors;
        let machine = Machine {
            chassis,
            setting,
            info: info.clone(),
            tjmax_c: sensors.temp_crit_c,
            has_fan: sensors.has_fan(),
            has_throttle: sensors.pkg_thr_ms.is_some(),
            classes: sensors.classes().iter().map(|c| (*c).to_owned()).collect(),
        };
        let mut engine = Engine::new(machine, state, refs, &now);
        engine.restore(&daily, &records, &cases, &events, &now);
        // Written with the first row's commit: a notice in state.json and an events.jsonl line.
        for (id, text) in unreadable {
            engine.store_unreadable(id, text, &now);
        }
        engine.state.pid = std::process::id();
        engine.state.boot_id = info.boot_id.clone();
        engine.state.started_at = Some(now.stamp());

        let settings_mtime = mtime(paths.settings.as_deref());
        let mut service = Service {
            paths: paths.clone(),
            clock,
            _lock: lock,
            listener,
            socket_id,
            signals,
            clients: Vec::new(),
            store,
            sampler,
            watch: Watch::new(engine),
            check: None,
            load: None,
            gpu_query: Some(root.join("usr/bin/nvidia-smi"))
                .filter(|p| p.is_file())
                .map(|p| Cmd::gpu_query(&p)),
            tops: Tops::default(),
            next_row_ns: now.mono_ns,
            last_read_ns: now.mono_ns - NS,
            next_t4_ns: None,
            burn_hung_up: None,
            next_poll_ns: now.mono_ns + POLL_NS,
            next_check_edge_ns: 0,
            settings_mtime,
            last_ac: None,
            last_lid: None,
            state_failing: false,
            turned_away: false,
        };
        // The first row sets the key, settles the days missed while the service was down and
        // writes state.json with the new pid and boot id.
        service.tick();
        let m = &service.watch.engine.machine;
        note(format_args!(
            "service started: pid {}, store {}, socket {}, chassis {} ({}), classes {}",
            std::process::id(),
            paths.dir.display(),
            paths.socket.display(),
            m.chassis.as_str(),
            m.setting.as_str(),
            m.classes.join(" ")
        ));
        Ok(service)
    }

    /// The state as the service holds it (state.json once written).
    pub fn state(&self) -> &State {
        &self.watch.engine.state
    }

    /// The loop, until SIGTERM or SIGINT.
    pub fn run(&mut self) -> i32 {
        while self.step(None) {}
        self.shutdown();
        EXIT_OK
    }

    fn next_deadline(&self) -> i64 {
        match self.next_t4_ns {
            Some(t4) if self.check.is_some() => t4.min(self.next_row_ns),
            _ => self.next_row_ns,
        }
    }

    /// One round: poll until the next deadline (at most `max_wait_ms`), serve the clients, then
    /// read a row if one is due. False once a stop signal has arrived.
    pub fn step(&mut self, max_wait_ms: Option<i64>) -> bool {
        let now = self.clock.now();
        // Rounded up, so the poll never wakes just short of the deadline and spins.
        let mut wait_ms = ((self.next_deadline() - now.mono_ns).max(0) + 999_999) / 1_000_000;
        if let Some(m) = max_wait_ms {
            wait_ms = wait_ms.min(m);
        }

        let mut fds = Vec::with_capacity(3 + self.clients.len());
        let pollfd = |fd: RawFd, events: i16| libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let sig_slot = self.signals.as_ref().map(|fd| {
            fds.push(pollfd(fd.as_raw_fd(), libc::POLLIN));
            fds.len() - 1
        });
        fds.push(pollfd(self.listener.as_raw_fd(), libc::POLLIN));
        let listen_slot = fds.len() - 1;
        let burn_slot = self
            .check
            .as_ref()
            .and_then(|c| c.burn_fd())
            .filter(|fd| self.burn_hung_up != Some(*fd))
            .map(|fd| {
                fds.push(pollfd(fd, libc::POLLIN));
                fds.len() - 1
            });
        let first_client = fds.len();
        for c in &self.clients {
            let mut events = 0;
            if !c.eof {
                events |= libc::POLLIN;
            }
            if c.pending() > 0 {
                events |= libc::POLLOUT;
            }
            fds.push(pollfd(c.fd(), events));
        }
        // SAFETY: `fds` is a valid array of `fds.len()` pollfd structs for the whole call.
        let n = unsafe {
            libc::poll(
                fds.as_mut_ptr(),
                fds.len() as libc::nfds_t,
                wait_ms.min(i32::MAX as i64) as libc::c_int,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                note(format_args!("poll: {e}"));
            }
        } else if n > 0 {
            if let Some(i) = sig_slot
                && fds[i].revents != 0
                && let Some(signo) = read_signal(fds[i].fd)
            {
                note(format_args!(
                    "stopping on {}",
                    if signo == libc::SIGINT { "SIGINT" } else { "SIGTERM" }
                ));
                return false;
            }
            // Before the clients: a cancel and a new check.start in this batch would otherwise
            // hand the old pipe's readiness to the new check.
            if let Some(i) = burn_slot
                && fds[i].revents != 0
                && self.check.as_ref().and_then(|c| c.burn_fd()) == Some(fds[i].fd)
            {
                // A pipe that has hung up stays readable for ever; polling it again would spin
                // this loop at full CPU and skew the check's own measurement. `burn_ready` reads
                // it to the end now.
                if fds[i].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    self.burn_hung_up = Some(fds[i].fd);
                }
                let now = self.clock.now();
                if let Some(step) = self.check.as_mut().map(|c| c.burn_ready(&now)) {
                    self.check_step(step, &now);
                }
            }
            for (k, fd) in fds[first_client..].iter().enumerate() {
                if fd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    self.serve_client(k);
                }
                if fd.revents & libc::POLLOUT != 0
                    && let Some(c) = self.clients.get_mut(k)
                {
                    c.flush();
                }
            }
            if fds[listen_slot].revents != 0 {
                self.accept_clients();
            }
        }
        self.sweep_clients();

        let now = self.clock.now();
        if let Some(t4) = self.next_t4_ns
            && self.check.is_some()
            && now.mono_ns >= t4
        {
            let reading = self.sampler.read_t4();
            if let Some(c) = self.check.as_mut() {
                c.t4(reading, &now);
            }
            let every = sampler::cadence(None, None, true).t4_ms.unwrap_or(250) as i64 * 1_000_000;
            let mut next = t4 + every;
            if next <= now.mono_ns {
                next = now.mono_ns + every;
            }
            self.next_t4_ns = Some(next);
        }
        if now.mono_ns >= self.next_row_ns {
            self.tick();
        }
        true
    }

    /// One row (§12.1): sampler → episodes → rules → present, then state.json if it changed.
    fn tick(&mut self) {
        let now = self.clock.now();
        // The stall line is relative to what the loop planned: from the last read to this deadline.
        let expected_s = (self.next_row_ns - self.last_read_ns).max(0) as f64 / 1e9;
        let row = self.sampler.read_at(now, expected_s).clone();
        self.last_read_ns = now.mono_ns;
        // An nvidia-smi query killed at its timeout is reaped once it has died, check or not.
        check::reap_orphans();
        self.journal_flips(&row);
        let key = self.key_ctx(&row);

        if let Some(step) = self.check.as_mut().map(|c| c.row(&row, &key, &now)) {
            self.check_step(step, &now);
        }
        if self.check.is_some() && now.mono_ns >= self.next_check_edge_ns {
            self.sampler.request_config();
            self.next_check_edge_ns = now.mono_ns + CHECK_EDGE_NS;
        }
        let (sampler, tops) = (&self.sampler, &mut self.tops);
        let outs = self
            .watch
            .feed(&row, &key, |o| tops.for_output(o, sampler));
        if outs.iter().any(|o| !matches!(o, Output::Window(_))) {
            // Config, lid and bch-reconcile are read again at every episode edge (§3.2). A light
            // window closes every minute and is no edge.
            self.sampler.request_edge();
        }
        self.sampler.detail = self.watch.detector.plateau_open() || self.check.is_some();
        // A window that cannot qualify (a build in the last 90 s, a check) needs no "before"
        // snapshot: without this, every minute of a build at T near TCC would walk /proc.
        let windows_possible = self.check.is_none()
            && row.ac != Some(false)
            && self.watch.detector.window_may_qualify(row.at.mono_s());
        self.tops
            .after_row(&row, key.tcc_c, &self.sampler, windows_possible);

        if now.mono_ns >= self.next_poll_ns {
            self.next_poll_ns = now.mono_ns + POLL_NS;
            self.clock.reload_zone();
            self.poll_settings(false, &now);
        }
        self.commit(&now);
        if self.clients.iter().any(|c| c.live) {
            let line = ipc::event_line("live", &row);
            self.broadcast(&line, true);
        }

        let cadence = sampler::cadence(row.ac, row.t, self.check.is_some());
        // §12.3 promises a subscriber one row each second, battery or not; the 0.2 Hz battery
        // rate is for when nobody is watching.
        let row_ms = if self.clients.iter().any(|c| c.live) {
            cadence.row_ms.min(1000)
        } else {
            cadence.row_ms
        };
        let every = row_ms as i64 * 1_000_000;
        let mut next = self.next_row_ns + every;
        if next <= now.mono_ns {
            // Behind (a stall, a suspend): start again from now rather than catch up in a burst.
            next = now.mono_ns + every;
        }
        self.next_row_ns = next;
    }

    /// The config key in force, from the deriver's latest readings.
    fn key_ctx(&self, row: &Row) -> KeyCtx {
        let d = &self.sampler.deriver;
        let none = ConfigRead::default();
        let cfg = d.config().unwrap_or(&none);
        let key = d
            .key()
            .cloned()
            .unwrap_or_else(|| ConfigKey::new(row.ac, cfg));
        let tcc = self.sampler.sensors.tcc_c(cfg.tcc_offset);
        let pl1_max_w = cfg.pl1_max_uw.map(|uw| (uw as f64 / 1e6).round() as u32);
        KeyCtx::new(row.key_seq, key, tcc, pl1_max_w)
    }

    /// AC and lid flips go to the journal only (§11.5).
    fn journal_flips(&mut self, row: &Row) {
        if row.ac.is_some() && row.ac != self.last_ac {
            if self.last_ac.is_some() {
                note(format_args!(
                    "{}",
                    if row.ac == Some(true) { "on AC" } else { "on battery" }
                ));
            }
            self.last_ac = row.ac;
        }
        if row.lid_open.is_some() && row.lid_open != self.last_lid {
            if self.last_lid.is_some() {
                note(format_args!(
                    "lid {}",
                    if row.lid_open == Some(true) { "opened" } else { "closed" }
                ));
            }
            self.last_lid = row.lid_open;
        }
    }

    /// `thermal.chassis`, re-read when the settings file's mtime moves (or always, for
    /// `settings.reload`).
    fn poll_settings(&mut self, force: bool, now: &Moment) {
        let Some(path) = self.paths.settings.clone() else {
            return;
        };
        let m = mtime(Some(&path));
        if !force && m == self.settings_mtime {
            return;
        }
        self.settings_mtime = m;
        let setting = profile::read_chassis_setting(Some(&path));
        let engine = &mut self.watch.engine;
        let chassis = profile::resolve_chassis(setting, &engine.machine.info);
        if (chassis, setting) != (engine.machine.chassis, engine.machine.setting) {
            note(format_args!(
                "chassis {} ({}), was {} ({})",
                chassis.as_str(),
                setting.as_str(),
                engine.machine.chassis.as_str(),
                engine.machine.setting.as_str()
            ));
            engine.set_chassis(chassis, setting, now);
        }
    }

    /// Writes what the engine queued, then state.json if it changed, broadcasting each write.
    fn commit(&mut self, now: &Moment) {
        let mut bulk = false;
        for effect in self.watch.engine.take_effects() {
            let result = match effect {
                Effect::Event {
                    at,
                    kind,
                    case,
                    text,
                    data,
                } => {
                    note(format_args!("event: {text}"));
                    match self.store.event(&at, kind, case.as_deref(), text, data) {
                        Ok(event) => {
                            if event.kind == EventKind::Notice {
                                let line = ipc::event_line("notice", &event);
                                self.broadcast(&line, false);
                            }
                            Ok(())
                        }
                        Err(e) => Err((&self.paths.events, e)),
                    }
                }
                Effect::Record(r) => {
                    // fsynced at episode close and day settle (§11.2), not for each light window:
                    // those reach disk with the next close, settle or the page cache's writeback.
                    bulk |= !matches!(r, Record::Light(_));
                    self.store
                        .record(&r, now)
                        .map_err(|e| (&self.paths.records, e))
                }
                Effect::Daily(d) => {
                    bulk = true;
                    self.store.daily(&d).map_err(|e| (&self.paths.daily, e))
                }
                Effect::Case(c) => self.store.case(&c).map_err(|e| (&self.paths.cases, e)),
                Effect::References => {
                    let written = self.store.write_references(&self.watch.engine.refs);
                    if written.is_err() {
                        // Kept for the next flush: the values are only in memory now.
                        self.watch.engine.mark_refs_dirty();
                    }
                    written.map_err(|e| (&self.paths.references, e))
                }
            };
            if let Err((path, e)) = result {
                note(format_args!("{}: {e}", path.display()));
            }
        }
        if bulk && let Err(e) = self.store.sync_bulk() {
            note(format_args!("records/daily fsync: {e}"));
        }
        match self.store.write_state(&mut self.watch.engine.state, now) {
            Ok(true) => {
                self.state_failing = false;
                let line = ipc::event_line("state", &self.watch.engine.state);
                self.broadcast(&line, false);
            }
            Ok(false) => {}
            Err(e) => {
                if !self.state_failing {
                    self.state_failing = true;
                    note(format_args!("{}: {e}", self.paths.state.display()));
                }
            }
        }
    }

    fn broadcast(&mut self, line: &[u8], live_only: bool) {
        for c in &mut self.clients {
            if !live_only || c.live {
                c.queue(line);
            }
        }
    }

    fn accept_clients(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.clients.len() >= MAX_CLIENTS {
                        if !self.turned_away {
                            self.turned_away = true;
                            note(format_args!(
                                "more than {MAX_CLIENTS} clients: turning new ones away"
                            ));
                        }
                        continue;
                    }
                    match Conn::new(stream) {
                        Ok(c) => {
                            self.clients.push(c);
                            // Its first request may already be waiting.
                            self.serve_client(self.clients.len() - 1);
                        }
                        Err(e) => note(format_args!("client: {e}")),
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    note(format_args!("accept: {e}"));
                    break;
                }
            }
        }
    }

    fn serve_client(&mut self, k: usize) {
        let Some(c) = self.clients.get_mut(k) else {
            return;
        };
        if c.dead.is_some() {
            return;
        }
        for line in c.read_lines() {
            self.handle_line(k, &line);
        }
    }

    /// Drops the clients that are dead, and those that have closed their side and been answered.
    fn sweep_clients(&mut self) {
        self.clients.retain(|c| {
            if let Some(why) = c.dead
                && why != "gone"
            {
                note(format_args!("dropped a client: {why}"));
            }
            c.dead.is_none() && !(c.eof && c.pending() == 0)
        });
        if self.clients.len() < MAX_CLIENTS {
            self.turned_away = false;
        }
    }

    /// MinkaSyu's `handle_line` shape: parse, dispatch (a panic answers `internal` instead of
    /// ending the service), write and broadcast what changed, then reply when there is an id.
    fn handle_line(&mut self, k: usize, line: &[u8]) {
        let now = self.clock.now();
        let (id, result) = match ipc::parse_request(line) {
            Err((id, e)) => (id, Err(e)),
            Ok(req) => {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.dispatch(k, &req.method, &req.params, &now)
                }));
                let result = outcome.unwrap_or_else(|_| {
                    Err(IpcError::internal(format!("{} failed in the service", req.method)))
                });
                (req.id, result)
            }
        };
        self.commit(&now);
        let Some(id) = id else {
            return;
        };
        let result = result.map(|r| match r {
            Reply::State => serde_json::to_value(&self.watch.engine.state).unwrap_or_default(),
            Reply::Value(v) => v,
        });
        let line = ipc::reply_line(&id, result);
        if let Some(c) = self.clients.get_mut(k) {
            c.queue(&line);
        }
    }

    /// The methods of §12.3.
    fn dispatch(
        &mut self,
        k: usize,
        method: &str,
        params: &Value,
        now: &Moment,
    ) -> Result<Reply, IpcError> {
        let action = |r: Result<(), String>| r.map(|()| Reply::State).map_err(IpcError::refused);
        let engine = &mut self.watch.engine;
        match method {
            "state.get" => Ok(Reply::State),
            "live.subscribe" => {
                let on = params.get("on").and_then(Value::as_bool).ok_or_else(|| {
                    IpcError::bad_request("live.subscribe needs on: true or false")
                })?;
                if let Some(c) = self.clients.get_mut(k) {
                    c.live = on;
                }
                if on {
                    // On battery the next row may be 5 s away; the first one comes within a second.
                    self.next_row_ns = self.next_row_ns.min(now.mono_ns + NS);
                }
                Ok(Reply::Value(json!({})))
            }
            "check.start" => {
                if params.get("confirm") != Some(&Value::Bool(true)) {
                    return Err(IpcError::refused(
                        "a cooling check starts only when confirmed: check.start needs confirm: true",
                    ));
                }
                let anyway = params.get("anyway").and_then(Value::as_bool).unwrap_or(false);
                self.start_check(anyway, now).map(Reply::Value)
            }
            "check.cancel" => {
                let Some(c) = self.check.as_mut() else {
                    return Err(IpcError::refused("no cooling check is running"));
                };
                c.abort("cancelled", now);
                self.end_check(None, now);
                Ok(Reply::Value(json!({})))
            }
            "check.rows" => Ok(Reply::Value(match &self.check {
                Some(c) => c.rows(),
                // The check that ended last, before a restart or not: its samples.csv has the
                // rows. Holding them in memory would keep a megabyte for the service's life.
                None => engine
                    .state
                    .check
                    .as_ref()
                    .filter(|c| c.dir.starts_with("checks/") && !c.dir.contains(".."))
                    .and_then(|c| {
                        check::read_samples_csv(&self.paths.dir.join(&c.dir).join("samples.csv"))
                    })
                    .unwrap_or_else(|| json!([])),
            })),
            "case.cleaned" => {
                let date = date_param(params, "date")?;
                action(engine.cleaned(now, date))
            }
            "case.not_dust" => action(engine.not_dust(now)),
            "case.snooze" => match params.get("what").and_then(Value::as_str) {
                Some("suspicion") => action(engine.snooze_suspicion(now)),
                _ => Err(IpcError::bad_request(
                    "case.snooze needs what: \"suspicion\"",
                )),
            },
            "setup.answer" => {
                let last = match params.get("last_cleaned").and_then(Value::as_str) {
                    Some("not_sure") => None,
                    Some(_) => date_param(params, "last_cleaned")?,
                    None => {
                        return Err(IpcError::bad_request(
                            "setup.answer needs last_cleaned: \"yyyy-mm-dd\" or \"not_sure\"",
                        ));
                    }
                };
                action(engine.setup_answer(now, last))
            }
            "fan.fixed" => action(engine.fan_fixed(now)),
            "fan.accept" => action(engine.accept_fan(now)),
            "fault.ack" => action(engine.ack_fault(now)),
            "settings.reload" => {
                self.poll_settings(true, now);
                Ok(Reply::Value(json!({})))
            }
            other => Err(IpcError::unknown_method(other)),
        }
    }

    /// `check.start` with her confirmation (§10.1): the purpose comes from the status, the
    /// references are fixed now, and the check waits for its gates. A key that differs from the
    /// reference's in ways waiting cannot fix (PL, TCC, turbo…) is refused at once unless she
    /// chose Run anyway; the AC, profile and lid gates wait, since she can fix those meanwhile.
    fn start_check(&mut self, anyway: bool, now: &Moment) -> Result<Value, IpcError> {
        if self.check.is_some() {
            return Err(IpcError::busy("a cooling check is already running"));
        }
        let Some(load) = self.load.clone() else {
            return Err(IpcError::refused(
                "the cooling check cannot run: no load generator is configured",
            ));
        };
        let engine = &self.watch.engine;
        let state = &engine.state;
        let refs = Refs {
            refs: engine.refs.clone(),
            chassis: engine.machine.chassis,
            board: engine.machine.info.board_name.clone(),
            before: state.verification.as_ref().and_then(|v| v.r_before.clone()),
            r_fan_override: (state.fan.accepted || state.fan.relearned)
                .then_some(state.fan.r_fan.map(f64::from))
                .flatten(),
        };
        if !anyway
            && let Some(row) = self.sampler.ring.last()
            && let check::KeyMatch::Differs(diff) = refs.key_match(&self.key_ctx(row).key)
        {
            return Err(IpcError::refused(present::check::key_differs(&diff)));
        }
        let (rel_dir, dir) = check::make_dir(&self.paths.checks, now)
            .map_err(|e| IpcError::internal(format!("{}: {e}", self.paths.checks.display())))?;
        let info = &engine.machine.info;
        let setup = Setup {
            purpose: check::purpose_for(state.status),
            anyway,
            rel_dir: rel_dir.clone(),
            dir,
            root: self.sampler.sensors.root.clone(),
            load,
            gpu_query: self.gpu_query.clone(),
            dgpu_status: self.sampler.sensors.dgpu.as_ref().map(|d| d.path.clone()),
            machine: CheckMachine {
                nproc: info.nproc.max(1),
                threads: info.nproc.max(1),
                cpu: info.cpu_model.clone(),
                bios: info.bios_version.clone(),
                kernel: info.kernel.clone(),
                board: info.board_name.clone(),
                chassis: engine.machine.chassis,
                has_fan: engine.machine.has_fan,
                has_throttle: engine.machine.has_throttle,
            },
            refs,
        };
        let run = Check::new(setup, self.sampler.ring.rows(), now);
        self.begin_check(Box::new(run));
        // Config and lid are read again for the gates (§3.2).
        self.sampler.request_config();
        Ok(json!({ "state": "gating", "dir": rel_dir }))
    }

    /// Hands an accepted check to the loop: passive detection pauses (what is open is void),
    /// rows go to 1 Hz with T4 at 4 Hz, and the `check` block is written and broadcast.
    pub fn begin_check(&mut self, run: Box<dyn CheckRun>) {
        let now = self.clock.now();
        let state = run.state().clone();
        self.watch.engine.log_event(
            &now,
            EventKind::CheckStart,
            present::check::started_event(state.purpose),
            json!({ "dir": state.dir, "purpose": state.purpose, "anyway": state.anyway }),
        );
        self.check = Some(run);
        self.burn_hung_up = None;
        for out in self.watch.detector.check_started() {
            self.watch.engine.handle(out, Vec::new(), &now);
        }
        self.tops.plateau = None;
        self.sampler.detail = true;
        self.next_t4_ns = Some(now.mono_ns);
        self.next_row_ns = self.next_row_ns.min(now.mono_ns + NS);
        self.watch.engine.state.check = Some(state.clone());
        present::recompute(&mut self.watch.engine.state, &now);
        self.commit(&now);
        let line = ipc::event_line("check", &json!({ "state": state }));
        self.broadcast(&line, false);
    }

    fn check_step(&mut self, step: CheckStep, now: &Moment) {
        match step {
            CheckStep::Running => {}
            CheckStep::Live(payload) => {
                let line = ipc::event_line("check", &payload);
                self.broadcast(&line, false);
            }
            CheckStep::Event(payload) => {
                if let Some(c) = &self.check {
                    self.watch.engine.state.check = Some(c.state().clone());
                }
                // Config, lid and the context zones at every gate change and phase start: the
                // idle phase records SEN2/SEN4, and a key change must be seen promptly.
                self.sampler.request_config();
                present::recompute(&mut self.watch.engine.state, now);
                self.commit(now);
                let line = ipc::event_line("check", &payload);
                self.broadcast(&line, false);
            }
            CheckStep::Finished(outcome) => self.end_check(outcome.map(|o| *o), now),
        }
    }

    /// The check is over (done, refused or aborted): its final block stays in state.json, a
    /// result goes to the rules, and passive detection resumes.
    fn end_check(&mut self, outcome: Option<CheckOutcome>, now: &Moment) {
        let Some(run) = self.check.take() else {
            return;
        };
        let state = run.state().clone();
        let detail = run.result();
        let fan_stopped = run.fan_stopped();
        // Dropping it kills a burn that is somehow still running.
        drop(run);
        let engine = &mut self.watch.engine;
        engine.state.check = Some(state.clone());
        let reason = state.reason.clone().unwrap_or_default();
        let data = json!({ "dir": state.dir, "purpose": state.purpose, "reason": reason });
        match (outcome, state.state) {
            (Some(o), _) => engine.check_result(now, o),
            (None, CheckPhase::Refused) => engine.log_event(
                now,
                EventKind::CheckResult,
                present::check::refused_event(&reason),
                data,
            ),
            (None, _) => engine.log_event(
                now,
                EventKind::CheckResult,
                present::check::aborted_event(&reason),
                data,
            ),
        }
        if fan_stopped {
            engine.fan_stopped_in_check(now);
        }
        self.watch.detector.check_ended();
        self.next_t4_ns = None;
        self.burn_hung_up = None;
        self.sampler.detail = self.watch.detector.plateau_open();
        present::recompute(&mut self.watch.engine.state, now);
        self.commit(now);
        let s = &self.watch.engine.state;
        let line = ipc::event_line(
            "check",
            &json!({ "state": s.check, "result": s.last_check, "detail": detail }),
        );
        self.broadcast(&line, false);
    }

    /// SIGTERM (§12.1): an open episode becomes `unjudged void:restart`, a running check is
    /// killed and marked aborted, and everything is written before exit.
    pub fn shutdown(&mut self) {
        let now = self.clock.now();
        for out in self.watch.detector.stop() {
            self.watch.engine.handle(out, Vec::new(), &now);
        }
        if let Some(c) = self.check.as_mut() {
            c.abort("service stop", &now);
            self.end_check(None, &now);
        }
        self.watch.engine.flush_references();
        self.commit(&now);
        if let Err(e) = self.store.sync_bulk() {
            note(format_args!("records/daily fsync: {e}"));
        }
        for c in &mut self.clients {
            c.flush();
        }
        // Remove the socket only if it is still the one this service bound.
        if let Ok(m) = fs::symlink_metadata(&self.paths.socket)
            && (m.dev(), m.ino()) == self.socket_id
        {
            let _ = fs::remove_file(&self.paths.socket);
        }
        note(format_args!("service stopped"));
    }
}

/// Step 3 of §12.1: a connect that succeeds means another service is live (exit 4). A socket file
/// that refuses is stale and is replaced. Anything else at the path is left alone.
fn bind_socket(path: &Path) -> Result<(UnixListener, (u64, u64)), StartError> {
    match UnixStream::connect(path) {
        Ok(_) => {
            return Err(StartError::Owned(format!(
                "another minka-thermal service is listening on {}",
                path.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            match fs::symlink_metadata(path) {
                Ok(m) if m.file_type().is_socket() => {
                    fs::remove_file(path).map_err(failed(path))?;
                    note(format_args!("replaced the stale socket {}", path.display()));
                }
                Ok(_) => {
                    return Err(StartError::Failed(format!(
                        "{} exists and is not a socket; leaving it alone",
                        path.display()
                    )));
                }
                Err(e) => return Err(failed(path)(e)),
            }
        }
        // EACCES and the like: whether something listens there cannot be told, so it stays.
        Err(e) => return Err(failed(path)(e)),
    }
    let listener = UnixListener::bind(path).map_err(failed(path))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(failed(path))?;
    listener.set_nonblocking(true).map_err(failed(path))?;
    let m = fs::symlink_metadata(path).map_err(failed(path))?;
    Ok((listener, (m.dev(), m.ino())))
}

/// Blocks `signals` in this thread (the whole service, which has one; the CLI's Ctrl-C) and
/// returns a signalfd that reads them.
pub(crate) fn signal_fd(signals: &[i32]) -> io::Result<OwnedFd> {
    // SAFETY: plain libc calls on a zeroed, then initialised, sigset_t owned by this frame; the
    // fd is checked before it is wrapped.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &s in signals {
            libc::sigaddset(&mut set, s);
        }
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

pub(crate) fn read_signal(fd: RawFd) -> Option<i32> {
    // SAFETY: an all-zero siginfo is valid, and read writes at most its size into it.
    let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::signalfd_siginfo>();
    let n = unsafe { libc::read(fd, (&raw mut info).cast(), size) };
    (n == size as isize).then_some(info.ssi_signo as i32)
}

/// A store file that does not parse is renamed `<file>.corrupt-<unix s>` and the service goes
/// on without it. Returns the new file name, for the notice.
fn set_aside(path: &Path, why: &str, now: &Moment, then: &str) -> Result<String, StartError> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".corrupt-{}", now.unix_s()));
    let target = path.with_file_name(&name);
    fs::rename(path, &target).map_err(failed(path))?;
    note(format_args!(
        "{} is not readable ({why}): kept as {}, {then}",
        path.display(),
        target.display()
    ));
    Ok(name.to_string_lossy().into_owned())
}

/// A check that was running when the service stopped is never resumed (§10.1).
fn abort_unfinished_check(state: &mut State) -> bool {
    match &mut state.check {
        Some(c)
            if matches!(
                c.state,
                CheckPhase::Gating | CheckPhase::Idle | CheckPhase::Load | CheckPhase::Recovery
            ) =>
        {
            c.state = CheckPhase::Aborted;
            c.reason = Some("service restart".to_owned());
            true
        }
        _ => false,
    }
}

/// The directory of a check that a restart cut short gets the result.json it never wrote, so it
/// does not look like a check still running.
fn close_check_dir(store_dir: &Path, c: &CheckState, now: &Moment) {
    // The name comes from our own state.json; anything else is not ours to write into.
    if !c.dir.starts_with("checks/") || c.dir.contains("..") {
        return;
    }
    let dir = store_dir.join(&c.dir);
    let file = dir.join("result.json");
    if !dir.is_dir() || file.exists() {
        return;
    }
    let body = json!({
        "state": c.state, "reason": c.reason, "purpose": c.purpose,
        "started_at": c.started_at, "ended_at": now.stamp(), "dir": c.dir,
    });
    let mut bytes = serde_json::to_vec_pretty(&body).unwrap_or_default();
    bytes.push(b'\n');
    if let Err(e) = store::atomic_write(&file, &bytes) {
        note(format_args!("{}: {e}", file.display()));
    }
}

/// The records of the last three months: the engine looks back 90 days, and the archives of
/// earlier months need not be read at every start.
fn recent_records(paths: &Paths, now: &Moment) -> Vec<Record> {
    let oldest = now.date().add_days(-RESTORE_DAYS).month_key();
    let mut out = Vec::new();
    for file in store::record_files(paths) {
        let month = file
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("records-"))
            .and_then(|n| n.strip_suffix(".jsonl"));
        if month.is_some_and(|m| m < oldest.as_str()) {
            continue;
        }
        match store::read_jsonl::<Record>(&file) {
            Ok(mut r) => out.append(&mut r),
            Err(e) => note(format_args!("{}: {e}", file.display())),
        }
    }
    out
}

fn mtime(path: Option<&Path>) -> Option<SystemTime> {
    fs::metadata(path?).and_then(|m| m.modified()).ok()
}

/// An optional `yyyy-mm-dd` parameter.
fn date_param(params: &Value, name: &str) -> Result<Option<Date>, IpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Date::parse_key(s)
            .map(Some)
            .ok_or_else(|| IpcError::bad_request(format!("{name}: '{s}' is not a yyyy-mm-dd date"))),
        Some(_) => Err(IpcError::bad_request(format!(
            "{name} must be a yyyy-mm-dd string"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{self, Command, EXIT_NO_SERVICE};
    use crate::clock::Timeline;
    use crate::ipc::CallError;
    use crate::model::{CaseOutcome, CheckPurpose, CheckVerdict, FanStatus, Status};
    use crate::rules::CheckMetrics;
    use crate::sensors::fake;
    use crate::testutil::{
        become_subreaper, family, put, read_pid, reap, sleeper, test_dir,
    };
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    const SEED_KEY: &str = "ac|performance|performance|28|35|8|0|100|open";

    /// A clock the test moves by hand; it starts on 2/12/2026 at 10:00 UTC.
    #[derive(Clone)]
    struct TestClock(Arc<Mutex<Timeline>>);

    impl TestClock {
        fn new() -> TestClock {
            let start = Timeline::starting(Date::new(2026, 12, 2).unwrap(), 10, 0, 0, 0);
            TestClock(Arc::new(Mutex::new(start)))
        }

        fn advance(&self, secs: f64) {
            self.0.lock().unwrap().advance(secs);
        }
    }

    impl Clock for TestClock {
        fn now(&mut self) -> Moment {
            self.0.lock().unwrap().now()
        }
    }

    /// A fake Duo under target/test-tmp/<name>/root with the store beside it. The socket name is
    /// short because sun_path holds 108 bytes.
    fn setup(name: &str) -> (PathBuf, PathBuf, Paths) {
        let dir = test_dir(name);
        let root = dir.join("root");
        fake::duo(&root);
        let paths = Paths::with(
            dir.join("store/state.json"),
            dir.join("s.sock"),
            Some(dir.join("settings.json")),
        );
        (dir, root, paths)
    }

    fn start(paths: &Paths, root: &Path, clock: &TestClock) -> Service {
        Service::start(paths, root, Box::new(clock.clone()), false)
            .unwrap_or_else(|e| panic!("{}", e.message()))
    }

    /// Reads whatever the client has received, then returns the complete lines.
    fn drain(client: &mut UnixStream, buf: &mut Vec<u8>) -> Vec<Value> {
        let mut chunk = [0u8; 64 * 1024];
        while let Ok(n) = client.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let mut out = Vec::new();
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            out.push(serde_json::from_slice(&line).unwrap());
        }
        out
    }

    /// One request from a client in this thread: the service is stepped until the reply comes.
    fn ask(svc: &mut Service, client: &mut UnixStream, method: &str, params: Value) -> Value {
        let line = format!("{}\n", json!({"id": 9, "method": method, "params": params}));
        client.write_all(line.as_bytes()).unwrap();
        client.set_nonblocking(true).unwrap();
        let mut buf = Vec::new();
        for _ in 0..200 {
            svc.step(Some(5));
            if let Some(reply) = drain(client, &mut buf)
                .into_iter()
                .find(|v| v.get("id") == Some(&json!(9)))
            {
                return reply;
            }
        }
        panic!("no reply to {method}");
    }

    fn code(reply: &Value) -> &str {
        reply["error"]["code"].as_str().unwrap_or("")
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A WATCH case opened on 1/12/2026 under her normal key.
    const WATCH_STATE: &str = r#"{
      "schema": 1, "status": "watch", "setup_needed": false,
      "case": {"id": "c-20261201-0900",
        "opened": {"utc": "2026-12-01T09:00:00Z", "local": "2026-12-01T09:00:00+00:00"},
        "first_evidence": {"at": {"utc": "2026-12-01T09:00:00Z", "local": "2026-12-01T09:00:00+00:00"},
          "label": "1/12/2026 09:00", "class": "E1",
          "summary": "build 90.4 °C, throttled 6.8 % (clean 86 °C, 0.9 %)",
          "key_id": "ac|performance|performance|28|35|8|0|100|open"},
        "evidence_days": ["2026-12-01"]}
    }"#;

    fn seed_state(paths: &Paths, json: &str) {
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(&paths.state, json).unwrap();
    }

    #[test]
    fn answers_on_its_socket() {
        let (_dir, root, paths) = setup("svc-basic");
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        assert_eq!(mode(&paths.dir), 0o700);
        assert_eq!(mode(&paths.socket), 0o600);
        let written = store::read_state(&paths.state).unwrap().unwrap();
        assert_eq!(written.pid, std::process::id());
        assert_eq!(written.key.id, SEED_KEY);

        let mut c = UnixStream::connect(&paths.socket).unwrap();
        let r = ask(&mut svc, &mut c, "state.get", json!({}));
        assert_eq!(r["result"]["pid"], json!(std::process::id()));
        assert_eq!(r["result"]["boot_id"], "4d5ab25d-6171-4ce0-ac1b-ec48694c2006");
        assert_eq!(r["result"]["chassis"]["profile"], "duo");
        assert_eq!(r["result"]["setup_needed"], true);
        assert_eq!(r["result"]["started_at"]["utc"], "2026-12-02T10:00:00Z");

        // A check starts only with her confirmation; with it, a service with no load command
        // (as tests start it) still refuses.
        let r = ask(&mut svc, &mut c, "check.start", json!({}));
        assert_eq!(code(&r), "refused");
        assert!(r["error"]["message"].as_str().unwrap().contains("confirm: true"));
        let r = ask(&mut svc, &mut c, "check.start", json!({"confirm": false}));
        assert_eq!(code(&r), "refused");
        let r = ask(&mut svc, &mut c, "check.start", json!({"confirm": true}));
        assert_eq!(code(&r), "refused");
        assert_eq!(code(&ask(&mut svc, &mut c, "check.cancel", json!({}))), "refused");
        assert_eq!(ask(&mut svc, &mut c, "check.rows", json!({}))["result"], json!([]));

        assert_eq!(code(&ask(&mut svc, &mut c, "nope", json!({}))), "unknown_method");
        let r = ask(&mut svc, &mut c, "case.snooze", json!({"what": "fan"}));
        assert_eq!(code(&r), "bad_request");
        let r = ask(&mut svc, &mut c, "case.cleaned", json!({"date": "2/12/2026"}));
        assert_eq!(code(&r), "bad_request");
        let r = ask(&mut svc, &mut c, "case.cleaned", json!({"date": "2026-12-09"}));
        assert_eq!(code(&r), "refused", "a date in the future");
        // Nothing to dismiss, snooze or acknowledge.
        for m in ["case.not_dust", "fan.fixed", "fan.accept", "fault.ack"] {
            assert_eq!(code(&ask(&mut svc, &mut c, m, json!({}))), "refused", "{m}");
        }
        let r = ask(&mut svc, &mut c, "case.snooze", json!({"what": "suspicion"}));
        assert_eq!(code(&r), "refused");

        let r = ask(&mut svc, &mut c, "setup.answer", json!({"last_cleaned": "2026-11-20"}));
        assert_eq!(r["result"]["setup_needed"], false);
        assert_eq!(r["result"]["known_clean_until"], "2026-12-20");
        // Written and broadcast before the reply.
        let on_disk = store::read_state(&paths.state).unwrap().unwrap();
        assert_eq!(on_disk.known_clean_until.as_deref(), Some("2026-12-20"));
        let events: Vec<Event> = store::read_jsonl(&paths.events).unwrap();
        assert_eq!(events.last().unwrap().kind, EventKind::Setup);

        // A live subscriber gets one row per tick; others do not.
        let mut quiet = UnixStream::connect(&paths.socket).unwrap();
        quiet.set_nonblocking(true).unwrap();
        assert_eq!(ask(&mut svc, &mut c, "live.subscribe", json!({"on": true}))["result"], json!({}));
        clock.advance(1.0);
        svc.step(Some(0));
        let got = drain(&mut c, &mut Vec::new());
        let live: Vec<&Value> = got.iter().filter(|v| v["event"] == "live").collect();
        assert_eq!(live.len(), 1, "{got:?}");
        assert_eq!(live[0]["payload"]["t"], json!(57.0));
        assert!(drain(&mut quiet, &mut Vec::new()).iter().all(|v| v["event"] != "live"));

        // Not JSON: answered with id null. No id: no answer.
        c.write_all(b"{oops\n{\"method\":\"state.get\"}\n").unwrap();
        svc.step(Some(5));
        let got = drain(&mut c, &mut Vec::new());
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!((got[0]["id"].clone(), code(&got[0])), (Value::Null, "bad_request"));
    }

    #[test]
    fn a_second_service_exits_4_and_leaves_the_first_alone() {
        let (dir, root, paths) = setup("svc-twice");
        let clock = TestClock::new();
        let mut first = start(&paths, &root, &clock);
        let socket_before = fs::symlink_metadata(&paths.socket).unwrap().ino();

        // Same store: the flock stops it before the socket is touched.
        let second = Service::start(&paths, &root, Box::new(clock.clone()), false);
        let e = second.err().expect("refused");
        assert_eq!(e.code(), EXIT_RUNNING);
        assert!(e.message().starts_with("another minka-thermal service owns"));

        // Another store on the same socket: a live socket is never unlinked.
        let other = Paths::with(dir.join("other/state.json"), paths.socket.clone(), None);
        let e = Service::start(&other, &root, Box::new(clock.clone()), false)
            .err()
            .expect("refused");
        assert_eq!(e.code(), EXIT_RUNNING);
        assert!(e.message().contains("listening on"), "{}", e.message());

        assert_eq!(fs::symlink_metadata(&paths.socket).unwrap().ino(), socket_before);
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        let r = ask(&mut first, &mut c, "state.get", json!({}));
        assert_eq!(r["result"]["pid"], json!(std::process::id()));
    }

    #[test]
    fn a_stale_socket_is_replaced_and_a_foreign_file_is_not() {
        let (_dir, root, paths) = setup("svc-stale");
        let clock = TestClock::new();
        // A service that died: its socket file is left, and nobody listens on it.
        drop(UnixListener::bind(&paths.socket).unwrap());
        assert!(!ipc::listening(&paths.socket));
        let mut svc = start(&paths, &root, &clock);
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        assert!(ask(&mut svc, &mut c, "state.get", json!({}))["result"].is_object());
        drop(svc);

        let (_dir, root, paths) = setup("svc-foreign");
        fs::write(&paths.socket, "not mine").unwrap();
        let e = Service::start(&paths, &root, Box::new(clock), false)
            .err()
            .expect("refused");
        assert_eq!(e.code(), EXIT_ERROR);
        assert_eq!(fs::read_to_string(&paths.socket).unwrap(), "not mine");
    }

    #[test]
    fn cleaned_turns_a_case_into_verifying() {
        let (_dir, root, paths) = setup("svc-cleaned");
        seed_state(&paths, WATCH_STATE);
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        assert_eq!(svc.state().status, Status::Watch);
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        // Before the first evidence: refused.
        let r = ask(&mut svc, &mut c, "case.cleaned", json!({"date": "2026-11-30"}));
        assert_eq!(code(&r), "refused");
        let r = ask(&mut svc, &mut c, "case.cleaned", json!({}));
        assert_eq!(r["result"]["status"], "verifying");
        assert_eq!(r["result"]["pill"]["show"], false);
        assert_eq!(r["result"]["mon"]["kind"], "verifying");
        assert_eq!(r["result"]["known_clean_until"], "2027-01-01");
        let on_disk = store::read_state(&paths.state).unwrap().unwrap();
        assert_eq!(on_disk.status, Status::Verifying);
        let events: Vec<Event> = store::read_jsonl(&paths.events).unwrap();
        let cleaned = events.iter().find(|e| e.kind == EventKind::Cleaned).unwrap();
        assert_eq!(cleaned.case.as_deref(), Some("c-20261201-0900"));
    }

    #[test]
    fn a_failed_references_write_is_tried_again() {
        let (_dir, root, paths) = setup("svc-refs-retry");
        seed_state(&paths, WATCH_STATE);
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        // references.json cannot be replaced: a directory stands where it goes.
        let _ = fs::remove_file(&paths.references);
        fs::create_dir(&paths.references).unwrap();
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        let r = ask(&mut svc, &mut c, "case.cleaned", json!({}));
        assert_eq!(r["result"]["status"], "verifying");
        assert!(paths.references.is_dir(), "not written");
        fs::remove_dir(&paths.references).unwrap();
        // The next flush writes what is only in memory.
        svc.shutdown();
        let refs: References =
            serde_json::from_str(&fs::read_to_string(&paths.references).unwrap()).unwrap();
        assert_eq!(refs.schema, SCHEMA);
    }

    #[test]
    fn not_dust_closes_the_case() {
        let (_dir, root, paths) = setup("svc-notdust");
        seed_state(&paths, WATCH_STATE);
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        let r = ask(&mut svc, &mut c, "case.not_dust", json!({}));
        assert_eq!(r["result"]["status"], "clean");
        assert_eq!(r["result"]["case"], Value::Null);
        let cases: Vec<CaseRecord> = store::read_jsonl(&paths.cases).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].outcome, CaseOutcome::Dismissed);
        let events: Vec<Event> = store::read_jsonl(&paths.events).unwrap();
        assert!(events.iter().any(|e| e.kind == EventKind::NotDust));
        // Nothing left to dismiss.
        assert_eq!(code(&ask(&mut svc, &mut c, "case.not_dust", json!({}))), "refused");
    }

    fn sigmask() -> libc::sigset_t {
        // SAFETY: reads this thread's mask into a zeroed set.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut set);
            set
        }
    }

    #[test]
    fn sigterm_voids_the_open_plateau_and_writes_state() {
        let (_dir, root, paths) = setup("svc-sigterm");
        let clock = TestClock::new();
        let mask = sigmask();
        let mut svc = Service::start(&paths, &root, Box::new(clock.clone()), true)
            .unwrap_or_else(|e| panic!("{}", e.message()));
        // A build: 99 % busy, one row a second.
        let (mut busy, mut idle) = (100_000u64, 10_000u64);
        for _ in 0..15 {
            busy += 990;
            idle += 10;
            put(&root, "proc/stat", &format!("cpu  {busy} 0 0 {idle} 0 0 0 0 0 0\n"));
            clock.advance(1.0);
            assert!(svc.step(Some(0)));
        }
        assert!(svc.watch.detector.plateau_open());
        assert!(svc.sampler.detail, "core temperatures and a 1 Hz fan while it is open");

        // SIGTERM to this thread alone: it is blocked here, so it waits on the signalfd.
        // SAFETY: a thread-directed signal to the calling thread.
        unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTERM) };
        assert!(!svc.step(Some(1000)), "the signalfd ends the loop");
        svc.shutdown();
        // The signal has been read, so unblocking it again is safe.
        // SAFETY: restores the mask saved above.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) };

        let records: Vec<Record> = store::read_jsonl(&paths.records).unwrap();
        assert!(
            records
                .iter()
                .any(|r| matches!(r, Record::Unjudged(u) if u.reason == "void:restart")),
            "{records:?}"
        );
        let state = store::read_state(&paths.state).unwrap().unwrap();
        assert_eq!(state.unjudged_30d.void, 1);
        assert!(!paths.socket.exists(), "its own socket is removed at exit");
    }

    #[test]
    fn an_unfinished_check_is_marked_aborted() {
        let (_dir, root, paths) = setup("svc-abort");
        seed_state(
            &paths,
            r#"{"schema": 1, "status": "clean",
                "check": {"purpose": "reference", "state": "load", "dir": "checks/2026-12-02_0950",
                          "started_at": {"utc": "2026-12-02T09:50:00Z", "local": "2026-12-02T09:50:00+00:00"}}}"#,
        );
        let svc = start(&paths, &root, &TestClock::new());
        let check = svc.state().check.clone().unwrap();
        assert_eq!(check.state, CheckPhase::Aborted);
        assert_eq!(check.reason.as_deref(), Some("service restart"));
        let events: Vec<Event> = store::read_jsonl(&paths.events).unwrap();
        assert!(events.iter().any(|e| e.kind == EventKind::CheckResult && e.text.contains("aborted")));
    }

    #[test]
    fn unreadable_and_newer_files() {
        let (_dir, root, paths) = setup("svc-corrupt");
        seed_state(&paths, "{ not json");
        fs::write(&paths.references, r#"{"schema": 1, "keys": []}"#).unwrap();
        let svc = start(&paths, &root, &TestClock::new());
        assert!(svc.state().setup_needed, "a fresh state");
        drop(svc);
        let kept = |prefix: &str| -> Vec<String> {
            fs::read_dir(&paths.dir)
                .unwrap()
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.starts_with(prefix))
                .collect()
        };
        let state_kept = kept("state.json.corrupt-");
        assert_eq!(state_kept.len(), 1);
        assert_eq!(fs::read_to_string(paths.dir.join(&state_kept[0])).unwrap(), "{ not json");
        assert_eq!(kept("references.json.corrupt-").len(), 1);
        // Not only in the journal: the window shows a notice and History an event, both naming
        // the copy, so a warning that vanished with the file leaves a trace.
        let on_disk = store::read_state(&paths.state).unwrap().unwrap();
        let ids: Vec<&str> = on_disk.notices.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["references_unreadable", "state_unreadable"]);
        assert!(on_disk.notices[1].text.contains(&state_kept[0]));
        let events: Vec<Event> = store::read_jsonl(&paths.events).unwrap();
        let logged: Vec<&str> = events
            .iter()
            .filter(|e| e.kind == EventKind::Notice)
            .filter_map(|e| e.data.get("id")?.as_str())
            .collect();
        assert_eq!(logged, ["references_unreadable", "state_unreadable"]);

        // A newer schema is refused and left as it was.
        let (_, root, paths) = setup("svc-newer");
        let newer = r#"{"schema": 99, "status": "warning"}"#;
        seed_state(&paths, newer);
        let e = Service::start(&paths, &root, Box::new(TestClock::new()), false)
            .err()
            .expect("refused");
        assert_eq!(e.code(), EXIT_ERROR);
        assert_eq!(fs::read_to_string(&paths.state).unwrap(), newer);
    }

    /// A stand-in for phase 4's check: it goes to Load at its second row and finishes at
    /// `finish_at` rows with an inconclusive result.
    struct FakeCheck {
        state: CheckState,
        rows: u32,
        finish_at: Option<u32>,
        log: Rc<RefCell<Vec<String>>>,
        /// A pipe's read end, returned by `burn_fd` for ever, as a careless check might.
        burn: Option<OwnedFd>,
        /// It ended on a fan standing still under load.
        stopped_fan: bool,
    }

    impl CheckRun for FakeCheck {
        fn burn_fd(&self) -> Option<RawFd> {
            self.burn.as_ref().map(|fd| fd.as_raw_fd())
        }

        fn burn_ready(&mut self, _now: &Moment) -> CheckStep {
            self.log.borrow_mut().push("burn".to_owned());
            CheckStep::Running
        }

        fn t4(&mut self, t4: Option<f64>, _now: &Moment) {
            self.log.borrow_mut().push(format!("t4 {t4:?}"));
        }

        fn row(&mut self, _row: &Row, key: &KeyCtx, now: &Moment) -> CheckStep {
            self.rows += 1;
            if Some(self.rows) == self.finish_at {
                self.state.state = CheckPhase::Done;
                return CheckStep::Finished(Some(Box::new(CheckOutcome {
                    at: *now,
                    dir: self.state.dir.clone(),
                    purpose: CheckPurpose::Reference,
                    verdict: CheckVerdict::Inconclusive,
                    post_clean: None,
                    comparable: true,
                    key_id: key.id.clone(),
                    metrics: CheckMetrics::default(),
                    matched: None,
                    generator: Some(1),
                    cpu: String::new(),
                    fan_flag: false,
                    diagnosis: Vec::new(),
                })));
            }
            if self.rows == 2 {
                self.state.state = CheckPhase::Load;
                return CheckStep::Event(json!({"state": "load"}));
            }
            CheckStep::Running
        }

        fn abort(&mut self, reason: &str, _now: &Moment) {
            self.state.state = CheckPhase::Aborted;
            self.state.reason = Some(reason.to_owned());
            self.log.borrow_mut().push(format!("abort {reason}"));
        }

        fn state(&self) -> &CheckState {
            &self.state
        }

        fn rows(&self) -> Value {
            json!([{ "rows": self.rows }])
        }

        fn fan_stopped(&self) -> bool {
            self.stopped_fan
        }
    }

    fn fake_check(finish_at: Option<u32>, log: &Rc<RefCell<Vec<String>>>) -> Box<FakeCheck> {
        Box::new(FakeCheck {
            state: CheckState {
                dir: "checks/2026-12-02_1000".into(),
                ..CheckState::default()
            },
            rows: 0,
            finish_at,
            log: log.clone(),
            burn: None,
            stopped_fan: false,
        })
    }

    /// A pipe whose write end is already closed: its read end polls as hung up at once.
    fn hung_up_pipe() -> OwnedFd {
        let mut fds = [0; 2];
        // SAFETY: pipe2 fills the two-int array; on success both fds are new and owned here.
        unsafe {
            assert_eq!(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK), 0);
            drop(OwnedFd::from_raw_fd(fds[1]));
            OwnedFd::from_raw_fd(fds[0])
        }
    }

    #[test]
    fn a_hung_up_burn_pipe_is_not_polled_again() {
        let (_dir, root, paths) = setup("svc-burn-hup");
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut check = fake_check(None, &log);
        check.burn = Some(hung_up_pipe());
        svc.begin_check(check);
        // The clock stands still, so nothing else is due: only the pipe could wake the loop.
        for _ in 0..5 {
            svc.step(Some(0));
        }
        let burns = log.borrow().iter().filter(|l| *l == "burn").count();
        assert_eq!(burns, 1, "told once, then left alone: {:?}", log.borrow());

        // A new check is polled afresh, even if its pipe reuses the number.
        let c = svc.check.as_mut().unwrap();
        c.abort("cancelled", &clock.clone().now());
        svc.end_check(None, &clock.clone().now());
        let mut again = fake_check(None, &log);
        again.burn = Some(hung_up_pipe());
        svc.begin_check(again);
        svc.step(Some(0));
        let burns = log.borrow().iter().filter(|l| *l == "burn").count();
        assert_eq!(burns, 2);
    }

    #[test]
    fn the_check_hook() {
        let (_dir, root, paths) = setup("svc-check");
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        let log = Rc::new(RefCell::new(Vec::new()));

        svc.begin_check(fake_check(None, &log));
        assert_eq!(svc.state().check.as_ref().unwrap().state, CheckPhase::Gating);
        assert_eq!(svc.state().mon.kind, "check");
        assert!(svc.sampler.detail);
        // T4 at 4 Hz, full rows at 1 Hz.
        for _ in 0..8 {
            clock.advance(0.25);
            svc.step(Some(0));
        }
        let t4s = log.borrow().iter().filter(|l| l.starts_with("t4 Some(58.0)")).count();
        assert!((8..=9).contains(&t4s), "{t4s} T4 reads in 2 s");
        assert_eq!(svc.state().check.as_ref().unwrap().state, CheckPhase::Load);
        let on_disk = store::read_state(&paths.state).unwrap().unwrap();
        assert_eq!(on_disk.check.unwrap().state, CheckPhase::Load);

        assert_eq!(code(&ask(&mut svc, &mut c, "check.start", json!({"confirm": true}))), "busy");
        assert_eq!(
            ask(&mut svc, &mut c, "check.rows", json!({}))["result"],
            json!([{"rows": 2}])
        );
        assert_eq!(ask(&mut svc, &mut c, "check.cancel", json!({}))["result"], json!({}));
        assert_eq!(log.borrow().last().map(String::as_str), Some("abort cancelled"));
        let check = svc.state().check.clone().unwrap();
        assert_eq!((check.state, check.reason.as_deref()), (CheckPhase::Aborted, Some("cancelled")));
        assert!(svc.check.is_none() && svc.next_t4_ns.is_none());

        // One that finishes: its result reaches the rules.
        svc.begin_check(fake_check(Some(2), &log));
        for _ in 0..3 {
            clock.advance(1.0);
            svc.step(Some(0));
        }
        assert!(svc.check.is_none());
        assert_eq!(svc.state().check.as_ref().unwrap().state, CheckPhase::Done);
        assert_eq!(svc.state().last_check.as_ref().unwrap().label, "Inconclusive");
    }

    #[test]
    fn a_running_check_reads_the_lid_within_seconds() {
        let (_dir, root, paths) = setup("svc-check-lid");
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        let log = Rc::new(RefCell::new(Vec::new()));
        svc.begin_check(fake_check(None, &log));
        for _ in 0..12 {
            clock.advance(0.25);
            svc.step(Some(0));
        }
        assert_eq!(svc.sampler.ring.last().unwrap().lid_open, Some(true));
        // Closed while docked, where logind does not suspend: the minute's poll is far off.
        put(&root, "proc/acpi/button/lid/LID/state", "state:      closed\n");
        for _ in 0..28 {
            clock.advance(0.25);
            svc.step(Some(0));
        }
        assert_eq!(svc.sampler.ring.last().unwrap().lid_open, Some(false));
    }

    #[test]
    fn the_loop_serves_the_cli_and_stops_on_sigterm() {
        let (_dir, root, paths) = setup("svc-thread");
        let clock = TestClock::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let (p, r, c) = (paths.clone(), root.clone(), clock.clone());
        let service = std::thread::spawn(move || {
            let mut svc = Service::start(&p, &r, Box::new(c), true)
                .unwrap_or_else(|e| panic!("{}", e.message()));
            // SIGTERM is blocked in this thread from here on, so it may be sent.
            // SAFETY: pthread_self has no preconditions.
            tx.send(unsafe { libc::pthread_self() }).unwrap();
            svc.run()
        });
        let thread = rx.recv().unwrap();

        assert_eq!(cli::run(Command::Cleaned { date: None }, &paths, &root), EXIT_OK);
        let state: State =
            serde_json::from_value(ipc::call(&paths.socket, "state.get", json!({})).unwrap())
                .unwrap();
        assert_eq!(state.known_clean_until.as_deref(), Some("2027-01-01"));
        assert!(!state.setup_needed, "a clean date answers the first-run question");
        // No case open: refused, and said so.
        assert_eq!(cli::run(Command::NotDust { yes: true }, &paths, &root), EXIT_ERROR);
        // On a terminal it does not ask about a case that is not there.
        let mut answer = std::io::Cursor::new(b"y\n".to_vec());
        assert_eq!(cli::not_dust(&paths, false, true, &mut answer), EXIT_ERROR);
        assert_eq!(answer.position(), 0, "no question was asked");
        assert!(matches!(
            ipc::call(&paths.socket, "check.start", json!({})),
            Err(CallError::Remote(e)) if e.code == "refused"
        ));

        // SAFETY: a signal to the service thread, which has it blocked and reads its signalfd.
        unsafe { libc::pthread_kill(thread, libc::SIGTERM) };
        assert_eq!(service.join().unwrap(), EXIT_OK);
        assert!(!paths.socket.exists());
        assert_eq!(
            cli::run(Command::FanFixed, &paths, &root),
            EXIT_NO_SERVICE,
            "and the CLI knows it is gone"
        );
        let state = store::read_state(&paths.state).unwrap().unwrap();
        assert_eq!(state.known_clean_until.as_deref(), Some("2027-01-01"));
    }

    #[test]
    fn not_dust_on_a_terminal_asks_about_an_open_case() {
        let (_dir, root, paths) = setup("svc-notdust-tty");
        seed_state(&paths, WATCH_STATE);
        let clock = TestClock::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let (p, r, c) = (paths.clone(), root.clone(), clock.clone());
        let service = std::thread::spawn(move || {
            let mut svc = Service::start(&p, &r, Box::new(c), true)
                .unwrap_or_else(|e| panic!("{}", e.message()));
            // SAFETY: pthread_self has no preconditions.
            tx.send(unsafe { libc::pthread_self() }).unwrap();
            svc.run()
        });
        let thread = rx.recv().unwrap();

        let mut no = std::io::Cursor::new(b"n\n".to_vec());
        assert_eq!(cli::not_dust(&paths, false, true, &mut no), EXIT_ERROR);
        assert_eq!(no.position(), 2, "asked, and a no changes nothing");
        let mut yes = std::io::Cursor::new(b"y\n".to_vec());
        assert_eq!(cli::not_dust(&paths, false, true, &mut yes), EXIT_OK);
        let state: State =
            serde_json::from_value(ipc::call(&paths.socket, "state.get", json!({})).unwrap())
                .unwrap();
        assert_eq!(state.status, Status::Clean);

        // SAFETY: a signal to the service thread, which has it blocked and reads its signalfd.
        unsafe { libc::pthread_kill(thread, libc::SIGTERM) };
        assert_eq!(service.join().unwrap(), EXIT_OK);
    }

    #[test]
    fn a_live_subscriber_gets_a_row_each_second_on_battery() {
        let (_dir, root, paths) = setup("svc-live-battery");
        put(&root, "sys/class/power_supply/AC0/online", "0\n");
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        let mono = || clock.clone().now().mono_ns;
        assert_eq!(svc.next_row_ns - mono(), 5 * NS, "on battery and cool: 0.2 Hz");

        let mut c = UnixStream::connect(&paths.socket).unwrap();
        assert_eq!(ask(&mut svc, &mut c, "live.subscribe", json!({"on": true}))["result"], json!({}));
        let mut live = 0;
        for _ in 0..3 {
            clock.advance(1.0);
            svc.step(Some(0));
            live += drain(&mut c, &mut Vec::new())
                .iter()
                .filter(|v| v["event"] == "live" && v["payload"]["ac"] == json!(false))
                .count();
        }
        assert_eq!(live, 3, "one row each second while she watches");

        // Nobody watching: back to 0.2 Hz after the row already planned.
        ask(&mut svc, &mut c, "live.subscribe", json!({"on": false}));
        clock.advance(1.0);
        svc.step(Some(0));
        assert_eq!(svc.next_row_ns - mono(), 5 * NS);
    }

    #[test]
    fn top_consumers_for_plateaus_and_hot_windows() {
        use crate::episodes::LightWindow;
        let dir = test_dir("svc-tops");
        fake::duo(&dir);
        let stat = |ticks: u64| format!("900 (rustc) R 1 0 0 0 -1 0 0 0 0 0 {ticks} 0 0 0 20 0\n");
        put(&dir, "proc/900/stat", &stat(1000));
        put(&dir, "proc/77/stat", "77 (floorp) S 1 0 0 0 -1 0 0 0 0 0 9000 0 0 0 20 0\n");
        let sampler = Sampler::new(&dir);
        let tl = Timeline::starting(Date::new(2026, 12, 2).unwrap(), 10, 0, 0, 0);
        let mut tops = Tops::default();

        // A plateau: the CPU time between its onset and its close, not each process's lifetime.
        let opened = Output::PlateauOpened { onset: tl.now() };
        assert!(tops.for_output(&opened, &sampler).is_empty());
        tops.fresh = None;
        put(&dir, "proc/900/stat", &stat(4000));
        let closed = Output::Plateau(Record::Plateau(Box::default()));
        assert_eq!(tops.for_output(&closed, &sampler), vec![("rustc".to_owned(), 1.0)]);
        tops.fresh = None;

        // A minute that starts warm keeps a snapshot; the hot window of that minute uses it.
        let row = |at: Moment, t: f64| Row {
            at,
            t: Some(t),
            ..Row::default()
        };
        tops.after_row(&row(tl.now(), 85.0), 92.0, &sampler, true);
        assert_eq!(tops.minutes.len(), 1);
        put(&dir, "proc/900/stat", &stat(4600));
        let mut later = tl.clone();
        let window = |hot_ms| {
            Output::Window(LightWindow {
                start: tl.now(),
                hot_ms,
                ..LightWindow::default()
            })
        };
        // It closes a row into the next minute, after that minute's own snapshot.
        tops.after_row(&row(later.advance(60.3), 85.0), 92.0, &sampler, true);
        assert_eq!(tops.minutes.len(), 2);
        assert_eq!(tops.for_output(&window(9000), &sampler), vec![("rustc".to_owned(), 1.0)]);
        // A window with no hot throttle needs none; a cool minute keeps none.
        assert!(tops.for_output(&window(0), &sampler).is_empty());
        tops.after_row(&row(later.advance(60.0), 60.0), 92.0, &sampler, true);
        assert!(tops.minutes.is_empty());
    }

    #[test]
    fn a_chassis_change_is_picked_up() {
        let (dir, root, paths) = setup("svc-chassis");
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        fs::write(dir.join("settings.json"), r#"{"thermal": {"chassis": "generic"}}"#).unwrap();
        assert_eq!(ask(&mut svc, &mut c, "settings.reload", json!({}))["result"], json!({}));
        assert_eq!(svc.state().chassis.profile.as_str(), "generic");
        // Generic mode has no seed, so her key is not referenced until it learns one.
        assert!(!svc.state().key.referenced);
        assert_eq!(svc.state().status, Status::Learning);
    }

    /// /proc/stat counters for the fake tree: one more second, `busy` of every 100 ticks busy.
    struct Stat {
        busy: u64,
        idle: u64,
    }

    impl Stat {
        fn second(&mut self, svc: &mut Service, root: &Path, clock: &TestClock, busy: u64) {
            self.busy += busy;
            self.idle += 100 - busy;
            put(
                root,
                "proc/stat",
                &format!("cpu  {} 0 0 {} 0 0 0 0 0 0\n", self.busy, self.idle),
            );
            clock.advance(1.0);
            svc.step(Some(0));
        }
    }

    fn phase(svc: &Service) -> Option<CheckPhase> {
        svc.state().check.as_ref().map(|c| c.state)
    }

    /// A service that has sat idle for a minute, with `load` as the check's load.
    fn idle_service(name: &str, load: Cmd) -> (PathBuf, PathBuf, Paths, TestClock, Service, Stat) {
        let (dir, root, paths) = setup(name);
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        svc.load = Some(load);
        let mut stat = Stat {
            busy: 100_000,
            idle: 10_000,
        };
        for _ in 0..61 {
            stat.second(&mut svc, &root, &clock, 1);
        }
        (dir, root, paths, clock, svc, stat)
    }

    #[test]
    fn a_whole_check_reaches_the_rules() {
        let (_dir, root, paths, clock, mut svc, mut stat) = idle_service("svc-check-whole", sleeper());
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        let r = ask(&mut svc, &mut c, "check.start", json!({"confirm": true}));
        let dir = r["result"]["dir"].as_str().unwrap().to_owned();
        assert_eq!(r["result"]["state"], "gating");
        assert!(dir.starts_with("checks/2026-12-02_10"), "{dir}");
        assert_eq!(phase(&svc), Some(CheckPhase::Gating));
        let r = ask(&mut svc, &mut c, "check.start", json!({"confirm": true}));
        assert_eq!(code(&r), "busy");

        // The ring holds a quiet minute, so the gates pass at the next row.
        stat.second(&mut svc, &root, &clock, 1);
        assert_eq!(phase(&svc), Some(CheckPhase::Idle));
        for _ in 0..30 {
            stat.second(&mut svc, &root, &clock, 1);
        }
        assert_eq!(phase(&svc), Some(CheckPhase::Load));
        let on_disk = store::read_state(&paths.state).unwrap().unwrap();
        assert_eq!(on_disk.check.unwrap().state, CheckPhase::Load, "written at the phase change");
        assert_eq!(svc.state().mon.badge, "COOLING CHECK · LOAD");
        for _ in 0..180 {
            stat.second(&mut svc, &root, &clock, 99);
        }
        assert_eq!(phase(&svc), Some(CheckPhase::Recovery));
        for _ in 0..90 {
            stat.second(&mut svc, &root, &clock, 1);
        }
        assert!(svc.check.is_none());
        assert_eq!(phase(&svc), Some(CheckPhase::Done));
        // The fake package sits at 57 °C without throttle: like the seed, and recorded.
        let last = svc.state().last_check.clone().unwrap();
        assert_eq!((last.verdict, last.label.as_str()), (CheckVerdict::Like, "Like the reference"));
        assert_eq!(last.dir, dir);
        let events: Vec<Event> = store::read_jsonl(&paths.events).unwrap();
        let kinds: Vec<EventKind> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::CheckStart | EventKind::CheckResult))
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, [EventKind::CheckStart, EventKind::CheckResult]);
        assert!(events.iter().any(|e| e.text.starts_with("Cooling check: Like the reference")));
        let result: Value =
            serde_json::from_str(&fs::read_to_string(paths.dir.join(&dir).join("result.json")).unwrap())
                .unwrap();
        assert_eq!((result["verdict"].clone(), result["window"].clone()), (json!("like"), json!("matched")));

        // check.rows: the last check's rows, from memory and, after a restart, from samples.csv.
        let rows = ask(&mut svc, &mut c, "check.rows", json!({}))["result"].clone();
        assert_eq!(rows.as_array().unwrap().len(), 300);
        assert_eq!(rows[40]["phase"], "load");
        drop(c);
        drop(svc);
        let mut svc = start(&paths, &root, &clock);
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        let rows = ask(&mut svc, &mut c, "check.rows", json!({}))["result"].clone();
        assert_eq!(rows.as_array().unwrap().len(), 300);
        assert_eq!(rows[40]["T"], json!(57.0));
    }

    #[test]
    fn stopping_the_service_kills_the_load_group() {
        become_subreaper();
        let scratch = test_dir("svc-check-stop-load");
        let (load, pid_file) = family(&scratch);
        let (_dir, root, paths, clock, mut svc, mut stat) = idle_service("svc-check-stop", load);
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        let dir = ask(&mut svc, &mut c, "check.start", json!({"confirm": true}))["result"]["dir"]
            .as_str()
            .unwrap()
            .to_owned();
        for _ in 0..31 {
            stat.second(&mut svc, &root, &clock, 1);
        }
        assert_eq!(phase(&svc), Some(CheckPhase::Load));
        let grandchild = read_pid(&pid_file);
        svc.shutdown();
        assert_eq!(reap(grandchild), Some(libc::SIGKILL), "the burn's whole group dies with the service");
        let state = store::read_state(&paths.state).unwrap().unwrap();
        let check = state.check.unwrap();
        assert_eq!((check.state, check.reason.as_deref()), (CheckPhase::Aborted, Some("service stop")));
        let events: Vec<Event> = store::read_jsonl(&paths.events).unwrap();
        assert!(events.iter().any(|e| e.text == "Cooling check aborted: service stop"));
        let result: Value =
            serde_json::from_str(&fs::read_to_string(paths.dir.join(&dir).join("result.json")).unwrap())
                .unwrap();
        assert_eq!(result["state"], "aborted");
    }

    #[test]
    fn cancel_over_the_socket_kills_the_load_group() {
        become_subreaper();
        let scratch = test_dir("svc-check-cancel-load");
        let (load, pid_file) = family(&scratch);
        let (_dir, root, paths, clock, mut svc, mut stat) = idle_service("svc-check-cancel", load);
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        ask(&mut svc, &mut c, "check.start", json!({"confirm": true}));
        // The gates, 30 s idle, then two seconds of load.
        for _ in 0..31 {
            stat.second(&mut svc, &root, &clock, 1);
        }
        for _ in 0..2 {
            stat.second(&mut svc, &root, &clock, 99);
        }
        let grandchild = read_pid(&pid_file);
        assert_eq!(phase(&svc), Some(CheckPhase::Load));
        let rows = ask(&mut svc, &mut c, "check.rows", json!({}))["result"].clone();
        assert_eq!(rows.as_array().unwrap().len(), 32, "the running check's rows");
        assert_eq!(ask(&mut svc, &mut c, "check.cancel", json!({}))["result"], json!({}));
        assert_eq!(reap(grandchild), Some(libc::SIGKILL));
        assert!(svc.check.is_none());
        let check = svc.state().check.clone().unwrap();
        assert_eq!((check.state, check.reason.as_deref()), (CheckPhase::Aborted, Some("cancelled")));
        // Nothing became a result.
        assert!(svc.state().last_check.is_none());
        let events: Vec<Event> = store::read_jsonl(&paths.events).unwrap();
        assert!(events.iter().any(|e| e.text == "Cooling check aborted: cancelled"));
        let rows = ask(&mut svc, &mut c, "check.rows", json!({}))["result"].clone();
        assert_eq!(rows.as_array().unwrap().len(), 32, "the cancelled check's rows stay readable");
    }

    #[test]
    fn a_key_that_differs_is_refused_unless_run_anyway() {
        let (_dir, root, paths) = setup("svc-check-key");
        put(&root, "sys/devices/system/cpu/intel_pstate/no_turbo", "1\n");
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        svc.load = Some(sleeper());
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        let r = ask(&mut svc, &mut c, "check.start", json!({"confirm": true}));
        assert_eq!(code(&r), "refused");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("turbo off (reference on): not comparable with the clean reference"),
            "{r}"
        );
        assert!(svc.check.is_none());
        assert!(!paths.checks.exists(), "a refused start leaves no directory");

        let r = ask(&mut svc, &mut c, "check.start", json!({"confirm": true, "anyway": true}));
        assert_eq!(r["result"]["state"], "gating");
        let check = svc.state().check.clone().unwrap();
        assert!(check.anyway);
        assert_eq!(check.purpose, CheckPurpose::Reference);
        assert_eq!(ask(&mut svc, &mut c, "check.cancel", json!({}))["result"], json!({}));
    }

    #[test]
    fn a_closed_lid_waits_at_its_gate() {
        let (_dir, root, paths) = setup("svc-check-lid-gate");
        put(&root, "proc/acpi/button/lid/LID/state", "state:      closed\n");
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        svc.load = Some(sleeper());
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        // Closing it would abort the load, so it is never a setting to run anyway with.
        let r = ask(&mut svc, &mut c, "check.start", json!({"confirm": true}));
        assert_eq!(r["result"]["state"], "gating", "{r}");
        clock.advance(1.0);
        svc.step(Some(0));
        let check = svc.state().check.clone().unwrap();
        assert!(!check.anyway);
        let lid = check.gates.iter().find(|g| g.id == "lid").unwrap();
        assert_eq!((lid.ok, lid.text.as_str()), (false, "lid closed: open the lid"));
        assert_eq!(ask(&mut svc, &mut c, "check.cancel", json!({}))["result"], json!({}));
    }

    #[test]
    fn a_restart_closes_the_directory_of_an_unfinished_check() {
        let (_dir, root, paths) = setup("svc-check-restart");
        seed_state(
            &paths,
            r#"{"schema": 1, "status": "clean",
                "check": {"purpose": "verify", "state": "recovery", "dir": "checks/2026-12-02_0950"}}"#,
        );
        let dir = paths.dir.join("checks/2026-12-02_0950");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("samples.csv"), format!("{}\n1.0,idle,0.02,55.0\n", check::CSV_HEADER)).unwrap();
        let mut svc = start(&paths, &root, &TestClock::new());
        let result: Value = serde_json::from_str(&fs::read_to_string(dir.join("result.json")).unwrap()).unwrap();
        assert_eq!((result["state"].clone(), result["reason"].clone()), (json!("aborted"), json!("service restart")));
        let mut c = UnixStream::connect(&paths.socket).unwrap();
        let rows = ask(&mut svc, &mut c, "check.rows", json!({}))["result"].clone();
        assert_eq!(rows, json!([{"t_s": 1.0, "phase": "idle", "b": 0.02, "T": 55.0}]));
    }

    #[test]
    fn a_check_ended_by_a_stopped_fan_raises_fan() {
        let (_dir, root, paths) = setup("svc-check-fan");
        let clock = TestClock::new();
        let mut svc = start(&paths, &root, &clock);
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut check = fake_check(None, &log);
        check.stopped_fan = true;
        svc.begin_check(check);
        let now = clock.clone().now();
        if let Some(c) = svc.check.as_mut() {
            c.abort("fan stopped at 85 °C", &now);
        }
        svc.end_check(None, &now);
        assert_eq!(svc.state().fan.status, FanStatus::Stopped);
        assert_eq!(svc.state().pill.kind, "fan");
        let events: Vec<Event> = store::read_jsonl(&paths.events).unwrap();
        let texts: Vec<&str> = events.iter().map(|e| e.text.as_str()).collect();
        assert!(texts.contains(&"Cooling check aborted: fan stopped at 85 °C"), "{texts:?}");
        assert!(texts.contains(&"Fan stopped under load"), "{texts:?}");
    }
}
