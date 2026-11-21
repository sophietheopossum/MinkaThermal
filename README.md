# MinkaThermal

A cooling watch for the Zenbook Duo (and, more roughly, any laptop). A small user service
compares sustained builds with a clean reference, and warns when the pattern says the laptop
needs cleaning. It records light-load throttling too, but since 21/11/2026 that counts for
nothing (section 2). It logs the date and time the first evidence appeared, and the warning
stays until you say the laptop has been cleaned. A 5-minute cooling check, which only you
start, confirms a suspicion and verifies a clean.

The full design is [DESIGN.md](DESIGN.md). This file is the short version.

**Built so far:** the service, the CLI and the cooling check (`minka-thermal`). **Not built
yet:** the window (`minka-thermal-gui`), the MinkaShell bar pill and the MinkaMon warning
triangle. Until they exist, `minka-thermal status` and `minka-thermal history` are the only
places a warning shows.

## 1. What it does and does not do

- It watches for **dust and blocked vents**: the cooling path getting worse at a known power.
- It does **not** model the room. A hot summer can look like dust; "Not dust" is the answer to
  that (section 3 below).
- A slow or stopped fan and a cooling fault (TCC not acting) get their own warnings. They never
  count as dust, and cleaning may not fix them.
- Worn paste or poor contact is detected like dust, and the verdict after a clean points at it
  ("Not improved").
- It never runs a stress test by itself.

## 2. How it decides

- **It compares only where the power is known.**
  - **Builds (E1):** all-core load held for 2 minutes or more. The package is held at PL1 =
    28 W, so the steady temperature and the share of time spent throttling measure the cooling.
  - **Light load (E2):** 5-30 % busy (about one or two threads, such as bch-reconcile). A clean
    machine rarely throttles here; the dusty one on 17/11/2026 throttled about 40 % of the time.
    **Off since 21/11/2026, by your decision:** on the clean machine, bch-reconcile alone
    throttled 42-48 % of the time at 86 °C or more, so E2 would warn about a clean laptop.
    Light-load windows are still recorded, but never count, and the light reference is not
    learned. Builds and the cooling check decide on their own.
  - Idle temperature and the since-boot throttle counters are shown but never used as evidence.
- **References are frozen.** The Duo seed comes from the clean of 20/11/2026. Builds (and light
  load, while E2 is on) are learned only inside a 30-day known-clean window, within bounds, and
  never drift by themselves.
- **Evidence has tiers.** One evidence day opens a case (logged only). Repeated days over a
  fortnight give a suspicion. Persistence over the next two to three weeks, or a cooling check
  that reads worse than the reference, latches the warning.
- Power limits come from `intel-rapl-mmio:0` only. The MSR domain's 60/60 W is a decoy.

## 3. States and where each shows

| State | Meaning | Where it shows (once the window, pill and triangle exist) |
|---|---|---|
| LEARNING | this config has no reference yet | window |
| CLEAN | normal | nowhere |
| WATCH | first evidence day; a case is open | window only |
| SUSPICION | evidence on several days | MinkaMon note `DUST? SINCE 3/12/2026` |
| WARNING | latched; stays until Cleaned or Not dust | amber bar pill (hidden in Duo while MinkaMon runs on the pad), MinkaMon amber triangle and header badge, window |
| VERIFYING | you said it was cleaned; waiting for a check or two weeks of builds | MinkaMon purple "verify cooling" |
| FAN | fan slow or stopped | amber pill and triangle |
| FAULT | TCC not acting | red pill and triangle |

- **Cleaned** removes the pill at once. A cooling check then says Restored, Improved, Not
  improved (the warning comes back) or Inconclusive. Without a check, the builds of the next one
  to two weeks give the verdict.
- **Not dust (accept as normal)** closes the case and makes the recent behaviour (the last
  check, or the last week of builds) the reference.
  Use it when the room, a new BIOS or a settings change is the real cause.

## 4. First start

The unit file says `/usr/bin/minka-thermal`, which is the packaged path, so a checkout needs a
drop-in. Build first (`cargo build --release`), then create
`~/.config/systemd/user/minka-thermal.service.d/checkout.conf` (the portal's `override.conf`
pattern) with:

```ini
[Service]
ExecStart=
ExecStart=%h/Documents/src/MinkaDE/MinkaThermal/target/release/minka-thermal service
```

and run:

```sh
systemctl --user link /home/seirra/Documents/src/MinkaDE/MinkaThermal/dist/minka-thermal.service
ln -s ~/Documents/src/MinkaDE/MinkaThermal/target/release/minka-thermal ~/.local/bin/minka-thermal
systemctl --user enable --now minka-thermal.service
minka-thermal cleaned --date 20/11/2026
```

- The last line answers "When was this laptop last cleaned?" (the window's setup card will ask
  the same). It opens the known-clean window to **20/12/2026**; `minka-thermal history` shows it.
- Then run **two reference checks on different days before 20/12/2026**
  (`minka-thermal check`, about 5 minutes of all-core load each, on a hard surface, on AC, in
  the performance profile). They replace the seed as the reference.
- **After each `cargo build --release`:** `systemctl --user restart minka-thermal`. The running
  service keeps the old binary until then, and a restart loses only a build in progress.
