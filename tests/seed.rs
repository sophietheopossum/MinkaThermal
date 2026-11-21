//! Recomputes the Duo seed (§6.1) from the baseline run of 20/11/2026 16:50 UTC, by timestamp:
//! samples are 1.062 s apart, the load starts at sample 60 and the burn ended at sample 172. The
//! compiled-in seed must be what this file says.

use minka_thermal::episodes::{COLD_BELOW_TCC, median};
use minka_thermal::profile::DUO_SEED;
use minka_thermal::references::seed_ref;
use minka_thermal::rules::duty_lines;

const CSV: &str = include_str!("data/seed-2026-11-20_1650.csv");
const DT: f64 = 1.062;
const LOAD: usize = 60;
const BURN_END: usize = 172;
/// The plateau window: load 74.3-118.9 s.
const WINDOW: std::ops::RangeInclusive<usize> = 130..=172;
/// temp1_crit 100 − TCC offset 8.
const TCC: f64 = 92.0;

struct Sample {
    phase: String,
    t: f64,
    fan: f64,
    f: f64,
    events: u64,
    thr_ms: u64,
}

fn samples() -> Vec<Sample> {
    let mut lines = CSV.lines();
    assert_eq!(
        lines.next(),
        Some("t,phase,pkg_c,fan_rpm,avg_mhz,throttle_events,throttle_ms,load1")
    );
    lines
        .enumerate()
        .map(|(i, line)| {
            let c: Vec<&str> = line.split(',').collect();
            assert_eq!(c[0].parse::<usize>().unwrap(), i, "the t column is the sample index");
            Sample {
                phase: c[1].to_owned(),
                t: c[2].parse().unwrap(),
                fan: c[3].parse().unwrap(),
                f: c[4].parse().unwrap(),
                events: c[5].parse().unwrap(),
                thr_ms: c[6].parse().unwrap(),
            }
        })
        .collect()
}

/// Seconds since the load started.
fn load_s(i: usize) -> f64 {
    (i as f64 - LOAD as f64) * DT
}

