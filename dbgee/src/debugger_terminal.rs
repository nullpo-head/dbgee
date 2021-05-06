use crate::debugger::Debugger;

use anyhow::{Context, Result};
use nix::unistd;
use std::{fs::File, io::Write, process::Command};

pub trait DebuggerTerminal {
    fn name(&self) -> &str;
    fn open(&mut self, debugger: &dyn Debugger) -> Result<()>;
}

pub struct Tmux {
    layout: TmuxLayout,
}

pub enum TmuxLayout {
    NewWindow,
    NewPane,
}

impl Tmux {
    pub fn new(layout: TmuxLayout) -> Tmux {
        Tmux { layout }
    }
}

impl TmuxLayout {
    pub fn to_command(&self) -> Vec<&str> {
        match self {
            TmuxLayout::NewWindow => vec!["new-window"],
            TmuxLayout::NewPane => vec!["splitw", "-h"],
        }
    }
}

impl DebuggerTerminal for Tmux {
    fn name(&self) -> &str {
        "tmux"
    }

    fn open(&mut self, debugger: &dyn Debugger) -> Result<()> {
        let debugger_cmd = debugger.build_attach_commandline()?;
        let is_tmux_active = Command::new("tmux")
            .args(&["ls"])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .with_context(|| "Failed to launch tmux. Is tmux installed?")?;

        if is_tmux_active.success() {
            let mut args = self.layout.to_command();
            args.extend(debugger_cmd.iter().map(|s| s.as_str()));
            Command::new("tmux")
                .args(&args)
                .status()
                .with_context(|| "Failed to open a new tmux window for an unexpected reason.")?;
        } else {
            let mut args = vec!["new-session"];
            args.extend(debugger_cmd.iter().map(|s| s.as_str()));
            Command::new("tmux")
                .args(&args)
                .spawn()
                .with_context(|| "Failed to open a new tmux session for an unexpected reason.")?;
            log::info!("the debugger has launched in a new tmux session. Try `tmux a` to attach.",);
        }

        Ok(())
    }
}

pub struct VsCode {
    fifo_path: String,
}

impl VsCode {
    pub fn new() -> VsCode {
        VsCode {
            fifo_path: "/tmp/dbgee-vscode-debuggees".to_owned(),
        }
    }
}

impl DebuggerTerminal for VsCode {
    fn name(&self) -> &str {
        "vscode"
    }

    fn open(&mut self, debugger: &dyn Debugger) -> Result<()> {
        let json = format!(
            "{{{}}}",
            debugger
                .build_attach_information()?
                .into_iter()
                .map(|(key, val)| format!("\"{}\": \"{}\"", key, val))
                .collect::<Vec<String>>()
                .join(", ")
        );

        let fifo_path = self.fifo_path.clone();
        match unistd::mkfifo(fifo_path.as_str(), nix::sys::stat::Mode::S_IRWXU) {
            Err(nix::Error::Sys(nix::errno::Errno::EEXIST)) => Ok(()),
            other => other,
        }?;
        std::thread::spawn(move || {
            if let Ok(mut fifo) = File::create(&fifo_path) {
                let _ = fifo.write_all(&json.as_bytes());
                log::info!("VSCode has attached to the debuggee")
            }
        });

        log::info!("The debuggee process is paused. Attach to it in VSCode");

        Ok(())
    }
}
