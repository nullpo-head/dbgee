use std::{
    env, fs,
    io::Read,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    str::FromStr,
};

use anyhow::Result;
use nix::{sys::signal, unistd};

#[test]
fn test_run_pid_debugger() -> Result<()> {
    set_fake_commands_path()?;
    let dbgee_pathbuf = get_dbgee_bin_path();
    let langs = ["c", "rust"];

    for lang in langs.iter() {
        let lang_bin_path = get_lang_testbin_path(lang)?;
        let cmd = vec![
            "run",
            "-t",
            "tmuxw",
            "--",
            lang_bin_path.as_str(),
            "arg0",
            "arg1",
        ];
        let output = Command::new(dbgee_pathbuf.as_os_str()).args(cmd).output()?;
        assert_eq!(Some(0), output.status.code());
        if cfg!(target_os = "linux") {
            assert_eq!(
                "'new-window' 'gdb' '-tui' '-p' '<NUM>' \nhello\n",
                &String::from_utf8(output.stdout)?
            );
        } else
        /* macOS */
        {
            // commands are wrapped by sudo in macOS
            assert_eq!(
                "'new-window' 'lldb' '-p' '<NUM>' \nhello\n",
                &String::from_utf8(output.stdout)?
            );
        }
    }

    Ok(())
}

#[test]
fn test_run_dlv() -> Result<()> {
    set_fake_commands_path()?;
    let dbgee_pathbuf = get_dbgee_bin_path();

    let lang_testbin = get_lang_testbin_path("go")?;
    let cmd = vec!["run", "-t", "tmuxw", "--", &lang_testbin, "arg0", "arg1"];
    let output = Command::new(dbgee_pathbuf.as_os_str()).args(cmd).output()?;
    assert_eq!(Some(0), output.status.code());
    let stdout = String::from_utf8(output.stdout)?
        .lines()
        .map(|s| s.to_owned())
        .collect::<Vec<String>>();
    // The command line for Delve is long, so test only part of it here
    assert!(stdout[0].starts_with("'exec' '--headless'"));
    assert!(stdout[1].starts_with("'new-window' 'dlv' 'connect'"));

    Ok(())
}

#[test]
fn test_set_pid_debugger() -> Result<()> {
    set_fake_commands_path()?;

    // copy the hello binary to a temporary file for testing
    let copied_hello = CopiedExecutable::new(&get_lang_testbin_path("c")?)?;

    // `set` should succeed
    let dbgee_pathbuf = get_dbgee_bin_path();
    let cmd_to_set = vec!["set", "-t", "tmuxw", &copied_hello.path];
    let status = Command::new(dbgee_pathbuf.as_os_str())
        .args(cmd_to_set)
        .status()?;
    assert_eq!(Some(0), status.code());

    // Running the copied hello binary now should trigger tmux
    let debuggee_output = Command::new(&copied_hello.path).output()?;
    assert_eq!(Some(0), debuggee_output.status.code());
    if cfg!(target_os = "linux") {
        assert_eq!(
            "'new-window' 'gdb' '-tui' '-p' '<NUM>' \nhello\n",
            &String::from_utf8(debuggee_output.stdout)?
        );
    } else
    /* macOS */
    {
        // commands are wrapped by sudo in macOS
        assert_eq!(
            "'new-window' 'lldb' '-p' '<NUM>' \nhello\n",
            &String::from_utf8(debuggee_output.stdout)?
        );
    }

    // `unset` should succeed
    let cmd_to_unset = vec!["unset", &copied_hello.path];
    let status = Command::new(dbgee_pathbuf.as_os_str())
        .args(cmd_to_unset)
        .status()?;
    assert_eq!(Some(0), status.code());

    // Now the copied_hello should be restored
    let original_debuggee_output = Command::new(&copied_hello.path).output()?;
    assert_eq!(Some(0), original_debuggee_output.status.code());
    assert_eq!(
        "hello\n",
        &String::from_utf8(original_debuggee_output.stdout)?
    );

    Ok(())
}

