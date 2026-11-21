//! The store (§11.1, §11.2): the directory, the service's lock, state.json and the jsonl logs.
//!
//! The service is the only writer. state.json is replaced atomically and only when something
//! other than `updated_at` changed, so consumers polling its mtime wake only for real changes.
//! The jsonl files are append-only, one write(2) per line; the human history (events, cases) is
//! fsynced line by line, the bulk (records, daily) at episode close and day settle.

use crate::clock::{Civil, Moment};
use crate::model::{CaseRecord, DailyRecord, Event, EventKind, Record, References, State};
use crate::paths::Paths;
use crate::{SCHEMA, note};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug)]
pub enum StoreError {
    Io(PathBuf, io::Error),
    /// The file is not JSON this build can read.
    Corrupt(PathBuf, String),
    /// Written by a newer MinkaThermal: this one must not write over it.
    Newer {
        path: PathBuf,
        found: u32,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(p, e) => write!(f, "{}: {e}", p.display()),
            StoreError::Corrupt(p, e) => write!(f, "{}: not readable: {e}", p.display()),
            StoreError::Newer { path, found } => write!(
                f,
                "{} has schema {found}, newer than this build's {SCHEMA}; refusing to write",
                path.display()
            ),
        }
    }
}

impl std::error::Error for StoreError {}

fn at(path: &Path) -> impl FnOnce(io::Error) -> StoreError + '_ {
    move |e| StoreError::Io(path.to_path_buf(), e)
}

/// Creates the store directory, mode 0700: it holds her cleaning history.
pub fn ensure_dir(paths: &Paths) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&paths.dir)
}

/// The service's flock on state.lock. Released when dropped (or when the process dies).
#[derive(Debug)]
pub struct StoreLock {
    _file: File,
}

/// Takes the lock without waiting; Ok(None) when another service holds it.
pub fn try_lock(paths: &Paths) -> io::Result<Option<StoreLock>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&paths.lock)?;
    // SAFETY: a valid open fd; flock has no memory effects.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(StoreLock { _file: file }));
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(err)
    }
}

fn sync_dir(file: &Path) -> io::Result<()> {
    let dir = match file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}

/// Write `<file>.tmp`, fsync it, rename it over `file`, fsync the directory: a reader sees the
/// old file or the new one, never a torn one, and a crash cannot leave an empty file behind.
pub fn atomic_write(file: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = file.with_file_name(name);
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, file)?;
    sync_dir(file)
}

/// Writes state.json only when it changed. The comparison leaves `updated_at` out, or every
/// tick would be a write.
#[derive(Debug, Default)]
pub struct StateWriter {
    last: Option<Vec<u8>>,
}

impl StateWriter {
    /// Returns whether it wrote. On a write, `state.updated_at` becomes `now`.
    pub fn write(&mut self, file: &Path, state: &mut State, now: &Moment) -> io::Result<bool> {
        let previous = std::mem::take(&mut state.updated_at);
        let body = serde_json::to_vec(state)?;
        if self.last.as_deref() == Some(body.as_slice()) {
            state.updated_at = previous;
            return Ok(false);
        }
        state.updated_at = now.stamp();
        let mut bytes = serde_json::to_vec_pretty(state)?;
        bytes.push(b'\n');
        atomic_write(file, &bytes)?;
        self.last = Some(body);
        Ok(true)
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, StoreError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::Io(path.to_path_buf(), e)),
    }
}

/// Parses a state or references file, which must be a JSON object. Anything else (`[]`, `0`,
/// `null`) is Corrupt: the migration would otherwise index into it and panic, or turn it into a
/// blank state without a word.
fn parse_object(path: &Path, text: &str) -> Result<serde_json::Map<String, Value>, StoreError> {
    let corrupt = |e: String| StoreError::Corrupt(path.to_path_buf(), e);
    match serde_json::from_str(text).map_err(|e| corrupt(e.to_string()))? {
        Value::Object(map) => Ok(map),
        _ => Err(corrupt("not a JSON object".to_owned())),
    }
}

