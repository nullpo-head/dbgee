use crate::{
    file_helper::{
        command_exists, get_abspath, get_cached_command_output, get_valid_executable_path,
    },
    DebuggerTerminal, RunOpts, SetOpts, UnsetOpts,
};
use crate::{Opts, SETOPTS_POSITIONAL_ARGS};

use std::ffi::CString;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::{collections::HashMap, fs::File};
use std::{env, fs};
use std::{
    io::{BufRead, BufReader},
    str,
};
use std::{path::PathBuf, str::FromStr};

use anyhow::{anyhow, bail, Context, Result};
use nix::sys::{ptrace, signal, wait};
use nix::unistd;
use nix::unistd::Pid;
use once_cell::sync::Lazy;
use regex::Regex;
use structopt::clap::ArgMatches;
use structopt::StructOpt;
use strum::{Display, EnumString};

pub trait Debugger {
    fn run(&mut self, run_opts: &RunOpts, terminal: &mut dyn DebuggerTerminal) -> Result<Pid>;
    fn set(&mut self, set_opts: &SetOpts, terminal: &mut dyn DebuggerTerminal) -> Result<()>;
    fn unset(&mut self, unset_opts: &UnsetOpts) -> Result<()>;
    fn build_attach_commandline(&self) -> Result<Vec<String>>;
    fn build_attach_information(&self) -> Result<HashMap<AttachInformationKey, String>>;
    // Note that a debugger could support debuggee even if is_surely_supported_debuggee == false
    // because Dbgee doesn't recognize all file types which each debugger supports.
    fn is_debuggee_surely_supported(&self, debuggee: &str) -> Result<bool>;
}

#[derive(Debug, PartialEq, Eq, Hash, EnumString, Display)]
#[strum(serialize_all = "camelCase")]
pub enum AttachInformationKey {
    DebuggerTypeHint,
    Pid,
    DebuggerPort,
    ProgramName,
}

pub struct GdbDebugger;

impl GdbDebugger {
    pub fn build() -> Result<GdbCompatibleDebugger> {
        let command_builder = |pid: Pid, _name: String| {
            Ok(vec![
                "gdb".to_owned(),
                "-tui".to_owned(),
                "-p".to_owned(),
                pid.as_raw().to_string(),
            ])
        };
        GdbCompatibleDebugger::new("gdb", Box::new(command_builder))
    }
}

pub struct LldbDebugger;

impl LldbDebugger {
    pub fn build() -> Result<GdbCompatibleDebugger> {
        let command_builder = |pid: Pid, _name: String| {
            Ok(vec![
                "lldb".to_owned(),
                "-p".to_owned(),
                pid.as_raw().to_string(),
            ])
        };
        GdbCompatibleDebugger::new("lldb", Box::new(command_builder))
    }
}

pub struct GdbCompatibleDebugger {
    debugger_name: String,
    debuggee_pid: Option<Pid>,
    debuggee_path: Option<String>,
    commandline_builder: Box<dyn Fn(Pid, String) -> Result<Vec<String>>>,
}

impl GdbCompatibleDebugger {
    pub fn new(
        debugger_name: &str,
        command_builder: Box<dyn Fn(Pid, String) -> Result<Vec<String>>>,
    ) -> Result<GdbCompatibleDebugger> {
        if !command_exists(debugger_name) {
            bail!(
                "'{}' is not in PATH. Did you install {}?",
                debugger_name,
                debugger_name
            )
        }
        Ok(GdbCompatibleDebugger {
            debugger_name: debugger_name.to_owned(),
            debuggee_pid: None,
            debuggee_path: None,
            commandline_builder: command_builder,
        })
    }
}

