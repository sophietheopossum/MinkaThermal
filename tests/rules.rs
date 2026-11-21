//! Synthetic 1 Hz traces through the whole pipeline: the real deriver (B5, B10, gaps, the key),
//! the episode detector, the verdicts and the state machine. No sensors, no load, no store: the
//! effects the service would write are collected and inspected.

use minka_thermal::clock::{Date, Moment, Stamp, Timeline};
use minka_thermal::episodes::Output;
use minka_thermal::model::{
    CaseOutcome, CaseRecord, CheckPurpose, CheckRef, CheckVerdict, DailyRecord, Event, EventKind,
    FanStatus, FaultStatus, LightRecord, LightRef, LightVerdict, Notice, PlateauRecord,
    PlateauVerdict, PostClean, Record, References, State, Status, Tone, UnjudgedRecord,
};
use minka_thermal::profile::{Chassis, ChassisSetting};
use minka_thermal::rules::{
    CheckMetrics, CheckOutcome, Effect, Engine, KeyCtx, LIGHT_LOAD_EVIDENCE, Machine, Watch,
};
use minka_thermal::sampler::{Deriver, Raw, Row};
use minka_thermal::sensors::{ConfigRead, CpuTimes, PowerRead, StaticInfo, XeRead};

const SEED_KEY: &str = "ac|performance|performance|28|35|8|0|100|open";

fn dec(d: u32) -> Date {
    Date::new(2026, 12, 1).unwrap().add_days(d as i64 - 1)
}

fn her_config() -> ConfigRead {
    ConfigRead {
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
    }
}

fn machine(chassis: Chassis) -> Machine {
    Machine {
        chassis,
        setting: ChassisSetting::Auto,
        info: StaticInfo {
            sys_vendor: "ASUSTeK COMPUTER INC.".into(),
            board_name: "UX482EG".into(),
            product_name: "ZenBook UX482EG_UX482EG".into(),
            bios_version: "UX482EG.311".into(),
            kernel: "7.3.4-1-cachyos".into(),
            cpu_model: "11th Gen Intel(R) Core(TM) i7-1165G7".into(),
            nproc: 8,
            boot_id: "boot".into(),
        },
        tjmax_c: Some(100.0),
        has_fan: true,
        has_throttle: true,
        light_load_evidence: LIGHT_LOAD_EVIDENCE,
        // What the sensors support; `Engine::new` drops E2 while light-load evidence is off.
        classes: vec!["E1".into(), "E2".into(), "check".into()],
    }
}

/// The machine with light-load evidence (E2) on. It is off by her decision of 21/11/2026; the
/// tests of the E2 rules run with it on, so the rules stay guarded for the day it comes back.
fn machine_e2(chassis: Chassis) -> Machine {
    Machine {
        light_load_evidence: true,
        ..machine(chassis)
    }
}

/// The service's loop without the I/O.
struct Rig {
    tl: Timeline,
    deriver: Deriver,
    busy: u64,
    idle: u64,
    thr: u64,
    config: ConfigRead,
    config_due: bool,
    /// On AC; false is on battery.
    ac: bool,
    /// The busy-CPU frequency of rows at b ≥ 0.8.
    freq: u32,
    watch: Watch,
    effects: Vec<Effect>,
}

impl Rig {
    /// Her Duo, set up with "last cleaned 20/11/2026" (known clean until 20/12/2026), on
    /// 1/12/2026 at 08:00.
    fn duo() -> Rig {
        Rig::with(Chassis::Duo, her_config())
    }

    /// `duo` with light-load evidence (E2) on: for the tests of the E2 rules (see `machine_e2`).
    fn duo_e2() -> Rig {
        Rig::on(
            machine_e2(Chassis::Duo),
            her_config(),
            Timeline::starting(dec(1), 8, 0, 0, 0),
        )
    }

    fn with(chassis: Chassis, config: ConfigRead) -> Rig {
        Rig::at(chassis, config, Timeline::starting(dec(1), 8, 0, 0, 0))
    }

    /// Set up as `with`, on the timeline given (another start or another UTC offset).
    fn at(chassis: Chassis, config: ConfigRead, tl: Timeline) -> Rig {
        Rig::on(machine(chassis), config, tl)
    }

    /// Set up as `at`, on the machine given.
    fn on(machine: Machine, config: ConfigRead, tl: Timeline) -> Rig {
        let engine = Engine::new(machine, None, References::default(), &tl.now());
        let mut rig = Rig {
            tl,
            deriver: Deriver::default(),
            busy: 0,
            idle: 0,
            thr: 0,
            config,
            config_due: true,
            ac: true,
            freq: 3899,
            watch: Watch::new(engine),
            effects: Vec::new(),
        };
        let now = rig.now();
        rig.engine()
            .setup_answer(&now, Date::new(2026, 11, 20))
            .unwrap();
        rig.idle(120);
        rig
    }

    /// A service started again at `date` `h`:00 around `engine`, with a fresh deriver.
    fn resume(engine: Engine, date: Date, h: u32) -> Rig {
        Rig {
            tl: Timeline::starting(date, h, 0, 0, 0),
            deriver: Deriver::default(),
            busy: 0,
            idle: 0,
            thr: 0,
            config: her_config(),
            config_due: true,
            ac: true,
            freq: 3899,
            watch: Watch::new(engine),
            effects: Vec::new(),
        }
    }

    /// What the store would hold now, read back as the service reads it at start.
    fn store(&self) -> (State, References, Vec<DailyRecord>, Vec<Record>, Vec<CaseRecord>, Vec<Event>) {
        let state: State =
            serde_json::from_str(&serde_json::to_string(self.state()).unwrap()).unwrap();
        let refs: References =
            serde_json::from_str(&serde_json::to_string(&self.watch.engine.refs).unwrap()).unwrap();
        let mut daily = Vec::new();
        let mut events = Vec::new();
        for e in &self.effects {
            match e {
                Effect::Daily(d) => daily.push(d.clone()),
                Effect::Event {
                    at,
                    kind,
                    case,
                    text,
                    data,
                } => events.push(Event {
                    seq: events.len() as u64 + 1,
                    at: at.stamp(),
                    kind: *kind,
                    case: case.clone(),
                    text: text.clone(),
                    data: data.clone(),
                }),
                _ => {}
            }
        }
        (state, refs, daily, self.records(), self.cases(), events)
    }

    /// The service stops now and starts again at `date` `h`:00 (the offset unchanged). The new
    /// rig keeps the old effects, so the history reads as one.
    fn restart(self, date: Date, h: u32) -> Rig {
        let light = self.watch.engine.machine.light_load_evidence;
        self.restart_as(date, h, light)
    }

    /// `restart`, into a build whose light-load evidence is `light_load_evidence`.
    fn restart_as(self, date: Date, h: u32, light_load_evidence: bool) -> Rig {
        let (state, refs, daily, records, cases, events) = self.store();
        let offset = self.now().offset_s;
        let tl = Timeline::starting(date, h, 0, 0, offset);
        let now = tl.now();
        let machine = Machine {
            light_load_evidence,
            ..machine(Chassis::Duo)
        };
        let mut engine = Engine::new(machine, Some(state), refs, &now);
        engine.restore(&daily, &records, &cases, &events, &now);
        Rig {
            tl,
            config: self.config,
            ac: self.ac,
            effects: self.effects,
            ..Rig::resume(engine, date, h)
        }
    }

    fn records(&self) -> Vec<Record> {
        self.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Record(r) => Some(r.clone()),
                _ => None,
            })
            .collect()
    }

    fn engine(&mut self) -> &mut Engine {
        &mut self.watch.engine
    }

    fn state(&self) -> &State {
        &self.watch.engine.state
    }

    fn now(&self) -> Moment {
        self.tl.now()
    }

    /// A light reference already learned for her key (300 windows take five hours of rows).
    fn learned_light(&mut self) {
        self.engine()
            .refs
            .keys
            .entry(SEED_KEY.into())
            .or_default()
            .light = Some(LightRef {
            n: 300,
            p90: Some(0.002),
            ..LightRef::default()
        });
    }

    fn tick(&mut self, b: f64, t: f64, thr_ms: u64, fan: u32) -> Row {
        let at = self.tl.advance(1.0);
        self.step(at, b, t, thr_ms, fan)
    }

    fn step(&mut self, at: Moment, b: f64, t: f64, thr_ms: u64, fan: u32) -> Row {
        let busy = (b * 800.0).round() as u64;
        self.busy += busy;
        self.idle += 800 - busy;
        self.thr += thr_ms;
        let config = std::mem::take(&mut self.config_due).then(|| self.config.clone());
        let row = self.deriver.derive(
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
                xe: Some(XeRead::default()),
                xe_act: Some(0),
                power: Some(PowerRead {
                    ac: Some(self.ac),
                    ..PowerRead::default()
                }),
                config,
                ..Raw::default()
            },
            1.0,
        );
        let key = KeyCtx::new(
            row.key_seq,
            self.deriver.key().expect("key").clone(),
            92.0,
            Some(28),
        );
        self.watch.feed(&row, &key, |o| match o {
            Output::Plateau(_) => vec![("rustc".into(), 0.71), ("cargo".into(), 0.1)],
            _ => vec![("bch-reconcile".into(), 0.6)],
        });
        self.effects.extend(self.watch.engine.take_effects());
        row
    }

    fn idle(&mut self, secs: u32) {
        for _ in 0..secs {
            self.tick(0.02, 50.0, 0, 4000);
        }
    }

    /// A 300-s build: the turbo transient (91 °C, 38 % throttled, fan 6100), then the plateau
    /// around `t` (t − 1, t, t + 1 in turn) with `duty` throttled and the fan at `fan`. Ends with
    /// 30 s idle so the episode closes. Returns the onset.
    fn build_with(&mut self, t: f64, duty: f64, fan: u32) -> Stamp {
        let onset = self.now().stamp();
        for i in 0..300u32 {
            if i < 56 {
                self.tick(1.0, 91.0, 380, fan.min(6100));
            } else {
                self.tick(1.0, t + (i % 3) as f64 - 1.0, (duty * 1000.0).round() as u64, fan);
            }
        }
        self.idle(30);
        onset
    }

    /// 90.5 °C at 6.8 %: hot against the seed (≥ 90 °C with ≥ 3 %).
    fn hot_build(&mut self) -> Stamp {
        self.build_with(90.5, 0.068, 6800)
    }

    /// 86 °C at 0.9 %: the seed itself.
    fn clean_build(&mut self) -> Stamp {
        self.build_with(86.0, 0.009, 6800)
    }

    /// `mins` minutes of 1-2 thread work at `t` °C with `duty` throttled.
    fn light(&mut self, mins: u32, t: f64, duty: f64) {
        for _ in 0..mins * 60 {
            self.tick(0.15, t, (duty * 1000.0).round() as u64, 4600);
        }
        self.idle(5);
    }

    /// An E2 evidence day with light-load evidence on: 13 minutes at 91 °C, 40 % throttled (the
    /// dust of 17/11/2026).
    fn hot_light(&mut self) {
        self.light(13, 91.0, 0.4);
    }

    /// bch-reconcile alone at nice 19, as on her clean machine on 21/11/2026: about one core,
    /// 88 °C, 45 % throttled, the fan on its 6100 stage.
    fn reconcile_light(&mut self, mins: u32) {
        for _ in 0..mins * 60 {
            self.tick(0.125, 88.0, 450, 6100);
        }
        self.idle(5);
    }

    fn light_records(&self) -> Vec<LightRecord> {
        self.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Record(Record::Light(l)) => Some(l.clone()),
                _ => None,
            })
            .collect()
    }

    /// The laptop sleeps until two minutes before `date` `h`:`m`, then idles up to it. The first
    /// row is a stall: the engine settles the days in between.
    fn goto(&mut self, date: Date, h: u32, m: u32) {
        let target =
            date.days() * 86_400 + (h * 3600 + m * 60) as i64 - self.now().offset_s as i64;
        let ahead = target - self.now().unix_s();
        assert!(ahead > 120, "goto goes forward");
        self.tl.advance(ahead as f64 - 120.0);
        self.idle(120);
        assert_eq!(self.now().unix_s(), target);
    }

    fn events(&self, kind: EventKind) -> Vec<String> {
        self.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Event { kind: k, text, .. } if *k == kind => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn cases(&self) -> Vec<CaseRecord> {
        self.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Case(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    fn plateaus(&self) -> Vec<PlateauRecord> {
        self.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Record(Record::Plateau(p)) => Some((**p).clone()),
                _ => None,
            })
            .collect()
    }

    fn unjudged(&self) -> Vec<UnjudgedRecord> {
        self.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Record(Record::Unjudged(u)) => Some(u.clone()),
                _ => None,
            })
            .collect()
    }

    fn daily(&self, d: Date) -> Option<DailyRecord> {
        self.effects.iter().find_map(|e| match e {
            Effect::Daily(l) if l.day == d.key() => Some(l.clone()),
            _ => None,
        })
    }

    fn status(&self) -> Status {
        self.state().status
    }

    /// A check that ended now, under her key: 90.5 °C at 7 % when Worse, else 86.2 °C at 1 %.
    fn outcome(&self, purpose: CheckPurpose, verdict: CheckVerdict, post: Option<PostClean>) -> CheckOutcome {
        let now = self.now();
        let worse = verdict == CheckVerdict::Worse || post == Some(PostClean::NotImproved);
        CheckOutcome {
            at: now.plus(-300.0),
            dir: format!("checks/{}", now.day_key()),
            purpose,
            verdict,
            post_clean: post,
            comparable: true,
            key_id: SEED_KEY.into(),
            metrics: CheckMetrics {
                t_ss: Some(if worse { 90.5 } else { 86.2 }),
                duty: Some(if worse { 0.07 } else { 0.01 }),
                fan: Some(6800.0),
                ..CheckMetrics::default()
            },
            matched: None,
            generator: Some(1),
            cpu: "11th Gen Intel(R) Core(TM) i7-1165G7".into(),
            fan_flag: false,
            diagnosis: Vec::new(),
        }
    }

    fn run_check(&mut self, outcome: CheckOutcome) {
        let now = self.now();
        self.engine().check_result(&now, outcome);
        let e = self.engine().take_effects();
        self.effects.extend(e);
    }

    fn check(&mut self, purpose: CheckPurpose, verdict: CheckVerdict, post: Option<PostClean>) {
        let o = self.outcome(purpose, verdict, post);
        self.run_check(o);
    }

    fn notice_ids(&self) -> Vec<String> {
        self.state().notices.iter().map(|n| n.id.clone()).collect()
    }

    fn act(&mut self, f: impl FnOnce(&mut Engine, &Moment) -> Result<(), String>) {
        let now = self.now();
        f(self.engine(), &now).unwrap();
        let e = self.engine().take_effects();
        self.effects.extend(e);
    }
}