/// Reads a schema'd file for the service, which will write it afterwards: a newer schema is
/// refused, and an older one is copied to `<file>.v<N>.bak` before it is brought up to date.
fn load_versioned<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    let Some(text) = read_optional(path)? else {
        return Ok(None);
    };
    // Checked before the backup below, so a file that is refused leaves no stray .bak.
    let mut map = parse_object(path, &text)?;
    let found = map.get("schema").and_then(Value::as_u64).unwrap_or(0) as u32;
    if found > SCHEMA {
        return Err(StoreError::Newer {
            path: path.to_path_buf(),
            found,
        });
    }
    if found < SCHEMA {
        let backup = Paths::backup_of(path, found);
        if !backup.exists() {
            fs::copy(path, &backup).map_err(at(&backup))?;
        }
        note(format_args!(
            "migrating {} from schema {found} to {SCHEMA} (old copy in {})",
            path.display(),
            backup.display()
        ));
        // Schema 1 is the first layout, so there is nothing to convert yet; later layouts add
        // their steps here, one schema at a time.
        map.insert("schema".to_owned(), Value::from(SCHEMA));
    }
    serde_json::from_value(Value::Object(map))
        .map(Some)
        .map_err(|e| StoreError::Corrupt(path.to_path_buf(), e.to_string()))
}

/// state.json for the service (see `load_versioned`). None when it has never run.
pub fn load_state(paths: &Paths) -> Result<Option<State>, StoreError> {
    load_versioned(&paths.state)
}

/// references.json for the service; empty when absent. The Duo seed is not in this file: it is
/// compiled into profile.rs and never dropped.
pub fn load_references(paths: &Paths) -> Result<References, StoreError> {
    Ok(load_versioned(&paths.references)?.unwrap_or(References {
        schema: SCHEMA,
        ..References::default()
    }))
}

/// state.json for a reader (the `status` verb): whatever the schema, nothing written, no backup.
pub fn read_state(path: &Path) -> Result<Option<State>, StoreError> {
    let Some(text) = read_optional(path)? else {
        return Ok(None);
    };
    serde_json::from_value(Value::Object(parse_object(path, &text)?))
        .map(Some)
        .map_err(|e| StoreError::Corrupt(path.to_path_buf(), e.to_string()))
}

/// references.json for a reader (`replay`): whatever the schema, nothing written, no backup.
/// Empty when absent.
pub fn read_references(path: &Path) -> Result<References, StoreError> {
    let Some(text) = read_optional(path)? else {
        return Ok(References::default());
    };
    serde_json::from_value(Value::Object(parse_object(path, &text)?))
        .map_err(|e| StoreError::Corrupt(path.to_path_buf(), e.to_string()))
}

/// Every record file of the store, oldest month first: the `records-YYYY-MM.jsonl` archives, then
/// this month's records.jsonl.
pub fn record_files(paths: &Paths) -> Vec<PathBuf> {
    let mut archives: Vec<PathBuf> = fs::read_dir(&paths.dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("records-") && n.ends_with(".jsonl"))
        })
        .collect();
    archives.sort();
    archives.push(paths.records.clone());
    archives
}

/// Reads a jsonl file leniently: a missing file is empty, and a line that does not parse (the
/// torn tail of a crash, a type from a newer build) is skipped. The file is split as bytes, not
/// read as one string: a tail torn inside a multi-byte character ("°", "·") is invalid UTF-8,
/// and it must cost only its own line, not the whole history.
pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> io::Result<Vec<T>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(bytes
        .split(|&b| b == b'\n')
        .filter(|l| !l.trim_ascii().is_empty())
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect())
}

/// An append-only jsonl file.
#[derive(Debug)]
pub struct Appender {
    file: File,
    fsync_each: bool,
    /// The file may end in part of a line (a crash, a write cut short by a full disk), so the
    /// next line starts with a newline of its own. An empty line costs readers nothing.
    torn: bool,
}

