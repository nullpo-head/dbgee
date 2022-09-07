use core::slice;
use std::{
    borrow::Cow,
    collections::HashSet,
    fs::{self, File},
    os::unix::prelude::{AsRawFd, CommandExt},
    path::{Path, PathBuf},
    process::Command,
    ptr::null_mut,
};

use anyhow::{anyhow, bail, Context, Result};
use log::{debug, info, trace};
use nix::{
    errno::Errno,
    libc::{PTRACE_EVENT_CLONE, PTRACE_EVENT_FORK, PTRACE_EVENT_VFORK},
    sys::{
        mman::{mmap, MapFlags, ProtFlags},
        ptrace, signal, wait,
    },
    unistd::Pid,
};
use object::{Object, ObjectSection};
use structopt::StructOpt;

use crate::{
    build_debugger, build_debugger_terminal, file_helper::get_abspath, AttachOpts, ErrorLogger,
};

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab")]
pub struct HookOpts {
    #[structopt(short = "e", long)]
    /// Attach not to <command> itself, but to a descendant process whose executable file is the specified path.
    hook_executable: Option<PathBuf>,

    #[structopt(short = "s", long)]
    /// Attach not to <command> itself, but to a descendant process which is built from any of the given source files.
    /// A process binary must include DWARF debug information, which compilers usually emit for a debug build.
    hook_source: Option<Vec<String>>,

    #[structopt(short = "i", long)]
    /// Attach not to <command> itself, but to a descendant process which is built from any files under the given directory.
    /// A process binary must include DWARF debug information, which compilers usually emit for a debug build.
    hook_source_dir: Option<PathBuf>,
}

pub fn is_any_hook_condition_set(hook_opts: &HookOpts) -> bool {
    let HookOpts {
        hook_executable,
        hook_source,
        hook_source_dir,
    } = hook_opts;
    [
        hook_executable.is_some(),
        hook_source.is_some(),
        hook_source_dir.is_some(),
    ]
    .iter()
    .any(|cond| *cond)
}

/// Run the action for subcommand `run` with hook conditions.
pub fn run_hook(
    command: String,
    command_args: Vec<String>,
    hook_opts: HookOpts,
    attach_opts: AttachOpts,
) -> Result<()> {
    let terminal = &mut build_debugger_terminal(&attach_opts.terminal);

    let hook_conditions = build_hook_conditions(
        hook_opts.hook_executable,
        hook_opts.hook_source,
        hook_opts.hook_source_dir,
    )
    .context("failed to build hook conditions")?;

    let start_command_pid = spawn_traced_command(command, command_args)
        .context("Failed to spawn the traced command")?;

    // wait for a process triggering the hook condition
    let (hooked_command_pid, hooked_command_path) = loop {
        debug!("waiting for a SIGTRAP, that is, a new process");
        let pid = match wait_sigtrap().context("Failed to wait until next SIGTRAP")? {
            Some(pid) => pid,
            // All ancestor processes exited without triggering the condition.
            None => {
                info!("No process triggered the hook condition");
                return Ok(());
            }
        };
        debug!("a new process({}) is trapped", pid);

        // ptrace all ancestor processes to find any process which meets the hook condition
        if pid == start_command_pid {
            ptrace::setoptions(
                pid,
                ptrace::Options::PTRACE_O_TRACEFORK
                    | ptrace::Options::PTRACE_O_TRACECLONE
                    | ptrace::Options::PTRACE_O_TRACEVFORK,
            )
            .context("Failed to set a ptrace option")?;
        }

        if hook_conditions
            .iter()
            .map(|cond| cond.hooks(pid))
            .collect::<Result<Vec<bool>>>()
            .context("Failed to check a hook condition")?
            .iter()
            .any(|b| *b)
        {
            let exe_path = get_exe_path(pid).context("Failed to get an executable path")?;
            debug!("hooking exe_path: {:?}", &exe_path);
            break (pid, exe_path);
        }

        // This executable is not the target one, let it continue
        ptrace::cont(pid, None).with_context(|| format!("Failed to ptrace::continue {}", pid))?;
    };

    // Detach from the hooked process so that the debugger can attach it.
    ptrace::detach(hooked_command_pid, signal::SIGSTOP).with_context(|| {
        format!(
            "Failed to detach from the hooked process pid:{} path: {:?}",
            hooked_command_pid, &hooked_command_path
        )
    })?;
    let mut debugger = build_debugger(
        &attach_opts.debugger,
        hooked_command_path
            .to_str()
            .ok_or_else(|| anyhow!("executable path is not a valid utf-8 str"))?,
    )?;
    debugger
        .attach(
            hooked_command_pid,
            hooked_command_path
                .to_str()
                .ok_or_else(|| anyhow!("exe_path is not a valid utf-8 path"))?,
            terminal.as_mut(),
        )
        .with_context(|| format!("debugger failed to attach {}", hooked_command_pid))?;

    // wait until the start command exits, while detaching from any other processes
    wait_pid_exit_and_detach_other(start_command_pid)
        .context("Failed to wait for pid to exit while detaching other")?;

    Ok(())
}

