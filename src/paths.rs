//! Where the store, the socket and the settings file live.
//!
//! `main.rs` is the only place that reads the environment: it passes a lookup function in here,
//! and tests pass a map instead. Every other file of the store sits beside state.json, so one
//! variable (`MINKA_THERMAL_STATE`) moves the whole store.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// The store directory (mode 0700), the parent of state.json.
    pub dir: PathBuf,
    pub state: PathBuf,
    /// flock'd by the service, and only by the service.
    pub lock: PathBuf,
    pub socket: PathBuf,
    /// `~/.config/minka-settings.json` (MinkaShell's path), which holds `thermal.chassis`.
    /// None when HOME is unset; the chassis setting then reads as `auto`.
    pub settings: Option<PathBuf>,
    pub references: PathBuf,
    pub events: PathBuf,
    pub records: PathBuf,
    pub daily: PathBuf,
    pub cases: PathBuf,
    pub checks: PathBuf,
}

impl Paths {
    /// Resolves the paths from environment lookups (§11.1):
    /// state.json is `$MINKA_THERMAL_STATE`, else `$XDG_STATE_HOME/minka-thermal/state.json`, else
    /// `~/.local/state/minka-thermal/state.json`; the socket is `$MINKA_THERMAL_SOCKET`, else
    /// beside a store moved by `$MINKA_THERMAL_STATE`, else `$XDG_RUNTIME_DIR/minka-thermal.sock`.
    /// Empty values count as unset, and so do relative XDG values, as the XDG spec says.
    pub fn from_env(get: impl Fn(&str) -> Option<OsString>) -> Result<Paths, String> {
        let var = |k: &str| get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let xdg = |k: &str| var(k).filter(|p| p.is_absolute());
        let home = var("HOME");

        let moved = var("MINKA_THERMAL_STATE");
        let state = match (&moved, xdg("XDG_STATE_HOME"), &home) {
            (Some(p), _, _) => p.clone(),
            (None, Some(x), _) => x.join("minka-thermal/state.json"),
            (None, None, Some(h)) => h.join(".local/state/minka-thermal/state.json"),
            (None, None, None) => {
                return Err("HOME is not set and neither is MINKA_THERMAL_STATE".to_owned());
            }
        };
        let dir = parent_dir(&state);
        let beside = dir.join("minka-thermal.sock");
        // A store moved with MINKA_THERMAL_STATE alone (a test, a fixture) takes its socket along:
        // the runtime-dir socket is her live service's, and a `cleaned` meant for the test store
        // would otherwise land in her real history. Without a runtime dir (a bare ssh or cron
        // shell) the socket is beside the store too, so the CLI and the service still meet.
        let socket = match (var("MINKA_THERMAL_SOCKET"), &moved) {
            (Some(s), _) => s,
            (None, Some(_)) => beside,
            (None, None) => xdg("XDG_RUNTIME_DIR")
                .map(|r| r.join("minka-thermal.sock"))
                .unwrap_or(beside),
        };
        let settings = home.map(|h| h.join(".config/minka-settings.json"));
        Ok(Paths::with(state, socket, settings))
    }

    /// Builds every path from an explicit state.json, socket and settings file.
    pub fn with(state: PathBuf, socket: PathBuf, settings: Option<PathBuf>) -> Paths {
        let dir = parent_dir(&state);
        Paths {
            lock: dir.join("state.lock"),
            references: dir.join("references.json"),
            events: dir.join("events.jsonl"),
            records: dir.join("records.jsonl"),
            daily: dir.join("daily.jsonl"),
            cases: dir.join("cases.jsonl"),
            checks: dir.join("checks"),
            dir,
            state,
            socket,
            settings,
        }
    }

    /// Everything inside `dir`, the socket and the settings file included. For tests.
    pub fn under(dir: &Path) -> Paths {
        Paths::with(
            dir.join("state.json"),
            dir.join("minka-thermal.sock"),
            Some(dir.join("minka-settings.json")),
        )
    }