impl Debugger for GdbCompatibleDebugger {
    fn run(&mut self, run_opts: &RunOpts, terminal: &mut dyn DebuggerTerminal) -> Result<Pid> {
        let debuggee_abspath = get_path_of_unset_debuggee(&run_opts.debuggee)?;
        let debuggee_pid = run_and_stop_dbgee(
            &debuggee_abspath,
            run_opts.debuggee_args.iter().map(String::as_str),
        )?;
        self.debuggee_pid = Some(debuggee_pid);
        self.debuggee_path = Some(debuggee_abspath);
        terminal.open(self)?;
        Ok(debuggee_pid)
    }

    fn set(&mut self, set_opts: &SetOpts, terminal: &mut dyn DebuggerTerminal) -> Result<()> {
        set_to_exec_dgeee(set_opts, terminal)
    }

    fn unset(&mut self, unset_opts: &UnsetOpts) -> Result<()> {
        unset_from_exec_dbgee(unset_opts)
    }

    fn build_attach_commandline(&self) -> Result<Vec<String>> {
        (self.commandline_builder)(
            self.debuggee_pid
                .ok_or_else(|| anyhow!("[BUG] uninitialized GdbCompatibleDebugger"))?,
            self.debuggee_path
                .clone()
                .ok_or_else(|| anyhow!("[BUG] uninitialized GdbCompatibleDebugger"))?,
        )
    }

    fn build_attach_information(&self) -> Result<HashMap<AttachInformationKey, String>> {
        let mut info = HashMap::new();
        info.insert(
            AttachInformationKey::DebuggerTypeHint,
            self.debugger_name.clone(),
        );
        info.insert(
            AttachInformationKey::Pid,
            format!(
                "{}",
                self.debuggee_pid.ok_or_else(|| anyhow!(
                    "[BUG] uninitialized GdbCompatibleDebugger: {}",
                    self.debugger_name
                ))?
            ),
        );
        info.insert(
            AttachInformationKey::ProgramName,
            self.debuggee_path.clone().ok_or_else(|| {
                anyhow!(
                    "[BUG] uninitialized GdbCompatibleDebugger: {}",
                    self.debugger_name
                )
            })?,
        );
        Ok(info)
    }

    fn is_debuggee_surely_supported(&self, debuggee: &str) -> Result<bool> {
        let file_output = get_cached_command_output(&["file", debuggee])?;
        if file_output.contains("ELF") || file_output.contains("Mach-O") {
            return Ok(true);
        }
        if file_output.contains("shell") && check_if_wrapped(debuggee) {
            return self.is_debuggee_surely_supported(&get_debuggee_backup_name(debuggee));
        }
        Ok(false)
    }
}

pub struct DelveDebugger {
    port: Option<i32>,
}

impl DelveDebugger {
    pub fn new() -> Result<DelveDebugger> {
        if !command_exists("dlv") {
            bail!("'dlv' is not in PATH. Did you install delve?")
        }
        Ok(DelveDebugger { port: None })
    }
}

impl Debugger for DelveDebugger {
    fn run(&mut self, run_opts: &RunOpts, terminal: &mut dyn DebuggerTerminal) -> Result<Pid> {
        let debuggee_abspath = get_path_of_unset_debuggee(&run_opts.debuggee)?;
        self.port = Some(5679);
        let debugger_args: Vec<&str> = vec![
            "exec",
            "--headless",
            "--log-dest",
            "/dev/null",
            "--api-version=2",
            "--listen",
            "localhost:5679",
            &debuggee_abspath,
            "--",
        ]
        .into_iter()
        .chain(run_opts.debuggee_args.iter().map(|s| s.as_str()))
        .collect();

        if cfg!(target_os = "macos") {
            log::info!("delve outputs logs from lldb-server to stderr on macos, which cannot be suppressed");
        }

        let pid = launch_debugger_server("dlv", &debugger_args)?;
        terminal.open(self)?;

        Ok(pid)
    }

    fn set(&mut self, set_opts: &SetOpts, terminal: &mut dyn DebuggerTerminal) -> Result<()> {
        set_to_exec_dgeee(set_opts, terminal)
    }

    fn unset(&mut self, unset_opts: &UnsetOpts) -> Result<()> {
        unset_from_exec_dbgee(unset_opts)
    }