#[test]
fn a_clean_build_stays_clean() {
    let mut r = Rig::duo();
    assert_eq!(r.status(), Status::Clean, "the seed is her key's reference");
    assert_eq!(
        r.state().key.reference.as_deref(),
        Some("duo-seed 20/11/2026")
    );
    r.goto(dec(1), 10, 0);
    r.clean_build();
    let p = r.plateaus();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].verdict, PlateauVerdict::Clean);
    assert_eq!(p[0].t_ss, Some(86.0));
    assert_eq!(p[0].top[0].0, "rustc");
    assert_eq!(
        r.state().last_plateau.as_ref().unwrap().verdict,
        PlateauVerdict::Clean
    );
    r.goto(dec(2), 9, 0);
    let day = r.daily(dec(1)).expect("day settled");
    assert_eq!((day.voting, day.clean, day.hot), (1, 1, 0));
    assert!(day.qualifying && !day.e1);
    assert!(day.bins.ge85.min > 4.0, "{:?}", day.bins.ge85);
    assert_eq!(r.status(), Status::Clean);
    assert!(!r.state().pill.show && !r.state().mon.show);
    assert!(r.events(EventKind::FirstEvidence).is_empty());
}

#[test]
fn a_hot_build_opens_watch_with_first_evidence_frozen() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    let onset = r.hot_build();
    r.goto(dec(1), 15, 0);
    r.hot_build();
    assert_eq!(r.status(), Status::Clean, "E1 is decided when the day settles");
    r.goto(dec(2), 9, 0);
    assert_eq!(r.status(), Status::Watch);
    let case = r.state().case.clone().unwrap();
    let first = case.first_evidence.clone().unwrap();
    assert_eq!(first.at, onset, "the earliest evidence of the day: the 10:00 build");
    assert_eq!(first.label, "1/12/2026 10:00");
    assert_eq!(first.class, "E1");
    assert_eq!(
        first.summary,
        "build 90.5 °C, throttled 6.8 % (clean 86 °C, 0.9 %), mostly rustc"
    );
    assert_eq!(first.top, vec!["rustc", "cargo"]);
    assert_eq!(case.id, "c-20261201-1000");
    assert_eq!(case.evidence_days, vec![dec(1).key()]);
    assert_eq!(
        r.events(EventKind::FirstEvidence),
        vec!["First evidence: build 90.5 °C, throttled 6.8 % (clean 86 °C, 0.9 %), mostly rustc"]
    );
    // WATCH is logged only.
    assert!(!r.state().pill.show && !r.state().mon.show);

    // More evidence later never moves it.
    r.goto(dec(2), 10, 0);
    r.hot_build();
    r.goto(dec(3), 9, 0);
    let case2 = r.state().case.clone().unwrap();
    assert_eq!(case2.first_evidence.unwrap(), first);
    assert_eq!(case2.evidence_days, vec![dec(1).key(), dec(2).key()]);
}

#[test]
fn suspicion_needs_two_evidence_days_at_half_the_qualifying_days() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.hot_build();
    for d in [2, 3, 4] {
        r.goto(dec(d), 10, 0);
        r.clean_build();
    }
    // Day 5 hot: 2 evidence days of 5 qualifying, under half.
    r.goto(dec(5), 10, 0);
    r.hot_build();
    r.goto(dec(6), 9, 0);
    assert_eq!(r.status(), Status::Watch);
    // Day 6 hot: 3 of 6, half: SUSPICION by rule (i).
    r.goto(dec(6), 10, 0);
    r.hot_build();
    r.goto(dec(7), 9, 0);
    assert_eq!(r.status(), Status::Suspicion);
    assert_eq!(
        r.events(EventKind::Suspicion),
        vec!["Possible dust: signs on 3 of 6 qualifying days in 14"]
    );
    let s = r.state();
    assert!(s.case.as_ref().unwrap().suspected_at.is_some());
    assert!(!s.pill.show, "no bar pill before WARNING");
    assert_eq!(s.mon.kind, "suspicion");
    assert_eq!(s.mon.badge, "DUST? SINCE 1/12/2026");
    assert_eq!(s.mon.actions, vec!["check", "later"]);

    // Later: hides the actions for 3 days, the badge stays.
    r.act(|e, now| e.snooze_suspicion(now));
    assert!(r.state().mon.show && r.state().mon.actions.is_empty());
    r.goto(dec(9), 9, 0);
    assert!(r.state().mon.show && r.state().mon.actions.is_empty(), "still hidden on day 9");
    r.goto(dec(10), 12, 0);
    assert_eq!(r.state().mon.actions, vec!["check", "later"]);
}

#[test]
fn warning_route_a_a_worse_check_in_watch() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.hot_build();
    r.goto(dec(2), 9, 0);
    assert_eq!(r.status(), Status::Watch);
    r.check(CheckPurpose::Confirm, CheckVerdict::Worse, None);
    assert_eq!(r.status(), Status::Warning);
    let case = r.state().case.clone().unwrap();
    assert_eq!(case.route.as_deref(), Some("a"));
    let s = r.state();
    assert_eq!(
        (s.pill.show, s.pill.kind.as_str(), s.pill.tone),
        (true, "dust_warning", Tone::Amber)
    );
    assert_eq!(s.pill.label, "Needs a clean since 1/12/2026");
    assert_eq!(s.mon.badge, "NEEDS A CLEAN · FIRST 1/12/2026 10:00");
    assert!(s.mon.triangle && s.mon.pulse);
}

#[test]
fn warning_route_a_waits_a_day_after_a_case_opened_by_a_check() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    // A reference check from CLEAN comes out Worse: SUSPICION, dated at the check.
    r.check(CheckPurpose::Reference, CheckVerdict::Worse, None);
    assert_eq!(r.status(), Status::Suspicion);
    let first = r.state().case.as_ref().unwrap().first_evidence.clone().unwrap();
    assert_eq!(first.class, "check");
    assert_eq!(first.label, "1/12/2026 09:55");
    // Again the same day: not a second opinion yet.
    r.goto(dec(1), 14, 0);
    r.check(CheckPurpose::Confirm, CheckVerdict::Worse, None);
    assert_eq!(r.status(), Status::Suspicion);
    // On a later day it is.
    r.goto(dec(2), 10, 0);
    r.check(CheckPurpose::Confirm, CheckVerdict::Worse, None);
    assert_eq!(r.status(), Status::Warning);
    assert_eq!(r.state().case.as_ref().unwrap().first_evidence.as_ref(), Some(&first));

    // A check like the reference clears a case in WATCH or SUSPICION.
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.hot_build();
    r.goto(dec(2), 9, 0);
    r.check(CheckPurpose::Confirm, CheckVerdict::Like, None);
    assert_eq!(r.status(), Status::Clean);
    assert_eq!(r.cases()[0].outcome, minka_thermal::model::CaseOutcome::ClearedByCheck);
    // Borderline changes nothing.
    r.goto(dec(2), 10, 0);
    r.hot_build();
    r.goto(dec(3), 9, 0);
    assert_eq!(r.status(), Status::Watch);
    r.check(CheckPurpose::Confirm, CheckVerdict::Borderline, None);
    assert_eq!(r.status(), Status::Watch);
}

/// Days 1-3 hot builds, day 4 an E2 day (with light-load evidence on, `Rig::duo_e2`), day 5 a
/// hot build.
fn five_evidence_days(r: &mut Rig) {
    r.learned_light();
    for d in [1, 2, 3] {
        r.goto(dec(d), 10, 0);
        r.hot_build();
    }
    r.goto(dec(4), 14, 0);
    r.hot_light();
    r.goto(dec(5), 10, 0);
    r.hot_build();
}

#[test]
fn warning_route_b() {
    // E2 on: route (b) needs an E2 day.
    let mut r = Rig::duo_e2();
    five_evidence_days(&mut r);
    assert_eq!(r.status(), Status::Suspicion);
    let case = r.state().case.clone().unwrap();
    assert_eq!(case.e2_days, vec![dec(4).key()], "E2 is decided at the 10th window");
    r.goto(dec(6), 9, 0);
    assert_eq!(r.status(), Status::Warning);
    let case = r.state().case.clone().unwrap();
    assert_eq!(case.route.as_deref(), Some("b"));
    assert_eq!(case.evidence_days.len(), 5);
    assert_eq!(
        r.events(EventKind::Warning),
        vec!["Needs a clean: signs on 5 of 5 days in 21, light load on 1"]
    );
    assert_eq!(r.state().pill.label, "Needs a clean since 1/12/2026");
}

#[test]
fn warning_route_c() {
    // E2 on: route (c) is E2 days alone.
    let mut r = Rig::duo_e2();
    r.learned_light();
    r.goto(dec(1), 14, 0);
    r.hot_light();
    // E2 days open the case at once, with the first hot window as the evidence.
    assert_eq!(r.status(), Status::Watch);
    let first = r.state().case.as_ref().unwrap().first_evidence.clone().unwrap();
    assert_eq!(first.class, "E2");
    assert_eq!(first.label, "1/12/2026 14:00");
    assert_eq!(
        first.summary,
        "light load 1.2 threads, throttled 40.0 % at 91 °C (clean 0.2 %), mostly bch-reconcile"
    );
    r.goto(dec(2), 14, 0);
    r.hot_light();
    assert_eq!(r.status(), Status::Suspicion);
    r.goto(dec(3), 14, 0);
    r.hot_light();
    assert_eq!(r.status(), Status::Warning);
    assert_eq!(r.state().case.as_ref().unwrap().route.as_deref(), Some("c"));
    assert_eq!(
        r.events(EventKind::Warning),
        vec!["Needs a clean: light-load throttling on 3 days in 14"]
    );
}

#[test]
fn warning_route_d_plateaus_only() {
    let mut r = Rig::duo();
    for d in [1, 2] {
        r.goto(dec(d), 10, 0);
        r.hot_build();
    }
    r.goto(dec(3), 9, 0);
    assert_eq!(r.status(), Status::Suspicion);
    // A hot build every third day, nothing else, no light load at all.
    for d in (4..=22).step_by(3) {
        r.goto(dec(d), 10, 0);
        r.hot_build();
        r.goto(dec(d + 1), 9, 0);
        assert_eq!(r.status(), Status::Suspicion, "day {d}");
    }
    // SUSPICION began as day 2 settled, early on day 3. Day 23 settling, early on day 24, makes
    // days 3-23: 21 days.
    r.goto(dec(24), 9, 0);
    assert_eq!(r.status(), Status::Warning);
    assert_eq!(r.state().case.as_ref().unwrap().route.as_deref(), Some("d"));
    assert_eq!(
        r.events(EventKind::Warning),
        vec!["Needs a clean: signs on 7 of 7 days in 21, after three weeks of suspicion"]
    );
}

#[test]
fn the_r2_guard_blocks_passive_escalation() {
    // E2 on: the guard is shown on route (b).
    let mut r = Rig::duo_e2();
    r.learned_light();
    for d in [1, 2, 3] {
        r.goto(dec(d), 10, 0);
        r.hot_build();
    }
    r.goto(dec(4), 14, 0);
    r.hot_light();
    r.goto(dec(5), 10, 0);
    r.hot_build();
    // The day's last build is clean: cooling that holds at 28 W refutes dust at lower power.
    r.goto(dec(5), 15, 0);
    r.clean_build();
    r.goto(dec(6), 9, 0);
    let day = r.daily(dec(5)).unwrap();
    assert!(day.e1, "hot ≥ clean: still an evidence day");
    assert_eq!(r.status(), Status::Suspicion, "route b blocked by R2");
    r.goto(dec(6), 10, 0);
    r.hot_build();
    r.goto(dec(7), 9, 0);
    assert_eq!(r.status(), Status::Warning);
    assert_eq!(r.state().case.as_ref().unwrap().route.as_deref(), Some("b"));
}