impl Appender {
    pub fn open(path: &Path, fsync_each: bool) -> io::Result<Appender> {
        // Readable too, for the torn-tail check below; writes still all go to the end.
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)?;
        let mut a = Appender {
            file,
            fsync_each,
            torn: false,
        };
        // A crash mid-line leaves a tail with no newline; end it, so the next line stays whole.
        // If that cannot be checked or written now (a full disk), the next append does it: the
        // service must still start, or its alarms would be off until there is space again.
        let ended = match a.file.metadata() {
            Ok(m) if m.len() == 0 => true,
            Ok(m) => {
                let mut last = [0u8; 1];
                a.file
                    .read_exact_at(&mut last, m.len() - 1)
                    .is_ok_and(|()| last[0] == b'\n')
            }
            // Unknown: an extra newline is an empty line, which readers skip.
            Err(_) => false,
        };
        if !ended && let Err(e) = a.file.write_all(b"\n") {
            note(format_args!("{}: ending a torn last line: {e}", path.display()));
            a.torn = true;
        }
        Ok(a)
    }

    /// Appends one line with a single write(2): serialised in full first, never streamed. A
    /// write that fails partway (ENOSPC) is cut back off, so the next line is not glued to it;
    /// if even that fails, the next line starts on a fresh one.
    pub fn append<T: Serialize>(&mut self, value: &T) -> io::Result<()> {
        let mut line = Vec::with_capacity(256);
        if self.torn {
            line.push(b'\n');
        }
        serde_json::to_writer(&mut line, value)?;
        line.push(b'\n');
        let before = self.file.metadata().map(|m| m.len());
        if let Err(e) = self.file.write_all(&line) {
            self.torn = before.and_then(|len| self.file.set_len(len)).is_err();
            return Err(e);
        }
        self.torn = false;
        if self.fsync_each {
            self.file.sync_data()?;
        }
        Ok(())
    }

    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

/// records.jsonl, rolled into records-YYYY-MM.jsonl when the local month changes.
#[derive(Debug)]
pub struct Records {
    paths: Paths,
    month: String,
    out: Appender,
}

impl Records {
    pub fn open(paths: &Paths, now: &Moment) -> io::Result<Records> {
        let mut month = now.date().month_key();
        // The file's month is its last write's: a service that was down over the month end
        // rolls it at start.
        if let Some(mtime) = fs::metadata(&paths.records)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        {
            let file_month = Civil::at(mtime.as_secs() as i64, now.offset_s)
                .date
                .month_key();
            if file_month != month
                && let Err(e) = roll(paths, &file_month)
            {
                // Not fatal at start: the file still holds that month, and the first append
                // tries the roll again.
                note(format_args!("{}: rolling to {file_month}: {e}", paths.records.display()));
                month = file_month;
            }
        }
        Ok(Records {
            out: Appender::open(&paths.records, false)?,
            paths: paths.clone(),
            month,
        })
    }

    /// Rolls the file over if the month has changed since it was opened. Every step can be
    /// tried again: a roll that failed partway (a full disk at the month change) is finished by
    /// the next append rather than losing every record after it.
    pub fn rollover(&mut self, now: &Moment) -> io::Result<()> {
        let month = now.date().month_key();
        if month != self.month {
            // The old month's last lines are in the page cache either way; a failed fsync must
            // not hold the new month's records back.
            if let Err(e) = self.out.sync() {
                note(format_args!("{}: {e}", self.paths.records.display()));
            }
            roll(&self.paths, &self.month)?;
            self.out = Appender::open(&self.paths.records, false)?;
            self.month = month;
        }
        Ok(())
    }

    pub fn append(&mut self, record: &Record, now: &Moment) -> io::Result<()> {
        self.rollover(now)?;
        self.out.append(record)
    }

    pub fn sync(&self) -> io::Result<()> {
        self.out.sync()
    }
}

fn roll(paths: &Paths, month: &str) -> io::Result<()> {
    // Already moved by an earlier try that failed after the rename: nothing left to roll.
    if !paths.records.exists() {
        return Ok(());
    }
    let target = paths.records_archive(month);
    if target.exists() {
        // The clock went back over a month end: keep one file per month by appending.
        let bytes = fs::read(&paths.records)?;
        let mut out = OpenOptions::new().append(true).open(&target)?;
        out.write_all(&bytes)?;
        out.sync_all()?;
        fs::remove_file(&paths.records)?;
    } else {
        fs::rename(&paths.records, &target)?;
    }
    sync_dir(&target)
}

/// The writer half of the store, owned by the service.
#[derive(Debug)]
pub struct Store {
    pub paths: Paths,
    events: Appender,
    next_seq: u64,
    cases: Appender,
    daily: Appender,
    records: Records,
    state: StateWriter,
}

impl Store {
    /// Creates the directory and opens the logs. The schema checks are `load_state` and
    /// `load_references`, which the service runs first.
    pub fn open(paths: &Paths, now: &Moment) -> Result<Store, StoreError> {
        ensure_dir(paths).map_err(at(&paths.dir))?;
        let next_seq = read_jsonl::<Event>(&paths.events)
            .map_err(at(&paths.events))?
            .iter()
            .map(|e| e.seq)
            .max()
            .map_or(1, |s| s + 1);
        Ok(Store {
            events: Appender::open(&paths.events, true).map_err(at(&paths.events))?,
            next_seq,
            cases: Appender::open(&paths.cases, true).map_err(at(&paths.cases))?,
            daily: Appender::open(&paths.daily, false).map_err(at(&paths.daily))?,
            records: Records::open(paths, now).map_err(at(&paths.records))?,
            state: StateWriter::default(),
            paths: paths.clone(),
        })
    }