    fn build_attach_commandline(&self) -> Result<Vec<String>> {
        Ok(vec![
            "dlv".to_owned(),
            "connect".to_owned(),
            "localhost:".to_owned()
                + &self
                    .port
                    .ok_or_else(|| anyhow!("[BUG] uninitialized DelveDebugger"))?
                    .to_string(),
        ])
    }

    fn build_attach_information(&self) -> Result<HashMap<AttachInformationKey, String>> {
        let mut info = HashMap::new();
        info.insert(AttachInformationKey::DebuggerTypeHint, "go".to_owned());
        info.insert(
            AttachInformationKey::DebuggerPort,
            self.port
                .ok_or_else(|| anyhow!("[BUG] uninitialized DelveDebugger"))?
                .to_string(),
        );
        Ok(info)
    }

    fn is_debuggee_surely_supported(&self, debuggee: &str) -> Result<bool> {
        let file_output = get_cached_command_output(&["file", debuggee])?;
        // GNU's file command detects Go binaries
        if file_output.contains("Go ") {
            return Ok(true);
        }
        static GO_VER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"go(\d+\.?)+").unwrap());
        if let Ok(true) = get_cached_command_output(&["go", "version", debuggee])
            .map(|output| GO_VER_RE.is_match(&output))
        {
            return Ok(true);
        }
        if file_output.contains("shell") && check_if_wrapped(debuggee) {
            return self.is_debuggee_surely_supported(&get_debuggee_backup_name(debuggee));
        }
        Ok(false)
    }
}

pub struct StopAndWritePidDebugger;

impl StopAndWritePidDebugger {
    pub fn new() -> StopAndWritePidDebugger {
        StopAndWritePidDebugger {}
    }
}

impl Debugger for StopAndWritePidDebugger {
    fn run(&mut self, run_opts: &RunOpts, _terminal: &mut dyn DebuggerTerminal) -> Result<Pid> {
        let debuggee_abspath = get_path_of_unset_debuggee(&run_opts.debuggee)?;
        let debuggee_pid = run_and_stop_dbgee(
            &debuggee_abspath,
            run_opts.debuggee_args.iter().map(String::as_str),
        )?;
        log::info!("The debuggee process is paused. Atach a debugger to it by PID.");
        log::info!(
            "PID: {}. It's also written to /tmp/dbgee_pid as a plain text number.",
            debuggee_pid.as_raw()
        );
        let mut pid_file = File::create("/tmp/dbgee_pid")?;
        write!(pid_file, "{}", debuggee_pid.as_raw())?;
        Ok(debuggee_pid)
    }

    fn set(&mut self, set_opts: &SetOpts, terminal: &mut dyn DebuggerTerminal) -> Result<()> {
        set_to_exec_dgeee(set_opts, terminal)
    }

    fn unset(&mut self, unset_opts: &UnsetOpts) -> Result<()> {
        unset_from_exec_dbgee(unset_opts)
    }

    fn build_attach_commandline(&self) -> Result<Vec<String>> {
        bail!("[BUG] build_attach_commandline should not be called for StopAndWritePidDebugger");
    }

    fn build_attach_information(&self) -> Result<HashMap<AttachInformationKey, String>> {
        bail!("[BUG] butild_attach_information should not be called for StopAndWritePidDebugger");
    }

    fn is_debuggee_surely_supported(&self, _debuggee: &str) -> Result<bool> {
        Ok(true)
    }
}

pub struct PythonDebugger {
    python_command: String,
    port: Option<i32>,
}

impl PythonDebugger {
    pub fn new() -> Result<PythonDebugger> {
        let python_path;
        if command_exists("python3") {
            python_path = "python3".to_owned();
        } else if command_exists("python") {
            python_path = "python".to_owned();
        } else {
            bail!("Neither 'python3' nor 'python' exist. Did you install python?");
        }

        let debugpy_exists = Command::new(&python_path)
            .args(&["-c", "'import debugpy'"])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status();
        if debugpy_exists.is_err() || !debugpy_exists.unwrap().success() {
            bail!("'debugpy' module is not installed. Please install debugpy via pip.");
        }

        Ok(PythonDebugger {
            python_command: python_path,
            port: None,
        })
    }
}

