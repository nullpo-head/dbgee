use cfg_if::cfg_if;

cfg_if! {
    if #[cfg(target_os = "linux")] {
        mod linux;
        use linux as os;
    } else if #[cfg(target_os = "macos")] {
        mod macos;
        use macos as os;
    }
}

pub use os::{is_any_hook_condition_set, run_hook, HookOpts};
