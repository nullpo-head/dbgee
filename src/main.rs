use std::os::unix::io::FromRawFd;
use std::{env, process::exit};
use std::{ffi::CString, io::Read};
use std::{fs::File, io::Write};

use nix::sys::{ptrace, wait};
use nix::unistd;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} bin [args...]", args[0]);
        exit(1);
    }
    let cargs: Vec<CString> = env::args().map(|arg| CString::new(arg).unwrap()).collect();
    let pid = nix::unistd::getpid();
    eprintln!("pid: {}", pid);

    let (read_fd, write_fd) =
        unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).expect("Error: pipe2 failed.");
    let mut sync_pipe_read: File = unsafe { File::from_raw_fd(read_fd) };
    let mut sync_pipe_write: File = unsafe { File::from_raw_fd(write_fd) };

    match unsafe { unistd::fork().expect("fork failed.") } {
        unistd::ForkResult::Parent { child: _ } => {
            let mut buf = [0; 1];
            let _ = sync_pipe_read.read(&mut buf);
            if unistd::execv(&cargs[1], &cargs[1..]).is_err() {
                eprintln!("exec {} failed. Error: {}", args[0], nix::Error::last());
            }
        }
        unistd::ForkResult::Child => {
            ptrace::attach(pid).expect("attach failed");
            let buf = [0; 1];
            let _ = sync_pipe_write.write(&buf);
            // Wait for the debuggee to be stopped by SIGSTOP, which is triggered by PTRACE_ATTACH
            match wait::waitpid(pid, None).expect("waiting for SIGSTOP failed.") {
                wait::WaitStatus::Stopped(_, nix::sys::signal::SIGSTOP) => {}
                other => {
                    eprintln!(
                        "The observed signal is not SISTOP, but continues. {:?}",
                        other
                    );
                }
            }

            ptrace::cont(pid, None).expect("Continuing the process failed");
            match wait::waitpid(pid, None).expect("waiting for SIGTRAP failed.") {
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

            ptrace::detach(pid, nix::sys::signal::SIGSTOP).expect("detach and stop failed");
        }
    }
}