impl Debugger for PythonDebugger {
    fn run(&mut self, run_opts: &RunOpts, terminal: &mut dyn DebuggerTerminal) -> Result<Pid> {
        self.port = Some(5679);
        let debugger_args: Vec<&str> = vec![
            "-m",
            "debugpy",
            "--wait-for-client",
            "--listen",
            "5679",
            &run_opts.debuggee,
        ]
        .into_iter()
        .chain(run_opts.debuggee_args.iter().map(|s| s.as_str()))
        .collect();

        let pid = launch_debugger_server(&self.python_command, &debugger_args)?;
        if terminal.name() != "vscode" {
            log::info!("only `-t vscode` is the supported option for Python.");
        };
        // Ignore the given terminal since Python supports only Vscode
        let mut vscode = crate::debugger_terminal::VsCode::new();
        vscode.open(self)?;

        Ok(pid)
    }

    fn set(&mut self, _set_opts: &SetOpts, _terminal: &mut dyn DebuggerTerminal) -> Result<()> {
        bail!("set is not implemented yet for Python");
    }

    fn unset(&mut self, _unset_opts: &UnsetOpts) -> Result<()> {
        bail!("unset is not implemented yet for Python");
    }

    fn build_attach_commandline(&self) -> Result<Vec<String>> {
        bail!("[BUG] build_attach_commandline should not be called for PythonDebugger");
    }

    fn build_attach_information(&self) -> Result<HashMap<AttachInformationKey, String>> {
        let mut info = HashMap::new();
        info.insert(AttachInformationKey::DebuggerTypeHint, "python".to_owned());
        info.insert(
            AttachInformationKey::DebuggerPort,
            format!(
                "{}",
                self.port
                    .ok_or_else(|| anyhow!("[BUG] PythonDebugger.port is not initializaed"))?
            ),
        );
        Ok(info)
    }

    fn is_debuggee_surely_supported(&self, debuggee: &str) -> Result<bool> {
        let file_output = get_cached_command_output(&["file", debuggee])?;
        if file_output.contains("Python") {
            return Ok(true);
        }
        Ok(false)
    }
}

fn launch_debugger_server(debugger_path: &str, debugger_args: &[&str]) -> Result<Pid> {
    let debugger = Command::new(debugger_path)
        .args(debugger_args)
        .spawn()
        .with_context(|| {
            anyhow!(
                "failed to launch {}. Perhaps is port 5679 being used?",
                debugger_path
            )
        })?;
    // To wait for the child process, not being signalled by Ctrl+C.
    // Ignore SIGINT after Command::spawn because spawn inherits the parent's signal handlers.
    // This makes some gap between the timing when the debugger launched and the timing when the host started to ignore SIGINT,
    // but I'm doing this just due to laziness
    ignore_sigint()?;

    // wait for the server to get ready
    std::thread::sleep(std::time::Duration::from_secs(1));
    Ok(Pid::from_raw(debugger.id() as i32))
}

fn run_and_stop_dbgee<'a>(debuggee: &'a str, args: impl Iterator<Item = &'a str>) -> Result<Pid> {
    let debuggee_cmd: Vec<&str> = vec![debuggee].into_iter().chain(args).collect();
    // To wait for the child process, not being signalled by Ctrl+C
    ignore_sigint()?;
    let debuggee_pid = fork_exec_stop(&debuggee_cmd)?;
    // Sleeping childs don't respond to SIGINT/SIGTERM. Kill them by SIGKILL for ergonomics
    kill9_child_by_sigint(debuggee_pid)?;
    Ok(debuggee_pid)
}