- **Launcher:** copy `dist/minka-thermal.desktop` to
  `~/.local/share/applications/MinkaThermal.desktop` with absolute `Exec=` paths, once the
  window exists.

## 5. The CLI

| Verb | Needs the service | What it does |
|---|---|---|
| `status [--json]` (the default) | no | the current state, with the first-evidence date |
| `sensors` | no | every sensor found, its value and path, the chassis, the config key |
| `history [--since d/M/yyyy]` | no | the event history: cases, first evidence, cleans, checks |
| `replay [--since d/M/yyyy]` | no | re-judges stored records with today's rules; read-only |
| `ui [--page status\|check\|history\|references]` | no | opens the window (not built yet) |
| `check [--yes] [--anyway] [--detach]` | yes | the 5-minute cooling check. Asks y/N first; Ctrl-C cancels; `--detach` returns once it is accepted; `--anyway` runs under other settings and marks the result not comparable |
| `cancel` | yes | cancels a running check |
| `cleaned [--date d/M/yyyy]` | yes | "I've cleaned it", today by default |
| `not-dust [--yes]` | yes | accept as normal; asks y/N first |
| `snooze suspicion` | yes | hides MinkaMon's CHECK and LATER for 3 days |
| `fan-fixed`, `accept-fan`, `ack-fault` | yes | clear FAN (re-learning the fan speed at the next check, or accepting it) and FAULT |
| `service` | — | run by the unit only |

- Dates are always `d/M/yyyy`.
- The verbs that change something go through the service and never write the store themselves.
  Without the service they print `service not running: systemctl --user start minka-thermal`.
- **Exit codes:** 0 OK, 1 error, 2 usage, 3 service not running, 4 already running.
- **Agents** never run `check` or `burn`, and never run the mutating verbs against the live
  store, which holds the cleaning history. `status`, `sensors`, `history` and `replay` are
  read-only.

## 6. Files and `replay`

Everything lives in `~/.local/state/minka-thermal/` (mode 0700). The service is the only writer.

| File | Holds |
|---|---|
| `state.json` | the current state, plus the ready-made `pill` and `mon` blocks the bar and MinkaMon read |
| `references.json` | references per config key (the Duo seed is compiled in) |
| `events.jsonl` | the human history, never pruned; `history` prints it |
| `records.jsonl` | this month's build and light-load records; older months as `records-YYYY-MM.jsonl` |
| `daily.jsonl` | one line per day |
| `cases.jsonl` | closed cases: the history of dust intervals |
| `checks/YYYY-MM-DD_HHMM/` | each check's `samples.csv`, `meta.json` and `result.json` |

The socket is `$XDG_RUNTIME_DIR/minka-thermal.sock`. `replay` is the tuning lever: it prints
which stored verdicts and evidence days would change under the current rules and references,
and writes nothing. The service's own log goes to the journal
(`journalctl --user -u minka-thermal`).

## 7. Environment overrides

| Variable | Effect |
|---|---|
| `MINKA_THERMAL_BIN` | the CLI the bar pill and MinkaMon run as `minka-thermal ui` (set in `shojiwm-env.fish`) |
| `MINKA_THERMAL_GUI` | the window binary `ui` runs; otherwise beside the CLI, then `gui/target/<profile>/` in a checkout, then PATH |
| `MINKA_THERMAL_STATE` | moves the whole store (every file sits beside `state.json`); the socket moves with it |
| `MINKA_THERMAL_SOCKET` | the socket path |

The last two are for tests and fixtures. Never export them from `shojiwm-env.fish`; the unit
does not read fish's environment anyway. `XDG_STATE_HOME`, if it is ever set, must be set in
the user manager too (`systemctl --user import-environment XDG_STATE_HOME`).

## 8. Duo and generic chassis

`thermal.chassis` in `~/.config/minka-settings.json` is `auto` (the default), `duo` or
`generic`. `auto` picks Duo on an ASUS UX482E board.

- **Duo** uses the chassis constants (fan 6800-7000 rpm at a plateau, stages near 5000, 6100 and
  6900) and the seed from 20/11/2026, which applies to her normal settings: AC, performance,
  PL1 28 W, PL2 35 W, TCC 92 °C, lid open. Other settings learn their own references.
- **Generic** finds the temperature, fan and throttle counters by name and learns everything.
  Its first start should record a reference check. Signals that are missing drop the rules that
  need them; `sensors` lists the classes still active.

## 9. The window and `--shot`

Not built yet (DESIGN.md §13). It will have four pages (Status, Check, History, References) and
the chassis choice, and `minka-thermal-gui --shot <page> <file.png>` will render a page from a
`state.json` fixture without a window. Until then `minka-thermal ui` names the paths it tried
and exits 1.

## 10. What is not included

- Packaging (MinkaPKG), a tray icon, desktop notifications, power-profile holds and D-Bus.
- Any QML, including a pill for the Quickshell fallback.
- Room modelling, NVMe temperatures, the second fan (it has no sensor) and RAPL energy, which
  needs root.
- Not yet: the window, the bar pill and the MinkaMon triangle (see the top of this file).