    /// Where a month's records go at rollover: `records-YYYY-MM.jsonl`.
    pub fn records_archive(&self, month: &str) -> PathBuf {
        self.dir.join(format!("records-{month}.jsonl"))
    }

    /// The copy kept when a lower schema is migrated: `state.json.v<N>.bak`.
    pub fn backup_of(file: &Path, schema: u32) -> PathBuf {
        let mut name = file.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".v{schema}.bak"));
        file.with_file_name(name)
    }
}

fn parent_dir(file: &Path) -> PathBuf {
    match file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn resolve(vars: &[(&str, &str)]) -> Result<Paths, String> {
        let map: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        Paths::from_env(|k| map.get(k).cloned())
    }

    #[test]
    fn home_fallback() {
        let p = resolve(&[("HOME", "/home/u")]).unwrap();
        assert_eq!(
            p.state,
            PathBuf::from("/home/u/.local/state/minka-thermal/state.json")
        );
        assert_eq!(p.dir, PathBuf::from("/home/u/.local/state/minka-thermal"));
        assert_eq!(p.lock, p.dir.join("state.lock"));
        assert_eq!(p.events, p.dir.join("events.jsonl"));
        assert_eq!(p.checks, p.dir.join("checks"));
        // No runtime dir: the socket sits beside the store.
        assert_eq!(p.socket, p.dir.join("minka-thermal.sock"));
        assert_eq!(
            p.settings,
            Some(PathBuf::from("/home/u/.config/minka-settings.json"))
        );
    }

    #[test]
    fn xdg_and_overrides() {
        let p = resolve(&[
            ("HOME", "/home/u"),
            ("XDG_STATE_HOME", "/xs"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ])
        .unwrap();
        assert_eq!(p.state, PathBuf::from("/xs/minka-thermal/state.json"));
        assert_eq!(p.socket, PathBuf::from("/run/user/1000/minka-thermal.sock"));

        let p = resolve(&[
            ("HOME", "/home/u"),
            ("XDG_STATE_HOME", "/xs"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("MINKA_THERMAL_STATE", "/scratch/a/state.json"),
            ("MINKA_THERMAL_SOCKET", "/scratch/a/t.sock"),
        ])
        .unwrap();
        assert_eq!(p.dir, PathBuf::from("/scratch/a"));
        assert_eq!(p.records, PathBuf::from("/scratch/a/records.jsonl"));
        assert_eq!(p.socket, PathBuf::from("/scratch/a/t.sock"));

        // The store moved alone: the socket goes with it, never to the live service's.
        let p = resolve(&[
            ("HOME", "/home/u"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("MINKA_THERMAL_STATE", "/scratch/a/state.json"),
        ])
        .unwrap();
        assert_eq!(p.socket, PathBuf::from("/scratch/a/minka-thermal.sock"));
        assert_eq!(
            p.records_archive("2026-11"),
            PathBuf::from("/scratch/a/records-2026-11.jsonl")
        );
        assert_eq!(
            Paths::backup_of(&p.state, 0),
            PathBuf::from("/scratch/a/state.json.v0.bak")
        );
    }

    #[test]
    fn empty_and_relative_values_are_unset() {
        let p = resolve(&[
            ("HOME", "/home/u"),
            ("XDG_STATE_HOME", "relative/dir"),
            ("XDG_RUNTIME_DIR", ""),
            ("MINKA_THERMAL_STATE", ""),
        ])
        .unwrap();
        assert_eq!(
            p.state,
            PathBuf::from("/home/u/.local/state/minka-thermal/state.json")
        );
        assert_eq!(p.socket, p.dir.join("minka-thermal.sock"));
    }

    #[test]
    fn no_home() {
        assert!(resolve(&[]).is_err());
        let p = resolve(&[("MINKA_THERMAL_STATE", "state.json")]).unwrap();
        assert_eq!(p.dir, PathBuf::from("."));
        assert_eq!(p.settings, None);
    }
}