fn set_to_exec_dgeee(set_opts: &SetOpts, _terminal: &mut dyn DebuggerTerminal) -> Result<()> {
    let clap_matches = Opts::clap().get_matches();
    let run_command = build_run_command(&clap_matches)?;
    wrap_debuggee_binary(&set_opts.debuggee, &run_command)?;

    if set_opts.start_cmd.is_empty() {
        return Ok(());
    }

    let mut child = Command::new(&set_opts.start_cmd[0])
        .args(&set_opts.start_cmd[1..])
        .spawn()?;
    let _ = child.wait()?;

    unwrap_debuggee_binary(&set_opts.debuggee)
}

fn unset_from_exec_dbgee(unset_opts: &UnsetOpts) -> Result<()> {
    unwrap_debuggee_binary(&unset_opts.debuggee)
}

fn get_path_of_unset_debuggee(debuggee: &str) -> Result<String> {
    let abspath = get_abspath(debuggee, "debuggee")?;
    Ok(match check_if_wrapped(&debuggee) {
        true => get_debuggee_backup_name(&abspath),
        false => abspath,
    })
}

fn wrap_debuggee_binary(debuggee: &str, run_command: &str) -> Result<()> {
    if check_if_wrapped(debuggee) {
        bail!(
            "{} is already wrapped by dbgee. Did you set it already?",
            debuggee
        );
    }

    let debuggee_path = get_valid_executable_path(Path::new(debuggee), "the debuggee")?;

    let wrapper_sh_template_bytes = include_bytes!("../resources/wrapper.sh");
    let wrapper_sh_template = str::from_utf8(wrapper_sh_template_bytes).unwrap();
    let wrapper_sh = wrapper_sh_template
        .replace("%run_cmd%", run_command)
        .replace("%debuggee%", &format!("\"{}\"", &debuggee_path));

    let mut debuggee_pathbuf = PathBuf::from_str(&debuggee_path)?;
    // unwrap should be OK here because debuggee_path is a valid UTF-8 path of an executable file.
    let debuggee_filename = debuggee_pathbuf
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    debuggee_pathbuf.pop();
    debuggee_pathbuf.push(get_debuggee_backup_name(&debuggee_filename));
    let debuggee_backup = debuggee_pathbuf.to_str().unwrap();

    let debuggee_perm = fs::metadata(&debuggee_path)?.permissions();
    fs::rename(&debuggee_path, &debuggee_backup)?;
    fs::write(&debuggee_path, wrapper_sh)?;
    fs::set_permissions(&debuggee_path, debuggee_perm)?;

    Ok(())
}

fn unwrap_debuggee_binary(debuggee: &str) -> Result<()> {
    let wrapper_path = get_valid_executable_path(Path::new(debuggee), "the debuggee")?;

    if !check_if_wrapped(debuggee) {
        bail!(
            "{} is not wrapped by dbgee. Did you unset it already?",
            debuggee
        );
    }

    let mut wrapper_pathbuf = PathBuf::from_str(&wrapper_path)?;
    // unwrap should be OK here because wrapper_path is a valid UTF-8 path of an executable file.
    let debuggee_filename = wrapper_pathbuf
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    wrapper_pathbuf.pop();
    wrapper_pathbuf.push(get_debuggee_backup_name(&debuggee_filename));
    let debuggee_path = wrapper_pathbuf.to_str().unwrap();

    fs::remove_file(&wrapper_path)?;
    fs::rename(&debuggee_path, &wrapper_path)?;

    Ok(())
}

fn check_if_wrapped<P: AsRef<Path>>(path: P) -> bool {
    let wrapper_sh_template_bytes = include_bytes!("../resources/wrapper.sh");
    let wrapper_sh_template = str::from_utf8(wrapper_sh_template_bytes).unwrap();
    let wrapper_signature: String = wrapper_sh_template.lines().take(2).collect();

    let file = File::open(path);
    if file.is_err() {
        return false;
    }
    let file = file.unwrap();

    let lines = BufReader::new(file).lines();
    let signature: String = lines
        .take(2)
        .map(|lresult| lresult.unwrap_or_default())
        .collect();

    signature == wrapper_signature
}