#[test]
fn watch_lapses_after_14_quiet_days() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.hot_build();
    for d in [3, 7, 11] {
        r.goto(dec(d), 10, 0);
        r.clean_build();
    }
    r.goto(dec(15), 9, 0);
    assert_eq!(r.status(), Status::Watch, "day 1 is still inside the 14 days");
    r.goto(dec(16), 9, 0);
    assert_eq!(r.status(), Status::Clean);
    let cases = r.cases();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].outcome, minka_thermal::model::CaseOutcome::Lapsed);
    assert_eq!(
        r.events(EventKind::Lapse),
        vec!["No signs for 14 days (3 with comparable load): the case of 1/12/2026 10:00 closed"]
    );
    // A new case within 60 days carries the earlier sighting.
    r.goto(dec(20), 10, 0);
    r.hot_build();
    r.goto(dec(21), 9, 0);
    assert_eq!(r.status(), Status::Watch);
    let case = r.state().case.clone().unwrap();
    assert_eq!(case.earlier_sighting.unwrap().label, "1/12/2026");
    assert!(
        r.events(EventKind::FirstEvidence)[1].ends_with("; earlier sighting 1/12/2026"),
        "{:?}",
        r.events(EventKind::FirstEvidence)
    );

    // With no qualifying days, WATCH stays open.
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.hot_build();
    r.goto(dec(25), 9, 0);
    assert_eq!(r.status(), Status::Watch);
}

/// WARNING on day 3 by route (c), then cleaned that afternoon. Needs light-load evidence on
/// (`Rig::duo_e2`).
fn cleaned_after_warning(r: &mut Rig) {
    r.learned_light();
    for d in [1, 2, 3] {
        r.goto(dec(d), 14, 0);
        r.hot_light();
    }
    assert_eq!(r.status(), Status::Warning);
    assert!(r.state().pill.show);
    r.goto(dec(3), 16, 0);
    r.act(|e, now| e.cleaned(now, None));
}

#[test]
fn cleaned_removes_the_pill_at_once() {
    // E2 on: `cleaned_after_warning` warns by route (c).
    let mut r = Rig::duo_e2();
    cleaned_after_warning(&mut r);
    // No tick in between: the pill is gone the moment she says so.
    let s = r.state();
    assert_eq!(s.status, Status::Verifying);
    assert!(!s.pill.show);
    assert_eq!(
        (s.mon.kind.as_str(), s.mon.tone, s.mon.badge.as_str()),
        ("verifying", Tone::Purple, "CLEANED 3/12/2026 · VERIFY COOLING")
    );
    assert_eq!(s.mon.actions, vec!["check"]);
    let v = s.verification.clone().unwrap();
    assert_eq!(v.label, "3/12/2026");
    assert!(!v.date_only);
    let pv = v.passive.unwrap();
    assert_eq!((pv.from.as_str(), pv.deadline.as_str()), ("2026-12-10", "2026-12-17"));
    assert_eq!(v.r_before.unwrap().source, "case");
    assert_eq!(s.known_clean_until.as_deref(), Some("2027-01-02"));
    assert_eq!(s.case.as_ref().unwrap().cleaned.len(), 1);
    assert_eq!(r.events(EventKind::Cleaned).len(), 1);
    assert!(r.effects.contains(&Effect::References));

    // A future date, or one before the first evidence, is refused.
    let now = r.now();
    assert!(r.engine().cleaned(&now, Some(dec(4))).is_err());
    assert!(r.engine().cleaned(&now, Date::new(2026, 11, 30)).is_err());
}

#[test]
fn passive_verification_not_improved_relatches_the_same_case() {
    // E2 on: `cleaned_after_warning` warns by route (c).
    let mut r = Rig::duo_e2();
    cleaned_after_warning(&mut r);
    let case = r.state().case.clone().unwrap();
    for d in 4..=8 {
        r.goto(dec(d), 10, 0);
        r.hot_build();
    }
    r.goto(dec(10), 9, 0);
    assert_eq!(r.status(), Status::Verifying, "no passive verdict before day 7");
    r.goto(dec(11), 9, 0);
    assert_eq!(r.status(), Status::Warning);
    let again = r.state().case.clone().unwrap();
    assert_eq!(again.id, case.id);
    assert_eq!(again.first_evidence, case.first_evidence);
    let s = r.state();
    assert_eq!(
        s.verification.as_ref().unwrap().verdict,
        Some(PostClean::NotImproved)
    );
    assert_eq!(
        (s.pill.kind.as_str(), s.pill.label.as_str()),
        ("dust_not_improved", "Still hot after cleaning 3/12/2026")
    );
    assert_eq!(s.mon.badge, "STILL HOT AFTER CLEANING 3/12/2026");
    assert_eq!(
        r.events(EventKind::NotImproved),
        vec!["Still hot after cleaning 3/12/2026: 5 hot builds since"]
    );
    // "Cleaned again" starts a new verification in the same case.
    r.act(|e, now| e.cleaned(now, None));
    assert_eq!(r.status(), Status::Verifying);
    assert_eq!(r.state().case.as_ref().unwrap().id, case.id);
    assert_eq!(r.state().case.as_ref().unwrap().cleaned.len(), 2);
}

#[test]
fn passive_verification_verified_by_builds() {
    // E2 on: `cleaned_after_warning` warns by route (c).
    let mut r = Rig::duo_e2();
    cleaned_after_warning(&mut r);
    for d in 4..=8 {
        r.goto(dec(d), 10, 0);
        r.clean_build();
    }
    r.goto(dec(11), 9, 0);
    assert_eq!(r.status(), Status::Clean);
    let cases = r.cases();
    assert_eq!(cases.len(), 1);
    assert_eq!(
        cases[0].outcome,
        minka_thermal::model::CaseOutcome::VerifiedByBuilds
    );
    assert_eq!(
        cases[0].first_evidence.as_ref().unwrap().label,
        "1/12/2026 14:00"
    );
    assert_eq!(
        r.events(EventKind::Verification),
        vec!["Verified by builds: 5 clean and no hot builds since the clean of 3/12/2026"]
    );
    assert!(!r.state().pill.show && !r.state().mon.show);
}

#[test]
fn passive_verification_unverified() {
    // E2 on: `cleaned_after_warning` warns by route (c).
    let mut r = Rig::duo_e2();
    cleaned_after_warning(&mut r);
    for d in [4, 5] {
        r.goto(dec(d), 10, 0);
        r.clean_build();
    }
    r.goto(dec(17), 9, 0);
    assert_eq!(r.status(), Status::Verifying, "the deadline day itself has not settled");
    r.goto(dec(18), 9, 0);
    assert_eq!(r.status(), Status::Clean);
    assert_eq!(
        r.cases()[0].outcome,
        minka_thermal::model::CaseOutcome::Unverified
    );
}

#[test]
fn not_dust_accepts_uncapped_until_the_next_clean() {
    // E2 on: `five_evidence_days` warns by route (b).
    let mut r = Rig::duo_e2();
    five_evidence_days(&mut r);
    r.goto(dec(6), 9, 0);
    assert_eq!(r.status(), Status::Warning);
    let first = r.state().case.as_ref().unwrap().first_evidence.clone();
    r.goto(dec(6), 10, 0);
    r.act(|e, now| e.not_dust(now));
    assert_eq!(r.status(), Status::Clean);
    assert!(!r.state().pill.show);
    let cases = r.cases();
    assert_eq!(cases[0].outcome, minka_thermal::model::CaseOutcome::Dismissed);
    assert_eq!(cases[0].first_evidence, first, "the history keeps the first evidence");
    let view = r.watch.engine.view(SEED_KEY);
    let active = view.active.clone().unwrap();
    assert_eq!((active.t, active.duty), (Some(90.5), Some(0.068)));
    assert_eq!(active.label, "accepted 6/12/2026");
    assert!(active.accepted);
    assert_eq!(view.best.unwrap().source, "seed", "R_best is untouched");
    assert_eq!(view.passive.unwrap().t, 90.5, "uncapped");
    assert_eq!(r.state().key.reference.as_deref(), Some("accepted 6/12/2026"));
    assert!(r.events(EventKind::NotDust)[0].contains("your decision"));

    // The same builds are now normal.
    r.goto(dec(7), 10, 0);
    r.hot_build();
    assert_eq!(r.plateaus().last().unwrap().verdict, PlateauVerdict::Clean);
    r.goto(dec(8), 9, 0);
    assert_eq!(r.status(), Status::Clean);

    // The next Cleaned drops the accepted reference: the seed is back.
    r.act(|e, now| e.cleaned(now, None));
    assert_eq!(r.status(), Status::Clean, "a preventive clean leaves the status alone");
    let view = r.watch.engine.view(SEED_KEY);
    assert_eq!(view.active.unwrap().source, "seed");
    assert_eq!(view.passive, None);
    r.goto(dec(8), 10, 0);
    r.hot_build();
    assert_eq!(r.plateaus().last().unwrap().verdict, PlateauVerdict::Hot);
    r.goto(dec(9), 9, 0);
    assert_eq!(r.status(), Status::Watch);
}

#[test]
fn a_clean_with_no_case_is_bounded() {
    // A mistyped year: its window would already be over, so nothing could re-learn after the
    // clean dropped the references. Refused, and nothing changes.
    let mut r = Rig::duo();
    let now = r.now();
    let err = r.engine().cleaned(&now, Date::new(2025, 11, 20)).unwrap_err();
    assert!(err.contains("too long ago"), "{err}");
    let err = r.engine().cleaned(&now, Date::new(2026, 10, 31)).unwrap_err();
    assert!(err.ends_with("ended on 30/11/2026"), "{err}");
    assert!(r.engine().take_effects().is_empty());
    assert_eq!(r.state().known_clean_until.as_deref(), Some("2026-12-20"));
    // Thirty days back still has a window that ends today.
    r.act(|e, now| e.cleaned(now, Date::new(2026, 11, 1)));
    assert_eq!(r.state().known_clean_until.as_deref(), Some("2026-12-01"));

    // Nor may a clean dated before her Not dust decision undo it.
    let mut r = Rig::duo();
    five_evidence_days(&mut r);
    r.goto(dec(6), 10, 0);
    r.act(|e, now| e.not_dust(now));
    r.goto(dec(8), 9, 0);
    let now = r.now();
    let err = r.engine().cleaned(&now, Some(dec(5))).unwrap_err();
    assert!(err.contains("before the Not dust decision of 6/12/2026"), "{err}");
    assert!(r.watch.engine.view(SEED_KEY).active.unwrap().accepted, "her decision stands");
    // The same day may have been either way round.
    r.act(|e, now| e.cleaned(now, Some(dec(6))));
    assert_eq!(r.watch.engine.view(SEED_KEY).active.unwrap().source, "seed");
}

#[test]
fn fan_stopped_alarm() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    // A momentary 0 at idle is the EC pausing the fan.
    for _ in 0..30 {
        r.tick(0.02, 55.0, 0, 0);
    }
    assert_eq!(r.state().fan.status, FanStatus::Ok);
    for _ in 0..19 {
        r.tick(0.5, 88.0, 0, 0);
    }
    assert_eq!(r.state().fan.status, FanStatus::Ok);
    r.tick(0.5, 88.0, 0, 0);
    let s = r.state();
    assert_eq!(s.fan.status, FanStatus::Stopped);
    assert_eq!(s.fan.first_evidence.as_ref().unwrap().class, "alarm");
    assert_eq!(
        (s.pill.kind.as_str(), s.pill.label.as_str(), s.pill.tone),
        ("fan", "Fan stopped", Tone::Amber)
    );
    assert_eq!(s.mon.badge, "FAN STOPPED");
    assert_eq!(r.events(EventKind::Fan), vec!["Fan stopped under load"]);
    assert_eq!(s.status, Status::Clean, "never dust");
    r.act(|e, now| e.fan_fixed(now));
    assert_eq!(r.state().fan.status, FanStatus::Ok);
    assert!(r.state().fan.relearn);
    assert!(!r.state().pill.show);
}

#[test]
fn fan_slow_on_two_days() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    // 89 °C at 3 % is neither hot nor clean; the fan never leaves its 6100 stage.
    r.build_with(89.0, 0.03, 6100);
    let p = r.plateaus();
    assert_eq!(p[0].verdict, PlateauVerdict::Neutral);
    assert!(p[0].flags.iter().any(|f| f == "fan_low"));
    assert!(p[0].flags.iter().any(|f| f == "fan_slow"));
    assert_eq!(r.state().fan.status, FanStatus::Ok, "one day is not enough");
    r.goto(dec(2), 10, 0);
    r.build_with(89.0, 0.03, 6100);
    let s = r.state();
    assert_eq!(s.fan.status, FanStatus::Slow);
    assert_eq!(
        (s.pill.label.as_str(), s.mon.badge.as_str()),
        ("Fan slow · 6100 rpm", "FAN SLOW 6100/6800 RPM")
    );
    assert_eq!(
        r.events(EventKind::Fan),
        vec!["Fan slower than normal (6800 → 6100 rpm): wear, cleaning may not fix it"]
    );
    r.goto(dec(3), 9, 0);
    assert_eq!(r.status(), Status::Clean, "fan evidence never counts as dust");
    r.act(|e, now| e.accept_fan(now));
    assert_eq!(r.state().fan.status, FanStatus::Ok);
    assert_eq!(r.state().fan.r_fan, Some(6100));
    // Accepted: the same build no longer flags the fan.
    r.goto(dec(3), 10, 0);
    r.build_with(89.0, 0.03, 6100);
    let flags = r.plateaus().last().unwrap().flags.clone();
    assert!(!flags.iter().any(|f| f == "fan_slow"), "{flags:?}");
    r.goto(dec(4), 10, 0);
    r.build_with(89.0, 0.03, 6100);
    assert_eq!(r.state().fan.status, FanStatus::Ok);
}