// Spawn the command, and ptrace it with the given ptrace option
fn spawn_traced_command(command: String, args: Vec<String>) -> Result<Pid> {
    let mut command = Command::new(command);
    command.args(args);
    // Safety: safe because we don't have any other threads.
    unsafe {
        command.pre_exec(|| {
            ptrace::traceme().map_err(|e| {
                e.as_errno().map_or(
                    std::io::Error::new(std::io::ErrorKind::Other, "ptrace::traceme failed"),
                    |e| e.into(),
                )
            })
        });
    }
    let child = command
        .spawn()
        .context("Failed to spawn the command to trace")?;
    Ok(Pid::from_raw(child.id() as i32)) // u32 to nix::Pid
}

/// Do wait loop until it finds SIGTRAP
/// On success, it returns Some(Pid) if it finds, or None if all children exited.
fn wait_sigtrap() -> Result<Option<Pid>> {
    loop {
        let wait_result = wait::wait();
        if matches!(wait_result, Err(nix::Error::Sys(Errno::ECHILD))) {
            // There's no child processes
            return Ok(None);
        }

        // Note: `ptrace::cont` can fail if the process always exited, thus don't return when
        // they fail, but just logs them by `debug_log_error` instead.
        match wait_result.with_context(|| "Unexpected error. Waiting for SIGTRAP failed.")? {
            // A new process completed execve and threw SIGTRAP. Return its pid.
            wait::WaitStatus::Stopped(pid, signal::SIGTRAP) => {
                trace!("trapped pid({})", pid);
                return Ok(Some(pid));
            }
            // A tracee forked. Let both of the parent and the child continue.
            wait::WaitStatus::PtraceEvent(pid, _, PTRACE_EVENT_FORK)
            | wait::WaitStatus::PtraceEvent(pid, _, PTRACE_EVENT_CLONE)
            | wait::WaitStatus::PtraceEvent(pid, _, PTRACE_EVENT_VFORK) => {
                trace!("forked: {}", pid);
                let child_pid = ptrace::getevent(pid)
                    .with_context(|| anyhow!("Failed to get event of pid {}", pid))?;
                trace!("child_pid: {}", child_pid);
                ptrace::cont(pid, None)
                    .context("Failed to do PTRACE_CONT for the parent process after fork")
                    .debug_log_error();
                ptrace::cont(Pid::from_raw(child_pid as i32), None)
                    .context("Failed to do PTRACE_CONT for the child process after fork")
                    .debug_log_error();
            }
            // Some tracee got a signal. Let it see the given signal.
            wait::WaitStatus::Stopped(pid, sig) => {
                trace!("stopped: pid({}) sig({})", pid, sig);
                ptrace::cont(pid, sig)
                    .context("Failed to do PTRACE_CONT after stop signal")
                    .debug_log_error();
            }
            // Some tracee exited. Do nothing.
            wait::WaitStatus::Exited(pid, exitcode) => {
                trace!("exited: pid({}) sig({})", pid, exitcode);
            }
            // Some tracee is terminated by a signal. Do nothing.
            wait::WaitStatus::Signaled(pid, sig, _) => {
                trace!("signaled: pid({}) sig({})", pid, sig);
            }
            other => {
                trace!("other wait event: {:#?}", other);
                ptrace::cont(other.pid().unwrap(), None)
                    .context("Failed to do PTRACE_CONT after other wait event")
                    .debug_log_error();
            }
        };
    }
}