    /// Appends to the human history (fsynced) and returns the event as written.
    pub fn event(
        &mut self,
        at: &Moment,
        kind: EventKind,
        case: Option<&str>,
        text: String,
        data: Value,
    ) -> io::Result<Event> {
        let event = Event {
            seq: self.next_seq,
            at: at.stamp(),
            kind,
            case: case.map(str::to_owned),
            text,
            data,
        };
        // Used up even if the append fails: a line written before its fsync failed is on disk
        // with this number, and a gap in the numbers is harmless where a repeat is not.
        self.next_seq += 1;
        self.events.append(&event)?;
        Ok(event)
    }

    pub fn record(&mut self, record: &Record, now: &Moment) -> io::Result<()> {
        self.records.append(record, now)
    }

    pub fn daily(&mut self, day: &DailyRecord) -> io::Result<()> {
        self.daily.append(day)
    }

    pub fn case(&mut self, case: &CaseRecord) -> io::Result<()> {
        self.cases.append(case)
    }

    /// fsyncs records.jsonl and daily.jsonl: at episode close and day settle.
    pub fn sync_bulk(&self) -> io::Result<()> {
        self.records.sync()?;
        self.daily.sync()
    }

    pub fn rollover(&mut self, now: &Moment) -> io::Result<()> {
        self.records.rollover(now)
    }

    /// Writes state.json if it changed; see `StateWriter`.
    pub fn write_state(&mut self, state: &mut State, now: &Moment) -> io::Result<bool> {
        self.state.write(&self.paths.state, state, now)
    }

