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
use sysinfo::{ProcessExt, SystemExt};

use crate::debugger::{
    DelveDebugger, GdbDebugger, LldbDebugger, PythonDebugger, StopAndWritePidDebugger,
};

/// Launches the given command and attaches a debugger to it.
#[derive(Debug, StructOpt)]
#[structopt(name = "dbgee", about = "the active debuggee")]
pub struct Opts {
    #[structopt(subcommand)]
    pub command: Subcommand,
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

    /// Specify the debugger used for the previous 'set' command, which will be used for 'unset'.
    /// Default is 'auto'. To explicitly specify it, choose one of 'gdb', 'lldb', 'dlv', 'stop-and-write-pid' and 'python'.
    #[structopt(short, long, default_value("auto"))]
    pub debugger: String,
}

#[derive(Debug, StructOpt)]
pub struct AttachOpts {
    /// Debugger to launch. Choose one of "auto", "gdb", "lldb", "dlv", "stop-and-write-pid" and "python".
    ///
    /// stop-and-write-pid: Stops the debuggee, and prints the debuggee's PID.
    /// dbgee writes the PID to /tmp/dbgee_pid. If stderr is a tty,
    /// dbgee outputs the PID to stderr as well.
    ///
    /// python: Use 'debugpy' module to debug Python in VSCode. Currently, 'python' ignores -t option and uses
    /// only VSCode.
    #[structopt(short, long, default_value("auto"))]
    pub debugger: String,

    /// Terminal to launch the debugger in.
    ///
    /// auto (default): choose 'vscode' if dbgee is running in an integrated terminal,
    /// choose 'tmuxp' otherwise.
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
    #[structopt(
        short,
        long,
        possible_values(DebuggerTerminalOpt::VARIANTS),
        default_value("auto")
    )]
    pub terminal: DebuggerTerminalOpt,
}

#[derive(Debug, EnumString, EnumVariantNames)]
#[strum(serialize_all = "kebab-case")]
pub enum DebuggerTerminalOpt {
    Auto,
    Tmuxw,
    Tmuxp,
    Vscode,
}

pub fn run(opts: Opts) -> Result<i32> {
    let (debuggee, debugger_name) = match opts.command {
        Subcommand::Run(ref run_opts) => (&run_opts.debuggee, &run_opts.attach_opts.debugger),
        Subcommand::Set(ref set_opts) => (&set_opts.debuggee, &set_opts.attach_opts.debugger),
        Subcommand::Unset(ref unset_opts) => (&unset_opts.debuggee, &unset_opts.debugger),
    };
    let mut debugger = build_debugger(debugger_name, debuggee)?;

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
        DebuggerTerminalOpt::Auto => build_debugger_terminal(&detect_debugger_terminal()),
        DebuggerTerminalOpt::Tmuxw => Box::new(Tmux::new(TmuxLayout::NewWindow)),
        DebuggerTerminalOpt::Tmuxp => Box::new(Tmux::new(TmuxLayout::NewPane)),
        DebuggerTerminalOpt::Vscode => Box::new(VsCode::new()),
    }
}

fn detect_debugger_terminal() -> DebuggerTerminalOpt {
    match is_in_vscode_term() {
        true => DebuggerTerminalOpt::Vscode,
        false => DebuggerTerminalOpt::Tmuxp,
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
            if cur_proc.name() == "node" && cur_proc.cmd().iter().any(|arg| arg.contains("vscode"))
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
