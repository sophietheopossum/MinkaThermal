//! `minka-thermal`: verb dispatch by hand (as MinkaSyu's main.rs, no clap). This is the only
//! place that reads the process environment; everything below it takes `Paths`.

use minka_thermal::cli::{self, Command, EXIT_ERROR, EXIT_USAGE};
use minka_thermal::note;
use minka_thermal::paths::Paths;
use std::io::Write;
use std::path::Path;

fn main() {
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
        std::process::exit(minka_thermal::burn::run(threads));
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