/// Wait for pid to exit, while detaching from any processes with other pids which
/// are caught by wait
fn wait_pid_exit_and_detach_other(pid_to_wait: Pid) -> Result<()> {
    loop {
        let wait_result = wait::wait();
        if matches!(wait_result, Err(nix::Error::Sys(Errno::ESRCH))) {
            // There's no child processes, which means `pid_to_wait` also exited.
            return Ok(());
        }

        // Note: `ptrace::detach` can fail if the process always exited, thus don't return when
        // they fail, but just logs them by `debug_log_error` instead.
        match wait_result.with_context(|| "Unexpected error. Waiting for SIGTRAP failed.")? {
            wait::WaitStatus::Exited(pid, _) => {
                trace!("exited: pid({})", pid);
                if pid == pid_to_wait {
                    return Ok(());
                }
            }
            // Fork event is caught. Let them continue and detach from them.
            wait::WaitStatus::PtraceEvent(pid, _, PTRACE_EVENT_FORK)
            | wait::WaitStatus::PtraceEvent(pid, _, PTRACE_EVENT_CLONE) => {
                trace!("forked: {}", pid);
                let child_pid = ptrace::getevent(pid)
                    .with_context(|| anyhow!("Failed to get event of pid {}", pid))?;
                trace!("detach from parent({}) and child({})", pid, child_pid);
                ptrace::detach(pid, None)
                    .context("Failed to detach from the parent process after fork")
                    .debug_log_error();
                ptrace::detach(Pid::from_raw(child_pid as i32), None)
                    .context("Failed to detach from the child process after fork")
                    .debug_log_error();
            }
            // Some tracee got a signal. Let it see the given signal, detaching from them.
            wait::WaitStatus::Stopped(pid, sig) => {
                trace!("detach from a stopped process: pid({}) sig({})", pid, sig);
                ptrace::detach(pid, sig)
                    .context("Failed to detach from a process stopped")
                    .debug_log_error();
            }
            other => {
                trace!("detach by other wait event: {:#?}", other);
                ptrace::detach(other.pid().unwrap(), None)
                    .context("Failed to detach from a process which triggered other wait event")
                    .debug_log_error();
            }
        };
    }
}

trait HookCondition {
    fn hooks(&self, pid: Pid) -> Result<bool>;
}

/// Builds the set of HookConditions
///
/// # Arguments
///
/// * `hook_executable` - Attach to a process with the specified path
/// * `hook_source` - Attach to a process which is built from any of the given comma-separated source files.
/// * `hook_source_dir` - Attach to a process which is built from any files under the given directory.
///
fn build_hook_conditions(
    hook_executable: Option<PathBuf>,
    hook_source: Option<Vec<String>>,
    hook_source_dir: Option<PathBuf>,
) -> Result<Vec<Box<dyn HookCondition>>> {
    let mut conditions: Vec<Box<dyn HookCondition>> = vec![];
    if let Some(path) = hook_executable {
        conditions.push(Box::new(
            build_hook_executable_condition(path)
                .context("Failed to build hook executable condition")?,
        ));
    }
    if let Some(source_paths) = hook_source {
        conditions.push(Box::new(
            build_hook_source_condition(source_paths)
                .context("Failed to build hook source condition")?,
        ));
    }
    if let Some(source_dir) = hook_source_dir {
        conditions.push(Box::new(
            build_hook_source_dir_condition(source_dir)
                .context("Failed to build hook source directory condition")?,
        ));
    }
    Ok(conditions)
}