/// Throttle duty between two samples, by their timestamps.
fn duty(s: &[Sample], a: usize, b: usize) -> f64 {
    (s[b].thr_ms - s[a].thr_ms) as f64 / ((b - a) as f64 * DT * 1000.0)
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

#[test]
fn the_plateau_numbers() {
    let s = samples();
    assert_eq!(s.len(), 240);
    assert_eq!(s[LOAD].phase, "load");
    assert_eq!(s[LOAD - 1].phase, "idle");
    let (a, b) = (*WINDOW.start(), *WINDOW.end());
    assert!(close(load_s(a), 74.3, 0.05), "{}", load_s(a));
    assert!(close(load_s(b), 118.9, 0.05), "{}", load_s(b));

    let temps: Vec<f64> = s[WINDOW].iter().map(|x| x.t).collect();
    let t_ss = median(&temps).unwrap();
    assert_eq!(t_ss, DUO_SEED.t_ss);
    let mean = temps.iter().sum::<f64>() / temps.len() as f64;
    let sd = (temps.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / temps.len() as f64).sqrt();
    assert!(close(mean, 86.0, 0.05), "{mean}");
    assert!(close(sd, 1.6, 0.1), "{sd}");
    let lo = temps.iter().copied().fold(f64::MAX, f64::min);
    let hi = temps.iter().copied().fold(f64::MIN, f64::max);
    assert_eq!((lo, hi), (83.0, 90.0));

    // 65 events, 414 ms in 44.6 s.
    assert_eq!(s[b].events - s[a].events, 65);
    assert_eq!(s[b].thr_ms - s[a].thr_ms, 414);
    assert!(close((b - a) as f64 * DT, 44.6, 0.01));
    let d = duty(&s, a, b);
    assert!(close(d, DUO_SEED.duty, 0.0002), "{d}");
    assert!(close(d, 0.0093, 0.0002));

    let fans: Vec<f64> = s[WINDOW].iter().map(|x| x.fan).collect();
    assert_eq!(median(&fans), Some(DUO_SEED.fan as f64));
    assert_eq!(
        (
            fans.iter().copied().fold(f64::MAX, f64::min),
            fans.iter().copied().fold(f64::MIN, f64::max)
        ),
        (6800.0, 7000.0)
    );
    let fs: Vec<f64> = s[WINDOW].iter().map(|x| x.f).collect();
    assert_eq!(median(&fs), Some(DUO_SEED.f as f64));

    // The duty lines of §7.1 come out at 3 / 8 / 2 / 2.5 % from the recomputed duty too.
    let l = duty_lines(d);
    assert_eq!((l.hot, l.only, l.clean, l.like), (0.03, 0.08, 0.02, 0.025));
}

#[test]
fn the_early_timings_and_the_turbo_transient() {
    let s = samples();
    let first = (LOAD + 1..=BURN_END)
        .find(|&i| s[i].thr_ms > s[i - 1].thr_ms)
        .unwrap();
    assert!(close(load_s(first), DUO_SEED.t_first_throttle_s, 0.1));
    let fan6500 = (LOAD..=BURN_END).find(|&i| s[i].fan >= 6500.0).unwrap();
    assert!(close(load_s(fan6500), DUO_SEED.t_fan6500_s, 0.1));

    // Duty over [15, 60] s of load.
    let a = (LOAD..).find(|&i| load_s(i) >= 15.0).unwrap();
    let b = (LOAD..=BURN_END).rev().find(|&i| load_s(i) <= 60.0).unwrap();
    assert!(close(duty(&s, a, b), DUO_SEED.duty_early, 0.01), "{}", duty(&s, a, b));

    // The turbo transient, 13-56 s: 91 °C, heavy throttle, the fan on its 6100 stage. The
    // episode rules never score it: the settle point is at 75 s.
    let a = (LOAD..).find(|&i| load_s(i) >= 13.0).unwrap();
    let b = (LOAD..=BURN_END).rev().find(|&i| load_s(i) <= 56.0).unwrap();
    let temps: Vec<f64> = s[a..=b].iter().map(|x| x.t).collect();
    let fans: Vec<f64> = s[a..=b].iter().map(|x| x.fan).collect();
    assert_eq!(median(&temps), Some(91.0));
    assert_eq!(median(&fans), Some(6100.0));
    let d = duty(&s, a, b);
    assert!((0.35..=0.45).contains(&d), "{d}");
    assert!(load_s(b) < 75.0);
}

#[test]
fn only_9_of_414_ms_are_booked_cold() {
    // §5.1 attribution at 1 Hz: a second whose 3-s maximum T is under TCC − 5 = 87 °C is cold.
    let s = samples();
    let (mut cold, mut total) = (0, 0);
    for i in *WINDOW.start() + 1..=*WINDOW.end() {
        let ms = s[i].thr_ms - s[i - 1].thr_ms;
        total += ms;
        let t3 = s[i - 2..=i].iter().map(|x| x.t).fold(f64::MIN, f64::max);
        if t3 < TCC - COLD_BELOW_TCC {
            cold += ms;
        }
    }
    assert_eq!((cold, total), (9, 414));
}

#[test]
fn the_post_load_numbers_are_provisional_but_close() {
    let s = samples();
    let after = |i: usize| (i as f64 - BURN_END as f64) * DT;
    assert_eq!(s[BURN_END].t, DUO_SEED.t_end);
    // T_hs: median over [+2, +6] s. At 1 Hz this lands within 2 °C of the provisional 56.
    let hs: Vec<f64> = (BURN_END..s.len())
        .filter(|&i| (2.0..=6.0).contains(&after(i)))
        .map(|i| s[i].t)
        .collect();
    let t_hs = median(&hs).unwrap();
    assert!(close(t_hs, DUO_SEED.t_hs, 2.0), "{t_hs}");
    // floor: median over [+45, +60] s.
    let fl: Vec<f64> = (BURN_END..s.len())
        .filter(|&i| (45.0..=60.0).contains(&after(i)))
        .map(|i| s[i].t)
        .collect();
    assert_eq!(median(&fl), Some(DUO_SEED.floor));
    let fan5000 = (BURN_END..s.len()).find(|&i| s[i].fan <= 5000.0).unwrap();
    assert!(close(after(fan5000), DUO_SEED.t_fan5000_s, 0.1));

    let r = seed_ref("UX482EG");
    assert_eq!((r.t, r.duty, r.fan), (Some(86.0), Some(0.0093), Some(6800.0)));
    assert_eq!(r.label, "duo-seed 20/11/2026");
    for p in ["djc", "ths", "floor", "t_cool"] {
        assert!(r.provisional.iter().any(|x| x == p), "{p}");
    }
}

#[test]
fn the_idle_temperature() {
    // The room line of the diagnosis compares a check's idle phase with the seed's.
    let s = samples();
    let idle: Vec<f64> = s[..LOAD].iter().map(|x| x.t).collect();
    assert!(s[..LOAD].iter().all(|x| x.phase == "idle"));
    assert_eq!(median(&idle), Some(DUO_SEED.t_idle));
    assert_eq!(seed_ref("UX482EG").t_idle, Some(62.0));
}
