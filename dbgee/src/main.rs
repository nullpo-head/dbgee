use colored::*;
use dbgee::{run, LogLevel, Opts};
use nix::unistd;
use structopt::StructOpt;

use std::{
    io::Write,
    os::unix::prelude::AsRawFd,
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
};

fn main() {
    let opts = Opts::from_args();
    init_logger(&opts.log_level);

    match run(opts) {
        Ok(exit_status) => {
            log::debug!("exiting with {}", exit_status);
            std::process::exit(exit_status);
        }
        Err(e) => {
            log::error!("{:?}", e);
            std::process::exit(1);
        }
    }
}

fn init_logger(log_level: &Option<LogLevel>) {
    let mut env_logger_builder = env_logger::Builder::new();

    let should_show_info_suppression_notice;
    if let Some(ref level) = log_level {
        should_show_info_suppression_notice = false;
        env_logger_builder.filter_level(
            log::LevelFilter::from_str(
                <LogLevel as strum::VariantNames>::VARIANTS[*level as usize],
            )
            .unwrap(),
        );
    } else if let Ok(true) = unistd::isatty(std::io::stderr().as_raw_fd()) {
        should_show_info_suppression_notice = true;
        env_logger_builder.filter_level(log::LevelFilter::Info);
    } else {
        should_show_info_suppression_notice = false;
        env_logger_builder.filter_level(log::LevelFilter::Error);
    }
    let should_show_info_suppression_notice = AtomicBool::new(should_show_info_suppression_notice);

    env_logger_builder.format(move |buf, record| {
        if record.level() > log::Level::Error
            && should_show_info_suppression_notice.fetch_and(false, Ordering::SeqCst)
        {
            writeln!(
                buf,
                "{} These dbgee's messages are suppressed if the stderr is redirected or piped.",
                "[Dbgee]".bright_green()
            )?;
        }
        writeln!(
            buf,
            "{}{} {}",
            "[Dbgee]".bright_green(),
            match record.level() {
                log::Level::Info => "".to_string(),
                log::Level::Error | log::Level::Warn =>
                    format!("[{}]", record.level()).red().to_string(),
                _ => format!("[{}]", record.level()).bright_green().to_string(),
            },
            record.args()
        )
    });
    env_logger_builder.init();
}