#[test]
fn fault_when_tcc_does_not_act() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    for _ in 0..4 {
        r.tick(0.5, 99.0, 900, 7000);
    }
    assert_eq!(r.state().fault.status, FaultStatus::Ok, "4 s is not 5");
    r.tick(0.5, 99.0, 900, 7000);
    let s = r.state();
    assert_eq!(s.fault.status, FaultStatus::Fault);
    assert_eq!(
        (s.pill.kind.as_str(), s.pill.tone, s.pill.label.as_str()),
        ("fault", Tone::Red, "Cooling fault")
    );
    assert_eq!(s.mon.badge, "TCC NOT ACTING 99 °C");
    assert_eq!(r.events(EventKind::Fault).len(), 1);
    r.act(|e, now| e.ack_fault(now));
    assert_eq!(r.state().fault.status, FaultStatus::Ok);
    assert!(!r.state().pill.show);

    // One reading 5 s after the last (a battery row, or the row after a stall) is one reading,
    // not 5 s at 99 °C; the 5 s are timed across readings.
    r.idle(10);
    let at = r.tl.advance(5.0);
    r.step(at, 0.5, 99.0, 900, 7000);
    assert_eq!(r.state().fault.status, FaultStatus::Ok, "one reading");
    for _ in 0..3 {
        r.tick(0.5, 99.0, 900, 7000);
    }
    assert_eq!(r.state().fault.status, FaultStatus::Ok, "four readings, 4 s");
    r.tick(0.5, 99.0, 900, 7000);
    assert_eq!(r.state().fault.status, FaultStatus::Fault);
}

#[test]
fn suspend_and_key_change_void_without_evidence() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    // A hot build cut by a suspend, then one cut by closing the lid.
    for _ in 0..250 {
        r.tick(1.0, 91.0, 300, 6800);
    }
    r.tl.suspend(1800.0);
    r.idle(60);
    for _ in 0..250 {
        r.tick(1.0, 91.0, 300, 6800);
    }
    r.config.lid_open = Some(false);
    r.config_due = true;
    r.tick(1.0, 91.0, 300, 6800);
    r.idle(30);
    let reasons: Vec<String> = r.unjudged().iter().map(|u| u.reason.clone()).collect();
    // The load went on under the new key: a new episode, too short when the load stopped.
    assert_eq!(reasons, vec!["void:suspend", "void:config", "too_short"]);
    assert!(r.plateaus().is_empty());
    assert_eq!(r.state().unjudged_30d.void, 2);
    assert_eq!(r.status(), Status::Learning, "lid closed: a key with no reference");
    r.config.lid_open = Some(true);
    r.config_due = true;
    r.goto(dec(2), 9, 0);
    assert_eq!(r.status(), Status::Clean);
    assert!(r.state().case.is_none());
}

#[test]
fn the_turbo_transient_and_dip_bursts_are_never_scored() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    // A turbo transient far hotter than the seed's: 93 °C, 60 % throttled for a minute.
    for _ in 0..60 {
        r.tick(1.0, 93.0, 600, 6100);
    }
    for i in 0..150 {
        r.tick(1.0, 85.0 + (i % 3) as f64, 9, 6800);
    }
    // A short dip refills the PL1 budget; the PL2 burst after it is hot and throttled.
    for _ in 0..3 {
        r.tick(0.5, 80.0, 0, 6800);
    }
    for _ in 0..15 {
        r.tick(1.0, 93.0, 400, 6900);
    }
    for i in 0..200 {
        r.tick(1.0, 85.0 + (i % 3) as f64, 9, 6800);
    }
    r.idle(30);
    let p = r.plateaus();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].verdict, PlateauVerdict::Clean);
    assert_eq!(p[0].t_ss, Some(86.0));
    assert!((p[0].duty.unwrap() - 0.009).abs() < 1e-9);
    assert!(p[0].early.duty_early.unwrap() > 0.4, "the transient is recorded, not scored");
    r.goto(dec(2), 9, 0);
    assert_eq!(r.status(), Status::Clean);
}

#[test]
fn evidence_only_under_a_referenced_key() {
    // Balanced EPP: not the seed's key, nothing learned yet.
    let mut cfg = her_config();
    cfg.epp = Some("balance_performance".into());
    let mut r = Rig::with(Chassis::Duo, cfg);
    assert_eq!(r.status(), Status::Learning);
    assert!(!r.state().key.referenced);
    r.goto(dec(1), 10, 0);
    r.hot_build();
    let p = r.plateaus();
    assert_eq!(p[0].verdict, PlateauVerdict::Neutral);
    assert!(p[0].flags.iter().any(|f| f == "no_reference"));
    r.goto(dec(2), 9, 0);
    assert_eq!(r.status(), Status::Learning);
    assert!(r.state().case.is_none());
    assert_eq!(r.state().unjudged_30d.no_reference, 1);
    assert_eq!(r.state().learning.voting, 1, "still stored for learning");

    // Saturated at full fan is hot with no reference at all.
    r.goto(dec(2), 10, 0);
    r.build_with(92.0, 0.35, 6900);
    assert_eq!(r.plateaus().last().unwrap().verdict, PlateauVerdict::Hot);
    r.goto(dec(3), 9, 0);
    assert_eq!(r.status(), Status::Watch);
    assert_eq!(
        r.state().case.as_ref().unwrap().first_evidence.as_ref().unwrap().summary,
        "build 92 °C, throttled 35.0 % with the fan at 6900 rpm (no reference yet), mostly rustc"
    );

    // Generic chassis: the seed never applies.
    let mut r = Rig::with(Chassis::Generic, her_config());
    assert_eq!(r.status(), Status::Learning);
    r.goto(dec(1), 10, 0);
    r.hot_build();
    r.goto(dec(2), 9, 0);
    assert_eq!(r.status(), Status::Learning);
}

#[test]
fn missed_days_settle_at_start_from_records() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    let onset = r.hot_build();
    // The service stops that evening, before day 1 settles, and starts again on day 3.
    let state: State =
        serde_json::from_str(&serde_json::to_string(r.state()).unwrap()).unwrap();
    let refs: References =
        serde_json::from_str(&serde_json::to_string(&r.watch.engine.refs).unwrap()).unwrap();
    let records = r.records();
    assert!(r.daily(dec(1)).is_none());
    let now = Timeline::starting(dec(3), 9, 0, 0, 0).now();
    let mut engine = Engine::new(machine(Chassis::Duo), Some(state), refs, &now);
    engine.restore(&[], &records, &[], &[], &now);
    let mut r = Rig::resume(engine, dec(3), 9);
    r.idle(5);
    let day1 = r.daily(dec(1)).expect("day 1 settled at start");
    assert!(day1.e1 && day1.voting == 1);
    assert!(r.daily(dec(2)).is_some(), "the empty day in between too");
    assert_eq!(r.status(), Status::Watch);
    let first = r.state().case.as_ref().unwrap().first_evidence.clone().unwrap();
    assert_eq!(first.at, onset);
    assert_eq!(
        first.summary,
        "build 90.5 °C, throttled 6.8 % (clean 86 °C, 0.9 %), mostly rustc"
    );
}

#[test]
fn drift_per_week() {
    let mut r = Rig::duo();
    for d in 1..=4 {
        r.goto(dec(d), 10, 0);
        r.build_with(86.0 + 0.5 * (d - 1) as f64, 0.009, 6800);
    }
    let drift = r.state().drift.clone().expect("three days or more");
    assert_eq!(drift.days, 30);
    assert!((drift.t_c_per_week - 3.5).abs() < 1e-9, "{drift:?}");
    assert!(drift.duty_pp_per_week.abs() < 1e-9);
}

#[test]
fn notices_are_never_evidence() {
    // PL1 15 W on the Duo: power capped, a key with no reference.
    let mut cfg = her_config();
    cfg.pl1_uw = Some(15_000_000);
    let mut r = Rig::with(Chassis::Duo, cfg);
    r.goto(dec(1), 10, 0);
    r.hot_build();
    let n = &r.state().notices;
    assert_eq!(n.len(), 1, "{n:?}");
    assert_eq!(n[0].id, "power_capped");
    assert_eq!(n[0].text, "power limit looks capped (PL1 15 W): not dust");
    assert_eq!(r.events(EventKind::Notice), vec![n[0].text.clone()]);
    // Raised again the same day: shown, not logged again. The next day it is logged once more.
    r.goto(dec(1), 14, 0);
    r.hot_build();
    assert_eq!(r.events(EventKind::Notice).len(), 1);
    // A restart the same day remembers that it was logged.
    let mut r = r.restart(dec(1), 18);
    r.idle(5);
    r.hot_build();
    assert_eq!(r.events(EventKind::Notice).len(), 1);
    r.goto(dec(2), 10, 0);
    r.hot_build();
    assert_eq!(r.events(EventKind::Notice).len(), 2);
    r.goto(dec(3), 9, 0);
    assert!(r.state().case.is_none());

    // Three weeks with nothing comparable: the starvation notice, logged once a day at most.
    let mut r = Rig::duo();
    r.goto(dec(21), 9, 0);
    assert!(r.state().notices.is_empty(), "{:?}", r.state().notices);
    // Days 1-21 settle at the first tick of day 22.
    r.goto(dec(22), 9, 0);
    assert_eq!(
        r.state().notices.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        vec!["starvation"]
    );
    r.goto(dec(22), 12, 0);
    r.goto(dec(23), 9, 0);
    assert_eq!(r.events(EventKind::Notice).len(), 2, "one a day");
    r.goto(dec(23), 10, 0);
    r.clean_build();
    r.goto(dec(24), 9, 0);
    assert!(r.state().notices.is_empty());
}

#[test]
fn firmware_and_settings_changes_are_logged() {
    let now = Timeline::starting(dec(1), 9, 0, 0, 0).now();
    let mut old = State::new();
    old.chassis.bios = "UX482EG.310".into();
    let mut e = Engine::new(machine(Chassis::Duo), Some(old), References::default(), &now);
    let effects = e.take_effects();
    assert!(effects.iter().any(|x| matches!(x,
        Effect::Event { kind: EventKind::BiosChange, text, .. } if text == "Firmware changed: UX482EG.310 → UX482EG.311")));
    assert_eq!(
        e.state.notices[0].text,
        "firmware changed (310 → 311): if builds now read differently, run a check; Not dust adopts the new behaviour"
    );
    // A comparable check has measured the new firmware: the notice has done its job.
    e.check_result(
        &now,
        CheckOutcome {
            at: now.plus(-300.0),
            dir: "checks/2026-12-01".into(),
            purpose: CheckPurpose::Reference,
            verdict: CheckVerdict::Like,
            post_clean: None,
            comparable: true,
            key_id: SEED_KEY.into(),
            metrics: CheckMetrics {
                t_ss: Some(86.2),
                duty: Some(0.01),
                fan: Some(6800.0),
                ..CheckMetrics::default()
            },
            matched: None,
            generator: Some(1),
            cpu: "11th Gen Intel(R) Core(TM) i7-1165G7".into(),
            fan_flag: false,
            diagnosis: Vec::new(),
        },
    );
    assert!(e.state.notices.is_empty(), "{:?}", e.state.notices);

    // A profile change is logged once it has lasted 10 minutes; a short one is not.
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.config.platform_profile = Some("balanced".into());
    r.config_due = true;
    r.idle(300);
    r.config.platform_profile = Some("performance".into());
    r.config_due = true;
    r.idle(60);
    assert!(r.events(EventKind::ConfigChange).is_empty());
    r.config.platform_profile = Some("balanced".into());
    r.config_due = true;
    r.idle(601);
    assert_eq!(
        r.events(EventKind::ConfigChange),
        vec![
            "Settings changed: AC · performance · PL1 28 W · PL2 35 W · TCC 92 °C · lid open → \
             AC · balanced · EPP performance · PL1 28 W · PL2 35 W · TCC 92 °C · lid open"
        ]
    );
}