struct HookExecutableCondition {
    executable_path: PathBuf,
}

fn build_hook_executable_condition(path: PathBuf) -> Result<HookExecutableCondition> {
    let executable_path = PathBuf::from(
        get_abspath(&path, "hook_executable")
            .with_context(|| format!("Failed to get the absolute path of {:?}", &path))?,
    );

    Ok(HookExecutableCondition { executable_path })
}

impl HookCondition for HookExecutableCondition {
    fn hooks(&self, pid: Pid) -> Result<bool> {
        let exe_path = get_exe_path(pid).context("Failed to get an executable path")?;
        debug!(
            "checking --hook-executable against exe_path: {:?}",
            &exe_path
        );

        Ok(self.executable_path == exe_path)
    }
}

struct HookSourceCondition {
    source_paths: HashSet<PathBuf>,
}

fn build_hook_source_condition(source_paths: Vec<String>) -> Result<HookSourceCondition> {
    let absolute_paths: Result<HashSet<PathBuf>> = source_paths
        .iter()
        .map(|path| {
            fs::canonicalize(path)
                .with_context(|| format!("Failed to get the canonicalized path of {:?}", path))
        })
        .collect();
    Ok(HookSourceCondition {
        source_paths: absolute_paths?,
    })
}

impl HookCondition for HookSourceCondition {
    fn hooks(&self, pid: Pid) -> Result<bool> {
        let exe_path = get_exe_path(pid)
            .with_context(|| format!("Failed to get the exe path of pid({})", pid))?;
        debug!("checking --hook-source against exe_path: {:?}", &exe_path);

        any_in_dwarf_decl_file(&exe_path, |path| self.source_paths.contains(path)).with_context(
            || {
                format!(
                    "Failed to find source_paths from decl_file({:?})",
                    &exe_path
                )
            },
        )
    }
}

struct HookSourceDirCondition {
    source_dir: PathBuf,
}

fn build_hook_source_dir_condition(source_dir: PathBuf) -> Result<HookSourceDirCondition> {
    let canonicalized = source_dir
        .canonicalize()
        .with_context(|| format!("Failed to canonicalize {:?}", &source_dir))?;
    Ok(HookSourceDirCondition {
        source_dir: canonicalized,
    })
}

impl HookCondition for HookSourceDirCondition {
    fn hooks(&self, pid: Pid) -> Result<bool> {
        let exe_path = get_exe_path(pid)
            .with_context(|| format!("Failed to get the exe path of pid({})", pid))?;
        debug!(
            "checking --hook-source-dir against exe_path: {:?}",
            &exe_path
        );

        any_in_dwarf_decl_file(&exe_path, |path| {
            debug!("comparing {:?} with {:?}", &self.source_dir, path);
            let is_triggered = path.starts_with(&self.source_dir);
            debug!("--- result: {}", is_triggered);
            is_triggered
        })
        .with_context(|| {
            format!(
                "Failed to find source_paths from decl_file({:?})",
                &exe_path
            )
        })
    }
}

fn get_exe_path(pid: Pid) -> Result<PathBuf> {
    fs::read_link(&format!("/proc/{}/exe", pid.as_raw()))
        .with_context(|| format!("Failed to read link /proc/{}/exe", pid.as_raw()))
}

