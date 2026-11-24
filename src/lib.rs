//! MinkaThermal core: the cooling-watch service, its store and the CLI verbs.
//!
//! Every module takes an explicit `Paths` (and, for sensors, an explicit filesystem root); only
//! `main` below reads the process environment. That keeps tests away from her live store and
//! socket. MinkaDE's one binary (minka/, at the top of the checkout) runs `main` as
//! `minka-thermal`.

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

/// One log line on stderr (the journal, under the unit). `minka_note` writes it with a single
/// write(2), so lines from the service and its burn child never interleave.
pub fn note(text: std::fmt::Arguments) {
    minka_note::note!("minka-thermal: {text}");
}

/// `minka-thermal`: verb dispatch by hand (as MinkaSyu's, no clap).
pub fn main() {
    use cli::{Command, EXIT_ERROR, EXIT_USAGE};
    use paths::Paths;
    use std::io::Write;
    use std::path::Path;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match cli::parse(&args) {
        Ok(cmd) => cmd,
        Err(reason) => {
            let text = format!("minka-thermal: {reason}\n{}", cli::USAGE);
            let _ = std::io::stderr().write_all(text.as_bytes());
            std::process::exit(EXIT_USAGE);
        }
    };
    if let Command::Burn { threads } = cmd {
        // The check's load generator needs no store; it must not touch one either.
        std::process::exit(crate::burn::run(threads));
    }
    if let Command::Ui { page } = cmd {
        // The window needs no store paths, only where to find itself and where to log.
        let env = |k: &str| std::env::var_os(k);
        let exe = std::env::current_exe().ok();
        std::process::exit(cli::ui(page, &env, exe.as_deref()));
    }
    let paths = match Paths::from_env(|k| std::env::var_os(k)) {
        Ok(p) => p,
        Err(e) => {
            note(format_args!("{e}"));
            std::process::exit(EXIT_ERROR);
        }
    };
    std::process::exit(cli::run(cmd, &paths, Path::new("/")));
}
