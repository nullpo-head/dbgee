use std::os::unix::io::FromRawFd;
use std::{env, process::exit};
use std::{ffi::CString, io::Read};
use std::{fs::File, io::Write};

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
    /// It does nothing if there is no active tmux session.
    ///
    /// write-pid: Stop the debuggee and print the debuggee's PID.
    /// active_attach traverses the stdout of the parent process until it finds the tty, and outputs the PID to the tty it finds.
    /// In adittion, active_attach writes the PID to /tmp/active_attach_pid
    #[structopt(
        short = "a",
        long = "attach-action",
        possible_values(AttachAction::VARIANTS),
        default_value("tmux")
    )]
    pub attach_action: AttachAction,

    /// Debugger to launch. Choose "gdb" or "dlv", or you can specify an arbitrary command line.
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
    let app = Opt::clap();
    //app.usage("active_attach [OPTIONS] -- debuggee [args-for-debuggee...]");
    let matches = app.get_matches();
    let opts = Opt::from_clap(&matches);

    let debuggee_pid = nix::unistd::getpid();
    eprintln!("pid: {}", debuggee_pid);

    // After this function, the child process continues main function.
    // The original active_attach process, which has debuggee_pid, does execve and never returns.
    fork_exec_stop(debuggee_pid);
}

fn fork_exec_stop(debuggee_pid: unistd::Pid) {
    let (read_fd, write_fd) =
        unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).expect("Error: pipe2 failed.");
    let mut sync_pipe_read: File = unsafe { File::from_raw_fd(read_fd) };
    let mut sync_pipe_write: File = unsafe { File::from_raw_fd(write_fd) };
    match unsafe { unistd::fork().expect("fork failed.") } {
        unistd::ForkResult::Parent { child: _ } => {
            let mut buf = [0; 1];
            let _ = sync_pipe_read.read(&mut buf);
            let cargs: Vec<CString> = env::args().map(|arg| CString::new(arg).unwrap()).collect();
            if unistd::execv(&cargs[1], &cargs[1..]).is_err() {
                eprintln!(
                    "exec {} failed. Error: {}",
                    &cargs[1].to_str().unwrap(),
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