/// Returns true if the dwarf file of `exe_path` contains any sources for which `predicate` returns true.
/// Note `any_in_dwarf_decl_file` does path comparison, resolving any path to canonicalized paths.
fn any_in_dwarf_decl_file<F>(exe_path: &Path, mut predicate: F) -> Result<bool>
where
    F: FnMut(&Path) -> bool,
{
    let mmap = Mmap::new(&exe_path).with_context(|| format!("Failed to mmap {:?}", &exe_path))?;
    {
        let buf = mmap.get();
        trace!("buf: {:?}", String::from_utf8_lossy(&buf[0..4]));
    }
    let object = object::File::parse(mmap.get()).unwrap();

    let load_section = |id: gimli::SectionId| -> Result<Cow<[u8]>, gimli::Error> {
        match object.section_by_name(id.name()) {
            Some(ref section) => Ok(section
                .uncompressed_data()
                .unwrap_or(Cow::Borrowed(&[][..]))),
            None => Ok(Cow::Borrowed(&[][..])),
        }
    };
    let dwarf_cow = gimli::Dwarf::load(&load_section).context("Failed to load a Dwarf file")?;
    // Borrow a `Cow<[u8]>` to create an `EndianSlice`.
    let borrow_section: &dyn for<'a> Fn(
        &'a Cow<[u8]>,
    ) -> gimli::EndianSlice<'a, gimli::RunTimeEndian> =
        &|section| gimli::EndianSlice::new(&*section, gimli::RunTimeEndian::Little);

    // Create `EndianSlice`s for all of the sections.
    let dwarf = dwarf_cow.borrow(&borrow_section);

    // Iterate over the compilation units.
    let mut iter = dwarf.units();
    trace!("iterates dwarf units");
    while let Some(header) = iter.next().context("Failed to iterate a unit")? {
        trace!(
            "Unit at <.debug_info+0x{:x}>",
            header.offset().as_debug_info_offset().unwrap().0
        );
        let unit = dwarf.unit(header)?;

        let path_resolver = match DwarfPathResolver::build(&dwarf, &unit)
            .context("Failed to build DwarfPathResolver")?
        {
            Some(path_resolver) => path_resolver,
            None => continue,
        };

        let header = match unit.line_program {
            Some(ref line_program) => line_program.header(),
            None => continue,
        };

        if header.file_names().iter().any(|file_entry| {
            let mut inner = || -> Result<bool> {
                let file_path = match path_resolver
                    .resolve_file(&dwarf, &unit, header, file_entry)
                    .context("Failed to resolve a file path")?
                {
                    Some(file_path) => file_path,
                    None => return Ok(false),
                };
                trace!("-- source file {:?}", &file_path);
                Ok(predicate(file_path.as_path()))
            };
            match inner() {
                Ok(res) => res,
                Err(e) => {
                    debug!("Error occurred during iterating an file name. {:?}", e);
                    false
                }
            }
        }) {
            return Ok(true);
        }
    }

    Ok(false)
}

struct DwarfPathResolver {
    comp_dir: PathBuf,
}

impl DwarfPathResolver {
    /// Build `DwarfPathResolver`. If the dwarf doesn't have AT_comp_dir or AT_comp_dir contains
    /// a path which doesn't exit on the running machine, `build` returns `Ok(None)`.
    pub fn build(
        dwarf: &gimli::Dwarf<gimli::EndianSlice<gimli::RunTimeEndian>>,
        unit: &gimli::Unit<gimli::EndianSlice<gimli::RunTimeEndian>, usize>,
    ) -> Result<Option<Self>> {
        // Get comp_dir path
        let comp_dir = match find_comp_dir(dwarf, unit).context("Failed to find comp dir")? {
            Some(comp_dir) => comp_dir,
            None => return Ok(None),
        };
        let comp_dir = match fs::canonicalize(comp_dir.to_string_lossy().as_ref()) {
            Ok(absolute_path) => absolute_path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => bail!(
                "Failed to canonicalize comp_dir({:?}); {:?}",
                comp_dir,
                error
            ),
        };

        Ok(Some(DwarfPathResolver { comp_dir }))
    }

