# MinkaThermal design (final, 20/11/2026)

This spec merges three inputs: the detection spec (the judges' Design 3 plus their grafts), the integration spec, and the critic's review. The anchors were checked read-only against MinkaShell `3099d48`, MinkaMon `84d5541`, MinkaSyu `5a58aec` and AntiWebUI `a5a45df`; line numbers may drift by a few lines. `MinkaThermal/` is already a submodule and holds only `README.md`. §22 lists the review points that were not taken, or were changed.

## What Sophie will see

1. While cooling is normal, nothing appears. A small user service compares her sustained builds with the clean reference from 20/11/2026. It records light-load throttling too, but since 21/11/2026 that counts for nothing (§0).
2. A first sign of dust is only logged. It opens a case and stamps the date and time of the first evidence, which the MinkaThermal window shows.
3. When signs repeat over several days, MinkaMon shows a quiet note: `DUST? SINCE 3/12/2026 ▸ CHECK ▸ LATER`.
4. When the pattern persists, or a cooling check confirms it, the warning latches. An amber warning triangle with "!" flashes up on MinkaMon's schematic, the panel header reads `NEEDS A CLEAN · FIRST 3/12/2026 14:05`, and an amber "Needs a clean since 3/12/2026" pill appears in the bar.
5. In Duo mode, while MinkaMon is running on the ScreenPad, the bar pill stays hidden. It appears if MinkaMon is not running.
6. The warning never goes away on its own. Only "I've cleaned it" clears it, or "Not dust (accept as normal)".
7. "I've cleaned it" removes the pill at once, and MinkaMon then offers "VERIFY COOLING": a 5-minute check that she starts herself. It reports Restored, Improved, Not improved (the warning returns) or Inconclusive, with a table of reference, before and after values.
8. If she runs no check, the builds of the following one to two weeks give a passive verdict instead.
9. A slow or stopped fan and a cooling fault get their own warnings in the same places. They never count as dust.
10. The window has four pages (Status, Check, History and References) and an Auto / Duo / Generic chassis choice. History shows every case, first evidence and clean date.

---

## 0. Her decisions (20/11/2026)

- **Name and form:** MinkaThermal, a Rust background service plus an AntiWebUI window, in `MinkaDE/MinkaThermal`.
- **Where the warning shows:** a MinkaShell bar pill, modelled on MinkaSyu's updates pill, and a MinkaMon highlight. The highlight is **a classic warning triangle with "!" in `WARN_AMBER` (#e0a026)**. She approved this existing token; there is no new yellow.
- **Duo:** "perhaps in duo mode the bar pill does not appear, as minkamon is always open in that instance".
- **Chassis:** a Duo option tailored to the UX482 series, since the numbers should look the same on any laptop with that chassis.
- **First evidence:** log the first time and date evidence appeared.
- **Clearing:** the warning disappears only when she says the laptop has been cleaned. A stress test can then confirm that cooling improved.
- **Room:** room conditions are not modelled ("we can deal with that as and when relevant").
- **No tray icon:** AntiWebUI can host other apps' tray items but cannot publish one.
- **AGENTS.md applies:** md3e in Compact density, Proustite tokens only, Roboto Flex, no new QML, dense layouts, dates in d/M/yyyy.
- **Light-load evidence (E2) off (21/11/2026):** "just turn the light load check off". On her clean machine (cleaned 20/11/2026), bch-reconcile alone, about one core at nice 19, throttled 42-48 % of light-load time at ≥ 86 °C, so E2 would warn on a clean laptop. She chose this over excluding reconcile windows or requiring a second hot thread. It is one switch, `rules::LIGHT_LOAD_EVIDENCE`, off on every chassis. The E2 text below is kept for the day it comes back; the notes dated 21/11/2026 say what applies meanwhile.

## 1. How it decides

1. **It compares only where the power is known.**
   - **E1, plateau:** sustained all-core load past the PL1 window. The package is held at PL1 = 28 W, so the steady temperature and the thermal-throttle duty measure the cooling path.
   - **E2, light load:** 5-30 % busy, about 0.4-2.4 threads. A clean machine rarely reaches TCC here; the dusty laptop on 17/11/2026 throttled about 40 % of the time.
     - *21/11/2026: off, her decision (§0).* Her clean machine throttled 42-48 % here under bch-reconcile alone. E1 and the check decide on their own.
   - **Display only:** idle temperature, since-boot counters and mixed load. The just-cleaned machine shows 5.3 % of its uptime throttled since boot, so those counters cannot be evidence.
2. **Tiers.**
   - An evidence day opens WATCH, which is logged only.
   - SUSPICION follows, as a quiet note in MinkaMon.
   - WARNING latches. It needs persistence over several days, or a failing cooling check.
3. **Room temperature is not modelled.** Margins, multi-day majorities, the R2 guard (§7.4), the check and "Not dust" stand in for it.
4. **References are frozen.** They are learned only inside a known-clean window, bounded, and never drift on their own.
5. **Measurement rules.**
   - Every rate divides by the measured Δt, never by a sample count.
   - Power limits come only from `intel-rapl-mmio:0`. The MSR domain's 60/60 W is a decoy (run.sh's meta.txt recorded it).

## 2. Architecture

