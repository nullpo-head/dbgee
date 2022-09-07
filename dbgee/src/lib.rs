mod debugger;
mod debugger_terminal;
mod file_helper;
mod os;

use debugger::Debugger;
use debugger_terminal::{DebuggerTerminal, Tmux, TmuxLayout, VsCode};
use file_helper::is_executable;
use log::debug;
use os::{is_any_hook_condition_set, run_hook};

use std::str;

use anyhow::{anyhow, bail, Context, Result};
use nix::sys::wait;
use nix::unistd::Pid;
use structopt::StructOpt;
use strum::{EnumString, EnumVariantNames, VariantNames as _};
use sysinfo::{ProcessExt, SystemExt};

use crate::debugger::{
    DelveDebugger, GdbDebugger, LldbDebugger, PythonDebugger, StopAndWritePidDebugger,
};

pub use debugger_terminal::set_vscode_communication_fifo_path_prefix;

#[derive(Debug, StructOpt)]
/// The zero-configuration debuggee for debuggers.
///
/// Dbgee is a handy utility that allows you to launch CLI debuggers and VSCode debuggers from the debuggee side.
/// Just start your program by a simple command in a terminal, and the debugger will automatically attach to it with zero configuration.
/// Dbgee also has the ability to preconfigure your program to automatically start a debug session no matter how the program is started.
///
/// Dbgee is very useful especially when your program requires command line arguments or redirection, or when your program is launched by some script.
/// In addition, Dbgee frees you from the hassle of writing `launch.json` for VSCode.
///  
#[structopt(name = "dbgee")]
pub struct Opts {
    #[structopt(short, long)]
    pub log_level: Option<LogLevel>,

    /// Prefix to override the path of VSCode communication FIFO paths. Mainly for integration tests.
    #[structopt(long, hidden = true)]
    pub vscode_fifo_prefix: Option<String>,

    #[structopt(subcommand)]
    pub command: Subcommand,
}

#[derive(Copy, Clone, Debug, EnumString, EnumVariantNames)]
#[strum(serialize_all = "kebab-case")]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, StructOpt)]
pub enum Subcommand {
    Run(RunOpts),
    Set(SetOpts),
    Unset(UnsetOpts),
}

/// Launches the debuggee, and attaches the specified debugger to it.
#[derive(Debug, StructOpt)]
#[structopt(
    usage = "dbgee run [OPTIONS] -- <command> [args-for-command]...",
    rename_all = "kebab"
)]
pub struct RunOpts {
    /// Path to the process to launch. The debugger attaches to this <command>
    /// unless any hook conditions are given.
    #[structopt()]
    pub command: String,

    #[structopt(name = "args")]
    pub command_args: Vec<String>,

    #[structopt(flatten)]
    attach_opts: AttachOpts,

    #[structopt(flatten)]
    hook_opts: os::HookOpts,
}

// Positional arguments of SetOpts. `debugger::set_exec_to_dbgee` needs this constants
// in order to construct `$ dbgee run` command to launch a debugger
const SETOPTS_POSITIONAL_ARGS: [&str; 2] = ["debuggee", "start-cmd"];

/// Replaces the debuggee with a wrapper script, so that the debugger will be attached to it whenever
/// it is launched by any processes from now on.
///
/// Please run "unset" command to restore the original debuggee if you don't want to attach debuggers anymore.
/// Or, if you give start_cmd, dbgee automatically does "unset" after start_cmd finishes.
#[derive(Debug, StructOpt)]
#[structopt(
    usage = "dbgee set [OPTIONS] <debuggee>  [-- <run_cmd> [args-for-debuggee]...]",
    rename_all = "kebab"
)]
pub struct SetOpts {
    /// Path to the debuggee process
    #[structopt()]
    pub debuggee: String,

    /// If start_cmd is given, dbgee launches start_cmd, and automatically unsets after
    /// start_cmd finishes
    #[structopt()]
    pub start_cmd: Vec<String>,

    #[structopt(flatten)]
    attach_opts: AttachOpts,
}

/// Removes the wrapper script which "set" put, and restores the original debuggee file.
#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab")]
pub struct UnsetOpts {
    /// Path to the debuggee process
    #[structopt(name = "debuggee")]
    pub debuggee: String,

    /// Specify the debugger used for the previous 'set' command, which will be used for 'unset'.
    /// Default is 'auto'. To explicitly specify it, choose one of 'gdb', 'lldb', 'dlv', 'stop-and-write-pid' and 'python'.
    #[structopt(short, long)]
    pub debugger: Option<DebuggerOptValues>,
}

