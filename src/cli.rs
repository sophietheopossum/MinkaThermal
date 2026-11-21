//! The CLI verbs (§12.2). `main.rs` parses the arguments with `parse`, resolves `Paths` from the
//! environment and hands both to `run`.

use crate::clock::{Date, Moment};
use crate::episodes::percentile;
use crate::ipc::{self, CallError};
use crate::model::{
    DailyRecord, Event, FaultStatus, LightVerdict, PlateauVerdict, Record, References, State,
    Status,
};
use crate::paths::Paths;
use crate::profile::{self, Chassis, ConfigKey, DUO};
use crate::references::KeyView;
use crate::rules::{JudgeCtx, LIGHT_LOAD_EVIDENCE, judge_light, judge_plateau};
use crate::sampler::{busy_fraction, scan_comms};
use crate::sensors::{self, Sensors, StaticInfo};
use crate::{note, present, store};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NO_SERVICE: i32 = 3;
pub const EXIT_RUNNING: i32 = 4;

pub const USAGE: &str = "\
usage: minka-thermal [verb]
  status [--json]              summary of state.json (the default)
  sensors                      discovered sensors, chassis, config key and classes
  history [--since d/M/yyyy]   the event history
  replay [--since d/M/yyyy]    re-evaluate stored records under the current rules
  ui [--page status|check|history|references]
  check [--yes] [--anyway] [--detach]
  cancel
  cleaned [--date d/M/yyyy]
  not-dust [--yes]
  snooze suspicion
  fan-fixed | accept-fan | ack-fault
  service                      run by the systemd user unit only
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Status,
    Check,
    History,
    References,
}

impl Page {
    pub fn as_str(&self) -> &'static str {
        match self {
            Page::Status => "status",
            Page::Check => "check",
            Page::History => "history",
            Page::References => "references",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Status {
        json: bool,
    },
    Sensors,
    History {
        since: Option<Date>,
    },
    Replay {
        since: Option<Date>,
    },
    Ui {
        page: Page,
    },
    Check {
        yes: bool,
        anyway: bool,
        detach: bool,
    },
    Cancel,
    Cleaned {
        date: Option<Date>,
    },
    NotDust {
        yes: bool,
    },
    SnoozeSuspicion,
    FanFixed,
    AcceptFan,
    AckFault,
    Service,
    Burn {
        threads: usize,
    },
}

/// Parses the verb and its flags. Err carries the reason for a usage error (exit 2).
pub fn parse(args: &[String]) -> Result<Command, String> {
    let verb = args.first().map(String::as_str).unwrap_or("status");
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let flags = |allowed: &[&str]| -> Result<Vec<&str>, String> {
        match rest.iter().find(|a| !allowed.contains(a)) {
            Some(bad) => Err(format!("{verb}: unexpected '{bad}'")),
            None => Ok(rest.clone()),
        }
    };
    let value = |flag: &str| -> Result<Option<&str>, String> {
        match rest.as_slice() {
            [] => Ok(None),
            [f, v] if *f == flag => Ok(Some(*v)),
            _ => Err(format!("{verb}: expected nothing or {flag} <value>")),
        }
    };
    let date = |flag: &str| -> Result<Option<Date>, String> {
        value(flag)?
            .map(|v| {
                Date::parse_dmy(v).ok_or_else(|| format!("{verb}: '{v}' is not a date in d/M/yyyy"))
            })
            .transpose()
    };
    Ok(match verb {
        "help" | "-h" | "--help" => Command::Help,
        "status" => Command::Status {
            json: flags(&["--json"])?.contains(&"--json"),
        },
        "sensors" => {
            flags(&[])?;
            Command::Sensors
        }
        "history" => Command::History {
            since: date("--since")?,
        },
        "replay" => Command::Replay {
            since: date("--since")?,
        },
        "ui" => Command::Ui {
            page: match value("--page")? {
                None | Some("status") => Page::Status,
                Some("check") => Page::Check,
                Some("history") => Page::History,
                Some("references") => Page::References,
                Some(p) => return Err(format!("ui: unknown page '{p}'")),
            },
        },
        "check" => {
            let f = flags(&["--yes", "--anyway", "--detach"])?;
            Command::Check {
                yes: f.contains(&"--yes"),
                anyway: f.contains(&"--anyway"),
                detach: f.contains(&"--detach"),
            }
        }
        "cleaned" => Command::Cleaned {
            date: date("--date")?,
        },
        "not-dust" => Command::NotDust {
            yes: flags(&["--yes"])?.contains(&"--yes"),
        },
        "snooze" if rest == ["suspicion"] => Command::SnoozeSuspicion,
        "snooze" => return Err("snooze: the only thing to snooze is 'suspicion'".to_owned()),
        "cancel" | "fan-fixed" | "accept-fan" | "ack-fault" | "service" => {
            flags(&[])?;
            match verb {
                "cancel" => Command::Cancel,
                "fan-fixed" => Command::FanFixed,
                "accept-fan" => Command::AcceptFan,
                "ack-fault" => Command::AckFault,
                _ => Command::Service,
            }
        }
        "burn" => match value("--threads")?.map(str::parse::<usize>) {
            Some(Ok(n)) if n > 0 => Command::Burn { threads: n },
            _ => return Err("burn: expected --threads N".to_owned()),
        },
        other => return Err(format!("unknown verb '{other}'")),
    })
}

/// Runs a parsed command. `root` is the sysfs/procfs root ("/" outside tests).
pub fn run(cmd: Command, paths: &Paths, root: &Path) -> i32 {
    match cmd {
        Command::Help => {
            out(USAGE);
            EXIT_OK
        }
        Command::Status { json } => status(paths, root, json),
        Command::Sensors => sensors(paths, root),
        Command::History { since } => history(paths, since),
        Command::Replay { since } => replay(paths, root, since),
        Command::Service => crate::service::main(paths, root),
        Command::Cleaned { date } => {
            let params = match date {
                Some(d) => json!({ "date": d.key() }),
                None => json!({}),
            };
            act(paths, "case.cleaned", params, |s| {
                let until = s
                    .known_clean_until
                    .as_deref()
                    .and_then(Date::parse_key)
                    .map(|d| format!("; known clean until {}", d.label()))
                    .unwrap_or_default();
                format!(
                    "Cleaning recorded. Status {}{until}.",
                    s.status.as_str().to_uppercase()
                )
            })
        }
        Command::NotDust { yes } => not_dust(paths, yes, is_tty(0), &mut io::stdin().lock()),
        Command::SnoozeSuspicion => act(
            paths,
            "case.snooze",
            json!({ "what": "suspicion" }),
            |s| match &s.snooze.suspicion_actions_until {
                Some(t) => format!("Suspicion actions hidden until {}.", t.label()),
                None => "Suspicion actions hidden.".to_owned(),
            },
        ),
        Command::FanFixed => act(paths, "fan.fixed", json!({}), |_| {
            "Fan marked fixed: the next cooling check re-learns its speed.".to_owned()
        }),
        Command::AcceptFan => act(paths, "fan.accept", json!({}), |s| {
            match (s.fan.accepted, s.fan.r_fan) {
                (true, Some(f)) => format!("Fan accepted as normal at {f} rpm."),
                _ => "Fan warning accepted.".to_owned(),
            }
        }),
        Command::AckFault => act(paths, "fault.ack", json!({}), |_| {
            "Cooling fault acknowledged.".to_owned()
        }),
        Command::Cancel => match ipc::call(&paths.socket, "check.cancel", json!({})) {
            Ok(_) => {
                out("Cooling check cancelled.\n");
                EXIT_OK
            }
            Err(e) => call_failed(e),
        },
        Command::Check {
            yes,
            anyway,
            detach,
        } => {
            let mut stdout = io::stdout();
            if let Some(code) =
                confirm_check(paths, yes, is_tty(0), &mut io::stdin().lock(), &mut stdout)
            {
                return code;
            }
            // From here Ctrl-C cancels the check instead of leaving it running unwatched. Before
            // this point it simply ends the CLI, since nothing has started.
            let sigint = if detach {
                None
            } else {
                crate::service::signal_fd(&[libc::SIGINT]).ok()
            };
            let cancel = sigint.as_ref().map(AsRawFd::as_raw_fd);
            follow_check(paths, anyway, detach, cancel, &mut stdout)
        }
        // Dispatched by main.rs before Paths are resolved.
        Command::Ui { .. } | Command::Burn { .. } => EXIT_USAGE,
    }
}

/// "the load starts…" → "The load starts…".
fn sentence(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// `check` before anything starts: on a terminal it says what the check is for and asks y/N
/// (§10.1); without a terminal it needs `--yes`. None means go ahead; Some is the exit code.
pub(crate) fn confirm_check(
    paths: &Paths,
    yes: bool,
    tty: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Option<i32> {
    if yes {
        return None;
    }
    if !tty {
        note(format_args!(
            "check: confirm with --yes when not run from a terminal"
        ));
        return Some(EXIT_USAGE);
    }
    // Ask only once it is known that the service is there to run it.
    let state: State = match ipc::call(&paths.socket, "state.get", json!({})) {
        Ok(v) => serde_json::from_value(v).unwrap_or_default(),
        Err(e) => return Some(call_failed(e)),
    };
    let purpose = present::check::purpose(crate::check::purpose_for(state.status));
    let _ = write!(
        output,
        "Cooling check ({purpose}): about five minutes, three of them at full load.\n\
         Before it starts: {}.\n{}.\nStart? [y/N] ",
        present::check::PROMPT,
        sentence(present::check::WAITING)
    );
    let _ = output.flush();
    let mut answer = String::new();
    let _ = input.read_line(&mut answer);
    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        let _ = writeln!(output, "Nothing started.");
        return Some(EXIT_ERROR);
    }
    None
}

/// Starts the check and follows its events until the verdict (§12.2). Readable `cancel` (the
/// SIGINT signalfd) sends `check.cancel` and waits for the check to end; `detach` returns as
/// soon as the service has accepted it.
pub(crate) fn follow_check(
    paths: &Paths,
    anyway: bool,
    detach: bool,
    mut cancel: Option<RawFd>,
    output: &mut impl Write,
) -> i32 {
    let mut stream = match UnixStream::connect(&paths.socket) {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return call_failed(CallError::NotRunning);
        }
        Err(e) => {
            return call_failed(CallError::Io(format!("{}: {e}", paths.socket.display())));
        }
    };
    let start = json!({"id": 1, "method": "check.start",
                       "params": {"confirm": true, "anyway": anyway}});
    if let Err(e) = stream
        .write_all(format!("{start}\n").as_bytes())
        .and_then(|()| stream.set_nonblocking(true))
    {
        return call_failed(CallError::Io(format!("{}: {e}", paths.socket.display())));
    }
    let mut follow = Follow::default();
    let mut buf = Vec::new();
    loop {
        let mut fds = vec![libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        if let Some(fd) = cancel {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // SAFETY: `fds` is a valid array of `fds.len()` pollfd structs for the whole call.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 1000) };
        if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            let e = io::Error::last_os_error();
            return call_failed(CallError::Io(e.to_string()));
        }
        if fds.get(1).is_some_and(|f| f.revents != 0) {
            let mut sink = [0u8; 128];
            // SAFETY: reads at most the buffer's length into it.
            let n = unsafe { libc::read(fds[1].fd, sink.as_mut_ptr().cast(), sink.len()) };
            if n <= 0 {
                // Readable with nothing to read is a hang-up, not a signal (a signalfd never
                // hangs up; the tests' pipe does): stop watching it rather than spin.
                cancel = None;
            } else if follow.cancelled {
                // A second Ctrl-C: the cancel is with the service, which may not be answering.
                // Stop following rather than leave her a terminal that ignores her.
                let _ = writeln!(
                    output,
                    "Stopped following. The cancel was sent; `minka-thermal status` shows the check."
                );
                return EXIT_ERROR;
            } else {
                follow.cancelled = true;
                let _ = writeln!(output, "Cancelling the cooling check…");
                let req = json!({"id": 2, "method": "check.cancel"});
                if stream.write_all(format!("{req}\n").as_bytes()).is_err() {
                    let _ = writeln!(output, "The service is gone.");
                    return EXIT_ERROR;
                }
            }
        }
        let mut chunk = [0u8; 16 * 1024];
        let mut closed = false;
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(k) => buf.extend_from_slice(&chunk[..k]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return call_failed(CallError::Io(e.to_string())),
            }
        }
        // Lines that came in before the close still count: the verdict may be among them.
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if let Some(code) = follow.handle(&msg, detach, output) {
                return code;
            }
        }
        if closed {
            let _ = writeln!(
                output,
                "The service closed the connection before the check ended."
            );
            return EXIT_ERROR;
        }
    }
}

