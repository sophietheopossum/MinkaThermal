//! Test scaffolding. Everything a test touches lives under `target/test-tmp/<name>/`; the path is
//! fixed at compile time, so no test ever reads the process environment.

use crate::check::Cmd;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A fresh, empty directory for one test.
pub fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-tmp")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// Writes `content` to `root/rel`, creating the parents.
pub fn put(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("create parents");
    fs::write(&path, content).expect("write fake file");
}

/// Creates the symlink `root/rel -> target`.
pub fn link(root: &Path, rel: &str, target: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("create parents");
    std::os::unix::fs::symlink(target, &path).expect("symlink");
}

/// Orphans this process causes are re-parented to it, so a test can reap a grandchild of the
/// load itself and see the signal that ended it.
pub fn become_subreaper() {
    // SAFETY: a process attribute; it affects only orphans of this test process.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
}

/// Waits up to 5 s for `pid` (a child, or an orphan adopted by this subreaper) and returns the
/// signal that ended it. One that is still alive then is killed by its exact pid.
pub fn reap(pid: i32) -> Option<i32> {
    let mut alive = false;
    for _ in 0..250 {
        let mut status = 0;
        // SAFETY: waits for one specific pid, without blocking.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            return libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status));
        }
        alive = r == 0;
        std::thread::sleep(Duration::from_millis(20));
    }
    if alive {
        // SAFETY: our own unreaped child, so the pid cannot have been reused.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    None
}

/// The pid the test script wrote, once it is there.
pub fn read_pid(path: &Path) -> i32 {
    for _ in 0..250 {
        if let Some(pid) = fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok()) {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{} was never written", path.display());
}

/// The direct child has been reaped: no such process any more.
pub fn reaped(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid, 0) == -1 && *libc::__errno_location() == libc::ESRCH }
}

/// A shell script as a check's load (or dGPU query): something harmless that stands in for the
/// burn. Absolute paths, so nothing depends on the test process's PATH.
pub fn sh(script: &str) -> Cmd {
    Cmd {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
    }
}

/// A load that does nothing for 30 s.
pub fn sleeper() -> Cmd {
    Cmd {
        program: "/usr/bin/sleep".into(),
        args: vec!["30".into()],
    }
}

/// A load that starts a grandchild in its own process group and writes that pid to a file, so a
/// test can prove the whole group is killed, not just the child.
pub fn family(dir: &Path) -> (Cmd, PathBuf) {
    let file = dir.join("grandchild.pid");
    let cmd = sh(&format!(
        "/usr/bin/sleep 30 & echo $! > '{}'; wait",
        file.display()
    ));
    (cmd, file)
}