#[derive(Debug, StructOpt)]
pub struct AttachOpts {
    /// Debugger to launch. Choose one of "gdb", "lldb", "dlv", "stop-and-write-pid" and "python".
    ///
    /// stop-and-write-pid: Stops the debuggee, and prints the debuggee's PID.
    /// dbgee writes the PID to /tmp/dbgee_pid. If stderr is a tty,
    /// dbgee outputs the PID to stderr as well.
    /// debugpy: Use 'debugpy' module to debug Python in VSCode. Currently, 'python' ignores -t option and uses
    /// only VSCode.
    ///
    /// If not given, dbgee tries to automatically detect the right debugger; use dlv if the debuggee
    /// file is compiled by Go, use gdb (on linux) / lldb (on macOS) for other compiled binary, use
    /// python if the debuggee is a Python file, and exits with error otherwise.
    ///
    #[structopt(short, long, possible_values(DebuggerOptValues::VARIANTS))]
    pub debugger: Option<DebuggerOptValues>,

    /// Terminal to launch the debugger in.
    ///
    /// If not given, the default values is 'vscode' if dbgee is running in an integrated terminal,
    /// or 'tmuxp' otherwise.
    ///
    /// tmuxw: Opens a new tmux window in last active tmux session,
    /// launches a debugger there, and has the debugger attach to the debuggee.
    /// If there is no active tmux session, it launches a new session in the background,
    /// and writes a notification to stderr (as far as stderr is a tty).
    ///
    /// tmuxp: Opens a new tmux pane in last active tmux session.
    ///
    /// vscode: Open nothing in the terminal, and wait for VSCode to connect to the debugger
    ///
    #[structopt(short, long, possible_values(TerminalOptValues::VARIANTS))]
    pub terminal: Option<TerminalOptValues>,
}

#[derive(Debug, EnumString, EnumVariantNames)]
#[strum(serialize_all = "kebab-case")]
pub enum TerminalOptValues {
    Tmuxw,
    Tmuxp,
    Vscode,
}

#[derive(Debug, Clone, Copy, EnumString, EnumVariantNames)]
#[strum(serialize_all = "kebab-case")]
pub enum DebuggerOptValues {
    Gdb,
    Lldb,
    Dlv,
    StopAndWritePid,
    Debugpy,
}

pub fn run(opts: Opts) -> Result<i32> {
    match opts.command {
        Subcommand::Run(run_opts) => {
            bail_if_not_executable(&run_opts.command)?;

            if is_any_hook_condition_set(&run_opts.hook_opts) {
                run_hook(
                    run_opts.command,
                    run_opts.command_args,
                    run_opts.hook_opts,
                    run_opts.attach_opts,
                )
                .context("Running with hook conditions failed")?;
                return Ok(0);
            }

            let mut debugger = build_debugger(&run_opts.attach_opts.debugger, &run_opts.command)?;
            let mut debugger_terminal = build_debugger_terminal(&run_opts.attach_opts.terminal);
            let pid = debugger.run(
                &run_opts.command,
                run_opts.command_args.iter().map(String::as_str).collect(),
                debugger_terminal.as_mut(),
            )?;
            Ok(wait_pid_exit(pid)?)
        }

        Subcommand::Set(set_opts) => {
            bail_if_not_executable(&set_opts.debuggee)?;

            let mut debugger = build_debugger(&set_opts.attach_opts.debugger, &set_opts.debuggee)?;
            let mut debugger_terminal = build_debugger_terminal(&set_opts.attach_opts.terminal);
            debugger.set(
                &set_opts.debuggee,
                set_opts.start_cmd.iter().map(String::as_str).collect(),
                debugger_terminal.as_mut(),
            )?;
            Ok(0)
        }

        Subcommand::Unset(unset_opts) => {
            bail_if_not_executable(&unset_opts.debuggee)?;

            let mut debugger = build_debugger(&unset_opts.debugger, &unset_opts.debuggee)?;
            debugger.unset(&unset_opts.debuggee)?;
            Ok(0)
        }
    }
}

fn bail_if_not_executable(debuggee: &str) -> Result<()> {
    if !is_executable(debuggee) {
        bail!(
            "the debugee (path: '{}') is not an executable file.",
            debuggee
        );
    }
    Ok(())
}

