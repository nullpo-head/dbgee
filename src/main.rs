use std::io::Error;
use std::process::exit;
use std::{ffi::CString, io::Read};
use std::{fs::File, io::Write};
use std::{
    os::unix::io::{AsRawFd, FromRawFd},
    process::Command,
};

use nix::sys::{ptrace, wait};
use nix::unistd;
use structopt::StructOpt;
use strum::{EnumString, EnumVariantNames, VariantNames as _};

/// Exec the given command and attach a debugger to it.
#[derive(Debug, StructOpt)]
#[structopt(name = "active_attach")]
struct Opt {
    #[structopt(name = "debuggee")]
    pub debuggee: String,

    #[structopt(name = "args")]
    pub debuggee_args: Vec<String>,

    /// Action to take after the debuggee launces.
    ///
    /// tmux (default): Open a new tmux window in last active tmux session, launch a debugger there, and have the debugger attach the debuggee.
    /// If there is no active tmux session, it launches a new session in the background, and writes a notification to stderr (as far as stderr is a tty).
    ///
    /// write-pid: Stop the debuggee and print the debuggee's PID.
    /// active_attach writes the PID to /tmp/active_attach_pid
    /// If stderr is a tty, active_attach outputs the PID to stderr as well.
    #[structopt(
        short = "a",
        long = "attach-action",
        possible_values(AttachAction::VARIANTS),
        default_value("tmux")
    )]
    pub attach_action: AttachAction,

    /// Debugger to launch. Choose "gdb" or "dlv", or you can specify an arbitrary command line. The debuggee's PID follows your command line as an argument.
    #[structopt(short = "d", long = "debugger", default_value("gdb"))]
    pub debugger: String,
}

#[derive(Debug, EnumString, EnumVariantNames)]
#[strum(serialize_all = "kebab-case")]
pub enum AttachAction {
    WritePid,
    Tmux,
}

fn main() {
    let app = Opt::clap().usage("active_attach [OPTIONS] -- debuggee [args-for-debuggee...]");
    let matches = app.get_matches();
    let opts = Opt::from_clap(&matches);

    let debuggee_pid = nix::unistd::getpid();
    let debuggee_cmd: Vec<&String> = vec![&opts.debuggee]
        .into_iter()
        .chain(opts.debuggee_args.iter())
        .collect();

    // After fork_exec_stop, the child process continues main function.
    // The original active_attach process, which has debuggee_pid, does execve and never returns.
    fork_exec_stop(debuggee_pid, &debuggee_cmd);

    match opts.attach_action {
        AttachAction::WritePid => {
            let _ = write_pid(debuggee_pid);
        }
        AttachAction::Tmux => {
            launch_debugger_in_tmux(&build_debugger_command(&opts.debugger, debuggee_pid));
        }
    }
}

fn fork_exec_stop<T: AsRef<str>>(debuggee_pid: unistd::Pid, debuggee_cmd: &[T]) {
    let (read_fd, write_fd) =
        unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).expect("Error: pipe2 failed.");
    let mut sync_pipe_read: File = unsafe { File::from_raw_fd(read_fd) };
    let mut sync_pipe_write: File = unsafe { File::from_raw_fd(write_fd) };
    match unsafe { unistd::fork().expect("fork failed.") } {
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
            ptrace::attach(debuggee_pid).expect("attach failed");
            let buf = [0; 1];
            let _ = sync_pipe_write.write(&buf);
            // Wait for the debuggee to be stopped by SIGSTOP, which is triggered by PTRACE_ATTACH
            match wait::waitpid(debuggee_pid, None).expect("waiting for SIGSTOP failed.") {
                wait::WaitStatus::Stopped(_, nix::sys::signal::SIGSTOP) => {}
                other => {
                    eprintln!(
                        "The observed signal is not SISTOP, but continues. {:?}",
                        other
                    );
                }
            }

            ptrace::cont(debuggee_pid, None).expect("Continuing the process failed");
            match wait::waitpid(debuggee_pid, None).expect("waiting for SIGTRAP failed.") {
                wait::WaitStatus::Exited(_, _) => {
                    eprintln!("The process exited for an unexpected reason");
                    exit(1);
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
                .expect("detach and stop failed");
        }
    }
}

fn write_pid(debuggee_pid: unistd::Pid) -> Result<(), Error> {
    print_message(
        "The debuggee process is stopped in the background. Atach a debugger to it by PID. \
            To do I/O with the debuggee, run `fg` in your shell to bring it to the foreground",
    );
    print_message(&format!(
        "PID: {}. It's also written to /tmp/active_attach_pid as a plain text number.",
        debuggee_pid.as_raw()
    ));
    let mut pid_file = File::create("/tmp/active_attach_pid")?;
    write!(pid_file, "{}", debuggee_pid.as_raw())
}

fn launch_debugger_in_tmux<T: AsRef<str>>(debugger_cmd: &[T]) {
    let is_tmux_active = Command::new("tmux")
        .args(&["ls"])
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap_or_else(|_| panic!(message_string("Failed to launch tmux. Is tmux installed?")));

    if is_tmux_active.success() {
        let mut args = vec!["new-window"];
        args.extend(debugger_cmd.iter().map(T::as_ref));
        let _ = Command::new("tmux")
            .args(&args)
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap_or_else(|_| {
                panic!(message_string(
                    "Failed to open a new tmux window for an unexpected reason."
                ))
            });
    } else {
        let mut args = vec!["new-session"];
        args.extend(debugger_cmd.iter().map(T::as_ref));
        let _ = Command::new("tmux")
            .args(&args)
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap_or_else(|_| {
                panic!(message_string(
                    "Failed to open a new tmux session for an unexpected reason."
                ))
            });
        print_message("the debugger has launched in a new tmux session. Try `tmux a` to attach.");
    }
    print_message(
        "The debuggee process is running in the background. Run `fg` to do I/O with the debuggee.",
    );
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
            command.to_string() + debuggee_pid.as_raw().to_string().as_str(),
        ],
    }
}

fn print_message(mes: &str) {
    if let Ok(true) = unistd::isatty(std::io::stderr().as_raw_fd()) {
        eprintln!("{}", message_string(mes))
    }
}

fn message_string(mes: &str) -> String {
    format!("[ActiveAttach] {}", mes)
}
