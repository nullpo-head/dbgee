mod debugger;
mod debugger_terminal;
mod file_helper;

use debugger::Debugger;
use debugger_terminal::{DebuggerTerminal, Tmux, TmuxLayout, VsCode};
use file_helper::is_executable;

use std::str;

use anyhow::{anyhow, bail, Result};
use nix::sys::wait;
use nix::unistd::Pid;
use structopt::StructOpt;
use strum::{EnumString, EnumVariantNames, VariantNames as _};

use crate::debugger::{
    DelveDebugger, GdbDebugger, LldbDebugger, PythonDebugger, StopAndWritePidDebugger,
};

/// Launches the given command and attaches a debugger to it.
#[derive(Debug, StructOpt)]
#[structopt(name = "dbgee", about = "the active debuggee")]
pub struct Opts {
    #[structopt(subcommand)]
    pub command: Subcommand,

    /// Debugger to launch. Choose "auto", "gdb", "dlv", "stop-and-write-pid", "python"
    /// or you can specify an arbitrary command line.
    /// The debuggee's PID follows your command line as an argument.
    ///
    /// stop-and-write-pid: Stops the debuggee, and prints the debuggee's PID.
    /// dbgee writes the PID to /tmp/dbgee_pid. If stderr is a tty,
    /// dbgee outputs the PID to stderr as well.
    #[structopt(short, long, default_value("auto"))]
    pub debugger: String,
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
    usage = "dbgee run [OPTIONS] -- <debuggee> [args-for-debuggee]...",
    rename_all = "kebab"
)]
pub struct RunOpts {
    /// Path to the debuggee process
    #[structopt()]
    pub debuggee: String,

    #[structopt(name = "args")]
    pub debuggee_args: Vec<String>,

    #[structopt(flatten)]
    attach_opts: AttachOpts,
}

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
}

#[derive(Debug, StructOpt)]
pub struct AttachOpts {
    /// Terminal to launch the debugger in.
    ///
    /// tmuxw (default): Opens a new tmux window in last active tmux session,
    /// launches a debugger there, and has the debugger attach to the debuggee.
    /// If there is no active tmux session, it launches a new session in the background,
    /// and writes a notification to stderr (as far as stderr is a tty).
    ///
    /// tmuxp: Opens a new tmux pane in last active tmux session.
    ///
    /// vscode: Open nothing in the terminal, and wait for VSCode to connect to the debugger
    ///
    #[structopt(
        short,
        long,
        possible_values(DebuggerTerminalOpt::VARIANTS),
        default_value("tmuxw")
    )]
    pub terminal: DebuggerTerminalOpt,
}

#[derive(Debug, EnumString, EnumVariantNames)]
#[strum(serialize_all = "kebab-case")]
pub enum DebuggerTerminalOpt {
    Tmuxw,
    Tmuxp,
    Vscode,
}

pub fn run(opts: Opts) -> Result<i32> {
    let debuggee = match opts.command {
        Subcommand::Run(ref run_opts) => &run_opts.debuggee,
        Subcommand::Set(ref set_opts) => &set_opts.debuggee,
        Subcommand::Unset(ref unset_opts) => &unset_opts.debuggee,
    };
    let mut debugger = build_debugger(&opts.debugger, debuggee)?;

    if !is_executable(debuggee) {
        bail!(
            "the debugee (path: '{}') is not an executable file.",
            debuggee
        );
    }

    match opts.command {
        Subcommand::Run(run_opts) => {
            let mut debugger_terminal = build_debugger_terminal(&run_opts.attach_opts.terminal);
            let pid = debugger.run(&run_opts, debugger_terminal.as_mut())?;
            Ok(wait_until_exit(pid)?)
        }
        Subcommand::Set(set_opts) => {
            let mut debugger_terminal = build_debugger_terminal(&set_opts.attach_opts.terminal);
            debugger.set(&set_opts, debugger_terminal.as_mut())?;
            Ok(0)
        }
        Subcommand::Unset(unset_opts) => {
            debugger.unset(&unset_opts)?;
            Ok(0)
        }
    }
}

fn build_debugger(debugger: &str, debuggee: &str) -> Result<Box<dyn Debugger>> {
    match debugger {
        "auto" => detect_debugger(debuggee),
        "gdb" => Ok(Box::new(GdbDebugger::build()?)),
        "lldb" => Ok(Box::new(LldbDebugger::build()?)),
        "dlv" => Ok(Box::new(DelveDebugger::new()?)),
        "stop-and-write-pid" => Ok(Box::new(StopAndWritePidDebugger::new())),
        "debugpy" => Ok(Box::new(PythonDebugger::new()?)),
        _ => Err(anyhow!("Unsupported debugger: {}", debugger)),
    }
}

fn detect_debugger(debuggee: &str) -> Result<Box<dyn Debugger>> {
    // lldb omitted in favor of gdb
    for debugger in ["dlv", "gdb", "debugpy", "stop-and-write-pid"].iter() {
        let candidate = build_debugger(debugger, debuggee);
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

fn build_debugger_terminal(action: &DebuggerTerminalOpt) -> Box<dyn DebuggerTerminal> {
    match action {
        DebuggerTerminalOpt::Tmuxw => Box::new(Tmux::new(TmuxLayout::NewWindow)),
        DebuggerTerminalOpt::Tmuxp => Box::new(Tmux::new(TmuxLayout::NewPane)),
        DebuggerTerminalOpt::Vscode => Box::new(VsCode::new()),
    }
}

fn wait_until_exit(pid: Pid) -> Result<i32> {
    let exitcode_signalled = 130;
    loop {
        match wait::waitpid(pid, None) {
            Ok(wait::WaitStatus::Exited(_, exit_status)) => {
                return Ok(exit_status);
            }
            Ok(wait::WaitStatus::Signaled(_, _, _)) => {
                return Ok(exitcode_signalled);
            }
            Err(nix::Error::Sys(nix::errno::Errno::ECHILD)) => {
                return Ok(0);
            }
            _ => (),
        }
    }
}
