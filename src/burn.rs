//! The cooling check's load generator (§10.4): the hidden verb `minka-thermal burn --threads N`.
//!
//! Each thread runs batches of 2^20 xorshift rounds and bumps its own counter after each batch;
//! the main thread prints `{"t":<mono s>,"rounds":[…]}` once a second for the service to read.
//! On x86_64 the rounds are a fixed instruction stream in inline asm, so the work done per second
//! compares across rustc versions; other targets use a plain loop under another generator version,
//! and their work never compares with x86_64's.

use crate::clock::Moment;
use serde_json::json;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Rounds per batch: about a millisecond on her i7-1165G7.
pub const BATCH_ROUNDS: u64 = 1 << 20;

/// Work and frequency compare only within one generator version (and one CPU model).
#[cfg(target_arch = "x86_64")]
pub const GENERATOR_VERSION: u32 = 1;
#[cfg(not(target_arch = "x86_64"))]
pub const GENERATOR_VERSION: u32 = 1001;

/// One xorshift64 round in plain Rust: what the asm must compute.
pub fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// `n` rounds from `x`. The instruction stream is fixed by the asm, so a newer rustc cannot make
/// the same work look faster or slower.
#[cfg(target_arch = "x86_64")]
pub fn batch(mut x: u64, n: u64) -> u64 {
    // `dec` from zero would wrap and loop 2^64 times.
    if n == 0 {
        return x;
    }
    // SAFETY: register-only arithmetic; no memory or stack is touched, and every register the
    // block writes is declared to the compiler.
    unsafe {
        std::arch::asm!(
            "2:",
            "mov {t},{x}", "shl {t},13", "xor {x},{t}",
            "mov {t},{x}", "shr {t},7", "xor {x},{t}",
            "mov {t},{x}", "shl {t},17", "xor {x},{t}",
            "dec {n}", "jnz 2b",
            x = inout(reg) x,
            t = out(reg) _,
            n = inout(reg) n => _,
            options(nomem, nostack),
        );
    }
    x
}

#[cfg(not(target_arch = "x86_64"))]
pub fn batch(mut x: u64, n: u64) -> u64 {
    for _ in 0..n {
        x = std::hint::black_box(xorshift(x));
    }
    x
}

/// Each thread's counter on its own cache line, so the threads do not slow each other down.
#[repr(align(64))]
#[derive(Default)]
struct Slot(AtomicU64);

/// Runs until killed. The service kills the whole process group with SIGKILL at the end of the
/// load; if it goes away first, PR_SET_PDEATHSIG does, and a closed pipe ends the loop below.
pub fn run(threads: usize) -> i32 {
    // The service starts this through /proc/self/exe, which names the process "exe"; give it the
    // binary's own name back for top and the process lists, before the threads inherit it.
    // SAFETY: PR_SET_NAME copies at most 16 bytes from a NUL-terminated string.
    unsafe { libc::prctl(libc::PR_SET_NAME, c"minka-thermal".as_ptr() as libc::c_ulong, 0, 0, 0) };
    let slots: Arc<Vec<Slot>> = Arc::new((0..threads).map(|_| Slot::default()).collect());
    for i in 0..threads {
        let slots = Arc::clone(&slots);
        let spawned = std::thread::Builder::new()
            .name(format!("burn-{i}"))
            .spawn(move || {
                // Any non-zero seed: xorshift never leaves zero once there.
                let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ (i as u64 + 1);
                loop {
                    x = batch(x, BATCH_ROUNDS);
                    slots[i].0.fetch_add(1, Ordering::Relaxed);
                }
            });
        if let Err(e) = spawned {
            crate::note(format_args!("burn: thread {i}: {e}"));
            return 1;
        }
    }
    let start = Moment::now().mono_ns;
    let mut stdout = std::io::stdout().lock();
    for tick in 1i64.. {
        // On whole seconds from the start, so a late wakeup does not shift every later line.
        let due = start + tick * 1_000_000_000;
        let now = Moment::now().mono_ns;
        if due > now {
            std::thread::sleep(Duration::from_nanos((due - now) as u64));
        }
        let rounds: Vec<u64> = slots
            .iter()
            .map(|s| s.0.load(Ordering::Relaxed) * BATCH_ROUNDS)
            .collect();
        let mut line = json!({ "t": Moment::now().mono_s(), "rounds": rounds }).to_string();
        line.push('\n');
        // The service closed its end: nobody is measuring any more.
        if stdout.write_all(line.as_bytes()).and_then(|()| stdout.flush()).is_err() {
            return 0;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_batch_matches_plain_xorshift() {
        // Exactly one batch, about a millisecond: never more load than that in a test.
        let seed = 0x0123_4567_89AB_CDEFu64;
        let mut expected = seed;
        for _ in 0..BATCH_ROUNDS {
            expected = xorshift(expected);
        }
        assert_eq!(batch(seed, BATCH_ROUNDS), expected);
        assert_eq!(batch(seed, 0), seed, "no rounds, no change");
    }
}
