use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::{env, fs};
use std::{ffi::CString, io::Read};
use std::{
    io::{BufRead, BufReader},
    str,
};
use std::{
    os::unix::fs::PermissionsExt,
    os::unix::io::{AsRawFd, FromRawFd},
    process::Command,
};
use std::{path::PathBuf, str::FromStr};

use anyhow::{anyhow, bail, Context, Result};
use nix::sys::{ptrace, wait};
use nix::unistd;
use structopt::clap::ArgMatches;
use structopt::StructOpt;
use strum::{EnumString, EnumVariantNames, VariantNames as _};

/// Launches the given command and attaches a debugger to it.
#[derive(Debug, StructOpt)]
#[structopt(name = "dbgee", about = "the active debuggee")]
enum Opts {
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
struct RunOpts {
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
struct SetOpts {
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
struct UnsetOpts {
    /// Path to the debuggee process
    #[structopt(name = "debuggee")]
    pub debuggee: String,
}

#[derive(Debug, StructOpt)]
struct AttachOpts {
    /// Action to take after the debuggee launces.
    ///
    /// tmux (default): Opens a new tmux window in last active tmux session, launches a debugger there, and has the debugger attach the debuggee.
    /// If there is no active tmux session, it launches a new session in the background, and writes a notification to stderr (as far as stderr is a tty).
    ///
    /// write-pid: Stops the debuggee, and prints the debuggee's PID.
    /// dbgee writes the PID to /tmp/dbgee_pid
    /// If stderr is a tty, dbgee outputs the PID to stderr as well.
    #[structopt(
        short,
        long,
        possible_values(AttachAction::VARIANTS),
        default_value("tmux")
    )]
    pub attach_action: AttachAction,