#[test]
fn verify_check_outcomes() {
    // Restored: the case closes as verified clean, and R_active follows the check.
    // E2 on: `cleaned_after_warning` warns by route (c).
    let mut r = Rig::duo_e2();
    cleaned_after_warning(&mut r);
    assert!(
        r.events(EventKind::Cleaned)[0].starts_with("Cleaned 3/12/2026: the warning is cleared."),
        "{:?}",
        r.events(EventKind::Cleaned)
    );
    r.goto(dec(3), 18, 0);
    r.check(CheckPurpose::Verify, CheckVerdict::Like, Some(PostClean::Restored));
    assert_eq!(r.status(), Status::Clean);
    assert_eq!(r.cases()[0].outcome, CaseOutcome::VerifiedClean);
    assert_eq!(r.events(EventKind::Verification), vec!["Verified clean 3/12/2026"]);
    let view = r.watch.engine.view(SEED_KEY);
    assert_eq!(view.active.as_ref().unwrap().label, "restored 3/12/2026");
    assert_eq!(
        view.best.as_ref().unwrap().source,
        "seed",
        "86.2 °C is not better than the seed"
    );
    let s = r.state();
    assert!(!s.pill.show);
    assert_eq!(
        (s.mon.badge.as_str(), s.mon.tone),
        ("CHECK 3/12/2026: RESTORED", Tone::Green)
    );

    // Improved: closed with how far it still is from the clean reference; R_active unchanged.
    // E2 on: `cleaned_after_warning` warns by route (c).
    let mut r = Rig::duo_e2();
    cleaned_after_warning(&mut r);
    r.goto(dec(4), 10, 0);
    let mut o = r.outcome(CheckPurpose::Verify, CheckVerdict::Worse, Some(PostClean::Improved));
    o.metrics.t_ss = Some(89.0);
    o.metrics.duty = Some(0.035);
    r.run_check(o);
    assert_eq!(r.status(), Status::Clean);
    assert_eq!(r.cases()[0].outcome, CaseOutcome::Improved);
    assert_eq!(
        r.events(EventKind::Verification),
        vec![
            "Improved, 3 °C above the clean of 20/11/2026: dust left in the fins, or paste or heatsink seating"
        ]
    );
    assert_eq!(r.watch.engine.view(SEED_KEY).active.unwrap().source, "seed");

    // Not improved: WARNING again in the same case, worded by what the check measured.
    // E2 on: `cleaned_after_warning` warns by route (c).
    let mut r = Rig::duo_e2();
    cleaned_after_warning(&mut r);
    let id = r.state().case.as_ref().unwrap().id.clone();
    r.goto(dec(4), 10, 0);
    r.check(CheckPurpose::Verify, CheckVerdict::Worse, Some(PostClean::NotImproved));
    assert_eq!(r.status(), Status::Warning);
    let s = r.state();
    assert_eq!(s.case.as_ref().unwrap().id, id);
    assert_eq!(
        s.verification.as_ref().unwrap().verdict,
        Some(PostClean::NotImproved)
    );
    assert_eq!(
        (s.pill.kind.as_str(), s.pill.label.as_str()),
        ("dust_not_improved", "Still hot after cleaning 3/12/2026")
    );
    assert_eq!(s.known_clean_until, None, "the clean did not work");
    assert_eq!(
        r.events(EventKind::NotImproved),
        vec![
            "Still hot after cleaning 3/12/2026: cooling check 90.5 °C, throttled 7.0 % (clean 86 °C, 0.9 %)"
        ]
    );

    // Inconclusive: still verifying, no pill, nothing closed.
    // E2 on: `cleaned_after_warning` warns by route (c).
    let mut r = Rig::duo_e2();
    cleaned_after_warning(&mut r);
    r.goto(dec(4), 10, 0);
    r.check(CheckPurpose::Verify, CheckVerdict::Inconclusive, None);
    assert_eq!(r.status(), Status::Verifying);
    assert!(!r.state().pill.show);
    assert!(r.cases().is_empty());
}

#[test]
fn a_restored_check_outweighs_builds_earlier_that_day() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.hot_build();
    r.goto(dec(2), 9, 0);
    assert_eq!(r.status(), Status::Watch);
    r.goto(dec(2), 10, 0);
    r.act(|e, now| e.cleaned(now, None));
    assert!(
        r.events(EventKind::Cleaned)[0]
            .starts_with("Cleaned 2/12/2026: the case is now being verified."),
        "no warning was shown in WATCH: {:?}",
        r.events(EventKind::Cleaned)
    );
    // Heat soak after the clean: hot in the seed phase.
    r.goto(dec(2), 12, 0);
    r.hot_build();
    r.goto(dec(2), 15, 0);
    r.check(CheckPurpose::Verify, CheckVerdict::Like, Some(PostClean::Restored));
    assert_eq!(r.status(), Status::Clean);
    r.goto(dec(3), 9, 0);
    assert!(r.state().case.is_none(), "the 12:00 build opens no new case");
    assert_eq!(r.events(EventKind::FirstEvidence).len(), 1);
}

#[test]
fn a_past_clean_date_counts_from_the_next_day() {
    // E2 on: route (c), and an E2 day after the clean date.
    let mut r = Rig::duo_e2();
    r.learned_light();
    for d in [1, 2, 3] {
        r.goto(dec(d), 14, 0);
        r.hot_light();
    }
    assert_eq!(r.status(), Status::Warning);
    // The morning of 5/12: three dusty builds. She cleans that afternoon.
    for h in [8, 9, 10] {
        r.goto(dec(5), h, 0);
        r.hot_build();
    }
    // 7/12: light-load throttling (an E2 day), then she reports the clean of 5/12.
    r.goto(dec(7), 10, 0);
    r.hot_light();
    r.goto(dec(7), 14, 0);
    r.act(|e, now| e.cleaned(now, Some(dec(5))));
    assert_eq!(r.status(), Status::Verifying);
    let s = r.state();
    let v = s.verification.as_ref().unwrap();
    assert!(v.date_only);
    let pv = v.passive.as_ref().unwrap();
    assert_eq!(
        (pv.voting, pv.hot),
        (0, 0),
        "the builds of the clean day came before the clean"
    );
    assert_eq!(pv.e2_days, 1, "today came wholly after it and keeps its E2 day");
    assert_eq!(s.evidence_from.as_deref(), Some(dec(6).key().as_str()));
}

#[test]
fn a_before_check_in_warning() {
    // Like the reference: the notice, then R_before at the clean, which clears the notice.
    // E2 on: `five_evidence_days` warns by route (b).
    let mut r = Rig::duo_e2();
    five_evidence_days(&mut r);
    r.goto(dec(6), 9, 0);
    assert_eq!(r.status(), Status::Warning);
    r.check(CheckPurpose::Before, CheckVerdict::Like, None);
    assert_eq!(r.status(), Status::Warning, "it only becomes R_before");
    assert_eq!(r.notice_ids(), vec!["like_in_warning"]);
    r.goto(dec(6), 12, 0);
    r.act(|e, now| e.cleaned(now, None));
    assert!(r.notice_ids().is_empty(), "{:?}", r.notice_ids());
    let rb = r.state().verification.as_ref().unwrap().r_before.clone().unwrap();
    assert_eq!((rb.source.as_str(), rb.t_ss), ("check", Some(86.2)));

    // Run anyway under another key: not comparable, so neither the notice nor R_before.
    // E2 on: `five_evidence_days` warns by route (b).
    let mut r = Rig::duo_e2();
    five_evidence_days(&mut r);
    r.goto(dec(6), 9, 0);
    let mut o = r.outcome(CheckPurpose::Before, CheckVerdict::Like, None);
    o.comparable = false;
    o.key_id = "ac|balanced|performance|28|35|8|0|100|open".into();
    r.run_check(o);
    assert!(r.notice_ids().is_empty());
    r.goto(dec(6), 12, 0);
    r.act(|e, now| e.cleaned(now, None));
    let rb = r.state().verification.as_ref().unwrap().r_before.clone().unwrap();
    assert_eq!((rb.source.as_str(), rb.t_ss), ("plateaus_7d", Some(90.5)));
}

#[test]
fn two_reference_checks_in_the_known_clean_window() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    // Too hot to replace the seed: kept, with a notice.
    let mut o = r.outcome(CheckPurpose::Reference, CheckVerdict::Borderline, None);
    o.metrics.t_ss = Some(89.5);
    r.run_check(o);
    assert_eq!(r.notice_ids(), vec!["reference_hot"]);
    assert_eq!(r.state().key.reference.as_deref(), Some("duo-seed 20/11/2026"));
    r.goto(dec(1), 14, 0);
    r.check(CheckPurpose::Reference, CheckVerdict::Like, None);
    assert!(r.notice_ids().is_empty(), "an own reference within bounds clears it");
    assert_eq!(r.state().key.reference.as_deref(), Some("check 1/12/2026"));
    // The second, on another day: R_T their mean, R_duty and R_fan the worse.
    r.goto(dec(2), 10, 0);
    let mut o = r.outcome(CheckPurpose::Reference, CheckVerdict::Like, None);
    o.metrics.t_ss = Some(85.8);
    o.metrics.fan = Some(6700.0);
    r.run_check(o);
    let view = r.watch.engine.view(SEED_KEY);
    let a = view.active.unwrap();
    assert_eq!(a.label, "checks 1/12/2026 + 2/12/2026");
    assert!((a.t.unwrap() - 86.0).abs() < 1e-9, "{:?}", a.t);
    assert_eq!((a.duty, a.fan), (Some(0.01), Some(6700.0)));
    assert_eq!(view.best.unwrap().label, "check 2/12/2026", "each check competes for R_best");
    assert_eq!(r.events(EventKind::Reference).len(), 2);
}

#[test]
fn not_dust_adopts_for_the_key_of_the_case() {
    // E2 on: `five_evidence_days` warns by route (b).
    let mut r = Rig::duo_e2();
    five_evidence_days(&mut r);
    r.goto(dec(6), 9, 0);
    assert_eq!(r.status(), Status::Warning);
    // A run-anyway check is no reference to adopt.
    let mut o = r.outcome(CheckPurpose::Before, CheckVerdict::Like, None);
    o.comparable = false;
    r.run_check(o);
    // She is on the balanced profile when she clicks.
    r.config.platform_profile = Some("balanced".into());
    r.config_due = true;
    r.idle(5);
    let key = r.state().key.id.clone();
    assert_ne!(key, SEED_KEY);
    r.act(|e, now| e.not_dust(now));
    let seed = r.watch.engine.view(SEED_KEY).active.unwrap();
    assert_eq!(
        (seed.label.as_str(), seed.t),
        ("accepted 6/12/2026", Some(90.5)),
        "the medians of her builds under the case's key"
    );
    assert!(r.watch.engine.view(&key).active.is_none());
}

#[test]
fn not_dust_holds_across_a_restart_the_same_day() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.hot_build();
    r.goto(dec(2), 9, 0);
    assert_eq!(r.status(), Status::Watch);
    r.goto(dec(2), 10, 0);
    r.hot_build();
    r.goto(dec(2), 15, 0);
    r.act(|e, now| e.not_dust(now));
    assert_eq!(r.status(), Status::Clean);
    // Shut down that evening, started the next morning.
    let mut r = r.restart(dec(3), 8);
    r.idle(60);
    assert_eq!(r.status(), Status::Clean, "the dismissed build of 2/12 stays dismissed");
    assert_eq!(r.events(EventKind::FirstEvidence).len(), 1);
    let day2 = r.daily(dec(2)).unwrap();
    assert_eq!((day2.voting, day2.hot, day2.e1), (0, 0, false));
}

#[test]
fn a_restart_restores_the_day_and_the_history() {
    // The same days twice: straight through, and with the service restarted twice.
    let run = |restarts: bool| -> Rig {
        // E2 on: the light-load day is what the restart must restore.
        let mut r = Rig::duo_e2();
        r.learned_light();
        r.goto(dec(1), 10, 0);
        r.hot_build();
        for d in [3, 7, 11] {
            r.goto(dec(d), 10, 0);
            r.clean_build();
        }
        r.goto(dec(16), 9, 0);
        assert_eq!(r.cases()[0].outcome, CaseOutcome::Lapsed);
        if restarts {
            r = r.restart(dec(18), 9);
            r.idle(5);
        }
        r.goto(dec(20), 10, 0);
        r.clean_build();
        r.goto(dec(20), 11, 0);
        r.hot_light();
        if restarts {
            r = r.restart(dec(20), 13);
            r.idle(5);
        }
        r.goto(dec(21), 9, 0);
        r
    };
    let (a, b) = (run(false), run(true));
    for r in [&a, &b] {
        let case = r.state().case.clone().expect("the E2 day opened a case");
        assert_eq!(case.earlier_sighting.unwrap().label, "1/12/2026");
    }
    let (da, db) = (a.daily(dec(20)).unwrap(), b.daily(dec(20)).unwrap());
    let sum = |d: &DailyRecord| {
        (
            d.voting,
            d.clean,
            d.hot,
            d.light_q,
            d.light_hot,
            d.qualifying,
            d.e1,
            d.e2,
            d.light_keys.clone(),
        )
    };
    assert_eq!(sum(&db), sum(&da));
    assert_eq!((da.light_q, da.light_hot), (13, 13));
}

#[test]
fn records_near_midnight_count_for_their_own_day() {
    // Summer time (UTC+1), ticking straight through two midnights.
    // E2 on: the 23:59 window must make 2/12 an E2 day.
    let mut r = Rig::on(
        machine_e2(Chassis::Duo),
        her_config(),
        Timeline::starting(dec(1), 8, 0, 0, 3600),
    );
    r.learned_light();
    // A build whose plateau ends at 23:59:56 and closes a few seconds into 2/12.
    r.goto(dec(1), 23, 54);
    r.idle(54);
    r.hot_build();
    r.idle(120);
    // Light load from 23:50 to 00:03: the 23:59 window is 2/12's tenth hot one.
    r.goto(dec(2), 23, 50);
    r.hot_light();
    r.goto(dec(3), 9, 0);
    let d1 = r.daily(dec(1)).unwrap();
    assert_eq!((d1.voting, d1.hot, d1.e1), (1, 1, true));
    let d2 = r.daily(dec(2)).unwrap();
    assert_eq!((d2.voting, d2.light_hot, d2.e2), (0, 10, true));
    let case = r.state().case.clone().unwrap();
    assert_eq!(case.first_evidence.unwrap().label, "1/12/2026 23:54");
    assert_eq!(case.evidence_days, vec![dec(1).key(), dec(2).key()]);
    assert_eq!(case.e2_days, vec![dec(2).key()]);
    // Replayed under the same rules, the records and the days agree.
    let (state, refs, daily, records, _, _) = r.store();
    let text = minka_thermal::cli::replay_report(
        Some(&state),
        (Chassis::Duo, "UX482EG"),
        &refs,
        &records,
        &daily,
        None,
        r.watch.engine.machine.light_load_evidence,
    );
    assert!(text.ends_with("no verdict would change\n"), "{text}");
}

/// A clean generic build: 4100 MHz for the first 10 s, then 3899 at PL1 with 0.5 % throttle.
fn generic_build(r: &mut Rig) {
    r.freq = 4100;
    for i in 0..300u32 {
        if i == 10 {
            r.freq = 3899;
        }
        if i < 56 {
            r.tick(1.0, 91.0, 380, 6100);
        } else {
            r.tick(1.0, 85.0 + (i % 3) as f64, 5, 6800);
        }
    }
    r.idle(30);
}

