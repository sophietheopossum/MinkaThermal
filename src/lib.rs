//! MinkaThermal core: the cooling-watch service, its store and the CLI verbs.
//!
//! Every module takes an explicit `Paths` (and, for sensors, an explicit filesystem root); only
//! `main.rs` reads the process environment. That keeps tests away from her live store and socket.

pub mod burn;
pub mod check;
pub mod cli;
pub mod clock;
pub mod episodes;
pub mod ipc;
pub mod model;
pub mod paths;
pub mod present;
pub mod profile;
pub mod references;
pub mod rules;
pub mod sampler;
pub mod sensors;
pub mod service;
pub mod store;

#[cfg(test)]
pub(crate) mod testutil;

/// File layout of state.json and references.json. A service that finds a higher one refuses to
/// write; a lower one is migrated after a `.v<N>.bak` copy.
pub const SCHEMA: u32 = 1;
/// Version of the thresholds. Every record carries it so `replay` can tell old verdicts apart.
pub const RULES_VERSION: u32 = 1;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One log line on stderr (the journal, under the unit). The line is formatted first and written
/// with a single `write_all`, so lines from the service and its burn child never interleave.
pub fn note(text: std::fmt::Arguments) {
    use std::fmt::Write as _;
    use std::io::Write as _;
    let mut line = String::with_capacity(128);
    let _ = writeln!(line, "minka-thermal: {text}");
    let _ = std::io::stderr().write_all(line.as_bytes());
}