    /// Debugger to launch. Choose "gdb" or "dlv", or you can specify an arbitrary command line. The debuggee's PID follows your command line as an argument.
    #[structopt(short, long, default_value("gdb"))]
    pub debugger: String,
}

#[derive(Debug, EnumString, EnumVariantNames)]
#[strum(serialize_all = "kebab-case")]
pub enum AttachAction {
    WritePid,
    Tmux,
}

fn main() {
    let clap_matches = Opts::clap().get_matches();
    let opts = Opts::from_clap(&clap_matches);

    let cmd_result = match opts {
        Opts::Run(run_opts) => run_debuggee(run_opts),
        Opts::Set(set_opts) => set_debuggee(set_opts, clap_matches),
        Opts::Unset(unset_opts) => unset_debuggee(unset_opts),
    };
    if let Err(e) = cmd_result {
        print_error(&e.to_string());
    }
}

fn run_debuggee(run_opts: RunOpts) -> Result<()> {
    let debuggee_pid = nix::unistd::getpid();
    let debuggee_cmd: Vec<&String> = vec![&run_opts.debuggee]
        .into_iter()
        .chain(run_opts.debuggee_args.iter())
        .collect();
    fork_exec_stop(debuggee_pid, &debuggee_cmd)?;
    match run_opts.attach_opts.attach_action {
        AttachAction::WritePid => {
            write_pid(debuggee_pid)?;
        }
        AttachAction::Tmux => {
            launch_debugger_in_tmux(&build_debugger_command(
                &run_opts.attach_opts.debugger,
                debuggee_pid,
            ))?;
        }
    }

    Ok(())
}

fn set_debuggee(set_opts: SetOpts, clap_matches: ArgMatches) -> Result<()> {
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

fn unset_debuggee(unset_opts: UnsetOpts) -> Result<()> {
    unwrap_debuggee_binary(&unset_opts.debuggee)
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

fn get_valid_executable_path<T: AsRef<Path>>(path: T, name: &str) -> Result<String> {
    let abspath = get_abspath(path, name)?;
    let path = Path::new(&abspath);
    let metadata = path.metadata()?;
    // TODO: more fine-grained permission check
    if !metadata.is_file() && (metadata.permissions().mode() & 0o111 != 0) {
        bail!("{} ({}) is not executable", name, abspath);
    }
    Ok(abspath)
}

fn get_abspath<T: AsRef<Path>>(path: T, name: &str) -> Result<String> {
    path.as_ref()
        .to_str()
        .ok_or_else(|| anyhow!("{} should exist in a valid UTF-8 path", name))?;
    let abspath = fs::canonicalize(&path).with_context(|| {
        format!(
            "{} (Path: '{}') does not exist.",
            &name,
            &path.as_ref().to_str().unwrap()
        )
    })?;
    let abspath = abspath.as_os_str().to_str().ok_or_else(|| {
        anyhow!(
            "{}",
            message_string(format!(
                "Error: {} should exist in a valid UTF-8 path",
                name
            ))
        )
    })?;
    Ok(abspath.to_owned())
}

fn fork_exec_stop<T: AsRef<str>>(debuggee_pid: unistd::Pid, debuggee_cmd: &[T]) -> Result<()> {
    get_valid_executable_path(debuggee_cmd[0].as_ref(), "the debuggee")?;
    let (read_fd, write_fd) =
        unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).with_context(|| "pipe2 failed")?;
    let mut sync_pipe_read: File = unsafe { File::from_raw_fd(read_fd) };
    let mut sync_pipe_write: File = unsafe { File::from_raw_fd(write_fd) };
    match unsafe { unistd::fork().with_context(|| "fork failed.")? } {
        unistd::ForkResult::Parent { child: _ } => {
            let mut buf = [0; 1];
            let _ = sync_pipe_read.read(&mut buf);
            let cargs: Vec<CString> = debuggee_cmd
                .iter()
                .map(|arg| CString::new(arg.as_ref()).unwrap())
                .collect();
            if unistd::execv(&cargs[0], &cargs[0..]).is_err() {
                eprintln!(
                    "exec {} failed. Error: {}",
                    &cargs[0].to_str().unwrap(),
                    nix::Error::last()
                );
            }
        }
        unistd::ForkResult::Child => {
            ptrace::attach(debuggee_pid).with_context(|| {
                "ptrace attach failed. Perhaps dgbee is being traced by some debugger?"
            })?;
            let buf = [0; 1];
            let _ = sync_pipe_write.write(&buf);
            // Wait for the debuggee to be stopped by SIGSTOP, which is triggered by PTRACE_ATTACH
            match wait::waitpid(debuggee_pid, None)
                .with_context(|| "Unexpected error. Waiting for SIGSTOP failed.")?
            {
                wait::WaitStatus::Stopped(_, nix::sys::signal::SIGSTOP) => {}
                other => {
                    eprintln!(
                        "The observed signal is not SISTOP, but dbgee continues. {:?}",
                        other
                    );
                }
            }

            ptrace::cont(debuggee_pid, None)
                .with_context(|| "Unexpected error. Continuing the process failed")?;
            match wait::waitpid(debuggee_pid, None)
                .with_context(|| "Unexpected error. Waiting for SIGTRAP failed.")?
            {
                wait::WaitStatus::Exited(_, _) => {
                    panic!("The process exited for an unexpected reason");
                }
                wait::WaitStatus::Stopped(_, nix::sys::signal::SIGTRAP) => {}
                other => {
                    eprintln!(
                        "The observed signal is not SIGTRAP, but continues. {:?}",
                        other
                    );
                }
            }

            ptrace::detach(debuggee_pid, nix::sys::signal::SIGSTOP)
                .with_context(|| "Unexpected error. Detach and stop failed")?;
        }
    };
    Ok(())
}

fn write_pid(debuggee_pid: unistd::Pid) -> Result<()> {
    print_message(
        "The debuggee process is stopped in the background. Atach a debugger to it by PID. \
            To do I/O with the debuggee, run `fg` in your shell to bring it to the foreground",
    );
    print_message(&format!(
        "PID: {}. It's also written to /tmp/dbgee_pid as a plain text number.",
        debuggee_pid.as_raw()
    ));
    let mut pid_file = File::create("/tmp/dbgee_pid")?;
    write!(pid_file, "{}", debuggee_pid.as_raw())?;
    Ok(())
}

fn launch_debugger_in_tmux<T: AsRef<str>>(debugger_cmd: &[T]) -> Result<()> {
    let is_tmux_active = Command::new("tmux")
        .args(&["ls"])
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .with_context(|| "Failed to launch tmux. Is tmux installed?")?;

    if is_tmux_active.success() {
        let mut args = vec!["new-window"];
        args.extend(debugger_cmd.iter().map(T::as_ref));
        let _ = Command::new("tmux")
            .args(&args)
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .with_context(|| "Failed to open a new tmux window for an unexpected reason.")?;
    } else {
        let mut args = vec!["new-session"];
        args.extend(debugger_cmd.iter().map(T::as_ref));
        let _ = Command::new("tmux")
            .args(&args)
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .with_context(|| "Failed to open a new tmux session for an unexpected reason.")?;
        print_message("the debugger has launched in a new tmux session. Try `tmux a` to attach.");
    }
    print_message(
        "The debuggee process is running in the background. Run `fg` to do I/O with the debuggee.",
    );

    Ok(())
}

fn build_debugger_command(debugger_opt: &str, debuggee_pid: unistd::Pid) -> Vec<String> {
    match debugger_opt {
        "gdb" => vec![
            "gdb".to_string(),
            "-p".to_string(),
            debuggee_pid.as_raw().to_string(),
        ],
        "dlv" => vec![
            "dlv".to_string(),
            "attach".to_string(),
            debuggee_pid.as_raw().to_string(),
        ],
        command => vec![
            "sh".to_string(),
            "-c".to_string(),
            command.to_string() + " " + debuggee_pid.as_raw().to_string().as_str(),
        ],
    }
}

fn print_error<T: AsRef<str>>(mes: T) {
    eprintln!("{}", message_string(format!("Error: {}", mes.as_ref())));
}

fn print_message<T: AsRef<str>>(mes: T) {
    if let Ok(true) = unistd::isatty(std::io::stderr().as_raw_fd()) {
        eprintln!("{}", message_string(mes.as_ref()))
    }
}

fn message_string<T: AsRef<str>>(mes: T) -> String {
    format!("[Dbgee] {}", mes.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
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
        let tmpfile = make_temp_file("dummy");
        let tmpfile_path = tmpfile.path().to_str().unwrap();
        wrap_debuggee_binary(tmpfile_path, "dummy run -- debuggee").unwrap();
        assert!(check_if_wrapped(tmpfile.path()));
        unwrap_debuggee_binary(tmpfile_path).unwrap();
        assert!(!check_if_wrapped(tmpfile.path()));
    }

    #[test]
    fn test_double_wrapping() {
        let tmpfile = make_temp_file("dummy");
        let tmpfile_path = tmpfile.path().to_str().unwrap();
        wrap_debuggee_binary(tmpfile_path, "dummy run -- debuggee").unwrap();
        assert!(wrap_debuggee_binary(tmpfile_path, "dummy run -- debuggee").is_err());
    }

    #[test]
    fn test_double_unwrapping() {
        let tmpfile = make_temp_file("dummy");
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
            "-a",
            "write-pid",
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

        let expected = vec![current_exe, "run", "-a", "write-pid", "--", debuggee];
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