#[test]
fn generic_mode_learns_f_allcore_max_from_unjudged_builds() {
    let mut r = Rig::with(Chassis::Generic, her_config());
    assert_eq!(r.watch.engine.f_allcore_max(), None);
    // Under 1 % throttle and no xe PL1: with no all-core maximum known, below_pl1.
    for d in 1..=3 {
        r.goto(dec(d), 10, 0);
        generic_build(&mut r);
    }
    let reasons: Vec<String> = r.unjudged().iter().map(|u| u.reason.clone()).collect();
    assert_eq!(reasons, vec!["below_pl1".to_owned(); 3]);
    assert_eq!(r.watch.engine.f_allcore_max(), Some(4100));
    r.goto(dec(4), 10, 0);
    generic_build(&mut r);
    let p = r.plateaus();
    assert_eq!(p.len(), 1, "valid now that the all-core maximum is known");
    assert!(p[0].flags.iter().any(|f| f == "no_reference"));
    assert_eq!(r.state().learning.voting, 1, "and R_passive starts learning");
}

#[test]
fn fan_slow_before_a_fan_action_no_longer_counts() {
    let mut r = Rig::duo();
    for d in [1, 2] {
        r.goto(dec(d), 10, 0);
        r.build_with(89.0, 0.03, 6100);
    }
    assert_eq!(r.state().fan.status, FanStatus::Slow);
    r.goto(dec(3), 9, 0);
    r.act(|e, now| e.fan_fixed(now));
    // A cool room: the EC holds the new fan on its 6100 stage for one build.
    r.goto(dec(3), 10, 0);
    r.build_with(89.0, 0.03, 6100);
    assert_eq!(r.state().fan.status, FanStatus::Ok, "one build after the fix is no pattern");
    r.goto(dec(4), 10, 0);
    r.build_with(89.0, 0.03, 6100);
    assert_eq!(r.state().fan.status, FanStatus::Slow, "two days after it are");
}

#[test]
fn fan_fixed_relearns_r_fan_from_the_next_check() {
    let mut r = Rig::duo();
    // R_passive learned from eight clean builds on three days, fan 6800.
    for (d, h) in [(1, 10), (1, 12), (1, 14), (2, 10), (2, 12), (2, 14), (3, 10), (3, 12)] {
        r.goto(dec(d), h, 0);
        r.clean_build();
    }
    assert_eq!(
        r.watch.engine.view(SEED_KEY).passive.unwrap().fan,
        Some(6800.0)
    );
    // The fan slows: neutral builds on its 6100 stage on two days.
    for d in [4, 5] {
        r.goto(dec(d), 10, 0);
        r.build_with(88.5, 0.025, 6100);
    }
    assert_eq!(r.state().fan.status, FanStatus::Slow);
    // A replacement that settles lower. Fan fixed, then a check measures it at 6300 rpm; the
    // check phase flags it as slow against the old fan, which must not raise FAN again.
    r.goto(dec(6), 9, 0);
    r.act(|e, now| e.fan_fixed(now));
    r.goto(dec(6), 10, 0);
    let mut o = r.outcome(CheckPurpose::Reference, CheckVerdict::Like, None);
    o.metrics.fan = Some(6300.0);
    o.fan_flag = true;
    r.run_check(o);
    let fan = r.state().fan.clone();
    assert_eq!(
        (fan.status, fan.r_fan, fan.relearn),
        (FanStatus::Ok, Some(6300), false)
    );
    // Builds at the new fan's speed are normal now, through refreshes and day settles.
    for d in [7, 8] {
        r.goto(dec(d), 10, 0);
        r.build_with(88.5, 0.025, 6300);
        let flags = r.plateaus().last().unwrap().flags.clone();
        assert!(!flags.iter().any(|f| f == "fan_slow"), "{flags:?}");
    }
    r.goto(dec(9), 9, 0);
    assert_eq!(r.state().fan.status, FanStatus::Ok);
    assert_eq!(r.state().fan.r_fan, Some(6300));
}

#[test]
fn references_relearned_after_a_clean_wait_for_its_verdict() {
    for clean in [true, false] {
        // E2 on: `cleaned_after_warning` warns by route (c).
        let mut r = Rig::duo_e2();
        cleaned_after_warning(&mut r);
        // Eight builds on three days after the clean: R_passive learns from them.
        for (d, h) in [(4, 10), (4, 12), (4, 14), (5, 10), (5, 12), (5, 14), (6, 10), (6, 12)] {
            r.goto(dec(d), h, 0);
            if clean {
                r.clean_build();
            } else {
                // 88.5 °C at 10 %: hot, yet under the suspect line, so it would be learned.
                r.build_with(88.5, 0.10, 6800);
            }
        }
        let held = r.watch.engine.refs.keys[SEED_KEY].passive.clone().unwrap().held;
        assert!(held.is_some(), "learned, and held");
        assert_eq!(
            r.watch.engine.view(SEED_KEY).passive,
            None,
            "not used while the clean is being verified"
        );
        assert!(
            r.events(EventKind::Reference)
                .iter()
                .any(|t| t.ends_with("(in use once the clean is verified)")),
            "{:?}",
            r.events(EventKind::Reference)
        );
        r.goto(dec(11), 9, 0);
        let view = r.watch.engine.view(SEED_KEY);
        if clean {
            assert_eq!(r.cases()[0].outcome, CaseOutcome::VerifiedByBuilds);
            assert_eq!(view.passive.unwrap().t, 86.0);
        } else {
            assert_eq!(r.status(), Status::Warning);
            assert_eq!(view.passive, None, "a failed clean teaches nothing");
            let p = r.watch.engine.refs.keys[SEED_KEY].passive.clone().unwrap();
            assert!(p.held.is_none());
            assert_eq!(r.state().known_clean_until, None);
        }
    }
}

#[test]
fn an_uncapped_relearn_clears_the_light_cap_notice() {
    // E2 on: the light reference is learned only then.
    let mut r = Rig::duo_e2();
    {
        let e = r.engine();
        e.refs.keys.entry(SEED_KEY.into()).or_default().light = Some(LightRef {
            n: 299,
            p90: Some(0.02),
            capped: true,
            relearning: true,
            ..LightRef::default()
        });
        e.state.notices.push(Notice {
            id: "light_cap".into(),
            text: "light reference capped".into(),
            ..Notice::default()
        });
    }
    r.goto(dec(1), 14, 0);
    r.light(2, 70.0, 0.0);
    assert!(r.notice_ids().is_empty(), "{:?}", r.notice_ids());
    let light = r.watch.engine.refs.keys[SEED_KEY].light.clone().unwrap();
    assert_eq!(
        (light.p90, light.capped, light.relearning),
        (Some(0.002), false, false)
    );
}

/// A build whose fan stays on 6100 for 130 s, then runs at its normal 6800.
fn ramp_build(r: &mut Rig) {
    for i in 0..300u32 {
        let fan = if i < 130 { 6100 } else { 6800 };
        if i < 56 {
            r.tick(1.0, 91.0, 380, fan);
        } else {
            r.tick(1.0, 88.0 + (i % 3) as f64, 30, fan);
        }
    }
    r.idle(30);
}

#[test]
fn a_fan_slow_to_ramp_is_worded_by_its_speed_at_two_minutes() {
    let mut r = Rig::duo();
    // The EC keeps the fan on 6100 for 130 s, then it runs at its normal 6800: the settled
    // median is normal, the speed at the settle point is not.
    for d in [1, 2] {
        r.goto(dec(d), 10, 0);
        ramp_build(&mut r);
    }
    for p in r.plateaus() {
        assert_eq!((p.verdict, p.fan_med), (PlateauVerdict::Neutral, Some(6800.0)));
        assert!(p.flags.iter().any(|f| f == "fan_low") && p.flags.iter().any(|f| f == "fan_slow"));
    }
    let s = r.state();
    assert_eq!(s.fan.status, FanStatus::Slow);
    assert_eq!(
        r.events(EventKind::Fan),
        vec![
            "Fan slow to speed up (6100 rpm two minutes into a build, normal 6800 rpm): wear, cleaning may not fix it"
        ]
    );
    assert_eq!(
        (s.pill.label.as_str(), s.mon.badge.as_str()),
        ("Fan slow · 6100 rpm", "FAN SLOW 6100/6800 RPM")
    );

    // On a profile with no reference yet, the badge keeps the speed FAN was raised against.
    r.config.epp = Some("balance_performance".into());
    r.config_due = true;
    r.idle(5);
    assert_ne!(r.state().key.id, SEED_KEY);
    assert_eq!(r.state().mon.badge, "FAN SLOW 6100/6800 RPM");
    r.config = her_config();
    r.config_due = true;
    r.idle(5);

    // Accepting it accepts the late ramp, not 6100 rpm as the fan's normal speed.
    r.goto(dec(3), 9, 0);
    r.act(|e, now| e.accept_fan(now));
    let fan = r.state().fan.clone();
    assert_eq!(
        (fan.status, fan.r_fan, fan.accepted, fan.ramp_accepted),
        (FanStatus::Ok, Some(6800), false, true)
    );
    assert_eq!(
        r.events(EventKind::FanAccepted),
        vec!["Fan accepted as normal: slow to speed up, normal speed 6800 rpm (your decision)"]
    );
    // The same ramp no longer flags a build...
    for d in [3, 4] {
        r.goto(dec(d), 10, 0);
        ramp_build(&mut r);
        let flags = r.plateaus().last().unwrap().flags.clone();
        assert!(!flags.iter().any(|f| f == "fan_slow"), "{flags:?}");
    }
    assert_eq!(r.state().fan.status, FanStatus::Ok);
    // ...but a fan that later slows at full speed still does, against its normal 6800.
    for d in [5, 6] {
        r.goto(dec(d), 10, 0);
        r.build_with(89.0, 0.03, 6300);
    }
    assert_eq!(r.state().fan.status, FanStatus::Slow);
    assert_eq!(
        r.events(EventKind::Fan).last().unwrap(),
        "Fan slower than normal (6800 → 6300 rpm): wear, cleaning may not fix it"
    );
}

#[test]
fn a_check_against_the_seed_reads_its_matched_window() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    // Like the seed over 75-118 s; the steady window soaked on to 89.5 °C.
    let mut o = r.outcome(CheckPurpose::Reference, CheckVerdict::Like, None);
    o.metrics.t_ss = Some(89.5);
    o.matched = Some(CheckMetrics {
        t_ss: Some(86.5),
        duty: Some(0.01),
        fan: Some(6800.0),
        ..CheckMetrics::default()
    });
    r.run_check(o);
    let last = r.state().last_check.clone().unwrap();
    assert_eq!((last.label.as_str(), last.t_ss), ("Like the reference", Some(86.5)));
    let line = r.events(EventKind::CheckResult).pop().unwrap();
    assert!(line.contains("check 86.5 °C") && line.contains("clean 86 °C"), "{line}");
    assert!(r.notice_ids().is_empty(), "within the seed's bound over the seed's window");
    let a = r.watch.engine.view(SEED_KEY).active.unwrap();
    assert_eq!(
        (a.label.as_str(), a.t),
        ("check 1/12/2026", Some(89.5)),
        "stored with the steady window, which later checks compare with"
    );
}

#[test]
fn recorded_only_when_a_first_reference_is_recorded() {
    let mut r = Rig::with(Chassis::Generic, her_config());
    r.goto(dec(1), 10, 0);
    // A check that adopts nothing is not "Recorded", even with nothing to compare against.
    r.check(CheckPurpose::Confirm, CheckVerdict::Borderline, None);
    assert_eq!(r.state().last_check.as_ref().unwrap().label, "Borderline");
    assert!(r.watch.engine.view(SEED_KEY).active.is_none());

    // A thin machine that throttles 6 % at PL1: its first reference is still recorded.
    let mut o = r.outcome(CheckPurpose::Reference, CheckVerdict::Borderline, None);
    o.metrics.t_ss = Some(80.0);
    o.metrics.duty = Some(0.06);
    r.run_check(o);
    assert_eq!(r.state().last_check.as_ref().unwrap().label, "Recorded");
    let line = r.events(EventKind::CheckResult).pop().unwrap();
    assert!(line.starts_with("Cooling check: Recorded · "), "{line}");
    assert_eq!(r.watch.engine.view(SEED_KEY).active.unwrap().label, "check 1/12/2026");

    // Hotter than that by more than 3 °C: kept, and the notice names its own date, not the seed's.
    r.goto(dec(2), 10, 0);
    let mut o = r.outcome(CheckPurpose::Reference, CheckVerdict::Borderline, None);
    o.metrics.t_ss = Some(84.0);
    o.metrics.duty = Some(0.05);
    r.run_check(o);
    assert_eq!(r.state().last_check.as_ref().unwrap().label, "Borderline");
    assert_eq!(r.notice_ids(), vec!["reference_hot"]);
    assert_eq!(
        r.state().notices[0].text,
        "reference ran hotter than the 1/12/2026 clean: check the profile, PL and vents"
    );
}