    pub fn write_references(&self, refs: &References) -> io::Result<()> {
        let mut bytes = serde_json::to_vec_pretty(refs)?;
        bytes.push(b'\n');
        atomic_write(&self.paths.references, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Date, Timeline};
    use crate::model::{Status, UnjudgedRecord};
    use crate::testutil::test_dir;
    use std::os::unix::fs::PermissionsExt;

    fn timeline() -> Timeline {
        Timeline::starting(Date::new(2026, 12, 3).unwrap(), 14, 5, 10, 0)
    }

    #[test]
    fn state_is_written_only_when_it_changes() {
        let dir = test_dir("store-state");
        let paths = Paths::under(&dir.join("store"));
        ensure_dir(&paths).unwrap();
        assert_eq!(
            fs::metadata(&paths.dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let mut tl = timeline();
        let mut w = StateWriter::default();
        let mut state = State::new();

        assert!(w.write(&paths.state, &mut state, &tl.now()).unwrap());
        assert_eq!(state.updated_at.utc, "2026-12-03T14:05:10Z");
        let first = fs::read_to_string(&paths.state).unwrap();
        assert!(first.contains("\"updated_at\""));
        assert!(!dir.join("store/state.json.tmp").exists());

        // Only the time moved: no write, and the stamp stays that of the bytes on disk.
        assert!(
            !w.write(&paths.state, &mut state, &tl.advance(60.0))
                .unwrap()
        );
        assert_eq!(state.updated_at.utc, "2026-12-03T14:05:10Z");
        assert_eq!(fs::read_to_string(&paths.state).unwrap(), first);

        state.status = Status::Watch;
        assert!(
            w.write(&paths.state, &mut state, &tl.advance(60.0))
                .unwrap()
        );
        let back = read_state(&paths.state).unwrap().unwrap();
        assert_eq!(back.status, Status::Watch);
        assert_eq!(back.updated_at.utc, "2026-12-03T14:07:10Z");
        assert_eq!(
            fs::metadata(&paths.state).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn jsonl_appends_whole_lines() {
        let dir = test_dir("store-jsonl");
        let path = dir.join("events.jsonl");
        // A torn tail from a crash: no newline at the end.
        fs::write(
            &path,
            "{\"seq\":1,\"type\":\"setup\",\"text\":\"a\"}\n{\"seq\":2,\"ty",
        )
        .unwrap();
        let mut a = Appender::open(&path, true).unwrap();
        a.append(&Event {
            seq: 3,
            kind: EventKind::Cleaned,
            text: "b".into(),
            ..Event::default()
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.ends_with("\"data\":null}\n"), "{text}");
        let events: Vec<Event> = read_jsonl(&path).unwrap();
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 3]);
        assert!(
            read_jsonl::<Event>(&dir.join("missing.jsonl"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_tail_torn_inside_a_character_costs_one_line() {
        let dir = test_dir("store-torn-utf8");
        let paths = Paths::under(&dir);
        // A crash cut "°" (0xC2 0xB0) after its first byte.
        let mut bytes =
            b"{\"seq\":4,\"type\":\"first_evidence\",\"text\":\"build 90.4 \xC2\xB0C\"}\n".to_vec();
        bytes.extend_from_slice(b"{\"seq\":5,\"type\":\"notice\",\"text\":\"86 \xC2");
        fs::write(&paths.events, &bytes).unwrap();
        let events: Vec<Event> = read_jsonl(&paths.events).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "build 90.4 °C");

        // The service still opens the store, repairs the tail and numbers on from the last whole line.
        let tl = timeline();
        let mut store = Store::open(&paths, &tl.now()).unwrap();
        let e = store
            .event(&tl.now(), EventKind::Notice, None, "n".into(), Value::Null)
            .unwrap();
        assert_eq!(e.seq, 5);
        let events: Vec<Event> = read_jsonl(&paths.events).unwrap();
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 5]);
    }

    #[test]
    fn store_numbers_events_across_opens() {
        let dir = test_dir("store-events");
        let paths = Paths::under(&dir);
        let tl = timeline();
        let mut store = Store::open(&paths, &tl.now()).unwrap();
        let e = store
            .event(
                &tl.now(),
                EventKind::Setup,
                None,
                "setup".into(),
                Value::Null,
            )
            .unwrap();
        assert_eq!(e.seq, 1);
        store
            .event(
                &tl.now(),
                EventKind::Cleaned,
                Some("c-1"),
                "cleaned".into(),
                Value::Null,
            )
            .unwrap();
        drop(store);
        let mut store = Store::open(&paths, &tl.now()).unwrap();
        let e = store
            .event(&tl.now(), EventKind::Notice, None, "n".into(), Value::Null)
            .unwrap();
        assert_eq!(e.seq, 3);
        assert_eq!(e.at.label(), "3/12/2026 14:05");
    }

    #[test]
    fn records_roll_over_by_month() {
        let dir = test_dir("store-records");
        let paths = Paths::under(&dir);
        let mut tl = Timeline::starting(Date::new(2026, 11, 30).unwrap(), 23, 58, 0, 3600);
        let mut store = Store::open(&paths, &tl.now()).unwrap();
        let rec = |reason: &str| {
            Record::Unjudged(UnjudgedRecord {
                reason: reason.into(),
                ..UnjudgedRecord::default()
            })
        };
        store.record(&rec("too_short"), &tl.now()).unwrap();
        tl.advance(180.0); // 1/12/2026 00:01 local
        store.record(&rec("dips"), &tl.now()).unwrap();
        store
            .record(&Record::Plateau(Box::default()), &tl.now())
            .unwrap();
        store.sync_bulk().unwrap();
        let nov: Vec<Record> = read_jsonl(&paths.records_archive("2026-11")).unwrap();
        let dec: Vec<Record> = read_jsonl(&paths.records).unwrap();
        assert_eq!(nov, vec![rec("too_short")]);
        assert_eq!(dec.len(), 2);
    }

    #[test]
    fn a_roll_that_failed_partway_is_finished_by_the_next_append() {
        let dir = test_dir("store-roll-retry");
        let paths = Paths::under(&dir);
        let mut tl = Timeline::starting(Date::new(2026, 11, 30).unwrap(), 23, 58, 0, 3600);
        let mut store = Store::open(&paths, &tl.now()).unwrap();
        let rec = |reason: &str| {
            Record::Unjudged(UnjudgedRecord {
                reason: reason.into(),
                ..UnjudgedRecord::default()
            })
        };
        store.record(&rec("too_short"), &tl.now()).unwrap();
        // The month changes, and an earlier roll stopped after its rename (the directory's
        // fsync failed): the file is already the archive.
        tl.advance(180.0);
        fs::rename(&paths.records, paths.records_archive("2026-11")).unwrap();
        store.record(&rec("dips"), &tl.now()).unwrap();
        store.sync_bulk().unwrap();
        let nov: Vec<Record> = read_jsonl(&paths.records_archive("2026-11")).unwrap();
        let dec: Vec<Record> = read_jsonl(&paths.records).unwrap();
        assert_eq!((nov, dec), (vec![rec("too_short")], vec![rec("dips")]));
    }

    #[test]
    fn a_failed_append_leaves_no_glued_line() {
        let dir = test_dir("store-failed-append");
        let paths = Paths::under(&dir);
        let tl = timeline();
        let mut store = Store::open(&paths, &tl.now()).unwrap();
        let now = tl.now();
        let ev = |store: &mut Store, text: &str| {
            store.event(&now, EventKind::Notice, None, text.into(), Value::Null)
        };
        ev(&mut store, "one").unwrap();
        // A write that fails and cannot be cut back (a read-only descriptor stands in for a full
        // disk): its number is used up, and the next line starts on a fresh one.
        let writable = std::mem::replace(&mut store.events.file, File::open(&paths.events).unwrap());
        assert!(ev(&mut store, "lost").is_err());
        assert!(store.events.torn);
        store.events.file = writable;
        // The half line such a write leaves behind.
        store.events.file.write_all(b"{\"seq\":2,\"ty").unwrap();
        ev(&mut store, "three").unwrap();
        assert!(!store.events.torn);
        let events: Vec<Event> = read_jsonl(&paths.events).unwrap();
        let got: Vec<(u64, &str)> = events.iter().map(|e| (e.seq, e.text.as_str())).collect();
        assert_eq!(got, vec![(1, "one"), (3, "three")]);
    }

    #[test]
    fn schema_versions() {
        let dir = test_dir("store-schema");
        let paths = Paths::under(&dir);
        assert_eq!(load_state(&paths).unwrap(), None);

        // Newer: refused, and the file is left exactly as it was.
        let newer = r#"{"schema": 99, "status": "warning"}"#;
        fs::write(&paths.state, newer).unwrap();
        assert!(matches!(
            load_state(&paths),
            Err(StoreError::Newer { found: 99, .. })
        ));
        assert_eq!(fs::read_to_string(&paths.state).unwrap(), newer);
        // A reader still shows it.
        assert_eq!(
            read_state(&paths.state).unwrap().unwrap().status,
            Status::Warning
        );

        // Older (no schema at all): migrated, with a backup.
        let older = r#"{"status": "watch", "pid": 7}"#;
        fs::write(&paths.state, older).unwrap();
        let s = load_state(&paths).unwrap().unwrap();
        assert_eq!((s.schema, s.status, s.pid), (SCHEMA, Status::Watch, 7));
        assert_eq!(
            fs::read_to_string(dir.join("state.json.v0.bak")).unwrap(),
            older
        );

        fs::write(&paths.state, "{ not json").unwrap();
        assert!(matches!(load_state(&paths), Err(StoreError::Corrupt(..))));

        // Valid JSON that is not an object is Corrupt too, never a panic or a blank state, and
        // it leaves no backup behind.
        fs::remove_file(dir.join("state.json.v0.bak")).unwrap();
        for text in ["[]", "0", "\"x\"", "true", "null"] {
            fs::write(&paths.state, text).unwrap();
            assert!(
                matches!(load_state(&paths), Err(StoreError::Corrupt(..))),
                "{text}"
            );
            assert!(
                matches!(read_state(&paths.state), Err(StoreError::Corrupt(..))),
                "{text}"
            );
            assert!(!dir.join("state.json.v0.bak").exists(), "{text}");
            fs::write(&paths.references, text).unwrap();
            assert!(
                matches!(load_references(&paths), Err(StoreError::Corrupt(..))),
                "{text}"
            );
        }
        fs::remove_file(&paths.references).unwrap();

        let refs = load_references(&paths).unwrap();
        assert_eq!(refs.schema, SCHEMA);
        fs::write(&paths.references, r#"{"schema": 2, "keys": {}}"#).unwrap();
        assert!(matches!(
            load_references(&paths),
            Err(StoreError::Newer { found: 2, .. })
        ));
    }

    #[test]
    fn one_service_per_store() {
        let dir = test_dir("store-lock");
        let paths = Paths::under(&dir);
        let held = try_lock(&paths).unwrap();
        assert!(held.is_some());
        assert!(
            try_lock(&paths).unwrap().is_none(),
            "flock is per open file, so this conflicts"
        );
        drop(held);
        assert!(try_lock(&paths).unwrap().is_some());
    }
}