- **`minka-thermal`** (core crate, no toolkit, no D-Bus):
  - `service`: a systemd user unit and the **only writer** of the store;
  - the CLI verbs;
  - a hidden `burn` verb (the check's load generator).
- **`minka-thermal-gui`** (`gui/`, AntiWebUI): a view over the store and the socket. It does not depend on the core crate, which keeps MinkaSyu's tolerance of version skew.
- **Consumers:** MinkaShell and MinkaMon poll the mtime of `state.json` and read its ready-made `pill` and `mon` blocks. Rules and wording live only in the service.
- **Actions** go over a Unix-socket NDJSON protocol (§12.3).

```
MinkaThermal/
  Cargo.toml .gitignore(/target) README.md DESIGN.md   (a member of the MinkaDE Cargo workspace)
  src/  main.rs      verb dispatch by hand (as MinkaSyu/src/main.rs:38-69, no clap); env → Paths here ONLY
        paths.rs     struct Paths {dir, state, lock, socket, settings}; every other module takes &Paths
        cli.rs  clock.rs  sensors.rs  sampler.rs  episodes.rs  rules.rs  profile.rs
        present.rs   pill/mon blocks and all user-facing wording
        store.rs  ipc.rs  check.rs  burn.rs
  dist/ minka-thermal.service  minka-thermal.desktop
  tests/data/seed-2026-11-20_1650.csv   (copy of ~/.local/share/minka-thermal-baseline/run-2026-11-20_1650/samples.csv)
        data/state-{learning,clean,watch,suspicion,warning,verifying,not-improved,fan,fault}.json
        seed.rs   recomputes §6.1 by timestamp (1.062 s/sample, load from idx 60, burn end idx 172)
        rules.rs  synthetic 1 Hz traces → episodes → verdicts → transitions (no sensors, no load)
  gui/  Cargo.toml   (add "MinkaThermal/gui" to the root members in the same change)
        src/ main.rs(--page, --shot) raise.rs link.rs app.rs model.rs files.rs theme.rs
             views/{mod,status,check,history,references,chart}.rs
```

**Core `Cargo.toml`:**
- edition 2024, `[[bin]] minka-thermal`; no `[profile]` (the root `MinkaDE/Cargo.toml` sets thin LTO for every member);
- dependencies: `libc` (poll, flock, clock_gettime, localtime_r, killpg, PR_SET_PDEATHSIG, signalfd), `serde` (derive) and `serde_json`;
- no zbus, no AntiWebUI, no wgpu patch table.

**GUI `gui/Cargo.toml`:**
- `anti-webui = { path = "../../AntiWebUI/src/anti-webui", default-features = false, features = ["wayland", "svg"] }`;
- `anti-webui-md3e = { path = "../../AntiWebUI/src/anti-webui-md3e" }`;
- `serde`, and `serde_json` with `preserve_order` (for the settings writer, §13.6);
- no `[patch]` table or lockfile of its own: the root `MinkaDE/Cargo.toml` carries the wgpu patch and `MinkaDE/Cargo.lock` the versions, for every member.

**Tests never read the environment.** They build `Paths` under `target/test-tmp/`, so a test can never reach her socket or her cleaning history.

---

## 3. Signals and sampling (no root needed)

### 3.1 Sensor discovery
- Sensors are found by name, never by number:
  - hwmon `name`: `coretemp` and `asus`;
  - thermal zone `type`: `x86_pkg_temp`, `SEN2`, `SEN4`, `acpitz`;
  - the xe card: the `/sys/class/drm/card*` whose `device/driver` resolves to `xe` (card0 today; the order is not guaranteed).
- File descriptors stay open and are re-read with `FileExt::read_at(buf, 0)`.
- Sensors are discovered again after a read error and after any resume.

### 3.2 Signal table

| Signal | Source | When (on AC) |
|---|---|---|
| Busy b | `/proc/stat` "cpu " line; idle and iowait count as idle | every tick → b, plus B5 and B10, Δt-weighted rolling means |
| T | coretemp `temp1_input` (1 s driver cache; the seed's method) | every tick |
| T4 | zone `x86_pkg_temp` (live MSR read) | 4 Hz, **during a check only** |
| Core temps | coretemp `temp2..5` | while a plateau or a check is open |
| Package throttle | `cpu0/thermal_throttle/package_throttle_total_time_ms` (`_count` for display) | every tick → Δms |
| Core throttle | `cpu0..3/thermal_throttle/core_throttle_total_time_ms` | while a plateau or a check is open |
| Fan | asus `fan1_input` (an EC call over WMI) | 1 Hz when B10 ≥ 0.3, T ≥ 75, or a plateau or check is open; otherwise every 5 s |
| Frequency | `cpuN/cpufreq/cpuinfo_avg_freq`, fallback `scaling_cur_freq` | only when b ≥ 0.80 → per-second median across CPUs |
| iGPU | xe `device/tile0/gt0/freq0/throttle/status`; the `reason_{pl1,prochot,thermal}` files when status ≠ 0; `act_freq` | every tick |
| Power | `AC0/online`, `BAT0/{status,capacity}`; `BAT0/power_now` when Discharging | every 5 s → `ac`, plus a `deficit` flag (Discharging, or power_now > 0, while AC0 = 1) |
| Config | `/sys/firmware/acpi/platform_profile`; cpu0 EPP; `intel-rapl-mmio:0/constraint_{0,1}_power_limit_uw` and `constraint_0_max_power_uw`; `/sys/bus/pci/devices/0000:00:04.0/tcc_offset_degree_celsius`; `intel_pstate/{no_turbo,max_perf_pct}`; the `cur_state` of cooling devices of type **`Processor` or `intel_powerclamp` only** (the `TCC Offset` device always reads 8); dGPU `power/runtime_status`; whether thermald is running | every 60 s, and at every episode open and close |
| Lid | `/proc/acpi/button/lid/*/state` (an ACPI evaluation, 1.7 ms) | with the 60-s config poll, at episode edges and at check gates |
| Read once at start | DMI `sys_vendor`, `board_name`, `product_name`, `bios_version`; kernel; CPU model; `temp1_crit` | start |
| bch-reconcile share | threads whose comm starts with `bch-reconcile`, found every 10 min; their utime + stime | every 60 s and at episode edges |
| Top 3 consumers | scan of `/proc/*/stat` | episode open and close, and the end of a hot light window |
| Context only | zones `SEN2`, `SEN4`, `acpitz` | every 60 s |
| **Never** | NVMe temperatures (they wake the drive; MinkaMon already reads them), RAPL `energy_uj` (root only), `uevent` files | |

**TCC point** = `temp1_crit`/1000 − TCC offset, read live: 100 − 8 = **92 °C**.

### 3.3 Timing
- **Loop:** one thread runs `poll()`. Its timeout is the next CLOCK_MONOTONIC deadline. The unit sets `TimerSlackNSec=50ms`; timerfd would ignore timer slack, a poll timeout honours it.
- **Rates:**
  - 1 Hz base;
  - 4 Hz during a check;
  - 0.2 Hz on battery, rising to 1 Hz while T ≥ 85 °C so the alarms can time 5 s. Nothing is evaluated on battery; only the alarms run.
- **Timestamps:** each sample is stamped with MONOTONIC, BOOTTIME and REALTIME.
- **Suspend and stall:**
  - suspend: (BOOTTIME − MONOTONIC) grew by more than 2 s;
  - stall: a MONOTONIC gap of more than 3 s;
  - either one voids the open plateau and the open light window, rebases the counters and discovers the sensors again;
  - a counter that decreases is rebased.
- **Ring buffer:** 900 s of 1-s rows, used for backdating and the dip guard.

## 4. Config key and chassis

**Config key** = {AC, platform_profile, EPP, MMIO PL1 W (rounded), MMIO PL2 W, TCC offset, no_turbo, max_perf_pct, lid}.
- A key change closes the open episode and window as `void:config`.
- References are stored per key. **Evidence counts only under a key that has a reference.** Other keys' records are still stored, for learning and `replay`.
- A `Processor` or `intel_powerclamp` cooling device with `cur_state > 0`, or thermald appearing, voids the overlapping blocks and raises a notice.
- `bios_version` and the CPU model are stored with each reference. A BIOS change logs `bios_change` and shows the notice "firmware changed (311 → X): if builds now read differently, run a check; Not dust adopts the new behaviour". References are kept.

**Chassis profile:** set by `thermal.chassis` in `~/.config/minka-settings.json`: `auto` | `duo` | `generic`, default `auto`.
- `auto` means duo when `sys_vendor` = "ASUSTeK COMPUTER INC." **and** (`board_name` matches `^UX482E` or `product_name` contains `UX482`), as MinkaMon's `machine_is_zenbook` does.
- It can be set from the window (§13.6). It is independent of the pill gate (§14).

**Duo chassis constants** (any UX482E, any CPU):
- fan from asus `fan1_input`: automatic plateau 6800-7000 rpm, forced maximum 7300 (replacement fan, 29/9/2026);
- EC stages at about 5000 / 6100 / 6900;
- settle at 75 s once fan ≥ 6500;
- the fan-slow line is R_fan − 400;
- `f_allcore_max` = 4100 MHz on the i7-1165G7, otherwise learned (§6.3).

**Seed key:** AC, performance, performance, 26 ≤ PL1 ≤ 30 W, PL2 ≥ 33 W, TCC offset 8, no_turbo 0, max_perf_pct 100, lid open. This is her normal state; it was verified live on 20/11/2026 (MMIO 28/35 W, τ 27.98 s).
- A UX482EA (same chassis, no MX450) uses the seed with the note "measured on UX482EG".
- A Duo machine under any other key learns its references as generic mode does, keeping the chassis constants.

---

## 5. Operating points

### 5.1 Plateau episodes (E1)
- **Open:** B10 ≥ 0.85. The onset is backdated to the first second of that run with b ≥ 0.85.
- **Stay open:** while B10 ≥ 0.75.
- **Close** at the first of:
  - B10 < 0.75 for 5 consecutive seconds (the end time is the first of those seconds);
  - a key change, suspend, stall or service stop (void);
  - a check starting (void).
- **Qualifies:** lasts ≥ 120 s.
- **Settle point:**
  - Duo: onset + 75 s, once fan ≥ 6500. If the fan is still < 6500 at onset + 120 s, settle there and flag `fan_low`.
  - Generic: onset + 75 s, once the fan's 20-s range is ≤ 200 rpm. With no fan sensor: onset + 120 s.
  - On the seed, the fan reached 6500 at 61.6 s.
- **Measurable second:** after the settle point, b ≥ 0.80, outside a dip guard, and xe `reason_prochot` = 0.
- **Dip guard:** after any run of n seconds with b < 0.80, the next **min(75, 10 + 3.5·n)** seconds are not measurable. The episode stays open.
  - The PL1 budget (τ ≈ 28 s) refills during a dip, and the package then bursts at PL2 = 35 W for about 3.3·n s.
  - The +10 s covers heat soak; 75 s is the full cold-start transient.
- **Blocks:** the settled part is cut into consecutive 120-s wall-time blocks; a last partial block is kept if it is ≥ 45 s. A block is **valid** when all of these hold:
  - ≥ 45 measurable seconds;
  - the Theil-Sen slope of T over its measurable seconds is within ±0.03 °C/s, else `slope`;
  - **power-limited**: median f ≤ f_allcore_max − 50 MHz (Duo i7 < 4050; seed 3899), or duty ≥ 1 %, or xe `reason_pl1` set in ≥ 50 % of its seconds. Otherwise `below_pl1`;
  - cold + PROCHOT throttle time ≤ 2 % of measurable time. Otherwise `power_delivery`.
- **Throttle attribution, per measurable second** (Δms of package throttle time):
  - xe `reason_prochot` set → **prochot**;
  - otherwise, the maximum T over seconds [s−2, s] is below TCC − 5 (87 °C) → **cold** (EC, charger or VR PROCHOT, which the package counter also counts);
  - otherwise → **thermal**.
  - At 1 Hz, this books only 9 of the seed's 414 steady-state ms as cold.
- **One vote per episode**, when it has ≥ 1 valid block. Its metrics are the median across valid blocks: `T_ss` (median T), `duty` (thermal ms / measurable s), `fan_med`, `f_med`.
- **Unjudged episodes** keep their reason: `too_short`, `unsettled`, `dips`, `below_pl1`, `power_delivery`, `slope`, `no_reference`, `void:<cause>`. They are counted over 30 days.

### 5.2 Light-load windows (E2)
- **Windows:** fixed 60-s windows aligned to the wall clock.
- **A window qualifies** when all of these hold:
  - mean b is in [0.05, 0.30];
  - no second with B5 ≥ 0.75 in the window or in the 90 s before it;
  - median xe `act_freq` < 900 MHz;
  - on AC, under a referenced key;
  - no suspend or stall.
- **Attribution:** if the window's maximum T is ≥ TCC − 6 (86 °C), its throttle ms are **hot**; otherwise **cold**. `duty_hot` = hot ms / window seconds.
- **Recorded per hot window:** `threads` = mean b × nproc, and the top consumers.
- **Cold throttle** is never dust. Over 10 min of it in a day gives the notice "throttling while cool: charger or EC limit".
- **E2 votes only once the light reference is learned** (§6.2). Until then the window shows "light load: learning n/300".
- *21/11/2026, E2 off (§0):* windows still qualify, are judged and recorded, and their cold throttle still counts. None votes. The light reference is not learned (§6.2), so there is no "learning n/300": it would stay at 0/300. The window shows "light load: off" instead, as `status` does (§12.2, §13).

### 5.3 Daily roll-up (display)
- For each local day and busy bin (<5, 5-30, 30-60, 60-85, ≥85 %): minutes, throttle ms, hot ms, median T and median fan.
- The day verdicts (§7.4).
- **Drift:** the Theil-Sen slope of daily plateau T and duty medians over 30 days, shown as for example "+0.4 °C/week".
- Since-boot throttle % is shown as context only.

---

## 6. References

### 6.1 Duo seed (20/11/2026 16:50 UTC, just cleaned, burn.c × 8, recomputed by timestamp)

| Quantity | Seed | Window |
|---|---|---|
| Plateau T_ss | **86 °C** median (mean 86.0 ± 1.6, range 83-90) | load 74.3-118.9 s (samples 130-172) |
| Plateau duty | **0.93 %** (65 events, 414 ms in 44.6 s) | same |
| Plateau fan | **6800** median (range 6800-7000) | same |
| Plateau f | 3899 MHz (display only; a different generator) | same |
| Turbo transient | 91 °C, 38 % duty, fan 6100 | 13-56 s. Never scored |
| Early timings | first throttle 4.2 s; fan ≥ 6500 at 61.6 s; duty over [15, 60] s 37 % | |
| Post-load | T_end 86, T_hs 56, D_jc 30, floor 49, t_cool 14.9 s, fan ≤ 5000 at 29.7 s | 1 Hz coretemp. **Provisional** until her first check re-measures them at 4 Hz |
| Light load | 0.11 % duty (idle with Floorp spikes; T p50 62, fan 4650) | display only; E2 uses the learned p90 |
| Dusty, for scale | 17/11/2026: about 40 % light-load duty at 91-92 °C (bch-reconcile on one core); 15/9/2026: 16 % of uptime throttled | memory notes |
| Work | 340,255 M rounds | the C `burn`. **Not comparable** with generator v1 |

### 6.2 Reference hierarchy (per key)
- **R_active** is what the check compares with. It starts as the seed (Duo seed key) or empty (generic), and is replaced by:
  - **(a)** the first own check inside a known-clean window, if T_ss ≤ seed + 3 °C and duty ≤ 3 %. Otherwise the seed is kept, with the notice "reference ran hotter than the 20/11/2026 clean: check the profile, PL and vents".
    - Recommended: two checks on different days. R_T becomes their mean; R_duty and R_fan take the worse of the two; R_f and R_work are set (same CPU and generator only).
    - For her the window runs to **20/12/2026**.
  - **(b)** a later post-clean **Restored** check, bounded at ≤ seed + 3 °C and ≤ 3 % duty (generic: ≤ first verified reference + 3 °C).
  - **(c)** "Not dust" (§9.2). This is uncapped, its provenance is `accepted d/M/yyyy`, and it is **dropped at the next Cleaned**, when R_active falls back to the last verified reference.
- **R_best** is the best verified clean reference (the seed or a check, never an accepted one). **Restored is judged against R_best.** It is also shown for display.
- **R_passive** is what plateaus compare with, since rustc at 28 W need not read like the burn at 28 W.
  - **Learned** inside a known-clean window from ≥ 8 voting episodes on ≥ 3 days: R_T = median T_ss, **R_duty = min(median duty, 3 %)**, R_fan = median fan_med.
  - **Suspect** if R_T > R_active.T + 3. Learning then continues. After 30 days still suspect, show the notice "builds run hotter than the reference check: long-build heat soak or a different load; run a check".
  - **After a clean**, re-learning is capped at the previous verified R_passive + 1.5 °C and + 1 pp of duty.
  - **Until learned**, plateaus use the seed-phase rules against R_active (§7.2).
- **Light reference** `light_p90`: the p90 of `duty_hot` over ≥ 300 qualifying windows inside a known-clean window (on Duo too), with a floor of 0.2 % and a cap of 2 % (a cap hit raises a notice). Re-learned after each clean.
  - *21/11/2026, E2 off (§0):* not learned, and Not dust does not re-learn it. Learned now, it would include the reconcile windows, and a capped learn would tell her to check the vents. Turning E2 back on starts a fresh learn of 300 windows.
- **Known-clean window:** 30 days after a Cleaned. Outside one (first run "Not sure"), references are learned but marked **unverified**.

### 6.3 Generic mode
- **Discovery:**
  - temperature: coretemp `temp1`, or `k10temp`/`zenpower` Tctl;
  - fan: the hwmon `fan*_input` with the highest observed maximum (optional);
  - throttle counters only where `thermal_throttle/` exists;
  - PL from `intel-rapl-mmio:0`, else `intel-rapl:0`;
  - TCC = `temp1_crit` − offset, else `temp1_crit`, else `temp1_max`, else 95 °C (AMD);
  - `f_allcore_max` = the 95th percentile of busy-CPU frequency in the first 10 s of plateaus;
  - `fan_auto_max` = the 99th percentile of plateau fan medians (needs ≥ 3 episodes).
- **First run** asks "When was this laptop last cleaned?" (§13.2). It offers "Record a reference now (5-min check)".
- **Saturation rules need no reference** and vote even while learning:
  - a plateau with duty ≥ 30 % and fan ≥ 0.96 × fan_auto_max is hot;
  - a light window with duty_hot ≥ 5 % and fan ≥ 0.85 × fan_auto_max is hot.
- **Missing signals drop their classes:**
  - with no throttle counters, duty = the fraction of seconds at ≥ TCC − 2; a plateau is hot when ΔT ≥ 4 and f ≤ 0.95·R_f; E2 is unavailable;
  - with no fan sensor, the fan rules drop.
  - The window lists the active classes.
- *21/11/2026, E2 off (§0):* every machine, the Duo included, now runs as one without the counter does for E2. No light window votes, so light load makes no qualifying day: a light-only day could never be an evidence day, and counting it would dilute the ratios that SUSPICION, route (d) and the lapse use. The counter still feeds plateau duty, the check and FAN/FAULT. `classes` omits E2, and state.json's `throttle_counter` says whether the counter exists (`replay` judges plateaus by it).

---

## 7. Evidence rules

### 7.1 Duty thresholds
Each multiplicative margin is capped by an additive one. This keeps an accepted high R_duty from blinding the duty arm (TCC clamps T at 92 °C, so the temperature arm goes dead near 90).
- `D_hot  = max(3 %,   min(3·R_duty,   R_duty + 3 pp))`
- `D_only = max(8 %,   min(6·R_duty,   R_duty + 5 pp))`
- `D_clean = max(2 %,  min(2·R_duty,   R_duty + 1 pp))`
- `D_like = max(2.5 %, min(2.5·R_duty, R_duty + 1.5 pp))` (check only)

For the Duo seed (R_duty 0.93 %) these are 3 / 8 / 2 / 2.5 %.

### 7.2 Plateau verdict (one per voting episode)

| | Against learned R_passive | Seed phase (against R_active) | Duo numbers (learned = seed / seed phase) |
|---|---|---|---|
| **hot** | (T_ss ≥ R_T + 3 **and** duty ≥ D_hot) **or** duty ≥ D_only | (T_ss ≥ R_T + 4 **and** duty ≥ D_hot) **or** duty ≥ D_only + 2 pp | ≥ 89 °C with ≥ 3 %, or ≥ 8 % / ≥ 90 °C with ≥ 3 %, or ≥ 10 % |
| **clean** | T_ss ≤ R_T + 1.5 **and** duty ≤ D_clean | T_ss ≤ R_T + 2.5 **and** duty ≤ D_clean + 2 pp | ≤ 87.5 °C and ≤ 2 % / ≤ 88.5 °C and ≤ 4 % |
| neutral | otherwise (no vote) | | |

- **Why these numbers:** the per-second sd is 1.6 °C, so a median over ≥ 45 s is good to about 0.5 °C. The AND resists the duty bursts of real builds.
- **`fan_slow`:** fan_med < R_fan − 400 (Duo < 6400), or `fan_low`, on an episode that is not clean. It feeds FAN only (§8.5).
- **`power_capped` notice**, never evidence. Text: "power limit looks capped (PL1 X W): not dust". It is raised by any of:
  - b ≥ 0.9 with T_ss < 75, f < 0.75·f_allcore_max and duty < 0.5 %;
  - MMIO PL2 < PL1;
  - PL1 < 20 W (Duo), or < 0.6 × `constraint_0_max_power_uw` (generic).

### 7.3 Light-window verdict
- **Hot** when `duty_hot` ≥ **L = max(5 %, 10 × light_p90)**.
- For scale: the 17/11/2026 dust was about 40 %.
- *21/11/2026, E2 off (§0):* the verdict is still stored on the record. It counts for nothing. Her clean machine reached 42-48 % under bch-reconcile alone.

### 7.4 Days
- **Days and settling:** a day is a local calendar day. It is settled at the first tick after the local date changes. Days the service missed are settled at start, from records.
- **Qualifying day:** ≥ 1 voting plateau, or ≥ 30 qualifying light windows once E2 is active.
- **E1 evidence day:** ≥ 1 hot plateau, **and** hot ≥ clean plateaus that day. Decided when the day settles.
- **E2 evidence day:** ≥ 10 hot light windows, which is at least 30 s of TCC throttling spread over ≥ 10 minutes of 1-2-thread work. Decided the moment the 10th window lands, since the count only grows.
- **Evidence day:** an E1 or an E2 day.
- *21/11/2026, E2 off (§0):* no day is an E2 day, and a qualifying day needs a voting plateau. An evidence day is an E1 day. A daily line written while E2 was on is read back the same way.
- **R2 guard:** passive escalation to WARNING is blocked while the most recent voting plateau of the last 7 days, if there is one, is **clean**. Cooling that holds at 28 W cannot plausibly be failing at lower power.

### 7.5 Immediate alarms (no persistence; these also run on battery)
- **Fan stopped:** 0 rpm for ≥ 20 s while T ≥ 85 °C raises FAN. A momentary 0 at idle is the EC pausing the fan.
- **TCC not acting:** T ≥ TCC + 6 (98 °C) for ≥ 5 s raises FAULT. Skipped when TCC + 6 > TjMax.

---

## 8. State machine

| State | Entered when | Shown |
|---|---|---|
| LEARNING | The current key has no reference | window: "learning (n/8 builds, d/3 days)" |
| CLEAN | default | nothing |
| WATCH | First evidence day: **a case opens, and `first_evidence` is stamped and frozen** | window only |
| SUSPICION | (i) ≥ 2 evidence days in the last 14, and ≥ 50 % of the qualifying days in those 14; or (ii) a check from CLEAN came out Worse (§10.8) | MinkaMon note; window |
| **WARNING (latched)** | Route (a), (b), (c) or (d) below | bar pill (Duo gate, §14), MinkaMon triangle and badge, window |
| VERIFYING | "Cleaned" with a case open | **no pill**; MinkaMon purple "verify cooling"; window |

**Routes to WARNING** (each from WATCH or SUSPICION):
- **(a)** a cooling check with the verdict Worse, unless the case was opened by a check on the same local day;
- **(b)** ≥ 5 evidence days within 21 days, at ≥ 2/3 of the qualifying days, **≥ 1 of them an E2 day**, with the R2 guard passing;
- **(c)** ≥ 3 E2 days within 14 days, with the R2 guard passing;
- **(d)** in SUSPICION for ≥ 21 days, with ≥ 7 evidence days in the last 21 at ≥ 2/3 of the qualifying days, with the R2 guard passing. This is the plateau-only path; no check is needed.
- *21/11/2026, E2 off (§0):* routes (b) and (c) cannot fire, not even from E2 days in a restored state. WARNING comes by (a) or (d). WATCH and SUSPICION open on E1 days or a check, and a case's first evidence is a build or a check.
  - *A case that a build with E2 on left open* is decided again at start (`Engine::restore`), with the event `withdrawn`:
    - with no E1 evidence day left and no comparable Worse check, it closes as `withdrawn`, which is no earlier sighting;
    - otherwise it keeps its builds and checks, drops its E2 days, and a light-load first evidence becomes its earliest build or check;
    - a SUSPICION, or a WARNING by (b) or (c), is decided again from the 14-day tallies without light load, and is never dated earlier than it was suspected. A WARNING by (a) or (d), or a case with a Worse check, keeps its status. (d) never fires with an E2 day in its 21 days, because (b) fires first.

### 8.1 `first_evidence`
- **What it holds:** the start time (episode onset or window start) of the **earliest evidence-class record on the case's first evidence day**, with its class, its values against the reference, the key and the top consumers. Example: "build 90.4 °C, throttled 6.8 % (clean 86 °C, 0.9 %), mostly rustc".
- **When it is written:** when the day becomes an evidence day (E1 at settle, E2 at its 10th window). It is written to `events.jsonl` and fsynced, and survives unchanged into WARNING and into the closed case.
- **A case opened by a check** is dated at the check's start.
- **Earlier sighting:** a new case opened within 60 days of a `lapsed` or `cleared_by_check` case adds "earlier sighting d/M/yyyy" to its text.

### 8.2 Lapse and clearing
- **WATCH lapses** after 14 consecutive days with no evidence day **and** ≥ 3 qualifying days. The case closes as `lapsed` and shows under "earlier sightings" for 60 days. With no qualifying days, it stays open.
- **SUSPICION never lapses** (she has seen it). It ends only in one of these ways:
  - a check that comes out like the reference;
  - Cleaned;
  - Not dust;
  - escalation to WARNING.
- **WARNING never clears on passive data.** Only Cleaned or Not dust clear it.

### 8.3 A check while a case is open (WATCH or SUSPICION)
- **Like the reference:** the case closes as `cleared_by_check` and the state returns to CLEAN. The next evidence opens a new case.
- **Borderline:** the state is unchanged. The check is suggested again after 7 days if new evidence arrives.
- **Worse:** WARNING, by route (a).
- **In WARNING** (the "measure before cleaning" check): the result becomes R_before and the state is unchanged. A like result shows "measured like the reference today; if you have not cleaned, consider Not dust".

### 8.4 Starvation notice (window only)
No voting plateau and no qualifying light day for 21 days → "not enough comparable load to judge; run a check?".
- *21/11/2026, E2 off (§0):* only a voting plateau keeps it away.

### 8.5 FAN and FAULT (separate from dust, each with its own `first_evidence`)
- **FAN** is raised by any of:
  - the fan-stopped alarm;
  - `fan_slow` on ≥ 2 voting plateaus on ≥ 2 days within 14 days;
  - a check's fan flag (§10.7).
- FAN text: "Fan slower than normal (6800 → 6100 rpm): wear, cleaning may not fix it", or "Fan stopped under load".
- FAN clears with "Fan fixed" (re-learns R_fan from the next check) or "Accept fan as normal".
- **FAULT** (TCC not acting) clears with "Acknowledge".
- Neither one counts toward dust.

---

## 9. "Cleaned" and "Not dust"

### 9.1 Cleaned
Sources: the window's "I've cleaned it" button, with a date picker defaulting to today and bounded from first evidence to today; or `minka-thermal cleaned [--date d/M/yyyy]`.

**With a case open:**
1. Stamp `cleaned_at` (`date_only` for a past date). **Remove the pill immediately.** The status becomes VERIFYING; the case stays open.
2. Freeze **R_before**, from the first source available:
   - this case's before-check;
   - the medians of T_ss, duty and fan over the voting plateaus of the 7 days before, plus the E2 hot windows per day;
   - the case's hot records.
3. Open a 30-day known-clean window. R_passive and `light_p90` re-learn within the caps of §6.2. Evidence before `cleaned_at` stops counting, and accepted references are dropped.
4. Offer "Run cooling check (about 5 min)". It **only ever starts on her click** (§10.1).
5. **Passive verification**, if no check has run 7 days after the clean. Once 5 voting plateaus exist since the clean, or at day 14 with whatever exists:
   - ≥ 3 hot, or ≥ 2 E2 days → **Not improved**: WARNING re-latches in the same case;
   - ≥ 3 clean and 0 hot → the case closes as `verified_by_builds`;
   - otherwise, at day 14, the case closes as `unverified`.
   - *21/11/2026, E2 off (§0):* R_before has no E2 rate, and E2 days count for nothing here, even ones a restored state still holds.
     - A passive Not improved that a build with E2 on reached by E2 days alone (fewer than 3 hot builds, no verify check) goes back to VERIFYING at start. The builds then decide, and `known_clean_until` returns to the clean date + 30 days. The references that the Not improved dropped stay dropped.

**With no case open** (first start, or a preventive clean):
- set `known_clean_until` to the date + 30 days;
- leave the status unchanged;
- the window suggests two reference checks;
- nothing appears in the bar.

### 9.2 Not dust (accept as normal)
- **Available in** WATCH, SUSPICION and WARNING, and after Not improved. It needs a confirm dialog.
- **Effect:**
  - the case closes as `dismissed`;
  - the latest valid check, or the last 7 days' voting-plateau medians (needs ≥ 3), becomes R_active and R_passive, **uncapped**, with provenance `accepted d/M/yyyy`;
  - `light_p90` is re-learned from the last 7 days;
  - R_best is untouched;
  - the decision is logged as hers.
- This is the lever for "if average conditions change, we can deal with that as and when relevant".

---

## 10. Cooling check (confirmation, before-clean, post-clean verification, reference)

### 10.1 Ownership and consent
- **The service owns the check**, so it outlives the window.
- **The load** is a child `minka-thermal burn`:
  - started with `process_group(0)`, and `PR_SET_PDEATHSIG(SIGKILL)` in `pre_exec`;
  - stdin null; stdout piped into the poll set; stderr goes to the journal;
  - `killpg(SIGKILL)` on end, cancel or abort.
- **Passive detection is suspended** during the check. Windows that overlap it never become passive records.
- **Starting it:**
  - socket `check.start` requires `confirm: true`; only the window's Start button and a confirmed CLI send it;
  - CLI `check` asks y/N on a tty, or needs `--yes`;
  - Ctrl-C **cancels**, and `--detach` returns once the check is accepted;
  - MinkaMon's CHECK and every other entry point only **open the Check page**.
- **A service start with an unfinished check** in `state.json` marks it "aborted: service restart". It is never resumed.
- **Purpose** is derived from the state: WATCH or SUSPICION → `confirm`; WARNING → `before`; VERIFYING → `verify`; otherwise `reference`.

### 10.2 Gates
The check waits up to 5 min after Start, then refuses with a reason. The Check page says: "the load starts by itself once the machine is idle (up to 5 min)".
- **Power:** AC online, BAT0 not Discharging, battery ≥ 50 %.
- **Key:**
  - the profile must be performance, else "switch to the Performance profile"; MinkaThermal does not change it;
  - PL1/PL2/TCC/lid must match the reference key, else refuse, or "Run anyway", marked "not comparable with the clean reference" and compared only with R_before.
- **dGPU:** if its `runtime_status` is active and `nvidia-smi` exists, one `utilization.gpu` query must read < 10 %. A suspended dGPU is never woken.
- **Settle:**
  - B10 < 10 % for 30 s;
  - fan ≤ 5000;
  - the 60-s Theil-Sen slope of T ≥ −0.02 °C/s, meaning the heatsink has stopped cooling;
  - there is no absolute temperature gate, so a summer room cannot block a check;
  - the page shows the busiest processes; after 5 min it gives up as "too busy".
- **Prompt:** hard surface · vents clear · lid open · usual room.

### 10.3 Phases and sampling
- **Phases:** 30 s idle (T_idle, fan_idle, SEN2/SEN4), **180 s load**, 90 s recovery.
- **Sampling:**
  - the full 1 Hz set, plus 4 Hz `x86_pkg_temp` (T4) throughout;
  - each burn thread's rounds per second;
  - the burn child's CPU time (`/proc/<pid>/task/*/stat`).

### 10.4 Generator v1 (`burn --threads N`)
- nproc threads, each running batches of 2^20 rounds and bumping its own `AtomicU64` per batch. The main thread prints `{"t":<mono s>,"rounds":[…]}` once a second.
- The inline asm fixes the instruction stream, so work compares across rustc versions:
  ```rust
  asm!("2:", "mov {t},{x}", "shl {t},13", "xor {x},{t}",
             "mov {t},{x}", "shr {t},7",  "xor {x},{t}",
             "mov {t},{x}", "shl {t},17", "xor {x},{t}",
             "dec {n}", "jnz 2b",
       x = inout(reg) x, t = out(reg) _, n = inout(reg) n => _, options(nomem, nostack));
  ```
- Non-x86 targets use a `black_box` loop with its own `generator_version`.
- A unit test runs one batch (about 1 ms) against a plain-Rust xorshift.
- **Comparability:** work and frequency compare only within the same generator version and CPU model. Temperature, duty and fan compare across versions, since both are PL1-bound.
- **Nice:** 0, from the unit. Her apps run at −4 and the compositor at −12 under ananicy. If ananicy renices the service, the burn child inherits that nice, the busy rule below makes the check Inconclusive with "load generator starved (nice N)", and `meta.json` records the nice.

### 10.5 Aborts, windows, metrics, validity
- **Aborts:**
  - cancel, AC lost, battery < 30 %, a key change, a suspend, the lid closing;
  - T ≥ 99 °C for 3 s;
  - fan at 0 rpm for ≥ 10 s while T > 80 °C (this also raises FAN).
- **Windows:**
  - steady = load [75, 178] s, used against own references;
  - matched = [75, 118] s, used against the seed (the seed's window was 74.3-118.9 s).
- **Metrics:**
  - `T_ss` (median coretemp, the seed's method) and `T_ss4` (from T4) alongside it;
  - `duty_ss` (thermal, attributed by the §5.1 rule using T4 maxima);
  - cold and PROCHOT fractions, `fan_ss`, `f_ss`;
  - work rate in M rounds per thread-second, and rounds per MHz-second;
  - duty over [15, 60] s, time to first throttle, time to fan ≥ 6500;
  - post-load: `T_hs` = median T4 over [+2, +6] s, `D_jc` = T_end − T_hs, `floor` = median over [+45, +60] s, `t_cool`, `t_fan5000`;
  - T_idle, deficit fraction, xe `reason_pl1` share.
- **Inconclusive**, with the reason, if any of these hold:
  - busy < 95 % over the steady window;
  - xe `reason_prochot` set in ≥ 10 % of the steady seconds;
  - cold + PROCHOT throttle time > 2 %;
  - the PL changed.
- Foreign CPU (busy minus the burn) above 5 % drops only the work metrics. Deficit is reported, never disqualifying.

### 10.6 Verdict against R_active (ΔT = T_ss − R_T)
- **Worse**, if any one holds:
  - ΔT ≥ 3 **and** duty ≥ D_hot;
  - duty ≥ D_only;
  - same CPU and generator: f_ss ≤ R_f − 150 **and** duty ≥ D_hot.
- **Like**, if all hold:
  - ΔT ≤ 2;
  - duty ≤ D_like;
  - (T_ss < R_T − 1, or fan_ss ≥ R_fan − 400);
  - (another CPU or generator, or work ≥ 0.97·R_work, or f ≥ R_f − 100).
- **Borderline** otherwise.
- **Against the Duo seed:** Worse is ≥ 89 °C with ≥ 3 %, or ≥ 8 %. Like is ≤ 88 °C, ≤ 2.5 %, and fan ≥ 6400 whenever T_ss ≥ 85.

### 10.7 Fan flag
fan_ss < R_fan − 400 **and** T_ss ≥ R_T − 1 raises FAN with "fan slower than reference (6800 → X rpm): wear, cleaning will not fix it". The EC follows temperature, so in a cool room a clean machine can sit on its 6100 stage, and the flag must not fire then.

### 10.8 Outcomes by purpose
- **`confirm`** (WATCH or SUSPICION): as §8.3.
- **`reference` from CLEAN or LEARNING:**
  - Like or Borderline may set R_active, by §6.2a;
  - Worse opens a case in **SUSPICION**, dated at the check, with "run again on another day". A second Worse check on a later day reaches WARNING by route (a).
- **`before`:** stored as R_before (§8.3).
- **`verify`** (P = this check, B = R_before, judged against **R_best**):
  - **Restored:** P is like R_best.
    - The case closes as "verified clean d/M/yyyy", and `first_evidence` is archived with it in `cases.jsonl`.
    - R_active is updated within the §6.2b bound, and R_best too if P is better.
  - **Improved:** not Restored, but ΔT(P) ≤ ΔT(B) − 2, **or** duty(P) ≤ duty(B)/2 and at least 2 pp lower. With no B: T_ss ≤ R_T + 5 and duty ≤ R_duty + 6 pp.
    - The case closes as "improved, X °C above the clean of d/M/yyyy: dust left in the fins, or paste or heatsink seating".
    - R_active is unchanged.
  - **Not improved:** valid, and neither of the above.
    - WARNING re-latches in the same case, with `first_evidence` kept, and the pill returns: "Still hot after cleaning 5/12/2026".
    - The actions are "Cleaned again" and "Not dust".
  - **Inconclusive:** stays VERIFYING, the pill stays hidden, and "Run again" is offered.
- **Diagnosis lines** (with Not improved, and as notes with Improved; R values from R_active, with the seed's post-load values provisional):
  - fan_ss < R_fan − 400 → "fan slower than reference: dust inside the fan, or wear";
  - D_jc ≥ R_Djc + 5, with T_hs within 3 °C of R_Ths → "heat is not reaching the heatsink: paste or heatsink contact";
  - T_hs ≥ R_Ths + 5, with D_jc normal → "heatsink still hot: fins or vents still blocked";
  - ΔT_idle ≥ 3 and ≥ 0.7·ΔT_ss → "everything is uniformly warmer than the reference: the room may be warmer".

---

## 11. Store

### 11.1 Location
- **state.json path:** `$MINKA_THERMAL_STATE`, else `$XDG_STATE_HOME/minka-thermal/state.json`, else `~/.local/state/minka-thermal/state.json`.
- **Every other file lives beside state.json**, so one variable moves the whole store.
- **Socket:** `$MINKA_THERMAL_SOCKET`, else `$XDG_RUNTIME_DIR/minka-thermal.sock`. When `$MINKA_THERMAL_STATE` is set and `$MINKA_THERMAL_SOCKET` is not, the socket is `minka-thermal.sock` beside the moved store, so a test store never reaches the live service (the GUI resolves it the same way).
- **XDG_STATE_HOME** is unset today, both in her session and in the user manager. If it is ever set, it must be set in both places (`systemctl --user import-environment XDG_STATE_HOME`).
- **Never export the test variables** from `shojiwm-env.fish`; the unit does not see fish's environment anyway.

```
~/.local/state/minka-thermal/          (mode 0700)
  state.lock       flock held by the service
  state.json       current state + pill/mon blocks
  references.json  references per key (the Duo seed is compiled into profile.rs and never dropped)
  events.jsonl     the human history: append-only, fsynced, never pruned
  records.jsonl    this month's records → records-YYYY-MM.jsonl at month rollover (kept; 10-30 KB/day)
  daily.jsonl      one line per local day (~1 KB)
  cases.jsonl      closed cases: the dust-interval history (15/9 → 17/11 → 20/11/2026 → …)
  checks/YYYY-MM-DD_HHMM/{samples.csv, meta.json, result.json}
```

### 11.2 Writes and versions
- **state.json:**
  - write `state.json.tmp`, fsync, `rename`, then fsync the directory;
  - write **only when the bytes (minus `updated_at`) change**. The `counts` refresh at an episode close, at a hot or cold light window, at day settle and hourly; there are no per-tick writes.
- **jsonl files:**
  - opened `O_APPEND`, one `write(2)` per line;
  - `events.jsonl` and `cases.jsonl` fsync every line;
  - `records.jsonl` and `daily.jsonl` fsync at episode close and day settle.
- **`schema`** (file layout) and **`rules_version`** (thresholds) are separate integers. Every record carries `rules_version`.
  - *21/11/2026:* `rules_version` stays 1 with E2 off (§0). No store had been written yet, so 1 names the first rules that ran. Bump it when E2 comes back, since that changes what a day counts.
- **Reading:** readers use `#[serde(default)]` everywhere and ignore unknown fields.
- **Upgrades and downgrades:**
  - a service that finds a **higher** `schema` refuses to write and exits 1;
  - a lower one is migrated, with the old file kept as `state.json.v<N>.bak`.
- **Timestamps:**
  - `Stamp = {"utc":"2026-12-03T14:05:10Z","local":"2026-12-03T14:05:10+00:00"}`;
  - display labels are pre-formatted as local `d/M/yyyy HH:mm`;
  - session logs are UTC; mixing the zones has produced phantom timelines before.

### 11.3 `state.json` (WARNING example)
```json
{
  "schema": 1, "rules_version": 1, "version": "0.1.0",
  "updated_at": {"utc": "2026-12-05T10:31:02Z", "local": "2026-12-05T10:31:02+00:00"},
  "pid": 1432, "boot_id": "6f0c…",
  "chassis": {"profile": "duo", "setting": "auto", "board": "UX482EG", "product": "ZenBook UX482EG_UX482EG",
              "bios": "UX482EG.311", "cpu": "11th Gen Intel(R) Core(TM) i7-1165G7", "tcc_c": 92},
  "key": {"id": "ac|performance|performance|28|35|8|0|100|open", "referenced": true,
          "label": "AC · performance · PL1 28 W · PL2 35 W · TCC 92 °C · lid open", "reference": "duo-seed 20/11/2026"},
  "classes": ["E1", "E2", "check"],
  "status": "warning", "setup_needed": false, "known_clean_until": null,
  "case": {
    "id": "c-20261203-1405", "opened": {"utc": "…", "local": "…"},
    "first_evidence": {"at": {"utc": "2026-12-03T14:05:10Z", "local": "…"}, "label": "3/12/2026 14:05", "class": "E1",
      "summary": "build 90.4 °C, throttled 6.8 % (clean 86 °C, 0.9 %), mostly rustc",
      "record_id": "p-20261203-1405", "key_id": "ac|performance|…", "top": ["rustc", "cargo", "floorp"]},
    "earlier_sighting": null,
    "suspected_at": {"utc": "…", "local": "…"}, "warning_since": {"utc": "…", "local": "…"}, "route": "b",
    "evidence_days": ["2026-12-03", "2026-12-04", "2026-12-05"], "e2_days": ["2026-12-04"],
    "qualifying_days": ["…"], "checks": [], "cleaned": []
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
}
```

**Other blocks, filled when they apply:**
- **`verification`:**
  ```json
  {"cleaned_at": Stamp, "date_only": false, "label": "5/12/2026",
   "r_before": {"source": "check|plateaus_7d|case", "t_ss": …, "duty": …, "fan": …, "e2_per_day": …},
   "passive": {"voting": 2, "hot": 0, "clean": 2, "e2_days": 0, "from": "2026-12-12", "deadline": "2026-12-19"},
   "verdict": null, "check_dir": null}
  ```
- **`check`** (written at phase changes only):
  ```json
  {"purpose": "confirm|before|verify|reference", "state": "gating|idle|load|recovery|done|refused|aborted",
   "started_at": Stamp, "phase_started_at": Stamp, "phase_s": 180, "anyway": false, "dir": "checks/2026-12-05_1102",
   "gates": [{"id": "ac", "ok": true, "text": "AC online"}, …], "reason": null}
  ```
- **`last_check`:**
  ```json
  {"at": Stamp, "dir": "…", "purpose": "…", "verdict": "like|borderline|worse|inconclusive",
   "post_clean": "restored|improved|not_improved|null", "t_ss": …, "duty": …, "fan": …, "label": "Restored"}
  ```
- **`notices[]`:** `{id, since: Stamp, text}`.
- *21/11/2026, E2 off (§0):* `classes` is `["E1", "check"]`, and the new `"throttle_counter": true` says the counter exists, which `classes` no longer shows. The example's route (b) and `e2_days` cannot occur meanwhile.

### 11.4 Presentation contract (`present.rs` writes it; consumers only map tone → token)
Tones map to tokens as follows: `amber` → `WARN_AMBER`, `red` → `RED`, `purple` → `PURPLE`, `green` → `OK_GREEN`, `neutral` → `TEXT_MUTED`.

| Situation | `pill` kind (tone) · label | `mon` kind (tone) · triangle · badge · actions |
|---|---|---|
| learning, clean, watch | none | none |
| suspicion | none | `suspicion` (neutral) · no triangle · `DUST? SINCE 3/12/2026` · `check`, `later` |
| warning | `dust_warning` (amber) · "Needs a clean since 3/12/2026" | `dust_warning` (amber) · triangle, pulse · `NEEDS A CLEAN · FIRST 3/12/2026 14:05` |
| warning after Not improved | `dust_not_improved` (amber) · "Still hot after cleaning 5/12/2026" | (amber) · triangle, pulse · `STILL HOT AFTER CLEANING 5/12/2026` |
| verifying | none (removed immediately) | `verifying` (purple) · `CLEANED 5/12/2026 · VERIFY COOLING` · `check` |
| check running | unchanged | `check` (purple) · `COOLING CHECK · LOAD` |
| check result, for 24 h | none | `result` · `CHECK 5/12/2026: RESTORED` (green; neutral for Improved, Borderline and Inconclusive) |
| FAN | `fan` (amber) · "Fan slow · 6100 rpm" or "Fan stopped" | `fan` (amber) · triangle, pulse · `FAN SLOW 6100/6800 RPM` or `FAN STOPPED` |
| FAULT | `fault` (red) · "Cooling fault" | `fault` (red) · triangle, pulse · `TCC NOT ACTING 99 °C` |

- **Priority:**
  - pill: fault, then dust, then fan;
  - mon: fault, dust, fan, check, verifying, suspicion, result.
  - The window shows every active item.
- **`actions`** is a fixed vocabulary that MinkaMon maps itself:
  - `check` → `ui --page check`;
  - `later` → `snooze suspicion`. It hides only the actions, for 3 days; the badge stays.
- **`badge_short`** is used when the panel is narrow (§15.3).
- **`page`** is the window page a click opens.
- **"Watch off"** is not in the file. MinkaMon computes it (§15.2).

### 11.5 `events.jsonl`, records, cases, references, checks
- **`events.jsonl`:**
  - each line: `{"seq":17,"at":Stamp,"type":"first_evidence","case":"c-…","text":"First evidence: build 90.4 °C, throttled 6.8 % …","data":{…}}`;
  - types: `setup`, `first_evidence`, `suspicion`, `warning`, `check_start`, `check_result`, `cleaned` (with the date she gave), `verification`, `not_improved`, `lapse`, `cleared_by_check`, `not_dust`, `snooze`, `fan`, `fan_fixed`, `fan_accepted`, `fault`, `fault_ack`, `reference`, `config_change`, `bios_change`, `notice`;
  - `config_change` is logged only for profile, EPP, PL, TCC, no_turbo or max_perf_pct changes that last ≥ 10 min. AC and lid flips go to the journal only;
  - a notice is logged at most once per id per day;
  - *21/11/2026, E2 off (§0):* new type `withdrawn`, for a case or Not improved from a build with E2 on that was decided again at start (§8, §9.1).
- **`records.jsonl`:**
  - plateau: `{"type":"plateau", id, rules_version, onset, end, key_id, bios, cpu, duration_s, measurable_s, blocks, valid_blocks, t_ss, t_p90, duty, cold_frac, prochot_frac, fan_med, f_med, core_spread, core_throttle_share[4], deficit_frac, reconcile_share, igpu_act_med, top[[comm,share]×3], early{t_first_throttle,duty_early,t_fan6500}, flags[], verdict}`;
  - light: `{"type":"light", id, start, busy, threads, t_p90, t_max, fan_p90, hot_ms, cold_ms, reconcile_share, deficit, top, verdict}`. Only hot and cold-throttle windows are written; the others only feed counts;
  - unjudged: `{"type":"unjudged", onset, duration_s, reason}`.
- **`daily.jsonl`:** `{"day":"2026-12-03","bins":{"lt5":{min,thr_ms,hot_ms,t_med,fan_med},"5_30":…,"30_60":…,"60_85":…,"ge85":…},"voting":2,"hot":1,"clean":1,"light_q":214,"light_hot":0,"qualifying":true,"e1":true,"e2":false,"unjudged":{…}}`.
  - *21/11/2026, E2 off (§0):* `light_q` and `light_hot` stay 0 and `e2` false. `light_keys` still counts the windows seen per key.
- **`cases.jsonl`:** `{id, opened, first_evidence, closed, outcome: verified_clean|verified_by_builds|improved|lapsed|cleared_by_check|dismissed|unverified, cleaned[], check_dirs[], summary}`.
  - *21/11/2026, E2 off (§0):* new outcome `withdrawn`, for a case that rested on E2 evidence alone (§8). A reader one version behind reads it as `unverified`.
- **`references.json`:** `{"schema":1,"keys":{"<key id>":{"active":{t,duty,fan,f,work,djc,ths,floor,t_cool,source,label,bios,cpu,generator,provisional[],accepted},"best":{…},"passive":{state:learning|learned|suspect,n,days,t,duty,fan,accepted},"light":{n,p90,accepted}}}}`.
  - Provenance labels use full dates, for example `learned 21/11/2026-20/12/2026`.
- **`checks/<dir>/`:**
  - `samples.csv` columns: `t_s,phase,b,T,T4_mean,T4_max,core0..3,fan,f_med,pkg_thr_ms,core_thr_ms0..3,xe_status,xe_reasons,xe_act,ac,deficit,rounds,burn_cpu_s`;
  - `meta.json`: MMIO PL, TCC offset, BAT0 state, key, BIOS, kernel, top processes, generator version, the service's nice;
  - `result.json`: verdicts, the steady and matched windows, the comparison columns, the diagnosis lines and the validity reasons.
- **`minka-thermal replay [--since d/M/yyyy]`:** re-evaluates the stored records under the current rules and prints what would change. It is read-only, and is the lever for tuning after shipping.

---

## 12. Service, CLI, socket, unit

### 12.1 Service
**Start-up:**
1. Resolve `Paths` and run `mkdir -p` with mode 0700.
2. Take `flock(state.lock, LOCK_EX|LOCK_NB)`. If it is held, print "another minka-thermal service owns <dir>" and **exit 4**. Only services take this lock.
3. Set up the socket. If a connect to the path succeeds, exit 4. Otherwise unlink the stale file, bind, and chmod 0600. **Never unlink a live socket.**
4. Load `references.json` and `state.json`. Mark an unfinished check as aborted. Rebuild the 30-day tallies from `daily.jsonl` and `records.jsonl`, and settle missed days.
5. Discover the sensors, read DMI, CPU and `temp1_crit`, compute the chassis and key, and compare the BIOS. Write state.json; its pid and boot_id are new.
6. Enter the loop.

**The loop** polls the listening socket and its clients, a `signalfd` (SIGTERM, SIGINT) and, during a check, the burn pipe. Each tick:
1. `sampler.read()` produces a row, which goes into the ring.
2. `episodes.feed(row)` closes records, which `store.append` writes.
3. `rules.feed(record)` makes any state transitions.
4. `present.recompute()` rebuilds the pill and mon blocks. If anything changed, the service writes state.json and broadcasts `state`.

**Periodic work:**
- every 60 s: the config key and lid, and `thermal.chassis` (by mtime);
- the first tick after the local date changes: day settle and the `daily.jsonl` line.

**SIGTERM:**
1. Record an open episode as `unjudged void:restart`.
2. `killpg` the burn child and mark the check aborted.
3. Write state and exit 0.

### 12.2 CLI

| Verb | Needs the service | What it does |
|---|---|---|
| *(none)* or `status [--json]` | no | Reads state.json, checks liveness (`/proc/<pid>/comm` = `minka-thermal` and the same `boot_id`) and prints a summary |
| `sensors` | no | Discovered paths and current values, chassis, key and active classes |
| `history [--since d/M/yyyy]` | no | events.jsonl as `d/M/yyyy HH:mm  text` |
| `replay [--since d/M/yyyy]` | no | Read-only re-evaluation |
| `ui [--page status\|check\|history\|references]` | no | Opens the window (§12.4) |
| `check [--yes] [--anyway] [--detach]` | yes, else exit 3 | y/N on a tty unless `--yes`; refuses with no tty and no `--yes`. Streams gates, phases and the verdict. **Ctrl-C cancels**; `--detach` returns once accepted |
| `cancel` | yes | Cancels a running check |
| `cleaned [--date d/M/yyyy]` | yes | Defaults to now |
| `not-dust [--yes]` | yes | y/N on a tty |
| `snooze suspicion` | yes | 3 days |
| `fan-fixed`, `accept-fan`, `ack-fault` | yes | §8.5 |
| `service` | — | Started only by the unit |
| `burn --threads N` (hidden) | — | The check's child |

- Mutating verbs never write the store themselves. Without the service they print "service not running: systemctl --user start minka-thermal" and exit 3.
- **Exit codes:** 0 OK, 1 error, 2 usage, 3 service not running, 4 already running.
- Dates are parsed only as `d/M/yyyy`.
- **`status` example:**
```
MinkaThermal  service running (pid 1432 since 3/12/2026 09:12)
chassis duo (auto: UX482EG, BIOS UX482EG.311)   key AC · performance · PL1 28 W · PL2 35 W · TCC 92 °C · lid open
reference duo-seed 20/11/2026 (passive: learning 3/8 builds, 2/3 days; light load 412/300 learned)
status   WARNING since 5/12/2026 10:31 · first evidence 3/12/2026 14:05
         build 90.4 °C, throttled 6.8 % (clean 86 °C, 0.9 %), mostly rustc
evidence 4 of 6 qualifying days in 21 (light-load on 1) · last build 5/12/2026 10:22 89.6 °C 5.1 % fan 6900 hot
fan ok 6800 rpm · fault none · notices none · unjudged 30 d: too_short 31, dips 6, slope 1
```
- *21/11/2026, E2 off (§0):* the `reference` line ends "light load off" whenever `classes` lacks E2, in place of "412/300 learned". The "(light-load on N)" suffix cannot appear, because no open case holds E2 days: one that a build with E2 on left behind is decided again at start (§8).

### 12.3 Socket protocol
- **Format:** NDJSON in MinkaSyu's `handle_line` shape (`MinkaSyu/src/main.rs:676-708`):
  - request: `{"id":1,"method":"…","params":{…}}`;
  - reply: `{"id":1,"result":…}` or `{"id":1,"error":{"code":"…","message":"…"}}`;
  - broadcast: `{"event":"…","payload":…}`.
- **Error codes:** `bad_request`, `unknown_method`, `refused`, `busy`, `internal`.
- **Back-pressure:** a client more than 1 MB behind is dropped.

| Method | Params | Result |
|---|---|---|
| `state.get` | — | the state object |
| `live.subscribe` | `{on}` | `{}`; while on, a `live` event (one row) each second |
| `check.start` | `{confirm: true, anyway}` | `{state: "gating", dir}`; `refused` without `confirm` |
| `check.cancel`, `check.rows` | — | `{}` / every row of the running or last check |
| `case.cleaned` | `{date?: "yyyy-mm-dd"}` | the state |
| `case.not_dust`, `case.snooze {what: "suspicion"}`, `setup.answer {last_cleaned: "yyyy-mm-dd"\|"not_sure"}` | | the state |
| `fan.fixed`, `fan.accept`, `fault.ack` | — | the state |
| `settings.reload` | — | `{}`; re-reads `thermal.chassis` now |

- **Events:** `state` (the full object after every write), `live`, `check` (gates, then phase plus a per-second row, then the result) and `notice`.
- History never goes over the socket. The window reads those files read-only.

### 12.4 `ui`
1. **Find the GUI** in this order, as MinkaSyu's `find_gui` does (`src/main.rs:446-467`):
   - `$MINKA_THERMAL_GUI`;
   - `minka-thermal-gui` beside `current_exe` (installed, every name is a link in /usr/bin to
     MinkaDE's one binary, `minka`);
   - PATH.

   Once the window is a library in `minka` (23/11/2026: every Minka app is), this becomes what
   MinkaSyu's `gui_command` does: `/proc/self/exe` started as `minka-thermal-gui`, so the window
   is always the build the CLI runs.

   There is no QML fallback. If nothing is found, name every path tried and exit 1.
2. **Logging,** when stderr is not a tty: append to `$XDG_STATE_HOME/minka/minka-thermal-ui.log` (default `~/.local/state/minka/`), rolled at 2 MB as `packaging/minka-keep` does (one generation kept, as `.log.1`). `dup2` it onto stdout and stderr, then write `=== MinkaThermal window starting: <date -Is> ===`.
3. **`exec`** the GUI, passing `--page` through.

### 12.5 `dist/minka-thermal.service`
```ini
[Unit]
Description=MinkaThermal cooling watch

[Service]
Type=simple
ExecStart=/usr/bin/minka-thermal service
Restart=on-failure
RestartSec=5
# 4 = another instance owns the store or socket; restarting cannot help.
RestartPreventExitStatus=4
# nice 0 on purpose: the check's load must get the CPU; her apps run at -4 and the compositor at -12.
Nice=0
TimerSlackNSec=50ms
IOSchedulingClass=best-effort
IOSchedulingPriority=7
MemoryMax=96M
TasksMax=64
NoNewPrivileges=yes
LockPersonality=yes
RestrictRealtime=yes
MemoryDenyWriteExecute=yes
# The check's load generator lives in this cgroup and goes with it.
KillMode=control-group
TimeoutStopSec=10

[Install]
WantedBy=default.target
```
- No `CPUQuota` and no lower `CPUWeight`, because the check runs in this cgroup.
- No graphical session is needed: the service reads only sysfs and procfs, and uses no D-Bus.

### 12.6 `dist/minka-thermal.desktop` (packaged form)
```ini
[Desktop Entry]
Type=Application
Name=MinkaThermal
GenericName=Cooling Monitor
Comment=Warns when the laptop needs cleaning and checks that a clean worked
Exec=minka-thermal ui
Icon=temperature-normal
Terminal=false
Categories=System;Monitor;
Keywords=thermal;temperature;fan;dust;clean;cooling;throttle;minka;
StartupWMClass=minka-thermal
Actions=check;history;

[Desktop Action check]
Name=Run cooling check
Exec=minka-thermal ui --page check

[Desktop Action history]
Name=Cleaning history
Exec=minka-thermal ui --page history
```
`temperature-normal` exists in breeze and breeze-dark on her system.

### 12.7 Running it (her commands)
Since 23/11/2026 MinkaThermal is part of MinkaDE's one binary, and the minkade package installs
`/usr/bin/minka-thermal`, the unit and the desktop entry:
```sh
systemctl --user enable --now minka-thermal.service
```
- **First start:** the window's setup card ("When was this laptop last cleaned?" → 20/11/2026), or `minka-thermal cleaned --date 20/11/2026`. Either opens the known-clean window to 20/12/2026. Then run two reference checks on different days before **20/12/2026**.
- **After each package upgrade:** `systemctl --user restart minka-thermal`. The running service holds the old inode until then, and the restart loses only an open episode.

---

## 13. The window (`minka-thermal-gui`)

### 13.1 Frame
- **`main()`:** handle `--shot` first. Then `minka_instance::claim("minka-thermal-gui")`, as in `MinkaSyu/gui/src/lib.rs`, with a `raise.rs` like `MinkaSyu/gui/src/raise.rs` (`APP_ID` "minka-thermal"), plus a page hand-off:
  - a duplicate launched with `--page <name>` sends `page <name>` with `minka_instance::request("minka-thermal-gui", …)`, then exits;
  - the holder takes it as `Event::Request`, forwards it through `ui_channel`, answers, and raises itself through ShojiWM IPC. An `Event::TurnedAway` (a duplicate's probe) only raises it.
- **App:** `App::new().title("MinkaThermal").app_id("minka-thermal").size(860, 640)`, after `theme::install()`.
  - `theme::install()` calls `set_density(Density::Compact)`, then `set_color_scheme(scheme())`, then `set_theme`, all copied from `MinkaShell/src/theme.rs:65-…/103-…`.
  - `theme.rs` adds `OK_GREEN = 0x7dc98c`, MinkaMon's value.
  - `FONT_FAMILY` is "Roboto Flex, Noto Sans"; numbers and tables use `"monospace"`.
- **Link:**
  - the first frame is drawn from state.json on disk, so the window works with the service down;
  - the socket is then connected for `state` events and actions, reconnecting every 3 s;
  - `live.subscribe` is on only while the Now card or the Check page is visible.
- **Offline:** a banner reads "Service not running · [Start service]". The button runs `systemctl --user start minka-thermal.service` and reaps it. Every other action is disabled.
- **Layout:** a 36 px header, the tabs, a scrolling body and a snackbar host. Padding is 12 and gaps are 8.
- **Header:**
  - "MinkaThermal" in Roboto Flex 15, bold;
  - an md3e `styled_assist_chip` in the status tone, with an icon (`icons::WARNING` for warning, `CHECK_CIRCLE` for clean);
  - a liveness dot: `OK_GREEN` when alive, `RED_DIM` when stale;
  - a mono `TEXT_MUTED` context line, amber when the key has no reference: `Duo · UX482EG · AC · performance · PL1 28 W · PL2 35 W · TCC 92 °C · ref duo-seed 20/11/2026`.
- **Tabs:** a tonal ExtraSmall `connected_button_group` (Status · Check · History · References), with md3e shared-axis motion between pages.

### 13.2 Status page
- **Left card**, an `outlined_card` with its border in the tone, by status:
  - **LEARNING:** "learning (3/8 builds, 2/3 days)" with a `linear_progress_colored`; in generic mode, the active classes.
  - **CLEAN:**
    - "Cooling looks normal";
    - the last build against the reference;
    - "known clean until 20/12/2026: 1 of 2 reference checks done";
    - [Run cooling check] [I've cleaned it].
  - **WATCH:** "One day with signs of dust (3/12/2026 14:05)", the summary, and [Run cooling check] [Not dust].
  - **SUSPICION:** "Possible dust since 3/12/2026", the day ratio, and [Run cooling check] [Later (3 days)] [Not dust].
  - **WARNING:**
    - title, first evidence, the summary, "Warning since d/M/yyyy HH:mm · 5 of 6 days · light load ×2";
    - the warning text: "Needs a clean, first evidence 3/12/2026 14:05. Builds 90.5 °C at 28 W, 7 % throttled (clean 86 °C, 0.9 %); light-load throttling on 2 days. Dust or blocked vents.", plus any earlier sighting;
    - [I've cleaned it] [Measure before cleaning] [Not dust (accept as normal)];
    - after Not improved: "Still hot after cleaning 5/12/2026", the diagnosis lines, and [Cleaned again] [Not dust].
  - **VERIFYING:** "Cleaned 5/12/2026 · verify cooling", [Run cooling check (about 5 min)], and "builds since clean 2/5 (2 clean, 0 hot), passive verdict from 12/12/2026".
- **Right column:**
  - a **Now** card: pkg T, fan, busy and B10, throttle ms/s, f, iGPU MHz, AC and lid, and the episode state;
  - a **Last 21 days** strip of day squares, with qualifying / evidence / E2 counts, drift and the unjudged total.
- *21/11/2026, E2 off (§0):* light load is off when `classes` lacks E2; `throttle_counter` says whether the machine has the counter at all. Then the WARNING line has no "light load ×N", the warning text has no light-load clause (WARNING comes by (a) or (d) only, §8), the 21-day strip has no E2 count, and LEARNING shows build learning only.
- **Banners above the card:**
  - FAN: amber, with [Fan fixed] [Accept fan as normal];
  - FAULT: red, with [Acknowledge];
  - notices: muted text, one line each.
- **Evidence table** for this case: mono rows such as `5/12/2026 10:22  build  9m12  89.6 °C  5.1 %  6900 rpm  3.71 GHz  hot  rustc 71 %`. The first-evidence row is marked `◂ first evidence`.
- **First run** (`setup_needed`): a card asking "When was this laptop last cleaned?", with [Pick date…] (`date_picker_dialog`), [Within the last month] and [Not sure], plus [Record a reference now (5-min check)].

### 13.3 Flows
1. **Run a check.** Every entry point (SUSPICION, "Measure before cleaning", VERIFYING, References, MinkaMon's CHECK) opens the Check page. **Only its Start button starts the load.**
2. **I've cleaned it.** A `dialog` with `date_picker` opens, defaulting to today and bounded from first evidence to today. [Record clean] sends `case.cleaned`. A snackbar says "Clean recorded 5/12/2026. The bar warning is gone." The page then shows VERIFYING, with the check offered and never started automatically.
3. **Not dust.** An `alert_dialog` asks: "Accept today's cooling as normal? The latest check (or the last 7 days of builds) becomes the reference, without caps, until the next clean. Logged as your decision."
4. **Cancel a check.** An `alert_dialog` confirms. The result is "Cancelled", and nothing becomes a reference.

### 13.4 Check page
- **Before Start:**
  - the prompt line;
  - the gates table (AC, battery, profile, PL1/PL2/TCC/lid against the reference key, dGPU) with values;
  - [Start] (filled; enabled when the gates pass), and [Run anyway] (outlined, labelled "not comparable with the clean reference") on a key mismatch.
- **After Start, while settling:** an `indeterminate_wavy_progress`, with "load starts by itself once idle (up to 5 min): B10 14 % (< 10), fan 5200 (≤ 5000), slope −0.05 °C/s (≥ −0.02); busiest: rustc 62 %, floorp 9 %".
- **Running:**
  - a phase strip, `idle 30 s · load 180 s · recovery 90 s`;
  - overall progress as `wavy_progress_colored` (a `PURPLE` line on a `GAUGE_DIM` track), plus a `linear_progress_colored` for the phase;
  - a Now grid: T and T4 max, fan, f, duty so far, work rate, foreign CPU, deficit;
  - a chart (SVG, `views/chart.rs`): T in `TEXT`, T4 max in `RED_DIM`, fan on the right axis in `PURPLE`, R_T dashed in `TEXT_FAINT`, TCC dashed in `RED`, phase bands in `SURFACE_RAISED`;
  - [Cancel].
- **Result:**
  - a verdict card in its tone;
  - a comparison table with full dates in its headers: `reference 20/11/2026 | before 4/12/2026 | after 5/12/2026`. Rows: T_ss, duty, fan, f, work, t_cool, T_hs/D_jc;
  - the diagnosis lines;
  - the check directory as selectable mono text;
  - [Run again] on Inconclusive; [Cleaned again] [Not dust] on Not improved.

### 13.5 History page
- **Filters:** `filter_chip`s for Events · Builds · Light load · Checks · Unjudged.
- **Dust intervals** from `cases.jsonl`: "15/9/2026 → 17/11/2026 (63 d) → 20/11/2026 → …".
- **A 30-day calendar** of day squares:
  - evidence day: `WARN_AMBER` (E2 days carry a small "L" in `GROUND`);
  - clean qualifying day: `OK_GREEN`;
  - neutral qualifying day: `TEXT_MUTED`;
  - no qualifying data: `SURFACE_RAISED`;
  - the first-evidence day gets a 1 px `TEXT` outline.
- **Rows:** dense mono rows, `d/M/yyyy HH:mm · type · text`.
- *21/11/2026, E2 off (§0):* no day carries the "L". The Light load filter still lists the recorded windows (hot and cold-throttle), which count for nothing. A `withdrawn` case (§8) shows with its outcome, not as a dust interval.

### 13.6 References page
- **A table per key:**
  - R_active with its provenance;
  - R_best;
  - R_passive (learning n/8 and days, learned, suspect or accepted);
  - the light reference (n/300, p90);
    - *21/11/2026, E2 off (§0):* when `classes` lacks E2, this row reads "light load off" in place of n/300 and p90. Nothing is learned, so `learning.light_windows` stays 0 against `light_needed` 300, and showing that as progress would suggest a learn that is stuck;
  - BIOS, CPU and generator.
- **A seed card** with the §6.1 numbers; T_hs, D_jc and floor are marked provisional.
- **A signals card** (the §19 checks):
  - discovered sensors with paths and live values;
  - active classes;
  - xe `reason_pl1` share at plateaus;
  - T_ss4 − T_ss from checks;
  - the R_passive − R_active offset;
  - unjudged counts by reason.
- **Chassis:** a `connected_button_group` with Auto / Duo / Generic, plus "detected: UX482EG".
  - It writes `thermal.chassis` into `~/.config/minka-settings.json`: read the whole file as `serde_json::Value` with `preserve_order`, set the key, write tmp and rename, then call `settings.reload`.
  - MinkaConf's settings page deep-copies the file, so other keys survive. A MinkaConf window that was opened before the change may write the old value back.
- **[Record a reference (5-min check)].**

### 13.7 md3e use and gaps
- **Widgets used:**
  - `styled_button` (ExtraSmall: filled for the main action, outlined and text for the rest);
  - `connected_button_group`, `outlined_card`, `filled_card`, `divider`;
  - `wavy_progress_colored`, `linear_progress_colored`, `indeterminate_wavy_progress`;
  - `alert_dialog`, `dialog` + `date_picker` / `date_picker_dialog`;
  - `snackbars()`, `filter_chip`, `styled_assist_chip`;
  - icons `CLEANING_SERVICES`, `WARNING`, `MODE_FAN`, `THERMOMETER`, `ERROR`, `CHECK_CIRCLE`, `HEAT`, `SPEED`.
- **No tooltips anywhere in v1.**
- **Gaps:** see §21.

### 13.8 Headless shots
`minka-thermal-gui --shot <page> <file.png>`, with `MINKA_THERMAL_STATE=tests/data/state-<status>.json`, renders through `anti_webui::app::headless::render_png(file, 1075, 800, 1.25, …)` from files only, with no socket and no window, as `MinkaSyu/gui/src/main.rs:44-77` does. There is one fixture per status.

---

## 14. MinkaShell bar pill

### 14.1 `src/settings.rs`
- **After `duo_mode` (:88-94):**
  ```rust
  /// MinkaMon's own rule for living on the ScreenPad (MinkaMon/src/main.rs:66). Not `duo_mode`:
  /// forced "duo" with the pad disabled floats MinkaMon on eDP-1.
  pub fn minkamon_hosts_pad(s: &Settings, all: &[Output]) -> bool {
      s.layout != "general" && screenpad_present(all)
  }
  ```
- **After `syu_bin` (:169-174), before `#[cfg(test)]` (:176):**
  - `enum Tone { Neutral, Amber, Red }`;
  - `struct ThermalPill { show, kind, tone, label, page }`, with `parse(&str)` (missing or malformed → hidden; `show` needs a label) and `load()`;
  - `thermal_state_path()` (shaped like `syu_state_path`, :152-165);
  - `thermal_bin()` (`$MINKA_THERMAL_BIN`, else `"minka-thermal"`);
  - `open_thermal(page)`:
    ```rust
    pub fn open_thermal(page: &str) {
        match std::process::Command::new(thermal_bin()).args(["ui", "--page", page])
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).spawn()
        {
            // Reaped on a thread: AntiWebUI has no SIGCHLD reaper, and a second `ui` hands off and exits at once.
            Ok(mut child) => { std::thread::spawn(move || { let _ = child.wait(); }); }
            Err(e) => crate::note(format_args!("minka-thermal ui: {e}")),
        }
    }
    ```
- **Tests,** next to `syu_parse_rules`: `thermal_parse_rules` (missing, malformed, `show:false`, each tone) and a `minkamon_hosts_pad` truth table.

### 14.2 `src/state.rs`
- **:8:** import `ThermalPill`.
- **Fields after :55:** `pub thermal: Signal<ThermalPill>` and `pub minkamon_running: Signal<bool>`.
- **Cells after :67:** `thermal_mtime: Rc<Cell<Option<SystemTime>>>` and `minkamon_pid: Rc<Cell<Option<u32>>>`. Initialise them at :86 and :95.
- **`start_services`** (after `poll_syu(true)` at :203): call `self.poll_thermal(true); self.poll_minkamon();` before the first tick, so the pill does not flash in Duo.
- **The `tick.is_multiple_of(4)` block (:260-264):** the same two calls, once a second.
- **`poll_thermal(force)`:** a copy of `poll_syu` (:294-304) with `set_if_changed`.
- **`poll_minkamon()`:**
  - it runs **only while `thermal.show` is true**; otherwise it clears the cache. A full scan costs about 4 ms on the UI thread;
  - it rechecks the cached PID: `/proc/<pid>/comm` = `minkamon` **and** the basename of `readlink /proc/<pid>/exe` starts with `minkamon`;
  - if the PID is gone, it rescans `/proc/*/comm` at most every 5 s (`tick % 20`).
- **Doc comment:** never probe `$XDG_RUNTIME_DIR/minkamon.lock` by connecting and closing: minka-instance reports that as `Event::TurnedAway`, and MinkaMon raises its window or lifts its pad layer on it (`MinkaMon/src/state.rs`, `service_tick` → `on_duplicate`). A request line (`minka_instance::request`) is dropped instead and the client gets `HungUp`, which shows MinkaMon is running without raising it, once the installed MinkaMon is a build with minka-instance (25/11/2026).

### 14.3 `src/bar.rs`
New `thermal_pill`, after `updates_pill` (:571-610) and before `health_dot` (:612). It needs no new imports beyond `icons::{WARNING, MODE_FAN, ERROR}`.
```rust
/// MinkaThermal's warning, an outlined md3e button like the updates pill. It gives way to
/// MinkaMon's own triangle while MinkaMon is running on the ScreenPad.
pub fn thermal_pill(state: AppState) -> Div {
    div().flex().flex_row().child(dyn_view(move || {
        let pill = state.thermal.get();
        if !pill.show
            || (settings::minkamon_hosts_pad(&state.settings.get(), &outputs())
                && state.minkamon_running.get())
        {
            return div();
        }
        let icon = match pill.kind.as_str() {
            "fan" => icons::MODE_FAN,
            "fault" => icons::ERROR,
            _ => icons::WARNING,
        };
        let tone = match pill.tone { settings::Tone::Red => RED, settings::Tone::Amber => WARN_AMBER, settings::Tone::Neutral => TEXT_MUTED };
        let page = pill.page.clone();
        styled_button(pill.label.clone(), ButtonStyle::outlined().size(ButtonSize::ExtraSmall).icon(icon))
            .dyn_style(move |s| s.border_color(color(tone)).text_color(color(tone)))
            .on_click(move |_| settings::open_thermal(&page))
    }))
}
```
- **Mount points:**
  - `bar_body`: after `.child(updates_pill(state.clone()))` at **bar.rs:658**;
  - Duo side column: after `.child(bar::updates_pill(state.clone()))` at **sidepanel.rs:50**.
- The top bar is never mapped in Duo (main.rs:77-84), so the two mounts give exactly one pill per layout.

### 14.4 Gate

| `shell.layout` | ScreenPad | MinkaMon (Rust) | Pill |
|---|---|---|---|
| duo or auto (hers) | present | running (on the pad) | **hidden**: MinkaMon shows the triangle |
| duo or auto | present | not running, or the QML fallback (`qs`) | shown in the side column |
| duo | disabled | floating on eDP-1 | shown |
| general, or auto without a pad | — | floating or closed | shown in the top bar |

- A dark pad hides the side column too, so there is no backlight test.
- The Quickshell fallback shell has no pill (no new QML). MinkaMon and the window still work then.

---

## 15. MinkaMon highlight (warning triangle + header badge)

### 15.1 New `src/thermal.rs` (register it in the mod list at `main.rs:3-12`)
- `ThermalFlag { show, kind, tone, triangle, pulse, badge, badge_short, actions: Vec<String>, page }`, with `parse(&str)` (reads `data["mon"]`, leniently) and `load()`.
- `state_path()`: the same rules as MinkaShell's `thermal_state_path`.
- `bin()`: `$MINKA_THERMAL_BIN`, else `"minka-thermal"`.
- `run(args)`: `std::process::Command` with null stdio and a reaper thread, as `processes.rs:156-161` does.
  - **Never use AntiWebUI's `Process::spawn`.** It kills the child when its owner is disposed (`anti-webui-wayland-shell/src/process.rs:177`), and these views rebuild on every resize.
- `alive(pid, boot_id) -> bool`: `/proc/<pid>/comm` = `minka-thermal` and `/proc/sys/kernel/random/boot_id` equal.
- `tone_hex()`: maps tones to the MinkaMon tokens `WARN_AMBER`, `RED`, `PURPLE`, `OK_GREEN` and `TEXT_MUTED`.
- `badge(state) -> Div` and `triangle_layer(state, size) -> Div`.

### 15.2 `src/state.rs`
- **Public fields (:97-131):** `thermal: Signal<ThermalFlag>`, `thermal_bright: Signal<bool>` (starts true), `thermal_off: Signal<bool>`.
- **Private cells:** `thermal_mtime` and `thermal_pulse_left: Rc<Cell<u32>>`.
- **`start_services` (:381):** call `poll_thermal(true)` before `schedule_service`.
- **`service_tick`, in the `tick % 5 == 1` block (:619-622), once a second:**
  - `poll_thermal(false)`, which reads only on an mtime change. A missing file reads as "off" and is not re-read every second, unlike `poll_settings`;
  - when the file exists, set `thermal_off` = !alive.
- **Pulse:**
  - when `show` turns on, or `kind` changes, with `pulse` set, set `thermal_pulse_left = 10`. This includes the first load at MinkaMon start;
  - every third tick (600 ms), while it is > 0, toggle `thermal_bright` and decrement; afterwards hold `bright` true;
  - the result is **five flashes in about 6 s, then steady**.

### 15.3 `src/schematic.rs`
- **Shared fan geometry:** lift the ZenBook fan centres (45,45 r30 and 279,45 r30, at :772-773) into `const ZENBOOK_FANS`, and the tower fans (:1014-1016, :1102) into `TOWER_FANS`. The structure drawing and the triangle use them.
- **New `fn triangle_layer(state, size)`**, beside `zone_layer` (:539-554):
  - it returns `div()` unless `flag.show && flag.triangle && state.alive.get()`. Without telemetry no board is drawn (:1166-1175), so there is nothing to put it on;
  - it is `div().absolute().inset(0.0).pointer_events_none()`, holding one `svg()` with `view_box(0, 0, w, h)` and `BoardGeom::new(w, h, zenbook)`;
  - **the triangle:** an equilateral, vertex-up triangle centred on the first fan disc (ZenBook (45, 45); tower: `TOWER_FANS[0]`), side about 1.7·r, rounded joins, filled in the tone (`WARN_AMBER`, or `RED` for a fault). The "!" (a bar and a dot) is drawn in `GROUND`, like the Material `warning` glyph;
  - **the pulse:** `.transition(Transition::ms(600))` with `.dyn_style(|s| s.opacity(if bright { 1.0 } else { 0.35 }))`. Only the opacity animates; the SVG is never rebuilt for the pulse.
- **Mount:** in `system_panel`'s map (:428-450), after the zone overlays: `.child(dyn_view(move || triangle_layer(glow_state.clone(), size.get())))`.
- **Header badge:** in `system_panel`'s header (:402-424), make the row `[thermal::badge][existing MAX °C text]`.
  - The badge is **15 px tall**, so the header keeps its fixed 23 px (`widgets.rs:52`) and `anchor_scene`'s `map_y = 71.0` (:234) stays correct.
  - **The label target:** an 11 px SVG triangle (when `flag.triangle`) plus `mono_text(badge, FONT_SIZE - 4.0, tone).letter_spacing(1.0)`, with a 1 px border in the tone. A click runs `run(["ui", "--page", page])`.
  - **Action targets:** sibling targets `▸ CHECK` (`ui --page check`) and `▸ LATER` (`snooze suspicion`). They are siblings, not nested, so a click on one does not reach the label.
  - **Narrow panels:** when the panel is narrower than 520 px (floating min_w is 440), show `badge_short`.
  - **Watch off:** while `thermal_off` is set, append a faint `TEXT_FAINT` tag `THERMAL WATCH OFF`. When nothing else shows, it stands alone. Nothing is shown when no state.json exists.
  - The badge sits outside the SVG, so it stays visible under "NO TELEMETRY". It works the same on the pad and floating.
- **No new top-bar chip:** that bar is about full at 620 px.
- **Tests,** next to :1189+: `ThermalFlag::parse` cases, and a check that `ZENBOOK_FANS` matches what `zenbook_structure` draws.

### 15.4 What she sees in MinkaMon
| State | Schematic | Header badge |
|---|---|---|
| WARNING | an amber triangle "!" on the fan flashes five times, then holds | `NEEDS A CLEAN · FIRST 3/12/2026 14:05` |
| SUSPICION | no triangle | a muted `DUST? SINCE 3/12/2026 ▸ CHECK ▸ LATER` |
| VERIFYING | no triangle | a purple `CLEANED 5/12/2026 · VERIFY COOLING ▸ CHECK` |
| FAN | amber triangle | `FAN SLOW 6100/6800 RPM` |
| FAULT | red triangle | `TCC NOT ACTING 99 °C` |
| 24 h after a check | no triangle | a green or muted `CHECK 5/12/2026: RESTORED` |

---

## 16. Repo edits outside MinkaThermal

- **`shojiwm-env.fish`:** nothing. The bar pill and MinkaMon run `minka-thermal ui` from PATH
  (the package's `/usr/bin/minka-thermal`); `MINKA_THERMAL_BIN` stays an override.
- **AGENTS.md component row,** after MinkaMon:
  `| **MinkaThermal** | Rust service + AntiWebUI window | Cooling watch. A user service compares sustained builds with a clean reference, warns when the pattern says dust (MinkaShell bar pill, hidden in Duo; MinkaMon amber warning triangle), logs the date of the first evidence, and stays until she marks the laptop cleaned. A 5-minute cooling check, started only by her, confirms and verifies. Duo seed from the 20/11/2026 clean. Light-load throttling (E2) is recorded but is not evidence: she turned it off on 21/11/2026, because bch-reconcile alone throttles the clean machine. |`
- **AGENTS.md build lines:**
  ```sh
  # MinkaThermal: two library crates (MinkaThermal/ = service + CLI, MinkaThermal/gui/ = window),
  # both in MinkaDE's one binary (minka/), which the minkade package installs
  cargo build --profile release-fast -p minka
  systemctl --user restart minka-thermal     # after a package upgrade: the service holds the old inode
  cargo test                                 # tests build Paths under target/test-tmp; they never read the env
  ```
- **AGENTS.md rule bullet:** agents never run `minka-thermal check` or `burn` (a 5-minute all-core stress test that only her click starts). They never run the mutating verbs (`cleaned`, `not-dust`, `snooze`, `fan-fixed`, `accept-fan`, `ack-fault`) against the live store, which holds her cleaning history. `status`, `sensors`, `history` and `replay` are read-only and fine.
- **README outline:**
  1. What it does and does not do (dust and blocked vents; not the room; fan wear and paste get their own lines).
  2. How it decides (§1).
  3. States and where each shows (§11.4).
  4. First start (§12.7).
  5. The CLI.
  6. Files and `replay`.
  7. Environment overrides: `MINKA_THERMAL_BIN`, `_GUI`, `_STATE`, `_SOCKET`.
  8. Duo and generic chassis.
  9. The window and `--shot`.
  10. What is not included.
- **Not included in v1:**
  - packaging (MinkaPKG);
  - a tray icon;
  - any QML, including a pill for the Quickshell fallback;
  - desktop notifications (probe notify-sends have landed on her session bus before);
  - ppd profile holds and D-Bus;
  - a `Zone::Cooling` in MinkaMon;
  - room modelling, NVMe temperatures, the unsensored second fan, and root-only RAPL energy.

---

## 17. Confounders

| Confounder | Handling |
|---|---|
| Room or season | Not modelled. Margins, multi-day majorities, the R2 guard, the check (no absolute temperature gate), the check's "room may be warmer" diagnosis line, and "Not dust" |
| Blocked intake (lap, bed) | Physically the same as dust. Persistence filters occasional use. The text says "dust or blocked vents"; the check prompts for a hard surface |
| AC or battery | Evidence on AC only. Battery runs the alarms only |
| Charger deficit | Recorded, not excluded. It matters only through PROCHOT, which xe `reason_prochot` and the cold rule catch. Blocks with > 2 % cold or PROCHOT are void, and counted |
| Profile, EPP, PL, TCC, lid | In the key. `power_capped` notices. A lower PL hides dust; a higher PL is a new key |
| Turbo bursts after busy dips | The dip guard, the Theil-Sen block slope, the AND rule, and R_duty learned from her builds and capped at 3 % |
| bch-reconcile (nice 19, about one core, AC only) | Inside the 28 W cap at the plateau. At light load it *is* the load. Its share and the thread count are recorded, and the light reference is learned on her own clean machine. *21/11/2026: alone, it throttled her clean machine 42-48 % at ≥ 86 °C, so E2 is off (§0).* |
| iGPU (xe) | Light windows need `act_freq` < 900 MHz. At the plateau it sits inside the package cap |
| dGPU MX450 | `runtime_status` is recorded; the check makes one `nvidia-smi` query. Passive use is not handled (residual) |
| Workload mix | Power is fixed at PL1; R_passive is learned from her builds and bounded; frequency votes only together with duty |
| BIOS change | Notice only |
| Suspend, stall, restart | Detected; open records voided; counters rebased |
| ananicy renice | Rates come from counter deltas over the measured Δt. A starved check is Inconclusive |

## 18. Overhead (read costs measured 20/11/2026; upper bounds)

- **Measured read costs:**
  - /proc/stat 11 µs;
  - coretemp 5 µs; x86_pkg_temp 4.6 µs;
  - throttle and avg_freq 0.4 µs each; xe 1-20 µs;
  - fan 131 µs; AC0 37 µs; BAT0/status 127 µs;
  - lid 1.7 ms.
- **Idle:** one wakeup a second, about 0.05 ms, plus the 5-s fan and power reads (about 60 µs a second averaged), plus the lid and config poll (about 2 ms a minute). Well under 0.01 % of one core.
- **Plateau:** about 0.25 ms a tick. **Check:** 4 wakeups a second while every core is already busy.
- **Occasional:** `/proc` scans of 5-15 ms at episode edges, at the end of hot light windows, and every 10 min for reconcile.
- **Memory:** a 900-row ring plus the tallies; RSS < 5 MB, since the service links no toolkit.
- **Disk:** no per-tick writes; about 10-30 KB a day of records.

## 19. Verify in the first checks and weeks (no extra stress)

1. Whether xe `reason_pl1` asserts at plateaus while `reason_prochot` stays 0. If it does, it becomes the authoritative power-limited test.
2. T_ss4 against T_ss (expected within 0.5 °C), and 4 Hz T_hs and D_jc against the provisional 56 / 30.
3. Two reference checks before 20/12/2026: the run-to-run spread, and they become R_active.
4. Qualifying plateaus per 14 days on her cargo builds. Under 5 → lower the open threshold to B10 ≥ 0.75 or relax the dip guard; judge with `replay`.
5. R_passive − R_active: expected within ±2 °C, capped at +3.
6. The learned clean `light_p90` at 1-2 threads (bch-reconcile, links), and so the L line. *21/11/2026: bch-reconcile alone throttled the clean machine 42-48 % at ≥ 86 °C, so E2 is off (§0) and nothing is learned.*
7. Charger deficit at full load, and whether it ever comes with PROCHOT.
8. Whether a ppd profile switch rewrites the MMIO PL.
9. Whether the badge's sibling click targets in MinkaMon behave.
10. Numbers for a UX482EA, if one ever appears.

## 20. Residual risks (accepted)

- **Could false-alarm:**
  - a hot summer of three weeks or more (route d), or a check run on a hot day. "Not dust" handles it;
  - habitual building on a soft surface;
  - a PROCHOT burst at 88-91 °C booked as thermal;
  - heavy MX450 use during builds;
  - a MinkaConf window that was already open writing back `thermal.chassis`.
- **Could miss dust:**
  - rarely sustaining ≥ 120 s at ≥ 85 % busy, or working in unreferenced keys. The unjudged counter and the starvation notice make this visible;
  - dust that stays just under +3 °C / 3 %; the drift line shows it;
  - a lower PL masking dust;
  - the second fan, which has no sensor;
  - *21/11/2026, E2 off (§0):* dust that shows only at light load. Builds and the check must show it.
- **Not fixable by cleaning:** paste or contact wear is detected, and the post-clean verdicts point at it.

## 21. Implementation order

0. **Before the GUI phase, ask Sophie the §22 questions.**
1. Core: `paths`, `clock`, `sensors`, `sampler`, `store`, and the `sensors` and `status` verbs.
2. `episodes`, `rules`, `profile`, `present`, records, `replay`, and the `tests/seed.rs` and `tests/rules.rs` traces.
3. `ipc` and the action verbs.
4. `check` and `burn`. Agents unit-test these with synthetic rows only.
5. The GUI, with `--shot` fixtures for every status.
6. The MinkaShell pill: settings.rs, state.rs, then bar.rs and sidepanel.rs.
7. The MinkaMon triangle and badge: thermal.rs, state.rs, then schematic.rs.
8. AGENTS.md, the README, the `shojiwm-env.fish` line, and her checkout wiring (§12.7).

## 22. For Sophie (gaps per the AGENTS.md rule, and confirmations)

1. **Toolkit gaps.**
   - md3e has **no chart**: the check chart would be hand-drawn SVG, as MinkaMon's charts are.
   - md3e has **no data table**, and chips, list rows and the date picker have no compact size: History, References and the evidence table would be dense mono grids.
   - Should these go into the AntiWebUI fork instead?
   - Tooltips are untested on a layer surface; v1 uses none.
2. **"Not dust" and "Accept fan as normal"** also clear a warning. Her words were "only disappear after the user tells it the system has been cleaned". Keep them as the false-alarm lever ("deal with it as and when relevant"), or remove them?
3. **The bar pill uses the same amber as the triangle** (and red only for a cooling fault), so the warning looks the same everywhere. Is that OK?

---

### Review fixes rejected or changed

- **R2 guard kept** (the critic proposed dropping it as room engineering). It is one comparison: a clean plateau at 28 W refutes dust at lower power, whatever the room.
- **E2 as a precondition:** relaxed, not removed. Routes (b) and (c) still use E2 as the fast paths; the critic's route (d) adds the slower plateau-only path, so no stress test is needed to turn red. *21/11/2026: E2 is off (§0), so (d) is the only passive route to WARNING.*
- **Lid read:** with the 60-s config poll as well as at edges and gates, not at edges only. Light windows need the lid for their key, and 1.7 ms a minute is negligible.
- **Fallback writer race:** chose "mutating verbs need the service" over "the service waits for the lock". It is simpler, and the service stays the only writer.
- **Chassis option:** built as a window control rather than only telling her. Her request names a Duo option.
- **Tones:** the integration spec's red WARNING glow was replaced by her own decision, the warnAmber warning triangle with "!". The glow and the `fan_shapes_in` refactor are dropped.
- **The "Cooling check?" pill** (detection §0 row 18) is dropped. Route (d) closes the gap it existed for.
- **Room diagnosis line:** kept inside the check, where it uses data the check already has. Only the passive post-load event and room label were cut.
- **Additive duty margins:** written as `min(k·R, R + n pp)`, so the rule stays continuous with the multiplicative one and the seed thresholds are unchanged.
- **4 Hz sampling:** removed from passive episodes entirely, which goes further than "optional". It now runs only during the check.

---

## 23. Review notes (20/11/2026, before implementation)

- **The effective power limit is MMIO: PL1 28 W, PL2 35 W, τ 27.98 s** (read live on 20/11/2026). The MSR domain
  `intel-rapl:0` reads 60/60 W, and that is what `run.sh`'s meta.txt recorded; the package obeys the lower pair. §1.5
  and §4 already use MMIO. Older notes that say "PL1 = PL2 = 60 W" are wrong.
- **MinkaMon draws the triangle with `svg()` paths, never an md3e icon.** MinkaMon's anti-webui dependency has no
  `svg` feature. That feature only gates resvg decoding, which md3e's icon glyphs use, so an icon would draw nothing
  and give no error (the MinkaShell icon bug of 13/11/2026). The `svg()` element itself needs no feature: MinkaMon's
  charts and globe already use it. The same applies to the badge's 11 px triangle.
- **Every sysfs path in §3.2 exists on this machine:**
  - xe `card0/device/tile0/gt0/freq0/throttle/{status,reason_pl1,reason_prochot,reason_thermal}`;
  - `cpuinfo_avg_freq`;
  - `/proc/acpi/button/lid/LID`;
  - the zones `x86_pkg_temp`, `SEN2`, `SEN4` and `acpitz`;
  - the `Processor` and `intel_powerclamp` cooling devices at 0, and `TCC Offset` at 8;
  - `core_throttle_total_time_ms`;
  - TCC offset 8 and `temp1_crit` 100 °C, so TCC is 92 °C.
- **The Duo gate is a refinement of her rule.** The pill hides in Duo **while MinkaMon is running on the pad**, and
  shows if MinkaMon has been closed or crashed, so the warning never disappears from every screen.