#[test]
fn on_battery_the_state_keeps_her_ac_key() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    let (key, learning) = (r.state().key.clone(), r.state().learning.clone());
    assert_eq!((r.status(), key.id.as_str(), key.on_battery), (Status::Clean, SEED_KEY, false));
    // Unplugged: nothing is judged on battery, and nothing reads as if it had been reset.
    r.ac = false;
    r.idle(10);
    let s = r.state();
    assert_eq!(s.status, Status::Clean, "not LEARNING");
    assert_eq!((s.key.id.as_str(), s.key.referenced, s.key.on_battery), (SEED_KEY, true, true));
    assert_eq!(s.key.reference, key.reference);
    assert_eq!(s.learning, learning);
    // Started again, still on battery.
    let mut r = r.restart(dec(1), 12);
    r.idle(10);
    assert_eq!(r.status(), Status::Clean);
    assert_eq!((r.state().key.id.as_str(), r.state().key.on_battery), (SEED_KEY, true));
    r.ac = true;
    r.idle(10);
    assert!(!r.state().key.on_battery);
}

#[test]
fn r_before_takes_builds_under_the_case_key_only() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.hot_build();
    r.goto(dec(2), 9, 0);
    assert_eq!(r.status(), Status::Watch);
    // The balanced profile, with a reference of its own: its builds vote, against other numbers.
    let mut cfg = her_config();
    cfg.platform_profile = Some("balanced".into());
    cfg.epp = Some("balance_performance".into());
    r.config = cfg;
    r.config_due = true;
    r.idle(5);
    let other = r.state().key.id.clone();
    assert_ne!(other, SEED_KEY);
    r.engine().refs.keys.entry(other.clone()).or_default().active = Some(CheckRef {
        t: Some(80.0),
        duty: Some(0.01),
        fan: Some(6000.0),
        source: "check".into(),
        label: "check 1/12/2026".into(),
        ..CheckRef::default()
    });
    r.goto(dec(2), 10, 0);
    r.build_with(82.0, 0.01, 6000);
    let p = r.plateaus().last().unwrap().clone();
    assert_eq!((p.key_id.as_str(), p.verdict), (other.as_str(), PlateauVerdict::Clean));
    r.goto(dec(2), 14, 0);
    r.act(|e, now| e.cleaned(now, None));
    let rb = r.state().verification.clone().unwrap().r_before.unwrap();
    assert_eq!((rb.source.as_str(), rb.t_ss), ("plateaus_7d", Some(90.5)));
}

/// A WATCH case that lapses when day 15 settles, at 00:00:15 on day 16; the rig stands at
/// 00:00:05, before that settle.
fn lapsing_at_midnight() -> Rig {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.hot_build();
    for d in [3, 7, 11] {
        r.goto(dec(d), 10, 0);
        r.clean_build();
    }
    r.goto(dec(15), 23, 58);
    r.idle(125);
    assert_eq!(r.status(), Status::Watch);
    r
}

#[test]
fn an_action_just_after_midnight_sees_the_case_after_the_settle() {
    // Not dust: the settle the action runs first lapses the case, so there is nothing to
    // dismiss (it used to panic halfway).
    let mut r = lapsing_at_midnight();
    let now = r.now();
    let err = r.engine().not_dust(&now).unwrap_err();
    assert!(err.starts_with("Not dust applies to an open dust case"), "{err}");
    assert_eq!(r.status(), Status::Clean);
    // Cleaned: a clean with no case open, not a verification of a case that is gone.
    let mut r = lapsing_at_midnight();
    r.act(|e, now| e.cleaned(now, None));
    assert_eq!(r.cases()[0].outcome, CaseOutcome::Lapsed);
    assert_eq!(r.status(), Status::Clean);
    assert!(r.state().verification.is_none());
    assert_eq!(r.state().known_clean_until.as_deref(), Some("2027-01-15"));
    assert_eq!(r.events(EventKind::Cleaned).len(), 1);
}

#[test]
fn a_day_whose_daily_line_was_lost_settles_at_start() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    let onset = r.hot_build();
    r.goto(dec(2), 10, 0);
    r.clean_build();
    // daily.jsonl lost day 1's line (a full disk); the service stops that evening and starts
    // again on day 3. Day 2 has records, day 1 only its records.
    let (state, refs, daily, records, cases, events) = r.store();
    assert!(daily.iter().any(|d| d.day == dec(1).key()));
    let daily: Vec<DailyRecord> = daily.into_iter().filter(|d| d.day != dec(1).key()).collect();
    let now = Timeline::starting(dec(3), 9, 0, 0, 0).now();
    let mut engine = Engine::new(machine(Chassis::Duo), Some(state), refs, &now);
    engine.restore(&daily, &records, &cases, &events, &now);
    let mut r = Rig::resume(engine, dec(3), 9);
    r.idle(5);
    let day1 = r.daily(dec(1)).expect("day 1 settled again at start");
    assert!(day1.e1 && day1.voting == 1);
    assert!(r.daily(dec(2)).is_some());
    assert_eq!(r.state().case.as_ref().unwrap().first_evidence.as_ref().unwrap().at, onset);
}

#[test]
fn a_wall_clock_step_ahead_is_held() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.hot_build();
    // The date set a year ahead by mistake, and put right a few seconds later.
    r.tl.step_wall(365.0 * 86_400.0);
    r.idle(5);
    r.tl.step_wall(-365.0 * 86_400.0);
    r.idle(5);
    let dailies = |r: &Rig| r.effects.iter().filter(|e| matches!(e, Effect::Daily(_))).count();
    assert_eq!(dailies(&r), 0, "no future days settled");
    // The days go on as before: day 1 settles at midnight with its hot build.
    r.goto(dec(2), 9, 0);
    assert!(r.daily(dec(1)).unwrap().e1);
    assert_eq!(dailies(&r), 1);
    assert_eq!(r.status(), Status::Watch);
    // A step that stands is taken once it has stood ten minutes.
    r.tl.step_wall(3.0 * 86_400.0);
    r.idle(300);
    assert!(r.daily(dec(2)).is_none(), "held");
    r.idle(400);
    assert!(r.daily(dec(2)).is_some() && r.daily(dec(4)).is_some());
}

#[test]
fn no_light_window_can_qualify_during_a_build() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    let may = |r: &Rig| r.watch.detector.window_may_qualify(r.now().mono_s());
    assert!(may(&r));
    for _ in 0..120 {
        r.tick(1.0, 88.0, 10, 6800);
    }
    assert!(!may(&r), "a build");
    r.idle(60);
    assert!(!may(&r), "within 90 s of it");
    r.idle(40);
    assert!(may(&r));
}

// ---------------------------------------------------------------------------------------------
// Light-load evidence (E2) off, her decision of 21/11/2026: on her clean machine bch-reconcile
// alone throttled 42-48 % of light-load time at ≥ 86 °C. These run with the default machine;
// where a test also runs with E2 on, that run shows what the switch prevents.

#[test]
fn light_load_evidence_is_off_and_the_classes_say_so() {
    // Her decision of 21/11/2026. Turning it back on means revisiting the tests in this section.
    const { assert!(!LIGHT_LOAD_EVIDENCE) };
    assert!(!Machine::default().light_load_evidence);
    let now = Timeline::starting(dec(1), 9, 0, 0, 0).now();
    let e = Engine::new(machine(Chassis::Duo), None, References::default(), &now);
    assert_eq!(e.state.classes, vec!["E1", "check"]);
    assert_eq!(e.machine.classes, vec!["E1", "check"]);
    // The counter is still there: plateau duty, the check and replay use it.
    assert_eq!(e.state.throttle_counter, Some(true));
    assert!(e.machine.has_throttle && e.state.has_throttle_counter());
    let e = Engine::new(machine_e2(Chassis::Duo), None, References::default(), &now);
    assert_eq!(e.state.classes, vec!["E1", "E2", "check"]);
}

/// The dust of 17/11/2026 at light load on days 1-3 (13 hot minutes each), and 31 cool minutes
/// on day 4: E2 days, a warning by route (c) and a day qualifying by light load alone, with E2 on.
fn dusty_light_days(r: &mut Rig) {
    r.learned_light();
    for d in [1, 2, 3] {
        r.goto(dec(d), 14, 0);
        r.hot_light();
    }
    r.goto(dec(4), 14, 0);
    r.light(31, 70.0, 0.0);
    r.goto(dec(5), 9, 0);
}

#[test]
fn light_load_is_never_dust_evidence() {
    let mut on = Rig::duo_e2();
    dusty_light_days(&mut on);
    assert_eq!(on.status(), Status::Warning, "E2 on: route (c)");
    assert!(on.daily(dec(4)).unwrap().qualifying, "E2 on: 31 voting windows");

    let mut r = Rig::duo();
    dusty_light_days(&mut r);
    assert_eq!(r.status(), Status::Clean);
    assert!(r.state().case.is_none());
    for kind in [EventKind::FirstEvidence, EventKind::Suspicion, EventKind::Warning] {
        assert!(r.events(kind).is_empty(), "{kind:?}");
    }
    assert!(!r.state().pill.show && !r.state().mon.show);
    for d in 1..=4 {
        let day = r.daily(dec(d)).unwrap();
        assert_eq!(
            (day.light_q, day.light_hot, day.e2, day.qualifying),
            (0, 0, false, false),
            "day {d}"
        );
    }
    assert_eq!(r.daily(dec(1)).unwrap().light_keys[SEED_KEY], 13, "the windows were seen");
    assert_eq!(r.daily(dec(4)).unwrap().light_keys[SEED_KEY], 31);
    let s = r.state();
    assert_eq!((s.counts.qualifying_days, s.counts.evidence_days, s.counts.hot_windows), (0, 0, 0));
    // Recorded as judged, all the same.
    let hot = r.light_records().iter().filter(|l| l.verdict == LightVerdict::Hot).count();
    assert_eq!(hot, 39);
}

#[test]
fn bch_reconcile_alone_is_never_dust_evidence() {
    let run = |e2: bool| -> Rig {
        let mut r = if e2 { Rig::duo_e2() } else { Rig::duo() };
        // The light reference one window short of learned, from windows like these.
        r.engine().refs.keys.entry(SEED_KEY.into()).or_default().light = Some(LightRef {
            n: 299,
            values: vec![0.45; 299],
            ..LightRef::default()
        });
        r.goto(dec(1), 14, 0);
        r.reconcile_light(13);
        r.goto(dec(2), 9, 0);
        r
    };
    // E2 on: this clean machine opens a case, and the learn ends capped with "check the vents".
    let on = run(true);
    let first = on.state().case.as_ref().expect("E2 on: a case").first_evidence.clone().unwrap();
    assert_eq!(first.class, "E2");
    assert!(first.summary.ends_with("mostly bch-reconcile"), "{}", first.summary);
    assert!(on.notice_ids().contains(&"light_cap".to_owned()), "{:?}", on.notice_ids());

    let r = run(false);
    assert_eq!(r.status(), Status::Clean);
    assert!(r.state().case.is_none());
    assert!(r.events(EventKind::FirstEvidence).is_empty());
    let day = r.daily(dec(1)).unwrap();
    assert_eq!((day.light_q, day.light_hot, day.e2, day.qualifying), (0, 0, false, false));
    // Nothing is learned, so there is no capped reference and no notice about the vents.
    assert!(r.notice_ids().is_empty(), "{:?}", r.notice_ids());
    assert!(r.events(EventKind::Reference).is_empty());
    let light = r.watch.engine.refs.keys[SEED_KEY].light.clone().unwrap();
    assert_eq!((light.n, light.p90), (299, None));
    // Recorded as judged: hot by saturation (45 % with the fan at 6100), mostly bch-reconcile.
    let windows = r.light_records();
    assert_eq!(windows.len(), 13);
    assert!(windows.iter().all(|l| l.verdict == LightVerdict::Hot && l.top[0].0 == "bch-reconcile"));
}

#[test]
fn builds_alone_warn_by_route_d_and_light_days_do_not_dilute_them() {
    // `warning_route_d_plateaus_only`, with a day of cool light load after every build day.
    let run = |e2: bool| -> Rig {
        let mut r = if e2 { Rig::duo_e2() } else { Rig::duo() };
        r.learned_light();
        for d in [1, 2] {
            r.goto(dec(d), 10, 0);
            r.hot_build();
        }
        r.goto(dec(3), 9, 0);
        assert_eq!(r.status(), Status::Suspicion);
        for d in (4..=22).step_by(3) {
            r.goto(dec(d), 10, 0);
            r.hot_build();
            r.goto(dec(d + 1), 14, 0);
            r.light(31, 70.0, 0.0);
        }
        r.goto(dec(24), 9, 0);
        r
    };
    let r = run(false);
    assert_eq!(r.status(), Status::Warning);
    assert_eq!(r.state().case.as_ref().unwrap().route.as_deref(), Some("d"));
    assert_eq!(
        r.events(EventKind::Warning),
        vec!["Needs a clean: signs on 7 of 7 days in 21, after three weeks of suspicion"]
    );
    // E2 on: the seven light days qualify too, and 7 of 14 is under 2/3.
    assert_eq!(run(true).status(), Status::Suspicion);
}

#[test]
fn throttling_while_cool_is_still_noticed() {
    let mut r = Rig::duo();
    r.goto(dec(1), 14, 0);
    // Throttled at 70 °C for over 10 minutes: a charger or EC limit.
    r.light(12, 70.0, 0.95);
    assert_eq!(r.notice_ids(), vec!["cold_throttle"]);
    assert_eq!(
        r.events(EventKind::Notice),
        vec!["throttling while cool: charger or EC limit"]
    );
    assert!(r.light_records().iter().all(|l| l.verdict == LightVerdict::Cold));
    r.goto(dec(2), 9, 0);
    assert!(r.state().case.is_none(), "never dust");
}