/// What the CLI has shown of a running check, so it prints changes and not every second.
#[derive(Debug, Default)]
struct Follow {
    cancelled: bool,
    phase: String,
    failing: String,
    last_gate_print: Option<f64>,
    last_progress: Option<f64>,
}

/// Progress lines while the check runs: one every this many seconds, and at each phase.
const PROGRESS_EVERY_S: f64 = 30.0;

impl Follow {
    fn handle(&mut self, msg: &Value, detach: bool, output: &mut impl Write) -> Option<i32> {
        let id = msg.get("id");
        if id == Some(&json!(1)) {
            if let Some(e) = msg.get("error") {
                let text = e["message"].as_str().unwrap_or("refused");
                let _ = writeln!(output, "Not started: {text}");
                return Some(EXIT_ERROR);
            }
            let dir = msg["result"]["dir"].as_str().unwrap_or("?");
            let _ = writeln!(output, "Cooling check accepted: {dir}");
            if detach {
                let _ = writeln!(
                    output,
                    "It runs in the service; `minka-thermal cancel` stops it."
                );
                return Some(EXIT_OK);
            }
            return None;
        }
        if id == Some(&json!(2)) {
            if let Some(m) = msg["error"]["message"].as_str() {
                let _ = writeln!(output, "{m}");
            }
            return None;
        }
        if msg["event"] != "check" {
            return None;
        }
        let p = &msg["payload"];
        let st = &p["state"];
        let phase = st["state"].as_str().unwrap_or_default();
        if matches!(phase, "done" | "refused" | "aborted") {
            return Some(finish_check(p, output));
        }
        let phase_s = st["phase_s"].as_u64().unwrap_or(0);
        if phase != self.phase {
            self.phase = phase.to_owned();
            self.last_progress = None;
            let _ = match phase {
                "gating" => writeln!(output, "Waiting: {}.", present::check::WAITING),
                "idle" => writeln!(output, "Idle {phase_s} s"),
                "load" => writeln!(output, "Load {phase_s} s"),
                "recovery" => writeln!(output, "Recovery {phase_s} s"),
                _ => Ok(()),
            };
        }
        let live = &p["live"];
        if phase == "gating" {
            let gates = live["gates"].as_array()?;
            let failing: Vec<&Value> = gates.iter().filter(|g| g["ok"] == false).collect();
            let ids: String = failing
                .iter()
                .filter_map(|g| g["id"].as_str())
                .collect::<Vec<_>>()
                .join(",");
            let waited = live["waited_s"].as_f64().unwrap_or(0.0);
            let due = self
                .last_gate_print
                .is_none_or(|t| waited - t >= PROGRESS_EVERY_S);
            if !failing.is_empty() && (ids != self.failing || due) {
                self.failing = ids;
                self.last_gate_print = Some(waited);
                let texts: Vec<&str> = failing.iter().filter_map(|g| g["text"].as_str()).collect();
                let busiest: Vec<String> = live["busiest"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| Some(format!("{} {:.0} %", b[0].as_str()?, b[1].as_f64()? * 100.0)))
                    .collect();
                let mut line = format!("  {:>3.0} s  {}", waited, texts.join("; "));
                if !busiest.is_empty() {
                    line += &format!("; busiest: {}", busiest.join(", "));
                }
                let _ = writeln!(output, "{line}");
            }
            return None;
        }
        let elapsed = live["phase_elapsed_s"].as_f64().unwrap_or(0.0);
        if self
            .last_progress
            .is_none_or(|t| elapsed - t >= PROGRESS_EVERY_S)
        {
            self.last_progress = Some(elapsed);
            let mut parts = vec![format!("  {phase} {elapsed:>3.0}/{phase_s} s")];
            if let Some(t) = live["T"].as_f64() {
                parts.push(format!("T {} °C", present::c(t)));
            }
            if let Some(t) = live["T4_max"].as_f64() {
                parts.push(format!("T4 max {} °C", present::c(t)));
            }
            if let Some(f) = live["fan"].as_f64() {
                parts.push(format!("fan {f:.0} rpm"));
            }
            if let Some(f) = live["f"].as_f64() {
                parts.push(format!("{f:.0} MHz"));
            }
            if let Some(d) = live["duty"].as_f64().filter(|_| phase != "idle") {
                parts.push(format!("throttled {}", present::pct(d)));
            }
            let _ = writeln!(output, "{}", parts.join(" · "));
        }
        None
    }
}

