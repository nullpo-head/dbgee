use crate::debugger::{AttachInformationKey, Debugger};

use anyhow::{anyhow, bail, Context, Result};
use nix::unistd;
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

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
        let sudo_user = std::env::var("SUDO_USER");
        let tmux_command = match sudo_user {
            Ok(ref sudo_user) => {
                log::info!(
                    "tmux is opened in a session of user '{}' instead of root's.",
                    sudo_user
                );
                vec!["sudo", "-u", sudo_user.as_str(), "tmux"]
            }
            _ => vec!["tmux"],
        };

        let is_tmux_active = Command::new(&tmux_command[0])
            .args(
                tmux_command[1..tmux_command.len()]
                    .iter()
                    .chain(["ls"].iter()),
            )
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .with_context(|| "Failed to launch tmux. Is tmux installed?")?;

        let debugger_cmd = debugger.build_attach_commandline()?;
        if is_tmux_active.success() {
            let mut args = self.layout.to_command();
            args.extend(debugger_cmd.iter().map(|s| s.as_str()));
            Command::new(&tmux_command[0])
                .args(
                    tmux_command[1..tmux_command.len()]
                        .iter()
                        .chain(args.iter()),
                )
                .status()
                .with_context(|| "Failed to open a new tmux window for an unexpected reason.")?;
        } else {
            let mut args = vec!["new-session"];
            args.extend(debugger_cmd.iter().map(|s| s.as_str()));
            Command::new(&tmux_command[0])
                .args(
                    tmux_command[1..tmux_command.len()]
                        .iter()
                        .chain(args.iter()),
                )
                .spawn()
                .with_context(|| "Failed to open a new tmux session for an unexpected reason.")?;
            log::info!("the debugger has launched in a new tmux session. Try `tmux a` to attach.",);
        }

        Ok(())
    }
}

pub struct VsCode {
    attach_information_fifo_path: String,
    attach_request_fifo_path: Option<String>,
    protocol_version: &'static str,
}

impl VsCode {
    pub fn new() -> VsCode {
        VsCode {
            attach_information_fifo_path: "/tmp/dbgee-vscode-debuggees".to_owned(),
            attach_request_fifo_path: VsCode::build_attach_request_fifo_path(),
            protocol_version: "0.2.0",
        }
    }

    fn send_json_to_vscode(
        &self,
        json: String,
        fifo_path: String,
        log_after_sent: Option<String>,
    ) -> Result<()> {
        log::debug!("sending json to vscode");
        log::trace!("json: {}", json);
        match unistd::mkfifo(fifo_path.as_str(), nix::sys::stat::Mode::S_IRWXU) {
            Err(nix::Error::Sys(nix::errno::Errno::EEXIST)) => Ok(()),
            other => other,
        }?;
        std::thread::spawn(move || {
            if let Ok(mut fifo) = File::create(fifo_path) {
                let _ = fifo.write_all(&json.as_bytes());
                if let Some(log) = log_after_sent {
                    log::info!("{}", log);
                }
            }
        });

        Ok(())
    }

    fn send_attach_information(&self, debugger: &dyn Debugger) -> Result<()> {
        let attach_information_keys = [
            AttachInformationKey::Pid,
            AttachInformationKey::ProgramName,
            AttachInformationKey::DebuggerPort,
        ];
        let json = format!(
            "{{{}}}",
            debugger
                .build_attach_information()?
                .into_iter()
                .filter_map(move |(key, val)| {
                    if attach_information_keys.contains(&key) {
                        Some(format!("\"{}\": \"{}\"", key, val))
                    } else {
                        None
                    }
                })
                .chain(
                    std::iter::once(format!(r#""protocolVersion": "{}""#, self.protocol_version))
                        .into_iter()
                )
                .collect::<Vec<String>>()
                .join(", ")
        );

        let fifo_path = self.attach_information_fifo_path.clone();
        self.send_json_to_vscode(
            json,
            fifo_path,
            Some("VSCode has attached to the debuggee".to_owned()),
        )
    }

    fn send_attach_request(&self, debugger: &dyn Debugger) -> Result<()> {
        let fifo_path = &self.attach_request_fifo_path;
        if fifo_path.is_none() {
            bail!("fifo for attach_request cannot be built. Maybe $VSCODE_GIT_IPC_HANDLE has changed?");
        }
        let fifo_path = fifo_path.clone().unwrap();
        if !Path::new(&fifo_path).exists() {
            bail!(
                "fifo for attach_request doesn't exist. Maybe using older extension?: {}",
                fifo_path
            );
        }

        let attach_request = debugger.build_attach_information()?;
        let debugger_type_hint = attach_request
            .get(&AttachInformationKey::DebuggerTypeHint)
            .ok_or_else(|| anyhow!("[BUG] debugger has no DebuggerTypeHint"))?;
        let debugger_type = match debugger_type_hint.as_str() {
            "gdb" => "lldb", // use CodeLLDB to attach to gdb
            other => other,
        };
        let json = format!(
            r#"{{"protocolVersion": "{}", "debuggerType": "{}"}}"#,
            self.protocol_version, debugger_type
        );
        log::debug!("json: {}", json);

        log::info!("Requesting VSCode to attach. You can also manually attach by starting debug with \"Dbgee:\" launch configs.");
        self.send_json_to_vscode(json, fifo_path, None)
    }

    fn build_attach_request_fifo_path() -> Option<String> {
        // **
        // Heuristics:
        // Each VSCode window seems to have one unique UNIX socket for Git IPC.
        // It can be retrieved by $VSCODE_GIT_IPC_HANDLE in shell sessions in the integrated terminal.
        // Use it to distinguish VSCode's windows
        // **
        let vscode_git_ipc_handle = std::env::var_os("VSCODE_GIT_IPC_HANDLE")?;
        let pathbuf = PathBuf::from(vscode_git_ipc_handle);
        let sock_name = pathbuf.file_name()?.to_str()?;
        if !sock_name.ends_with(".sock") {
            return None;
        }
        let mut path = "/tmp/dbgee-vscode-debuggee-for-".to_owned();
        path.extend(sock_name[0..sock_name.len() - 5].chars().into_iter());
        Some(path)
    }
}

impl DebuggerTerminal for VsCode {
    fn name(&self) -> &str {
        "vscode"
    }

    fn open(&mut self, debugger: &dyn Debugger) -> Result<()> {
        self.send_attach_information(debugger)?;
        self.send_attach_request(debugger).or_else(|e| {
            log::debug!("Error: {}", e);
            // When not in VSCode terminal, or using older VSCode extension
            log::info!("The debuggee process is paused. Attach to it in VSCode");
            Ok(())
        })
    }
}
