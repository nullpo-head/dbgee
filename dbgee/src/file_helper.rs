use std::collections::HashMap;
use std::path::Path;
use std::str;
use std::sync::Mutex;
use std::{env, fs};
use std::{os::unix::fs::PermissionsExt, process::Command};

use anyhow::{anyhow, bail, Context, Result};
use once_cell::sync::Lazy;

static FILE_CMD_OUTPUT_CACHE: Lazy<Mutex<HashMap<String, String>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn get_filetype_by_filecmd(path: &str) -> Result<String> {
    let mut filecmd_cache = FILE_CMD_OUTPUT_CACHE
        .lock()
        .map_err(|_| anyhow!("Failed to acquire the lock for file command"))?;

    if let Some(cached) = filecmd_cache.get(path) {
        return Ok(cached.clone());
    }

    let file_output = Command::new("file").args(&[path]).output()?;
    let file_output = str::from_utf8(&file_output.stdout)?;
    filecmd_cache.insert(path.to_owned(), file_output.to_owned());
    Ok(file_output.to_owned())
}

pub fn command_exists(command: &str) -> bool {
    if let Ok(path) = env::var("PATH") {
        for dir in path.split(':') {
            let path = format!("{}/{}", dir, command);
            if is_executable(path) {
                return true;
            }
        }
    }
    false
}

pub fn get_valid_executable_path<T: AsRef<Path>>(path: T, name: &str) -> Result<String> {
    let abspath = get_abspath(path, name)?;
    if !is_executable(&abspath) {
        bail!("{} ({}) is not executable", name, abspath);
    }
    Ok(abspath)
}

pub fn get_abspath<T: AsRef<Path>>(path: T, name: &str) -> Result<String> {
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
    let abspath = abspath
        .as_os_str()
        .to_str()
        .ok_or_else(|| anyhow!("{} should exist in a valid UTF-8 path", name))?;
    Ok(abspath.to_owned())
}

pub fn is_executable<P: AsRef<Path>>(path: P) -> bool {
    if let Ok(metadata) = fs::metadata(path) {
        // TODO: more fine-grained permission check
        if metadata.is_file() && (metadata.permissions().mode() & 0o111 != 0) {
            return true;
        }
    }
    false
}