/// The last `check` event: the verdict with its comparison table, or why it did not finish.
fn finish_check(p: &Value, output: &mut impl Write) -> i32 {
    let st = &p["state"];
    let reason = st["reason"].as_str().unwrap_or_default();
    match st["state"].as_str() {
        Some("refused") => {
            let _ = writeln!(output, "Refused: {reason}");
            return EXIT_ERROR;
        }
        Some("aborted") if reason == present::check::CANCELLED => {
            let _ = writeln!(output, "Cancelled: nothing becomes a reference.");
            return EXIT_ERROR;
        }
        Some("aborted") => {
            let _ = writeln!(output, "Aborted: {reason}");
            return EXIT_ERROR;
        }
        _ => {}
    }
    let d = &p["detail"];
    let label = p["result"]["label"].as_str().unwrap_or("done");
    let _ = writeln!(output, "Result: {label}");
    let cmp = &d["comparison"];
    let columns: Vec<&Value> = ["reference", "before", "check"]
        .iter()
        .map(|k| &cmp[*k])
        .filter(|c| c.is_object())
        .collect();
    if !columns.is_empty() {
        type Cell = fn(f64) -> String;
        let rows: [(&str, &str, Cell); 8] = [
            ("T_ss", "t_ss", |v| format!("{} °C", present::c(v))),
            ("duty", "duty", present::pct),
            ("fan", "fan", |v| format!("{v:.0} rpm")),
            ("f", "f", |v| format!("{v:.0} MHz")),
            ("work", "work", |v| format!("{v:.1} M/s")),
            ("T_hs", "t_hs", |v| format!("{} °C", present::c(v))),
            ("D_jc", "d_jc", |v| format!("{} °C", present::c(v))),
            ("t_cool", "t_cool", |v| format!("{v:.1} s")),
        ];
        let mut table: Vec<Vec<String>> = vec![
            std::iter::once(String::new())
                .chain(columns.iter().map(|c| c["label"].as_str().unwrap_or("").to_owned()))
                .collect(),
        ];
        for (name, key, cell) in rows {
            if columns.iter().all(|c| c[key].is_null()) {
                continue;
            }
            table.push(
                std::iter::once(name.to_owned())
                    .chain(columns.iter().map(|c| c[key].as_f64().map(cell).unwrap_or_default()))
                    .collect(),
            );
        }
        let widths: Vec<usize> = (0..table[0].len())
            .map(|i| table.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
            .collect();
        for r in &table {
            let cells: Vec<String> = r
                .iter()
                .zip(&widths)
                .map(|(c, w)| format!("{c:<w$}"))
                .collect();
            let _ = writeln!(output, "  {}", cells.join("   ").trim_end());
        }
    }
    let list = |key: &str| -> Vec<String> {
        d[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    for r in list("inconclusive") {
        let _ = writeln!(output, "  inconclusive: {r}");
    }
    for r in list("diagnosis") {
        let _ = writeln!(output, "  {r}");
    }
    for r in list("notes") {
        let _ = writeln!(output, "  note: {r}");
    }
    if let Some(dir) = d["dir"].as_str() {
        let _ = writeln!(output, "Data: {dir}");
    }
    EXIT_OK
}

/// Reports a failed socket call and returns its exit code. Mutating verbs never write the store
/// themselves, so without the service there is nothing they can do (§12.2).
fn call_failed(e: CallError) -> i32 {
    match e {
        CallError::NotRunning => {
            note(format_args!(
                "service not running: systemctl --user start minka-thermal"
            ));
            EXIT_NO_SERVICE
        }
        CallError::Remote(e) => {
            note(format_args!("{}", e.message));
            EXIT_ERROR
        }
        CallError::Io(e) => {
            note(format_args!("{e}"));
            EXIT_ERROR
        }
    }
}

/// One action over the socket; `done` words the state the service answers with.
fn act(paths: &Paths, method: &str, params: Value, done: impl FnOnce(&State) -> String) -> i32 {
    match ipc::call(&paths.socket, method, params) {
        Ok(v) => {
            let state: State = serde_json::from_value(v).unwrap_or_default();
            out(&format!("{}\n", done(&state)));
            EXIT_OK
        }
        Err(e) => call_failed(e),
    }
}

fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

/// `not-dust`: closes the case as a false alarm, so it asks first on a terminal (§9.2 wants a
/// confirmation), and without one it needs `--yes`.
pub(crate) fn not_dust(paths: &Paths, yes: bool, tty: bool, input: &mut impl BufRead) -> i32 {
    if !yes {
        if !tty {
            note(format_args!(
                "not-dust: confirm with --yes when not run from a terminal"
            ));
            return EXIT_USAGE;
        }
        // Ask only once it is known that the service is there to act on the answer, and that
        // there is a case for the question to be about.
        let state: State = match ipc::call(&paths.socket, "state.get", json!({})) {
            Ok(v) => serde_json::from_value(v).unwrap_or_default(),
            Err(e) => return call_failed(e),
        };
        if !state.status.dust_case_open() {
            note(format_args!(
                "{} (the status is {})",
                present::NOT_DUST_NO_CASE,
                state.status.as_str()
            ));
            return EXIT_ERROR;
        }
        let case = state
            .case
            .as_ref()
            .and_then(|c| c.first_evidence.as_ref())
            .map(|f| format!(" of {}", f.label))
            .unwrap_or_default();
        out(&format!(
            "Not dust: close the case{case} as a false alarm and accept the current cooling as \
             normal? The references become today's values until the next clean. [y/N] "
        ));
        let _ = io::stdout().flush();
        let mut answer = String::new();
        let _ = input.read_line(&mut answer);
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            out("Nothing changed.\n");
            return EXIT_ERROR;
        }
    }
    act(paths, "case.not_dust", json!({}), |s| {
        format!(
            "Case closed as not dust. Status {}.",
            s.status.as_str().to_uppercase()
        )
    })
}

pub const GUI_NAME: &str = "minka-thermal-gui";
/// The window's log rolls to `.1` past this, as MinkaMon's does.
pub const UI_LOG_ROLL: u64 = 2 * 1024 * 1024;

/// The window binary (§12.4), searched as MinkaSyu's `find_gui` does: `$MINKA_THERMAL_GUI`, then
/// beside this binary (installed, both are in /usr/bin), then the checkout's
/// `gui/target/<profile>/`, then PATH. A set `$MINKA_THERMAL_GUI` is final: if it names nothing,
/// that is the error, rather than another build opening instead. Err lists every place tried.
pub fn find_gui(
    env: &dyn Fn(&str) -> Option<OsString>,
    exe: Option<&Path>,
) -> Result<PathBuf, Vec<String>> {
    let mut tried = Vec::new();
    if let Some(p) = env("MINKA_THERMAL_GUI").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        tried.push(format!("{} ($MINKA_THERMAL_GUI)", p.display()));
        return Err(tried);
    }
    if let Some(dir) = exe.and_then(Path::parent) {
        let beside = dir.join(GUI_NAME);
        if beside.is_file() {
            return Ok(beside);
        }
        tried.push(beside.display().to_string());
        // target/<profile>/minka-thermal → gui/target/<profile>/minka-thermal-gui
        if let (Some(profile), Some(checkout)) =
            (dir.file_name(), dir.parent().and_then(Path::parent))
        {
            let dev = checkout.join("gui/target").join(profile).join(GUI_NAME);
            if dev.is_file() {
                return Ok(dev);
            }
            tried.push(dev.display().to_string());
        }
    }
    match env("PATH") {
        Some(path) => {
            for dir in std::env::split_paths(&path).filter(|d| !d.as_os_str().is_empty()) {
                let p = dir.join(GUI_NAME);
                if p.is_file() {
                    return Ok(p);
                }
                tried.push(p.display().to_string());
            }
        }
        None => tried.push("$PATH (not set)".to_owned()),
    }
    Err(tried)
}

/// `$XDG_STATE_HOME/minka/minka-thermal-ui.log`, default `~/.local/state/minka/`.
pub fn ui_log_path(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let state_home = env("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            env("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(state_home.join("minka/minka-thermal-ui.log"))
}

/// Opens the window's log for appending, first rolling it to `.1` once it is past 2 MB: a log
/// directory here has reached 71 GB before.
pub fn open_ui_log(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > UI_LOG_ROLL) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        fs::rename(path, old)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// `ui [--page …]` (§12.4): find the window, send its output to the log when there is no
/// terminal to see it, then become it. `env` is main.rs's environment lookup.
pub fn ui(page: Page, env: &dyn Fn(&str) -> Option<OsString>, exe: Option<&Path>) -> i32 {
    use std::os::unix::process::CommandExt;
    let gui = match find_gui(env, exe) {
        Ok(p) => p,
        Err(tried) => {
            note(format_args!(
                "no window to open ({GUI_NAME} not found); tried:\n  {}",
                tried.join("\n  ")
            ));
            return EXIT_ERROR;
        }
    };
    if !is_tty(2)
        && let Some(log) = ui_log_path(env)
    {
        match open_ui_log(&log) {
            Ok(file) => {
                // SAFETY: dup2 onto the standard descriptors of this process, which is about to
                // exec; `file` stays open until both calls have returned.
                unsafe {
                    libc::dup2(file.as_raw_fd(), 1);
                    libc::dup2(file.as_raw_fd(), 2);
                }
                let header = format!(
                    "\n=== MinkaThermal window starting: {} ===\n",
                    Moment::now().stamp().local
                );
                let _ = io::stderr().write_all(header.as_bytes());
            }
            Err(e) => note(format_args!("{}: {e}", log.display())),
        }
    }
    let err = std::process::Command::new(&gui)
        .arg("--page")
        .arg(page.as_str())
        .exec();
    note(format_args!("{}: {err}", gui.display()));
    EXIT_ERROR
}

/// Writes to stdout in one go and ignores a closed pipe (`| head`), which print! would panic on.
fn out(text: &str) {
    let _ = std::io::stdout().write_all(text.as_bytes());
}

/// Whether the service that wrote `state` is running now: its pid is a `minka-thermal` in this
/// boot. The boot id rules out a pid reused after a reboot.
pub fn service_alive(root: &Path, pid: u32, boot_id: &str) -> bool {
    if pid == 0 || boot_id.is_empty() {
        return false;
    }
    let comm = std::fs::read_to_string(root.join("proc").join(pid.to_string()).join("comm"));
    comm.is_ok_and(|c| c.trim() == "minka-thermal") && sensors::boot_id(root) == boot_id
}

fn status(paths: &Paths, root: &Path, json: bool) -> i32 {
    let state = match store::read_state(&paths.state) {
        Ok(s) => s,
        Err(e) => {
            note(format_args!("{e}"));
            return EXIT_ERROR;
        }
    };
    let alive = state
        .as_ref()
        .is_some_and(|s| service_alive(root, s.pid, &s.boot_id));
    if json {
        let doc = serde_json::json!({ "alive": alive, "state": state });
        out(&format!("{doc}\n"));
    } else {
        match &state {
            Some(s) => out(&format_status(s, alive)),
            None => out(&format!(
                "MinkaThermal  the service has never run (no {})\n",
                paths.state.display()
            )),
        }
    }
    EXIT_OK
}

fn pct(fraction: f64) -> String {
    format!("{:.1} %", fraction * 100.0)
}

/// The §12.2 summary.
pub fn format_status(s: &State, alive: bool) -> String {
    let mut lines = Vec::new();
    let since = s
        .started_at
        .as_ref()
        .map(|t| format!(" since {}", t.label()))
        .unwrap_or_default();
    lines.push(if alive {
        format!("MinkaThermal  service running (pid {}{since})", s.pid)
    } else {
        format!(
            "MinkaThermal  service not running (last pid {}, state from {})",
            s.pid,
            s.updated_at.label()
        )
    });

    let c = &s.chassis;
    let mut key = if s.key.referenced || s.key.label.is_empty() {
        s.key.label.clone()
    } else {
        format!("{} (no reference)", s.key.label)
    };
    if s.key.on_battery {
        key += " (on battery now: nothing is judged)";
    }
    lines.push(format!(
        "chassis {} ({}: {}, BIOS {})   key {key}",
        c.profile.as_str(),
        c.setting.as_str(),
        c.board,
        c.bios
    ));

    let l = &s.learning;
    let passive = if l.voting >= l.needed && l.days >= l.needed_days {
        "learned".to_owned()
    } else {
        format!(
            "learning {}/{} builds, {}/{} days",
            l.voting, l.needed, l.days, l.needed_days
        )
    };
    // No E2 among known classes: light load is not judged (off since 21/11/2026, or no counter).
    let light = if !s.classes.is_empty() && !s.classes.iter().any(|c| c == "E2") {
        "off".to_owned()
    } else if l.light_windows >= l.light_needed {
        format!("{}/{} learned", l.light_windows, l.light_needed)
    } else {
        format!("{}/{}", l.light_windows, l.light_needed)
    };
    lines.push(format!(
        "reference {} (passive: {passive}; light load {light})",
        s.key.reference.as_deref().unwrap_or("none")
    ));

    let case = s.case.as_ref();
    let since = match s.status {
        Status::Warning => case.and_then(|c| c.warning_since.as_ref()),
        Status::Suspicion => case.and_then(|c| c.suspected_at.as_ref()),
        Status::Watch => case.map(|c| &c.opened),
        Status::Verifying => s.verification.as_ref().map(|v| &v.cleaned_at),
        _ => None,
    };
    let mut head = format!("status   {}", s.status.as_str().to_uppercase());
    if let Some(t) = since {
        head += &format!(" since {}", t.label());
    }
    let first = case.and_then(|c| c.first_evidence.as_ref());
    if let Some(f) = first {
        head += &format!(" · first evidence {}", f.label);
    }
    lines.push(head);
    if let Some(f) = first.filter(|f| !f.summary.is_empty()) {
        lines.push(format!("         {}", f.summary));
    }
    if let Some(e) = case.and_then(|c| c.earlier_sighting.as_ref()) {
        lines.push(format!("         earlier sighting {}", e.label));
    }
    if let Some(v) = &s.verification {
        lines.push(format!(
            "verify   cleaned {}{}",
            v.label,
            match &v.passive {
                Some(p) => format!(
                    " · builds since clean {} ({} clean, {} hot), verdict from {}",
                    p.voting,
                    p.clean,
                    p.hot,
                    Date::parse_key(&p.from).map_or_else(|| p.from.clone(), |d| d.label())
                ),
                None => String::new(),
            }
        ));
    }

    let n = &s.counts;
    let mut evidence = format!(
        "evidence {} of {} qualifying days in {}",
        n.evidence_days, n.qualifying_days, n.days
    );
    if let Some(c) = case.filter(|c| !c.e2_days.is_empty()) {
        evidence += &format!(" (light-load on {})", c.e2_days.len());
    }
    if let Some(p) = &s.last_plateau {
        evidence += &format!(" · last build {} {:.1} °C {}", p.label, p.t_ss, pct(p.duty));
        if let Some(fan) = p.fan {
            evidence += &format!(" fan {fan}");
        }
        evidence += &format!(" {}", p.verdict.as_str());
    }
    lines.push(evidence);

    if let Some(ch) = &s.check {
        lines.push(
            format!(
                "check    {:?} ({:?}, started {})",
                ch.state,
                ch.purpose,
                ch.started_at.label()
            )
            .to_lowercase(),
        );
    }
    if let Some(lc) = &s.last_check {
        lines.push(format!("last check {} {}", lc.at.label(), lc.label));
    }

    let fan = match (s.fan.status, s.fan.last_fan) {
        (st, Some(rpm)) => format!("fan {} {rpm} rpm", format!("{st:?}").to_lowercase()),
        (st, None) => format!("fan {}", format!("{st:?}").to_lowercase()),
    };
    let fault = match s.fault.status {
        FaultStatus::Ok => "fault none".to_owned(),
        FaultStatus::Fault => format!(
            "fault TCC not acting {}{}",
            s.fault
                .t_max
                .map(|t| format!("{t:.0} °C "))
                .unwrap_or_default(),
            s.fault.at.as_ref().map(|t| t.label()).unwrap_or_default()
        ),
    };
    let notices = match s.notices.len() {
        0 => "notices none".to_owned(),
        k => format!("notices {k}"),
    };
    let unjudged: Vec<String> = s
        .unjudged_30d
        .entries()
        .iter()
        .filter(|(_, v)| *v > 0)
        .map(|(k, v)| format!("{k} {v}"))
        .collect();
    let unjudged = if unjudged.is_empty() {
        "none".to_owned()
    } else {
        unjudged.join(", ")
    };
    lines.push(format!(
        "{fan} · {fault} · {notices} · unjudged 30 d: {unjudged}"
    ));
    for notice in &s.notices {
        lines.push(format!("notice   {}", notice.text));
    }

    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Whether a stamp's local day is on or after `since`.
fn on_or_after(at: &crate::clock::Stamp, since: Option<Date>) -> bool {
    since.is_none_or(|s| at.local_civil().is_some_and(|c| c.date >= s))
}

/// events.jsonl as `d/M/yyyy HH:mm  text` (§12.2).
pub fn format_history(events: &[Event], since: Option<Date>) -> String {
    let mut text = String::new();
    for e in events.iter().filter(|e| on_or_after(&e.at, since)) {
        text += &format!("{}  {}\n", e.at.label(), e.text);
    }
    text
}

fn history(paths: &Paths, since: Option<Date>) -> i32 {
    match store::read_jsonl::<Event>(&paths.events) {
        Ok(events) if events.is_empty() => {
            out(&format!("no history yet ({})\n", paths.events.display()));
            EXIT_OK
        }
        Ok(events) => {
            out(&format_history(&events, since));
            EXIT_OK
        }
        Err(e) => {
            note(format_args!("{}: {e}", paths.events.display()));
            EXIT_ERROR
        }
    }
}

/// `replay`: every stored plateau and light record judged again under the current rules and
/// references, and each day's E1/E2 decision recounted. Prints only what would change. Read-only:
/// nothing in the store is written or migrated.
/// "1 build", "2 builds".
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `chassis` and `board` are the machine's: from state.json when there is one, else resolved as
/// the service would resolve them. `light_load_evidence` is the rules' (`LIGHT_LOAD_EVIDENCE`):
/// while it is off, no day is an E2 day.
pub fn replay_report(
    state: Option<&State>,
    (chassis, board): (Chassis, &str),
    refs: &References,
    records: &[Record],
    daily: &[DailyRecord],
    since: Option<Date>,
    light_load_evidence: bool,
) -> String {
    let fans: Vec<f64> = records
        .iter()
        .filter_map(|r| match r {
            Record::Plateau(p) => p.fan_med,
            _ => None,
        })
        .collect();
    let ctx = JudgeCtx {
        has_fan: !fans.is_empty(),
        has_throttle: state.is_none_or(State::has_throttle_counter),
        fan_auto_max: if chassis == Chassis::Duo {
            Some(DUO.fan_auto_max as f64)
        } else if fans.len() >= 3 {
            percentile(&fans, 0.99)
        } else {
            None
        },
        r_fan_override: state
            .filter(|s| s.fan.accepted || s.fan.relearned)
            .and_then(|s| s.fan.r_fan)
            .map(f64::from),
        ramp_accepted: state.is_some_and(|s| s.fan.ramp_accepted),
    };
    let mut views: BTreeMap<String, KeyView> = BTreeMap::new();
    let mut view = |key: &str| -> KeyView {
        views
            .entry(key.to_owned())
            .or_insert_with(|| KeyView::of(refs, key, chassis, board))
            .clone()
    };
    // Per local day: (voting, hot, clean, hot windows) under the current rules.
    let mut days: BTreeMap<String, (u32, u32, u32, u32)> = BTreeMap::new();
    let mut lines = Vec::new();
    let (mut builds, mut windows) = (0, 0);
    for r in records {
        match r {
            Record::Plateau(p) if on_or_after(&p.onset, since) => {
                builds += 1;
                let j = judge_plateau(p, &view(&p.key_id), &ctx);
                let was_voting = !p.flags.iter().any(|f| f == "no_reference");
                let d = days.entry(p.end.day_key()).or_default();
                if j.voting {
                    d.0 += 1;
                    d.1 += (j.verdict == PlateauVerdict::Hot) as u32;
                    d.2 += (j.verdict == PlateauVerdict::Clean) as u32;
                }
                let word = |voting: bool, v: PlateauVerdict| {
                    if voting { v.as_str() } else { "not counted" }
                };
                if j.verdict != p.verdict || j.voting != was_voting {
                    lines.push(format!(
                        "{}  build {} °C {}: {} → {}",
                        p.onset.label(),
                        p.t_ss.map_or("?".to_owned(), present::c),
                        p.duty.map_or("?".to_owned(), present::pct),
                        word(was_voting, p.verdict),
                        word(j.voting, j.verdict)
                    ));
                }
            }
            Record::Light(l) if on_or_after(&l.start, since) => {
                windows += 1;
                let (v, _) = judge_light(l, &view(&l.key_id), &ctx);
                if v == LightVerdict::Hot {
                    days.entry(l.start.day_key()).or_default().3 += 1;
                }
                if v != l.verdict {
                    let word = |v: LightVerdict| match v {
                        LightVerdict::Hot => "hot",
                        LightVerdict::Cold => "cold",
                        LightVerdict::Neutral => "neutral",
                    };
                    lines.push(format!(
                        "{}  light load {:.1} threads, {} hot: {} → {}",
                        l.start.label(),
                        l.threads,
                        present::pct(l.hot_ms as f64 / (l.seconds.max(1.0) * 1000.0)),
                        word(l.verdict),
                        word(v)
                    ));
                }
            }
            _ => {}
        }
    }
    for line in daily {
        let Some(day) = Date::parse_key(&line.day) else {
            continue;
        };
        if since.is_some_and(|s| day < s) {
            continue;
        }
        let (voting, hot, clean, hot_windows) = days.get(&line.day).copied().unwrap_or_default();
        let e1 = hot >= 1 && hot >= clean;
        let e2 = light_load_evidence && hot_windows >= crate::rules::E2_DAY_WINDOWS;
        let yes = |b: bool| if b { "yes" } else { "no" };
        if e1 != line.e1 || e2 != line.e2 {
            lines.push(format!(
                "{}  evidence day: build {} → {}, light load {} → {} ({})",
                day.label(),
                yes(line.e1),
                yes(e1),
                yes(line.e2),
                yes(e2),
                count(voting as usize, "comparable build", "comparable builds")
            ));
        }
    }
    let scope = since.map(|s| format!(" since {}", s.label())).unwrap_or_default();
    let mut text = format!(
        "replay{scope}: {} and {} under rules v{}\n",
        count(builds, "build", "builds"),
        count(windows, "light-load window", "light-load windows"),
        crate::RULES_VERSION
    );
    if lines.is_empty() {
        text += "no verdict would change\n";
    } else {
        text += &format!("{}:\n", count(lines.len(), "change", "changes"));
        for l in lines {
            text += &l;
            text.push('\n');
        }
    }
    text
}

fn replay(paths: &Paths, root: &Path, since: Option<Date>) -> i32 {
    let state = match store::read_state(&paths.state) {
        Ok(s) => s,
        Err(e) => {
            note(format_args!("{e}"));
            return EXIT_ERROR;
        }
    };
    let refs = match store::read_references(&paths.references) {
        Ok(r) => r,
        Err(e) => {
            note(format_args!("{e}"));
            return EXIT_ERROR;
        }
    };
    let mut records = Vec::new();
    for file in store::record_files(paths) {
        match store::read_jsonl::<Record>(&file) {
            Ok(mut r) => records.append(&mut r),
            Err(e) => {
                note(format_args!("{}: {e}", file.display()));
                return EXIT_ERROR;
            }
        }
    }
    let daily = match store::read_jsonl::<DailyRecord>(&paths.daily) {
        Ok(d) => d,
        Err(e) => {
            note(format_args!("{}: {e}", paths.daily.display()));
            return EXIT_ERROR;
        }
    };
    let machine = match &state {
        Some(s) => (s.chassis.profile, s.chassis.board.clone()),
        None => {
            let info = StaticInfo::read(root);
            let setting = profile::read_chassis_setting(paths.settings.as_deref());
            (profile::resolve_chassis(setting, &info), info.board_name)
        }
    };
    out(&replay_report(
        state.as_ref(),
        (machine.0, &machine.1),
        &refs,
        &records,
        &daily,
        since,
        LIGHT_LOAD_EVIDENCE,
    ));
    EXIT_OK
}

fn sensors(paths: &Paths, root: &Path) -> i32 {
    let s = Sensors::discover(root);
    let info = StaticInfo::read(root);
    let setting = profile::read_chassis_setting(paths.settings.as_deref());
    let chassis = profile::resolve_chassis(setting, &info);
    let mut config = s.config();
    config.thermald = scan_comms(&root.join("proc"))
        .iter()
        .any(|(_, c)| c == "thermald");
    let power = s.power();
    let key = ConfigKey::new(power.ac, &config);
    let tcc = s.tcc_c(config.tcc_offset);

    // Busy needs two readings; half a second apart is enough for a glance.
    let a = s.cpu_times();
    std::thread::sleep(Duration::from_millis(500));
    let busy = a
        .zip(s.cpu_times())
        .and_then(|(a, b)| busy_fraction(&a, &b))
        .map_or("?".to_owned(), pct);

    let mut t = String::new();
    t += "MinkaThermal sensors (read only)\n";
    t += &format!(
        "machine   {} {} (board {}, BIOS {})\n",
        info.sys_vendor, info.product_name, info.board_name, info.bios_version
    );
    t += &format!(
        "cpu       {} · {} CPUs · kernel {}\n",
        info.cpu_model, info.nproc, info.kernel
    );
    t += &format!(
        "chassis   {} ({}: {}{})\n",
        chassis.as_str(),
        setting.as_str(),
        if profile::is_ux482(&info) {
            "UX482E family"
        } else {
            "not a UX482E"
        },
        profile::f_allcore_max(chassis, &info.cpu_model)
            .map(|f| format!(", all-core max {f} MHz"))
            .unwrap_or_default()
    );
    t += &format!(
        "key       {}{}\n          {}\n",
        key.id(),
        if chassis == profile::Chassis::Duo && key.is_seed_key() {
            "  (the Duo seed's key)"
        } else {
            ""
        },
        key.label(Some(tcc))
    );
    t += &format!(
        "TCC       {tcc:.0} °C (crit {}, offset {})\n",
        s.temp_crit_c.map_or("?".to_owned(), |c| format!("{c:.0}")),
        config.tcc_offset.map_or("?".to_owned(), |o| o.to_string())
    );
    t += &format!(
        "classes   {}{}\n",
        s.classes(LIGHT_LOAD_EVIDENCE).join(" "),
        if s.has_fan() {
            " · fan rules on"
        } else {
            " · no fan sensor: fan rules off"
        }
    );
    t += &format!(
        "now       busy {busy} · power {} · deficit {} · cooling devices {} · thermald {}\n",
        match power.ac {
            Some(true) => "AC",
            Some(false) => "battery",
            None => "?",
        },
        if power.deficit() { "yes" } else { "no" },
        config.cooling_max.map_or("?".to_owned(), |m| m.to_string()),
        if config.thermald {
            "running"
        } else {
            "not running"
        }
    );
    t += "signals\n";
    let lines = s.describe();
    let name_w = lines
        .iter()
        .map(|l| l.name.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    let value_w = lines
        .iter()
        .map(|l| l.value.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    for line in lines {
        t += &format!(
            "  {:<name_w$}{:<value_w$}{}\n",
            line.name, line.value, line.path
        );
    }
    if s.failed() {
        t += "warning   at least one sensor failed to read\n";
    }
    out(&t);
    EXIT_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::WARNING_EXAMPLE;
    use crate::testutil::{put, test_dir};

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parsing() {
        assert_eq!(parse(&[]), Ok(Command::Status { json: false }));
        assert_eq!(
            parse(&args(&["status", "--json"])),
            Ok(Command::Status { json: true })
        );
        assert!(parse(&args(&["status", "--yaml"])).is_err());
        assert!(parse(&args(&["frobnicate"])).is_err());
        assert_eq!(
            parse(&args(&["cleaned", "--date", "20/11/2026"])),
            Ok(Command::Cleaned {
                date: Date::new(2026, 11, 20)
            })
        );
        assert!(parse(&args(&["cleaned", "--date", "2026-11-20"])).is_err());
        assert_eq!(
            parse(&args(&["cleaned"])),
            Ok(Command::Cleaned { date: None })
        );
        assert_eq!(
            parse(&args(&["check", "--detach", "--yes"])),
            Ok(Command::Check {
                yes: true,
                anyway: false,
                detach: true
            })
        );
        assert_eq!(
            parse(&args(&["ui", "--page", "references"])),
            Ok(Command::Ui {
                page: Page::References
            })
        );
        assert!(parse(&args(&["ui", "--page", "nope"])).is_err());
        assert_eq!(
            parse(&args(&["snooze", "suspicion"])),
            Ok(Command::SnoozeSuspicion)
        );
        assert!(parse(&args(&["snooze"])).is_err());
        assert_eq!(
            parse(&args(&["burn", "--threads", "8"])),
            Ok(Command::Burn { threads: 8 })
        );
        assert!(parse(&args(&["burn"])).is_err());
        assert!(parse(&args(&["service", "--now"])).is_err());
    }

    #[test]
    fn status_matches_the_spec_example() {
        let state: State = serde_json::from_str(WARNING_EXAMPLE).unwrap();
        let expected = "\
MinkaThermal  service running (pid 1432 since 3/12/2026 09:12)
chassis duo (auto: UX482EG, BIOS UX482EG.311)   key AC · performance · PL1 28 W · PL2 35 W · TCC 92 °C · lid open
reference duo-seed 20/11/2026 (passive: learning 3/8 builds, 2/3 days; light load 412/300 learned)
status   WARNING since 5/12/2026 10:31 · first evidence 3/12/2026 14:05
         build 90.4 °C, throttled 6.8 % (clean 86 °C, 0.9 %), mostly rustc
evidence 4 of 6 qualifying days in 21 (light-load on 1) · last build 5/12/2026 10:22 89.6 °C 5.1 % fan 6900 hot
fan ok 6800 rpm · fault none · notices none · unjudged 30 d: too_short 31, unsettled 2, dips 6, slope 1, void 3
";
        assert_eq!(format_status(&state, true), expected);
        assert!(format_status(&state, false).starts_with(
            "MinkaThermal  service not running (last pid 1432, state from 5/12/2026 10:31)\n"
        ));
    }

    #[test]
    fn status_writes_the_verification_dates_as_d_m_yyyy() {
        use crate::model::{PassiveVerification, Verification};
        let mut state: State = serde_json::from_str(WARNING_EXAMPLE).unwrap();
        state.status = Status::Verifying;
        state.verification = Some(Verification {
            label: "3/12/2026".into(),
            passive: Some(PassiveVerification {
                voting: 2,
                clean: 2,
                from: "2026-12-10".into(),
                deadline: "2026-12-17".into(),
                ..PassiveVerification::default()
            }),
            ..Verification::default()
        });
        let text = format_status(&state, true);
        assert!(
            text.contains(
                "verify   cleaned 3/12/2026 · builds since clean 2 (2 clean, 0 hot), verdict from 10/12/2026\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn liveness_needs_the_name_and_the_boot() {
        let root = test_dir("cli-alive");
        put(&root, "proc/sys/kernel/random/boot_id", "boot-a\n");
        put(&root, "proc/1432/comm", "minka-thermal\n");
        put(&root, "proc/77/comm", "bash\n");
        assert!(service_alive(&root, 1432, "boot-a"));
        assert!(
            !service_alive(&root, 1432, "boot-b"),
            "same pid, earlier boot"
        );
        assert!(
            !service_alive(&root, 77, "boot-a"),
            "pid reused by another program"
        );
        assert!(!service_alive(&root, 9, "boot-a"));
        assert!(!service_alive(&root, 0, "boot-a"));
    }

    #[test]
    fn history_lines() {
        use crate::clock::Stamp;
        let at = |d: u32, h: i64| Stamp::at(Date::new(2026, 12, d).unwrap().days() * 86_400 + h * 3600 + 300, 0);
        let events = vec![
            Event {
                at: at(3, 14),
                text: "First evidence: build 90.4 °C".into(),
                ..Event::default()
            },
            Event {
                at: at(5, 10),
                text: "Needs a clean".into(),
                ..Event::default()
            },
        ];
        assert_eq!(
            format_history(&events, None),
            "3/12/2026 14:05  First evidence: build 90.4 °C\n5/12/2026 10:05  Needs a clean\n"
        );
        assert_eq!(
            format_history(&events, Date::new(2026, 12, 4)),
            "5/12/2026 10:05  Needs a clean\n"
        );

        let dir = test_dir("cli-history");
        let paths = Paths::under(&dir);
        assert_eq!(history(&paths, None), EXIT_OK);
        assert!(!paths.events.exists(), "reading creates nothing");
    }

    #[test]
    fn replay_prints_only_changes() {
        use crate::clock::Stamp;
        use crate::model::{LightRecord, PlateauRecord};
        let key = "ac|performance|performance|28|35|8|0|100|open";
        let at = |h: i64| Stamp::at(Date::new(2026, 12, 3).unwrap().days() * 86_400 + h * 3600, 0);
        let plateau = |h, t, duty, verdict| {
            Record::Plateau(Box::new(PlateauRecord {
                onset: at(h),
                end: at(h),
                key_id: key.into(),
                t_ss: Some(t),
                duty: Some(duty),
                fan_med: Some(6800.0),
                verdict,
                ..PlateauRecord::default()
            }))
        };
        let state: State = serde_json::from_str(WARNING_EXAMPLE).unwrap();
        let records = vec![
            plateau(10, 86.0, 0.009, PlateauVerdict::Clean),
            // Stored as neutral by an older rule; the seed phase calls it hot now.
            plateau(14, 90.4, 0.068, PlateauVerdict::Neutral),
            Record::Light(LightRecord {
                start: at(15),
                key_id: key.into(),
                seconds: 60.0,
                hot_ms: 24_000,
                verdict: LightVerdict::Hot,
                ..LightRecord::default()
            }),
        ];
        let daily = vec![DailyRecord {
            day: "2026-12-03".into(),
            e1: false,
            ..DailyRecord::default()
        }];
        let duo = (Chassis::Duo, "UX482EG");
        let text =
            replay_report(Some(&state), duo, &References::default(), &records, &daily, None, true);
        // No light reference in an empty references.json: the window no longer votes hot.
        assert_eq!(
            text,
            "replay: 2 builds and 1 light-load window under rules v1\n\
             3 changes:\n\
             3/12/2026 14:00  build 90.4 °C 6.8 %: neutral → hot\n\
             3/12/2026 15:00  light load 0.0 threads, 40.0 % hot: hot → neutral\n\
             3/12/2026  evidence day: build no → yes, light load no → no (2 comparable builds)\n"
        );
    }

    #[test]
    fn replay_counts_no_e2_day_while_light_load_evidence_is_off() {
        use crate::clock::Stamp;
        use crate::model::{LightRecord, LightRef};
        let key = "ac|performance|performance|28|35|8|0|100|open";
        let at = |m: i64| Stamp::at(Date::new(2026, 12, 3).unwrap().days() * 86_400 + 14 * 3600 + m * 60, 0);
        // Ten hot windows against a learned light reference: an E2 day while it was on.
        let records: Vec<Record> = (0..10)
            .map(|m| {
                Record::Light(LightRecord {
                    start: at(m),
                    key_id: key.into(),
                    seconds: 60.0,
                    hot_ms: 24_000,
                    verdict: LightVerdict::Hot,
                    ..LightRecord::default()
                })
            })
            .collect();
        let mut refs = References::default();
        refs.keys.entry(key.into()).or_default().light = Some(LightRef {
            n: 300,
            p90: Some(0.002),
            ..LightRef::default()
        });
        let line = |e2| DailyRecord {
            day: "2026-12-03".into(),
            e2,
            ..DailyRecord::default()
        };
        let duo = (Chassis::Duo, "UX482EG");
        let replay = |e2_line, on| replay_report(None, duo, &refs, &records, &[line(e2_line)], None, on);
        assert!(replay(true, true).ends_with("no verdict would change\n"));
        assert!(replay(false, false).ends_with("no verdict would change\n"));
        // A line written while it was on is no E2 day now.
        assert!(
            replay(true, false).contains("3/12/2026  evidence day: build no → no, light load yes → no"),
            "{}",
            replay(true, false)
        );
    }

    #[test]
    fn replay_reads_the_throttle_counter_not_the_classes() {
        use crate::clock::Stamp;
        use crate::model::PlateauRecord;
        let at = Stamp::at(Date::new(2026, 12, 3).unwrap().days() * 86_400 + 14 * 3600, 0);
        // Hot by its throttle duty against the seed. Judged as if there were no counter, it would
        // need a frequency it does not have, and read neutral.
        let records = vec![Record::Plateau(Box::new(PlateauRecord {
            onset: at.clone(),
            end: at,
            key_id: "ac|performance|performance|28|35|8|0|100|open".into(),
            t_ss: Some(90.4),
            duty: Some(0.068),
            fan_med: Some(6800.0),
            verdict: PlateauVerdict::Hot,
            ..PlateauRecord::default()
        }))];
        let mut state: State = serde_json::from_str(WARNING_EXAMPLE).unwrap();
        // As the service writes it with light-load evidence off: no E2 among the classes.
        state.classes = vec!["E1".into(), "check".into()];
        state.throttle_counter = Some(true);
        let duo = (Chassis::Duo, "UX482EG");
        let replay = |s: &State| {
            replay_report(Some(s), duo, &References::default(), &records, &[], None, false)
        };
        assert!(replay(&state).ends_with("no verdict would change\n"), "{}", replay(&state));
        state.throttle_counter = Some(false);
        assert!(replay(&state).contains("hot → neutral"), "{}", replay(&state));
    }

    #[test]
    fn status_says_light_load_is_off_without_e2() {
        let mut state: State = serde_json::from_str(WARNING_EXAMPLE).unwrap();
        state.classes = vec!["E1".into(), "check".into()];
        let text = format_status(&state, true);
        assert!(
            text.contains("reference duo-seed 20/11/2026 (passive: learning 3/8 builds, 2/3 days; light load off)\n"),
            "{text}"
        );
    }

    fn env_of(vars: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let map: BTreeMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_os_str().to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn the_window_is_searched_in_order() {
        let dir = test_dir("cli-find-gui");
        put(&dir, "co/target/release/minka-thermal", "");
        let exe = dir.join("co/target/release/minka-thermal");
        let (bin, bin2) = (dir.join("bin"), dir.join("bin2"));
        let path = std::env::join_paths([&bin, &bin2]).unwrap();
        let env = env_of(&[("PATH", Path::new(&path))]);

        // Nothing anywhere: every place tried is named.
        let beside = dir.join("co/target/release/minka-thermal-gui");
        let tried = find_gui(&env, Some(&exe)).unwrap_err();
        let expected: Vec<String> = [
            beside.clone(),
            dir.join("co/gui/target/release/minka-thermal-gui"),
            bin.join(GUI_NAME),
            bin2.join(GUI_NAME),
        ]
        .iter()
        .map(|p| p.display().to_string())
        .collect();
        assert_eq!(tried, expected);

        put(&dir, "bin2/minka-thermal-gui", "");
        assert_eq!(find_gui(&env, Some(&exe)), Ok(bin2.join(GUI_NAME)));
        put(&dir, "co/gui/target/release/minka-thermal-gui", "");
        assert_eq!(
            find_gui(&env, Some(&exe)),
            Ok(dir.join("co/gui/target/release/minka-thermal-gui")),
            "the checkout's window before PATH"
        );
        put(&dir, "co/target/release/minka-thermal-gui", "");
        assert_eq!(find_gui(&env, Some(&exe)), Ok(beside), "beside this binary first");

        // The variable wins, and when it names nothing that is the error.
        let custom = dir.join("custom-gui");
        let env = env_of(&[("PATH", Path::new(&path)), ("MINKA_THERMAL_GUI", &custom)]);
        assert_eq!(
            find_gui(&env, Some(&exe)),
            Err(vec![format!("{} ($MINKA_THERMAL_GUI)", custom.display())])
        );
        put(&dir, "custom-gui", "");
        assert_eq!(find_gui(&env, Some(&exe)), Ok(custom));

        assert_eq!(
            find_gui(&|_: &str| None, None),
            Err(vec!["$PATH (not set)".to_owned()])
        );
    }

    #[test]
    fn the_window_log_rolls_at_2_mb() {
        let dir = test_dir("cli-ui-log");
        let xs = dir.join("xs");
        assert_eq!(
            ui_log_path(&env_of(&[("XDG_STATE_HOME", &xs), ("HOME", &dir)])),
            Some(xs.join("minka/minka-thermal-ui.log"))
        );
        let rel = Path::new("relative");
        assert_eq!(
            ui_log_path(&env_of(&[("XDG_STATE_HOME", rel), ("HOME", &dir)])),
            Some(dir.join(".local/state/minka/minka-thermal-ui.log"))
        );
        assert_eq!(ui_log_path(&|_: &str| None), None);

        let log = xs.join("minka/minka-thermal-ui.log");
        open_ui_log(&log).unwrap().write_all(b"one\n").unwrap();
        open_ui_log(&log).unwrap().write_all(b"two\n").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        open_ui_log(&log).unwrap().set_len(UI_LOG_ROLL + 1).unwrap();
        open_ui_log(&log).unwrap().write_all(b"three\n").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "three\n");
        assert_eq!(
            fs::metadata(xs.join("minka/minka-thermal-ui.log.1")).unwrap().len(),
            UI_LOG_ROLL + 1
        );
    }

    #[test]
    fn action_verbs_need_the_service() {
        let dir = test_dir("cli-no-service");
        let paths = Paths::with(dir.join("store/state.json"), dir.join("s.sock"), None);
        for cmd in [
            Command::Cleaned { date: None },
            Command::NotDust { yes: true },
            Command::SnoozeSuspicion,
            Command::FanFixed,
            Command::AcceptFan,
            Command::AckFault,
            Command::Cancel,
        ] {
            assert_eq!(run(cmd.clone(), &paths, &dir), EXIT_NO_SERVICE, "{cmd:?}");
        }
        assert!(!dir.join("store").exists(), "they never write the store themselves");
        assert_eq!(Page::References.as_str(), "references");
    }

    #[test]
    fn status_without_a_state_file_creates_nothing() {
        let dir = test_dir("cli-status-empty");
        let paths = Paths::under(&dir.join("store"));
        assert_eq!(status(&paths, &dir, false), EXIT_OK);
        assert!(!dir.join("store").exists());
    }

    /// A stand-in service on `sock` for one connection: `script` gets the request lines and
    /// writes what the real one would.
    fn fake_service(
        sock: &Path,
        script: impl FnOnce(&mut io::BufReader<UnixStream>, &mut UnixStream) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let listener = std::os::unix::net::UnixListener::bind(sock).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = io::BufReader::new(stream);
            script(&mut reader, &mut writer);
        })
    }

    fn request(reader: &mut io::BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn send(writer: &mut UnixStream, v: Value) {
        writer.write_all(format!("{v}\n").as_bytes()).unwrap();
    }

    fn check_event(state: &str, phase_s: u32, live: Value) -> Value {
        json!({"event": "check", "payload": {"state": {"state": state, "phase_s": phase_s}, "live": live}})
    }

    fn cli_paths(name: &str) -> (PathBuf, Paths) {
        let dir = test_dir(name);
        let paths = Paths::with(dir.join("store/state.json"), dir.join("s.sock"), None);
        (dir, paths)
    }

    #[test]
    fn check_follows_the_run_to_its_verdict() {
        let (_dir, paths) = cli_paths("cli-check-follow");
        let server = fake_service(&paths.socket, |r, w| {
            let req = request(r);
            assert_eq!(req["method"], "check.start");
            assert_eq!(req["params"], json!({"confirm": true, "anyway": false}));
            // As the service does it: the check event first, then the reply.
            send(w, check_event("gating", 300, Value::Null));
            send(w, json!({"id": 1, "result": {"state": "gating", "dir": "checks/2026-12-05_1102"}}));
            send(w, json!({"event": "state", "payload": {}}));
            send(w, check_event("gating", 300, json!({"waited_s": 1.0,
                "gates": [{"id": "ac", "ok": true, "text": "AC online"},
                          {"id": "settle", "ok": false, "text": "B10 14 % (< 10)"}],
                "busiest": [["rustc", 0.62], ["floorp", 0.09]]})));
            // The same failing gate a second later is not printed again.
            send(w, check_event("gating", 300, json!({"waited_s": 2.0,
                "gates": [{"id": "settle", "ok": false, "text": "B10 13 % (< 10)"}]})));
            send(w, check_event("idle", 30, json!({"phase_elapsed_s": 0.0, "T": 60.0})));
            send(w, check_event("load", 180, json!({"phase_elapsed_s": 0.0, "T": 86.0,
                "T4_max": 90.0, "fan": 6800, "f": 3700, "duty": 0.009})));
            send(w, json!({"event": "check", "payload": {
                "state": {"state": "done"},
                "result": {"label": "Restored"},
                "detail": {"dir": "checks/2026-12-05_1102",
                    "comparison": {
                        "reference": {"label": "duo-seed 20/11/2026", "t_ss": 86.0, "duty": 0.0093, "fan": 6800.0},
                        "before": {"label": "before cleaning (check)", "t_ss": 91.0, "duty": 0.09},
                        "check": {"label": "5/12/2026", "t_ss": 86.5, "duty": 0.012, "fan": 6800.0}},
                    "diagnosis": [], "inconclusive": [],
                    "notes": ["charger deficit in 3.0 % of the steady window"]}}}));
        });
        let mut out = Vec::new();
        assert_eq!(follow_check(&paths, false, false, None, &mut out), EXIT_OK);
        server.join().unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "Waiting: the load starts by itself once the machine is idle (up to 5 min).\n\
             Cooling check accepted: checks/2026-12-05_1102\n\
             \x20   1 s  B10 14 % (< 10); busiest: rustc 62 %, floorp 9 %\n\
             Idle 30 s\n\
             \x20 idle   0/30 s · T 60 °C\n\
             Load 180 s\n\
             \x20 load   0/180 s · T 86 °C · T4 max 90 °C · fan 6800 rpm · 3700 MHz · throttled 0.9 %\n\
             Result: Restored\n\
             \x20        duo-seed 20/11/2026   before cleaning (check)   5/12/2026\n\
             \x20 T_ss   86 °C                 91 °C                     86.5 °C\n\
             \x20 duty   0.9 %                 9.0 %                     1.2 %\n\
             \x20 fan    6800 rpm                                        6800 rpm\n\
             \x20 note: charger deficit in 3.0 % of the steady window\n\
             Data: checks/2026-12-05_1102\n"
        );
    }

    #[test]
    fn check_refused_and_detached() {
        let (_dir, paths) = cli_paths("cli-check-refused");
        let server = fake_service(&paths.socket, |r, w| {
            request(r);
            send(w, json!({"id": 1, "error": {"code": "refused",
                "message": "lid closed (reference open): not comparable with the clean reference"}}));
        });
        let mut out = Vec::new();
        assert_eq!(follow_check(&paths, false, false, None, &mut out), EXIT_ERROR);
        server.join().unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Not started: lid closed (reference open): not comparable with the clean reference\n"
        );

        let (_dir, paths) = cli_paths("cli-check-detach");
        let server = fake_service(&paths.socket, |r, w| {
            let req = request(r);
            assert_eq!(req["params"]["anyway"], true);
            send(w, json!({"id": 1, "result": {"state": "gating", "dir": "checks/x"}}));
        });
        let mut out = Vec::new();
        assert_eq!(follow_check(&paths, true, true, None, &mut out), EXIT_OK);
        server.join().unwrap();
        assert!(String::from_utf8(out).unwrap().starts_with("Cooling check accepted: checks/x\n"));

        // No service: exit 3, like every verb that needs it.
        let (_dir, paths) = cli_paths("cli-check-none");
        assert_eq!(follow_check(&paths, false, false, None, &mut Vec::new()), EXIT_NO_SERVICE);
    }

    #[test]
    fn ctrl_c_cancels_the_check() {
        let (_dir, paths) = cli_paths("cli-check-cancel");
        // A pipe stands in for the SIGINT signalfd.
        let mut fds = [0; 2];
        // SAFETY: pipe2 fills the two-int array.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both ends are new and owned here.
        let (rx, tx) = unsafe {
            use std::os::fd::FromRawFd;
            (std::os::fd::OwnedFd::from_raw_fd(fds[0]), File::from_raw_fd(fds[1]))
        };
        let server = fake_service(&paths.socket, move |r, w| {
            request(r);
            send(w, json!({"id": 1, "result": {"state": "gating", "dir": "checks/x"}}));
            send(w, check_event("load", 180, json!({"phase_elapsed_s": 12.0})));
            // Ctrl-C.
            let mut tx = tx;
            tx.write_all(b"!").unwrap();
            let req = request(r);
            assert_eq!(req["method"], "check.cancel");
            send(w, json!({"event": "check", "payload": {
                "state": {"state": "aborted", "reason": "cancelled"}, "result": null, "detail": {}}}));
            send(w, json!({"id": 2, "result": {}}));
        });
        let mut out = Vec::new();
        assert_eq!(follow_check(&paths, false, false, Some(rx.as_raw_fd()), &mut out), EXIT_ERROR);
        server.join().unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Cancelling the cooling check…\n"), "{text}");
        assert!(text.ends_with("Cancelled: nothing becomes a reference.\n"), "{text}");
    }

    #[test]
    fn a_second_ctrl_c_stops_following() {
        let (_dir, paths) = cli_paths("cli-check-cancel-twice");
        let mut fds = [0; 2];
        // SAFETY: pipe2 fills the two-int array.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both ends are new and owned here.
        let (rx, tx) = unsafe {
            use std::os::fd::FromRawFd;
            (std::os::fd::OwnedFd::from_raw_fd(fds[0]), File::from_raw_fd(fds[1]))
        };
        // A service that takes the cancel and then never ends the check.
        let server = fake_service(&paths.socket, move |r, w| {
            request(r);
            send(w, json!({"id": 1, "result": {"state": "gating", "dir": "checks/x"}}));
            let mut tx = tx;
            tx.write_all(b"!").unwrap();
            assert_eq!(request(r)["method"], "check.cancel");
            tx.write_all(b"!").unwrap();
            // The CLI hangs up.
            let mut rest = String::new();
            assert_eq!(r.read_line(&mut rest).unwrap(), 0);
        });
        let mut out = Vec::new();
        assert_eq!(follow_check(&paths, false, false, Some(rx.as_raw_fd()), &mut out), EXIT_ERROR);
        server.join().unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.ends_with("Stopped following. The cancel was sent; `minka-thermal status` shows the check.\n"), "{text}");
    }

    #[test]
    fn check_asks_first() {
        let (_dir, paths) = cli_paths("cli-check-ask");
        // Without a terminal it needs --yes; with it, nothing is asked.
        let mut nothing = std::io::Cursor::new(Vec::new());
        assert_eq!(confirm_check(&paths, false, false, &mut nothing, &mut Vec::new()), Some(EXIT_USAGE));
        assert_eq!(confirm_check(&paths, true, false, &mut nothing, &mut Vec::new()), None);
        // No service to run it: exit 3 before asking.
        assert_eq!(
            confirm_check(&paths, false, true, &mut nothing, &mut Vec::new()),
            Some(EXIT_NO_SERVICE)
        );
        for (answer, expected) in [("n\n", Some(EXIT_ERROR)), ("y\n", None)] {
            let _ = fs::remove_file(&paths.socket);
            let server = fake_service(&paths.socket, |r, w| {
                assert_eq!(request(r)["method"], "state.get");
                send(w, json!({"id": 1, "result": {"status": "verifying"}}));
            });
            let mut input = std::io::Cursor::new(answer.as_bytes().to_vec());
            let mut out = Vec::new();
            assert_eq!(confirm_check(&paths, false, true, &mut input, &mut out), expected);
            server.join().unwrap();
            let text = String::from_utf8(out).unwrap();
            assert!(
                text.starts_with(
                    "Cooling check (verify the clean): about five minutes, three of them at full load.\n\
                     Before it starts: hard surface · vents clear · lid open · usual room.\n\
                     The load starts by itself once the machine is idle (up to 5 min).\n\
                     Start? [y/N] "
                ),
                "{text}"
            );
        }
    }
}