fn build_run_command(set_opts: &ArgMatches) -> Result<String> {
    let self_pathbuf = env::current_exe()?;
    let self_path = get_valid_executable_path(&self_pathbuf, "dbgee")?;
    let global_opts = reconstruct_flags(set_opts, &[]);
    let attach_opts = reconstruct_flags(
        set_opts.subcommand_matches("set").unwrap(),
        &SETOPTS_POSITIONAL_ARGS,
    );
    let debuggee_path = get_valid_executable_path(
        &set_opts
            .subcommand_matches("set")
            .unwrap()
            .value_of("debuggee")
            .unwrap(),
        "debuggee",
    )?;

    Ok(format!(
        "{} {} run {} -- {} \"$@\"",
        self_path,
        &global_opts,
        &attach_opts,
        &get_debuggee_backup_name(&debuggee_path)
    ))
}

fn get_debuggee_backup_name(debuggee_filename: &str) -> String {
    format!("{}-original", debuggee_filename)
}

fn reconstruct_flags(opts: &ArgMatches, positional_args: &[&str]) -> String {
    let mut command = vec![];
    for &key in opts.args.keys() {
        if positional_args.contains(&key) {
            continue;
        }
        command.push(format!("--{}", key.replace("_", "-")));
        if let Some(values) = opts.values_of(key) {
            for value in values {
                command.push(format!("'{}'", escape_single_quote(value)));
            }
        }
    }
    command.join(" ")
}

fn escape_single_quote(s: &str) -> String {
    s.replace("'", "'\"'\"'")
}

fn fork_exec_stop<T: AsRef<str>>(debuggee_cmd: &[T]) -> Result<Pid> {
    get_valid_executable_path(debuggee_cmd[0].as_ref(), "the debuggee")?;
    match unsafe { unistd::fork().with_context(|| "fork failed.")? } {
        unistd::ForkResult::Child => {
            ptrace::traceme().with_context(|| "ptrace::traceme failed.")?;
            let cargs: Vec<CString> = debuggee_cmd
                .iter()
                .map(|arg| CString::new(arg.as_ref()).unwrap())
                .collect();
            let _ = unistd::execv(&cargs[0], &cargs[0..]);
            bail!(
                "exec {} failed. Error: {}",
                &cargs[0].to_str().unwrap(),
                nix::Error::last()
            );
        }
        unistd::ForkResult::Parent {
            child: debuggee_pid,
        } => {
            // Wait for the debuggee to be stopped by SIGSTOP, which is triggered by PTRACE_ATTACH
            match wait::waitpid(debuggee_pid, None)
                .with_context(|| "Unexpected error. Waiting for SIGTRAP failed.")?
            {
                wait::WaitStatus::Stopped(_, signal::SIGTRAP) => {}
                other => {
                    log::warn!(
                        "The observed signal is not SIGTRAP, but dbgee continues. {:?}",
                        other
                    );
                }
            }

            // macOS's bug prevents you from delivering SIGSTOP by detach directly.
            // Thus, send SIGSTOP by kill before detach
            signal::kill(debuggee_pid, signal::SIGSTOP)
                .with_context(|| "Unexpected error. Sending a signal failed")?;
            ptrace::detach(debuggee_pid, None)
                .with_context(|| "Unexpected error. Detach and stop failed")?;

            Ok(debuggee_pid)
        }
    }
}

fn ignore_sigint() -> Result<()> {
    unsafe {
        signal::signal(signal::Signal::SIGINT, signal::SigHandler::SigIgn)?;
    }
    Ok(())
}

