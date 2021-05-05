use dbgee::{run, Opts};
use nix::unistd;
use structopt::StructOpt;

use std::{
    io::Write,
    os::unix::prelude::AsRawFd,
    sync::atomic::{AtomicBool, Ordering},
};

fn main() {
    init_logger();

    match run(Opts::from_args()) {
        Ok(exit_status) => {
            std::process::exit(exit_status);
        }
        Err(e) => {
            log::error!("{}", e);
            std::process::exit(1);
        }
    }
}

fn init_logger() {
    let mut env_logger_builder = env_logger::Builder::new();
    let is_first_info = AtomicBool::new(true);
    env_logger_builder.format(move |buf, record| {
        if record.level() > log::Level::Error && is_first_info.fetch_and(false, Ordering::SeqCst) {
            writeln!(
                buf,
                "[Dbgee] Messages from dbgee are suppressed if the stderr is redirected or piped.",
            )?;
        }
        writeln!(
            buf,
            "[Dbgee]{} {}",
            if record.level() <= log::Level::Error {
                format!(" {}:", record.level())
            } else {
                "".to_owned()
            },
            record.args()
        )
    });
    if let Ok(true) = unistd::isatty(std::io::stderr().as_raw_fd()) {
        env_logger_builder.filter_level(log::LevelFilter::Info);
    } else {
        env_logger_builder.filter_level(log::LevelFilter::Error);
    }
    env_logger_builder.init();
}