fn build_debugger(
    debugger: &Option<DebuggerOptValues>,
    debuggee: &str,
) -> Result<Box<dyn Debugger>> {
    match debugger {
        None => detect_debugger(debuggee).context("Failed to detect the right debugger"),
        Some(debugger_type) => match *debugger_type {
            DebuggerOptValues::Gdb => Ok(Box::new(GdbDebugger::build()?)),
            DebuggerOptValues::Lldb => Ok(Box::new(LldbDebugger::build()?)),
            DebuggerOptValues::Dlv => Ok(Box::new(DelveDebugger::new()?)),
            DebuggerOptValues::StopAndWritePid => Ok(Box::new(StopAndWritePidDebugger::new())),
            DebuggerOptValues::Debugpy => Ok(Box::new(PythonDebugger::new()?)),
        },
    }
}

fn detect_debugger(debuggee: &str) -> Result<Box<dyn Debugger>> {
    use DebuggerOptValues::*;

    let debuggers = if cfg!(target_os = "linux") {
        // prefer gdb to lldb  in Linux
        [Dlv, Gdb, Debugpy, StopAndWritePid]
    } else {
        // macOS
        // prefer lldb
        [Dlv, Lldb, Debugpy, StopAndWritePid]
    };
    for debugger in debuggers.iter() {
        let candidate = build_debugger(&Some(*debugger), debuggee);
        if candidate.is_err() {
            continue;
        }
        let candidate = candidate.unwrap();
        if let Ok(true) = candidate.is_debuggee_surely_supported(debuggee) {
            return Ok(candidate);
        }
    }
    bail!("Could not automatically detect the proper debugger for the given debuggee")
}

fn build_debugger_terminal(terminal: &Option<TerminalOptValues>) -> Box<dyn DebuggerTerminal> {
    match terminal {
        None => build_debugger_terminal(&Some(detect_debugger_terminal())),
        Some(terminal) => match *terminal {
            TerminalOptValues::Tmuxw => Box::new(Tmux::new(TmuxLayout::NewWindow)),
            TerminalOptValues::Tmuxp => Box::new(Tmux::new(TmuxLayout::NewPane)),
            TerminalOptValues::Vscode => Box::new(VsCode::new()),
        },
    }
}

fn detect_debugger_terminal() -> TerminalOptValues {
    match is_in_vscode_term() {
        true => TerminalOptValues::Vscode,
        false => TerminalOptValues::Tmuxp,
    }
}

fn is_in_vscode_term() -> bool {
    let inner = || -> Result<bool> {
        let process_info = sysinfo::RefreshKind::new();
        process_info.with_processes();
        let mut sysinfo_system = sysinfo::System::new_with_specifics(process_info);
        sysinfo_system.refresh_processes();
        let processes = sysinfo_system.get_processes();

        let cur_pid = sysinfo::get_current_pid().map_err(|e| anyhow!(e))?;
        let mut cur_proc_opt = processes.get(&cur_pid);
        while let Some(cur_proc) = cur_proc_opt {
            // for remote development
            if cur_proc.name() == "node" && cur_proc.cmd().iter().any(|arg| arg.contains("vscode"))
            {
                return Ok(true);
            }
            // for at least macOS
            if cur_proc.name() == "Electron"
                && cur_proc
                    .exe()
                    .as_os_str()
                    .to_string_lossy()
                    .contains("Visual Studio Code")
            {
                return Ok(true);
            }
            if let Some(parent_pid) = cur_proc.parent() {
                cur_proc_opt = processes.get(&parent_pid);
            } else {
                break;
            }
        }
        Ok(false)
    };
    inner().unwrap_or(false)
}

/// wait for pid to exit and returns its exit code
fn wait_pid_exit(pid: Pid) -> Result<i32> {
    let exitcode_signaled = 130;
    loop {
        match wait::waitpid(pid, None) {
            Ok(wait::WaitStatus::Exited(_, exit_status)) => {
                return Ok(exit_status);
            }
            Ok(wait::WaitStatus::Signaled(_, _, _)) => {
                return Ok(exitcode_signaled);
            }
            Err(nix::Error::Sys(nix::errno::Errno::ECHILD)) => {
                return Ok(0);
            }
            _ => (),
        }
    }
}

pub trait ErrorLogger: std::fmt::Debug {
    fn debug_log_error(&self);
}

impl<T: std::fmt::Debug> ErrorLogger for Result<T> {
    fn debug_log_error(&self) {
        if let Err(e) = self {
            debug!("non fatal error: {:?}", e);
        }
    }
}
