use anyhow::Result;
use structopt::StructOpt;

use crate::AttachOpts;

////
// macOS does not support Hook option.
///

/// Run a command and attach a debugger to its child process which triggered the specified hook condition.
#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab")]
pub struct HookOpts {
    #[structopt(long, hidden = true)]
    _dummy: bool,
}

pub fn is_any_hook_condition_set(_hook_opts: &HookOpts) -> bool {
    // since macOS does not support any hook conditions
    false
}

/// Run the action for subcommand `run` with hook conditions.
pub fn run_hook(
    _command: String,
    _command_args: Vec<String>,
    _hook_opts: HookOpts,
    _attach_opts: AttachOpts,
) -> Result<()> {
    unimplemented!("macOS does not support hook conditions");
}