#[test]
fn test_run_debuggee_which_is_set_before() -> Result<()> {
    set_fake_commands_path()?;

    let copied_hello = CopiedExecutable::new(&get_lang_testbin_path("c")?)?;

    // `set` the debuggee first
    let dbgee_pathbuf = get_dbgee_bin_path();
    let cmd_to_set = vec!["set", "-t", "tmuxw", &copied_hello.path];
    let status = Command::new(dbgee_pathbuf.as_os_str())
        .args(cmd_to_set)
        .status()?;
    assert_eq!(Some(0), status.code());

    // dbgee should be able to `run` the debuggee which is being `set`
    let cmd = vec![
        "run",
        "-t",
        "tmuxw",
        "--",
        &copied_hello.path,
        "arg0",
        "arg1",
    ];
    let output = Command::new(dbgee_pathbuf.as_os_str()).args(cmd).output()?;
    assert_eq!(Some(0), output.status.code());
    if cfg!(target_os = "linux") {
        assert_eq!(
            "'new-window' 'gdb' '-tui' '-p' '<NUM>' \nhello\n",
            &String::from_utf8(output.stdout)?
        );
    } else
    /* macOS */
    {
        // commands are wrapped by sudo in macOS
        assert_eq!(
            "'new-window' 'lldb' '-p' '<NUM>' \nhello\n",
            &String::from_utf8(output.stdout)?
        );
    }

    // `unset` should succeed
    let cmd_to_unset = vec!["unset", &copied_hello.path];
    let status = Command::new(dbgee_pathbuf.as_os_str())
        .args(cmd_to_unset)
        .status()?;
    assert_eq!(Some(0), status.code());

    // Now the copied_hello should be restored
    let output = Command::new(&copied_hello.path).output()?;
    assert_eq!(Some(0), output.status.code());
    assert_eq!("hello\n", &String::from_utf8(output.stdout)?);

    Ok(())
}

#[test]
fn test_run_for_vscode() -> Result<()> {
    set_fake_commands_path()?;
    let dbgee_pathbuf = get_dbgee_bin_path();

    // launch `dbgee`
    let lang_testbin = get_lang_testbin_path("python")?;
    let debuggee_args = vec!["run", "-t", "vscode", "--", &lang_testbin, "arg0", "arg1"];
    let mut dbgee_command = Command::new(dbgee_pathbuf.as_os_str());
    dbgee_command
        .args(debuggee_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        // to kill dbgee including its child later.
        dbgee_command.pre_exec(|| {
            unistd::setpgid(unistd::Pid::from_raw(0), unistd::Pid::from_raw(0)).unwrap();
            Ok(())
        });
    }
    let dbgee = dbgee_command.spawn().unwrap();

    // read from the fifo in a thread with timtout because it may block if there's a bug
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let fifo_path = "/tmp/dbgee-vscode-debuggees";
        let _ = unistd::mkfifo(fifo_path, nix::sys::stat::Mode::S_IRWXU);
        let mut file = fs::File::open(fifo_path).unwrap();
        let mut json = String::new();
        file.read_to_string(&mut json).unwrap();
        let _ = sender.send(json);
    });
    let json = receiver
        .recv_timeout(std::time::Duration::from_secs(20))
        .unwrap();
    // assert that it is a json
    assert!(json.starts_with('{'));
    assert!(json.ends_with('}'));
    assert!(json.contains(r#"debuggerPort": "#));

    // kill dbgee including its child.
    signal::killpg(unistd::Pid::from_raw(dbgee.id() as i32), signal::SIGTERM).unwrap();
    let mut debugpy_args = String::new();
    let _ = dbgee.stdout.unwrap().read_to_string(&mut debugpy_args);
    assert_eq!(
        format!(
            "'-m' 'debugpy' '--wait-for-client' '--listen' '<NUM>' '{}' 'arg0' 'arg1' \n",
            &get_lang_testbin_path("python")?
        ),
        debugpy_args
    );

    Ok(())
}

fn set_fake_commands_path() -> Result<()> {
    let mut pathbuf = get_tests_dir()?;
    pathbuf.push("fake_commands:");

    let mut path = env::var("PATH")?;
    if path.starts_with(pathbuf.to_str().unwrap()) {
        return Ok(());
    }
    path.insert_str(0, pathbuf.to_str().unwrap());
    env::set_var("PATH", path);
    Ok(())
}

fn get_dbgee_bin_path() -> PathBuf {
    let mut pathbuf = env::current_exe().unwrap();
    pathbuf.pop();
    // https://github.com/rust-lang/cargo/issues/5758
    if pathbuf.ends_with("deps") {
        pathbuf.pop();
    }
    pathbuf.push("dbgee");
    pathbuf
}

fn get_lang_testbin_path(lang: &str) -> Result<String> {
    Ok(format!(
        "{}/lang_projects/{}/hello-{}-{}",
        get_tests_dir()?.to_str().unwrap(),
        lang,
        std::env::consts::ARCH,
        std::env::consts::OS,
    ))
}

fn get_tests_dir() -> Result<PathBuf> {
    let mut pathbuf = PathBuf::from_str(&env::var("CARGO_MANIFEST_DIR")?)?;
    pathbuf.push("tests");
    Ok(pathbuf)
}

struct CopiedExecutable {
    path: String,
}

impl CopiedExecutable {
    fn new(path: &str) -> Result<CopiedExecutable> {
        let copied_path = format!("/tmp/dbgee-copied-debuggee-{}", uuid::Uuid::new_v4());
        fs::copy(&path, &copied_path)?;
        Ok(CopiedExecutable { path: copied_path })
    }
}

impl Drop for CopiedExecutable {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
