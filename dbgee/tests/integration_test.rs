use std::{env, fs, path::PathBuf, process::Command, str::FromStr};

use anyhow::Result;

#[test]
fn test_run_pid_debugger() -> Result<()> {
    set_fake_commands_path()?;
    let dbgee_pathbuf = get_bin_path();
    let langs = ["c", "rust"];

    for lang in langs.iter() {
        let cmd = vec![
            "run".to_owned(),
            "-t".to_owned(),
            "tmuxw".to_owned(),
            "--".to_owned(),
            format!(
                "{}/lang_projects/{}/hello",
                get_tests_dir()?.to_str().unwrap(),
                lang
            ),
            "arg0".to_owned(),
            "arg1".to_owned(),
        ];
        let output = Command::new(dbgee_pathbuf.as_os_str()).args(cmd).output()?;
        assert_eq!(Some(0), output.status.code());
        assert_eq!(
            "'new-window' 'gdb' '-p' '<NUM>' \nhello\n",
            &String::from_utf8(output.stdout)?
        );
    }

    Ok(())
}

#[test]
fn test_run_dlv() -> Result<()> {
    set_fake_commands_path()?;
    let dbgee_pathbuf = get_bin_path();

    let cmd = vec![
        "run".to_owned(),
        "-t".to_owned(),
        "tmuxw".to_owned(),
        "--".to_owned(),
        format!(
            "{}/lang_projects/go/hello",
            get_tests_dir()?.to_str().unwrap()
        ),
        "arg0".to_owned(),
        "arg1".to_owned(),
    ];
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

    let copied_hello = CopiedExecutable::new(&format!(
        "{}/lang_projects/c/hello",
        get_tests_dir()?.to_str().unwrap()
    ))?;

    let dbgee_pathbuf = get_bin_path();
    let cmd_to_set = vec!["set", "-t", "tmuxw", &copied_hello.path];
    let status = Command::new(dbgee_pathbuf.as_os_str())
        .args(cmd_to_set)
        .status()?;
    assert_eq!(Some(0), status.code());

    let debuggee_output = Command::new(&copied_hello.path).output()?;
    assert_eq!(Some(0), debuggee_output.status.code());
    assert_eq!(
        "'new-window' 'gdb' '-p' '<NUM>' \nhello\n",
        &String::from_utf8(debuggee_output.stdout)?
    );

    let cmd_to_unset = vec!["unset", &copied_hello.path];
    let status = Command::new(dbgee_pathbuf.as_os_str())
        .args(cmd_to_unset)
        .status()?;
    assert_eq!(Some(0), status.code());

    let original_debuggee_output = Command::new(&copied_hello.path).output()?;
    assert_eq!(Some(0), debuggee_output.status.code());
    assert_eq!(
        "hello\n",
        &String::from_utf8(original_debuggee_output.stdout)?
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

fn get_bin_path() -> PathBuf {
    let mut pathbuf = env::current_exe().unwrap();
    pathbuf.pop();
    // https://github.com/rust-lang/cargo/issues/5758
    if pathbuf.ends_with("deps") {
        pathbuf.pop();
    }
    pathbuf.push("dbgee");
    pathbuf
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
        let copied_path = "/tmp/dbgee-copied-debuggee";
        fs::copy(&path, copied_path)?;
        Ok(CopiedExecutable {
            path: copied_path.to_string(),
        })
    }
}

impl Drop for CopiedExecutable {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