#[test]
fn e2_days_from_a_restored_store_cannot_warn() {
    let run = |e2_after: bool| -> Rig {
        // Written with E2 on: two E2 days, SUSPICION since day 2, and day 1 settled with its
        // daily line.
        let mut r = Rig::duo_e2();
        r.learned_light();
        for d in [1, 2] {
            r.goto(dec(d), 14, 0);
            r.hot_light();
        }
        assert_eq!(r.status(), Status::Suspicion);
        assert_eq!(
            r.state().case.as_ref().unwrap().e2_days,
            vec![dec(1).key(), dec(2).key()]
        );
        // Started again that evening, day 2 still open: it is rebuilt from its records.
        let mut r = r.restart_as(dec(2), 18, e2_after);
        r.idle(5);
        // Seven days of hot builds. With the two E2 days, three of them are route (b). Kept as
        // SUSPICION since day 2, they would be route (d) once day 22 settles.
        for d in 3..=9 {
            r.goto(dec(d), 10, 0);
            r.hot_build();
        }
        r.goto(dec(26), 9, 0);
        r
    };
    let on = run(true);
    assert_eq!(on.state().case.as_ref().unwrap().route.as_deref(), Some("b"));

    let r = run(false);
    // The case on light load alone closed at the restart, and the builds opened their own on
    // day 3: SUSPICION since day 4 settled, too late for route (d) to find 7 of them in 21 days.
    assert_eq!(r.status(), Status::Suspicion);
    assert!(r.events(EventKind::Warning).is_empty(), "{:?}", r.events(EventKind::Warning));
    let closed = r.cases();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].outcome, CaseOutcome::Withdrawn);
    assert_eq!(closed[0].first_evidence.as_ref().unwrap().class, "E2");
    let case = r.state().case.clone().unwrap();
    assert_ne!(case.id, closed[0].id);
    assert_eq!(case.first_evidence.as_ref().unwrap().label, "3/12/2026 10:00");
    assert_eq!(case.suspected_at.as_ref().unwrap().date_label(), "5/12/2026");
    assert_eq!((case.route, case.e2_days.len()), (None, 0));
    assert!(case.earlier_sighting.is_none(), "a withdrawn case is no sighting");
    assert_eq!(r.state().classes, vec!["E1", "check"]);
    let d2 = r.daily(dec(2)).unwrap();
    assert_eq!((d2.light_hot, d2.e2), (0, false), "rebuilt under the rules in force");
}

/// WARNING by route (c) on day 3 with light-load evidence on (`Rig::duo_e2`), then the service
/// restarts that evening with it `on` or off.
fn light_warning_restarted(on: bool) -> Rig {
    let mut r = Rig::duo_e2();
    r.learned_light();
    for d in [1, 2, 3] {
        r.goto(dec(d), 14, 0);
        r.hot_light();
    }
    assert_eq!(r.state().case.as_ref().unwrap().route.as_deref(), Some("c"));
    let mut r = r.restart_as(dec(3), 18, on);
    r.idle(5);
    r
}

#[test]
fn a_warning_on_light_load_alone_closes_at_a_restart_with_it_off() {
    let on = light_warning_restarted(true);
    assert_eq!(on.status(), Status::Warning);
    assert!(on.events(EventKind::Withdrawn).is_empty());

    let mut r = light_warning_restarted(false);
    assert_eq!(r.status(), Status::Clean);
    assert!(r.state().case.is_none());
    assert!(!r.state().pill.show && !r.state().mon.show, "the pill goes");
    assert_eq!(
        r.events(EventKind::Withdrawn),
        vec!["Light-load evidence is off: the case of 1/12/2026 14:00 rested on light load alone and is closed"]
    );
    let closed = r.cases();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].outcome, CaseOutcome::Withdrawn);
    // No case is open, so her builds teach R_passive again.
    r.goto(dec(4), 10, 0);
    r.clean_build();
    let passive = r.watch.engine.refs.keys[SEED_KEY].passive.clone().unwrap();
    assert_eq!(passive.n, 1);
    // Build evidence opens a case of its own, first evidence a build, no earlier sighting.
    r.goto(dec(5), 10, 0);
    r.hot_build();
    r.goto(dec(6), 9, 0);
    assert_eq!(r.status(), Status::Watch);
    let case = r.state().case.clone().unwrap();
    assert_eq!(case.first_evidence.unwrap().class, "E1");
    assert!(case.earlier_sighting.is_none());
}

#[test]
fn a_case_with_builds_keeps_them_and_loses_its_light_days() {
    // Written with E2 on: an E2 day opens the case on day 1, hot builds follow on days 2-5, and
    // route (b) warns as day 5 settles. SUSPICION came as day 2 settled, the E2 day counting.
    let run = |on: bool| -> Rig {
        let mut r = Rig::duo_e2();
        r.learned_light();
        r.goto(dec(1), 14, 0);
        r.hot_light();
        for d in 2..=5 {
            r.goto(dec(d), 10, 0);
            r.hot_build();
        }
        r.goto(dec(6), 9, 0);
        let case = r.state().case.clone().unwrap();
        assert_eq!(case.route.as_deref(), Some("b"));
        assert_eq!(case.first_evidence.unwrap().class, "E2");
        assert_eq!(case.suspected_at.unwrap().date_label(), "3/12/2026");
        let mut r = r.restart_as(dec(6), 10, on);
        r.idle(5);
        r
    };
    assert_eq!(run(true).status(), Status::Warning);

    let r = run(false);
    // Without day 1, builds alone made it a SUSPICION as day 3 settled.
    assert_eq!(r.status(), Status::Suspicion);
    let s = r.state();
    assert!(!s.pill.show);
    assert_eq!(s.mon.kind, "suspicion");
    let case = s.case.clone().unwrap();
    assert_eq!(case.id, "c-20261201-1400", "the same case");
    assert_eq!((case.route, case.warning_since), (None, None));
    assert_eq!(case.suspected_at.unwrap().date_label(), "4/12/2026");
    let days: Vec<String> = (2..=5).map(|d| dec(d).key()).collect();
    assert_eq!(case.evidence_days, days);
    assert_eq!(case.qualifying_days, days);
    assert!(case.e2_days.is_empty());
    let first = case.first_evidence.unwrap();
    assert_eq!((first.class.as_str(), first.label.as_str()), ("E1", "2/12/2026 10:00"));
    assert!(first.summary.starts_with("build 90.5 °C"), "{}", first.summary);
    assert_eq!(
        r.events(EventKind::Withdrawn),
        vec!["Light-load evidence is off: the case of 1/12/2026 14:00 keeps only its builds (signs on 4 days); back to suspicion"]
    );
    // Restarted again, nothing more changes.
    let r = r.restart_as(dec(6), 11, false);
    assert_eq!(r.events(EventKind::Withdrawn).len(), 1);
    assert_eq!(r.status(), Status::Suspicion);
}

#[test]
fn a_suspicion_decided_again_is_never_suspected_earlier() {
    // Written with E2 on: an E2 day opens the case, four days qualify by light load alone and
    // dilute the ratio, and hot builds on days 6-8 make it SUSPICION only as day 8 settles.
    let mut r = Rig::duo_e2();
    r.learned_light();
    r.goto(dec(1), 14, 0);
    r.hot_light();
    for d in 2..=5 {
        r.goto(dec(d), 14, 0);
        r.light(31, 70.0, 0.0);
    }
    for d in 6..=8 {
        r.goto(dec(d), 10, 0);
        r.hot_build();
    }
    r.goto(dec(9), 9, 0);
    assert_eq!(r.status(), Status::Suspicion);
    let suspected = r.state().case.as_ref().unwrap().suspected_at.clone().unwrap();
    assert_eq!(suspected.date_label(), "9/12/2026");
    // Without light load, days 6 and 7 would have done it a day earlier. The clock of route (d)
    // does not move back.
    let mut r = r.restart_as(dec(9), 10, false);
    r.idle(5);
    assert_eq!(r.status(), Status::Suspicion);
    let case = r.state().case.clone().unwrap();
    assert_eq!(case.suspected_at.unwrap().date_label(), "9/12/2026");
    assert_eq!(case.evidence_days, vec![dec(6).key(), dec(7).key(), dec(8).key()]);
    assert_eq!(case.qualifying_days, case.evidence_days, "light-only qualifying days go too");
    assert_eq!(
        r.events(EventKind::Withdrawn),
        vec!["Light-load evidence is off: the case of 1/12/2026 14:00 keeps only its builds (signs on 3 days)"]
    );
}

#[test]
fn a_warning_by_a_worse_check_stands_without_its_light_days() {
    // Written with E2 on: an E2 day opens the case, and a check the next day comes out Worse.
    let mut r = Rig::duo_e2();
    r.learned_light();
    r.goto(dec(1), 14, 0);
    r.hot_light();
    r.goto(dec(2), 10, 0);
    r.check(CheckPurpose::Confirm, CheckVerdict::Worse, None);
    assert_eq!(r.state().case.as_ref().unwrap().route.as_deref(), Some("a"));
    let mut r = r.restart_as(dec(2), 18, false);
    r.idle(5);
    assert_eq!(r.status(), Status::Warning);
    let case = r.state().case.clone().unwrap();
    assert_eq!(case.route.as_deref(), Some("a"));
    assert!(case.e2_days.is_empty() && case.evidence_days.is_empty());
    let first = case.first_evidence.unwrap();
    assert_eq!((first.class.as_str(), first.label.as_str()), ("check", "2/12/2026 09:55"));
    assert_eq!(r.state().pill.label, "Needs a clean since 2/12/2026");
    assert_eq!(
        r.events(EventKind::Withdrawn),
        vec!["Light-load evidence is off: the case of 1/12/2026 14:00 keeps only its cooling check"]
    );
}

#[test]
fn a_not_improved_from_light_load_alone_is_verified_again() {
    // Written with E2 on: two E2 days after the clean of day 3, then five clean builds. Day 10
    // settling gives the passive verdict, Not improved by the E2 days.
    let mut r = Rig::duo_e2();
    cleaned_after_warning(&mut r);
    for d in [4, 5] {
        r.goto(dec(d), 14, 0);
        r.hot_light();
    }
    for d in 6..=10 {
        r.goto(dec(d), 10, 0);
        r.clean_build();
    }
    r.goto(dec(11), 9, 0);
    assert_eq!(r.status(), Status::Warning);
    assert_eq!(r.state().known_clean_until, None);
    let mut r = r.restart_as(dec(11), 10, false);
    r.idle(5);
    let s = r.state();
    assert_eq!(s.status, Status::Verifying);
    assert_eq!(s.verification.as_ref().unwrap().verdict, None);
    assert_eq!(s.known_clean_until.as_deref(), Some("2027-01-02"));
    assert!(!s.pill.show);
    assert_eq!(
        r.events(EventKind::Withdrawn),
        vec!["Light-load evidence is off: \"Still hot after cleaning 3/12/2026\" rested on light-load days alone, so the clean is being verified again"]
    );
    // The builds decide as day 11 settles.
    r.goto(dec(12), 9, 0);
    assert_eq!(r.status(), Status::Clean);
    assert_eq!(r.cases()[0].outcome, CaseOutcome::VerifiedByBuilds);

    // A Not improved that the hot builds would have given anyway stands, E2 days or not.
    let mut r = Rig::duo_e2();
    cleaned_after_warning(&mut r);
    for d in 4..=8 {
        r.goto(dec(d), 10, 0);
        r.hot_build();
        if d <= 5 {
            r.goto(dec(d), 14, 0);
            r.hot_light();
        }
    }
    r.goto(dec(11), 9, 0);
    assert_eq!(r.status(), Status::Warning);
    let pv = r.state().verification.clone().unwrap().passive.unwrap();
    assert_eq!((pv.hot, pv.e2_days), (5, 2));
    let mut r = r.restart_as(dec(11), 10, false);
    r.idle(5);
    assert_eq!(r.status(), Status::Warning);
    assert_eq!(r.state().pill.kind, "dust_not_improved");
    assert!(r.events(EventKind::Withdrawn).is_empty());
}

#[test]
fn e2_days_restored_into_a_verification_do_not_fail_the_clean() {
    let run = |e2_after: bool| -> Rig {
        // Written with E2 on: `cleaned_after_warning` warns by route (c), and two E2 days follow
        // the clean.
        let mut r = Rig::duo_e2();
        cleaned_after_warning(&mut r);
        for d in [4, 5] {
            r.goto(dec(d), 14, 0);
            r.hot_light();
        }
        let pv = r.state().verification.clone().unwrap().passive.unwrap();
        assert_eq!(pv.e2_days, 2);
        let mut r = r.restart_as(dec(5), 18, e2_after);
        r.idle(5);
        for d in 6..=10 {
            r.goto(dec(d), 10, 0);
            r.clean_build();
        }
        r.goto(dec(11), 9, 0);
        r
    };
    assert_eq!(run(true).status(), Status::Warning, "E2 on: not improved");

    let r = run(false);
    assert_eq!(r.cases()[0].outcome, CaseOutcome::VerifiedByBuilds);
    assert!(r.events(EventKind::NotImproved).is_empty());
}

#[test]
fn not_dust_leaves_the_light_reference_alone() {
    let mut r = Rig::duo();
    r.goto(dec(1), 10, 0);
    r.hot_build();
    r.goto(dec(1), 14, 0);
    r.light(31, 70.0, 0.0);
    r.goto(dec(2), 9, 0);
    assert_eq!(r.status(), Status::Watch);
    r.act(|e, now| e.not_dust(now));
    assert!(r.watch.engine.refs.keys[SEED_KEY].light.is_none());
    let text = &r.events(EventKind::NotDust)[0];
    assert!(!text.contains("light load"), "{text}");
}