fn kill9_child_by_sigint(pid: Pid) -> Result<()> {
    ctrlc::set_handler(move || {
        let _ = signal::kill(pid, signal::Signal::SIGKILL);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::NamedTempFile;

    #[test]
    fn test_check_if_wrapperd_by_strings() {
        let actually_wrapped = indoc! {r#"
            #!/bin/sh
            # a wrapper script generated by dbgee
            some scripts
        "#};
        let tmpfile = make_temp_file(actually_wrapped);
        assert!(check_if_wrapped(tmpfile.path()));

        let not_wrapped = indoc! {r#"
            #!/bin/sh
            some scripts in the wild
        "#};
        let tmpfile = make_temp_file(not_wrapped);
        assert!(!check_if_wrapped(tmpfile.path()));
    }

    #[test]
    fn test_check_if_wrapperd_by_actually_wrapping() {
        let tmpfile = make_temp_executable_file("dummy");
        let tmpfile_path = tmpfile.path().to_str().unwrap();
        wrap_debuggee_binary(tmpfile_path, "dummy run -- debuggee").unwrap();
        assert!(check_if_wrapped(tmpfile.path()));
        unwrap_debuggee_binary(tmpfile_path).unwrap();
        assert!(!check_if_wrapped(tmpfile.path()));
    }

    #[test]
    fn test_double_wrapping() {
        let tmpfile = make_temp_executable_file("dummy");
        let tmpfile_path = tmpfile.path().to_str().unwrap();
        wrap_debuggee_binary(tmpfile_path, "dummy run -- debuggee").unwrap();
        assert!(wrap_debuggee_binary(tmpfile_path, "dummy run -- debuggee").is_err());
    }

    #[test]
    fn test_double_unwrapping() {
        let tmpfile = make_temp_executable_file("dummy");
        let tmpfile_path = tmpfile.path().to_str().unwrap();
        wrap_debuggee_binary(tmpfile_path, "dummy run -- debuggee").unwrap();
        unwrap_debuggee_binary(tmpfile_path).unwrap();
        assert!(unwrap_debuggee_binary(tmpfile_path).is_err());
    }

    #[test]
    fn test_build_run_command_normal() {
        let debuggee_file = make_temp_executable_file("dummy");
        let debuggee = debuggee_file.as_ref().to_str().unwrap();
        let start_cmd_file = make_temp_executable_file("dummy");
        let start_cmd = start_cmd_file.as_ref().to_str().unwrap();
        let current_exe_pathbuf = std::env::current_exe().unwrap();
        let current_exe = current_exe_pathbuf.to_str().unwrap();

        let command = vec![
            current_exe,
            "set",
            debuggee,
            "-t",
            "tmuxw",
            "--",
            start_cmd,
            "some_args",
        ];
        let clap_matches = Opts::clap().get_matches_from(command.iter());

        let constructed_run_command: Vec<String> = build_run_command(&clap_matches)
            .unwrap()
            .split(' ')
            .map(|s| strip_quote(s).to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        let constructed_clap_matches =
            Opts::clap().get_matches_from(constructed_run_command.iter());

        let expected = vec![current_exe, "run", "-t", "tmuxw", "--", debuggee];
        let expected_clap_matches = Opts::clap().get_matches_from(expected.iter());

        assert!(compare_argmatches(
            &expected_clap_matches,
            &constructed_clap_matches
        ));
    }

    fn strip_quote(s: &str) -> &str {
        if s.starts_with('\'') {
            &s[1..s.len() - 1]
        } else {
            s
        }
    }

    fn compare_argmatches(a: &ArgMatches, b: &ArgMatches) -> bool {
        a.args.len() == b.args.len()
            && a.args
                .keys()
                .all(|a_key| a.args.get(*a_key).unwrap().vals == b.args.get(*a_key).unwrap().vals)
    }

    fn make_temp_executable_file(contents: &str) -> NamedTempFile {
        let tempfile = make_temp_file(contents);
        fs::set_permissions(tempfile.as_ref(), fs::Permissions::from_mode(0o555)).unwrap();
        tempfile
    }

    fn make_temp_file(contents: &str) -> NamedTempFile {
        let mut tempfile = NamedTempFile::new().unwrap();
        tempfile.write_all(contents.as_bytes()).unwrap();
        tempfile
    }
}