    pub fn resolve_directory(
        &self,
        dwarf: &gimli::Dwarf<gimli::EndianSlice<gimli::RunTimeEndian>>,
        unit: &gimli::Unit<gimli::EndianSlice<gimli::RunTimeEndian>, usize>,
        header: &gimli::LineProgramHeader<gimli::EndianSlice<gimli::RunTimeEndian>, usize>,
        directory_index: u64,
    ) -> Result<Option<PathBuf>> {
        if directory_index == 0 {
            trace!("--- a directory is comp_dir: {:?}", self.comp_dir);
            return Ok(Some(self.comp_dir.clone()));
        }
        let directory = match header.directory(directory_index) {
            Some(directory) => directory,
            None => return Ok(None),
        };

        let directory_slice = dwarf
            .attr_string(unit, directory)
            .context("Failed to convert the directory slice to string")?;

        let directory_str = directory_slice.to_string_lossy();
        // return if it's an absolute path
        if directory_str.starts_with('/') {
            trace!("--- a directory is absolute: {:?}", directory_str);
            return Ok(Some(PathBuf::from(directory_str.as_ref())));
        }
        // otherwise, concat it with comp_dir
        trace!("--- a directory is relative: {:?}", directory_str);
        Ok(Some(self.comp_dir.as_path().join(directory_str.as_ref())))
    }

    pub fn resolve_file(
        &self,
        dwarf: &gimli::Dwarf<gimli::EndianSlice<gimli::RunTimeEndian>>,
        unit: &gimli::Unit<gimli::EndianSlice<gimli::RunTimeEndian>, usize>,
        header: &gimli::LineProgramHeader<gimli::EndianSlice<gimli::RunTimeEndian>, usize>,
        file: &gimli::FileEntry<gimli::EndianSlice<gimli::RunTimeEndian>, usize>,
    ) -> Result<Option<PathBuf>> {
        let file_slice = dwarf
            .attr_string(unit, file.path_name())
            .context("Failed to get attr_string of a file path_name")?;

        let file_str = file_slice.to_string_lossy();
        // return if it's an absolute path
        if file_slice.starts_with(&[b'/']) {
            trace!("--- a file path is absolute: {:?}", file_str);
            return Ok(Some(PathBuf::from(file_str.as_ref())));
        }

        trace!("--- a file path is relative: {:?}", file_str);
        let mut directory = self
            .resolve_directory(dwarf, unit, header, file.directory_index())
            .context("Failed to get the directory of a file")?
            .ok_or_else(|| {
                anyhow!(
                    "a directory corresponding to a file not found. file({:?}), directory({})",
                    file,
                    file.directory_index()
                )
            })?;

        directory.push(file_str.as_ref());
        Ok(Some(directory))
    }
}

fn find_comp_dir<'a>(
    dwarf: &'a gimli::Dwarf<gimli::EndianSlice<'a, gimli::RunTimeEndian>>,
    unit: &'a gimli::Unit<gimli::EndianSlice<'a, gimli::RunTimeEndian>, usize>,
) -> Result<Option<gimli::EndianSlice<'a, gimli::RunTimeEndian>>> {
    let mut entries = unit.entries();
    while let Some((_, entry)) = entries.next_dfs().context("Failed to get next_dfs entry")? {
        let mut attrs = entry.attrs();
        while let Some(attr) = attrs.next().context("Failed to get next attr")? {
            if attr.name() == gimli::constants::DW_AT_comp_dir {
                let str_slice = dwarf
                    .attr_string(unit, attr.value())
                    .context("Failed to convert the comp_dir to string")?;
                return Ok(Some(str_slice));
            }
        }
    }
    Ok(None)
}

struct Mmap {
    _file: File,
    file_size: usize,
    mmapped_addr: *mut u8,
}

impl Mmap {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path.as_ref())
            .with_context(|| format!("Failed to open {:?}", &path.as_ref()))?;
        let file_size = file
            .metadata()
            .with_context(|| format!("Failed to get the metadata of {:?}", path.as_ref()))?
            .len() as usize;

        // Safe because the backing file is also owned by this Mmap struct.
        let mmapped_addr = unsafe {
            mmap(
                null_mut(),
                file_size,
                ProtFlags::PROT_READ,
                MapFlags::MAP_FILE | MapFlags::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
            .with_context(|| format!("Failed to mmap {:?}", path.as_ref()))? as *mut u8
        };
        Ok(Mmap {
            _file: file,
            file_size,
            mmapped_addr,
        })
    }

    pub fn get(&self) -> &[u8] {
        // safe because self owns the backing file
        unsafe { slice::from_raw_parts(self.mmapped_addr, self.file_size) }
    }
}
