//! The environment and config settings every git child of this crate starts from.
//!
//! Local object plumbing and the remote transport each build their git command on the same base:
//! an emptied environment holding a fixed set of `GIT_*` variables, then a leading run of `-c`
//! settings. Each builder adds what is specific to it afterwards, so the shared part lives here
//! and a change to it reaches both.

use std::process::Command;

/// Settings every git invocation carries first, in the form `git -c` takes.
///
/// Each builder passes these before its own settings, so the final argument order is this list
/// followed by the builder's additions.
pub(crate) const SHARED_SETTINGS: &[&str] = &[
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "commit.gpgsign=false",
    "credential.helper=",
    "core.sshCommand=/usr/bin/false",
    "protocol.allow=never",
];

/// Replace the command's environment with the variables every git invocation runs under.
///
/// Callers add their own variables after this call; it must run before them because it empties
/// whatever the command held.
pub(crate) fn apply_shared_env(command: &mut Command) {
    // Inherited GIT_DIR, index/object paths, config injection, and identities
    // must not redirect an operation away from the caller's authorized repo.
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1");
}

/// The arguments and the environment mappings a command will start with, as plain strings.
///
/// The environment is keyed by variable name, so it does not depend on the order the variables
/// were set in. A variable removed from the command is reported as `None`.
#[cfg(test)]
pub(crate) fn describe(
    command: &Command,
) -> (
    Vec<String>,
    std::collections::BTreeMap<String, Option<String>>,
) {
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let envs = command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect();
    (args, envs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_env_drops_variables_set_before_it() {
        let mut command = Command::new("git");
        command.env("KHIVE_GIT_ENV_PRESET", "inherited");
        apply_shared_env(&mut command);
        let (_, envs) = describe(&command);
        assert!(
            !envs.contains_key("KHIVE_GIT_ENV_PRESET"),
            "the shared environment must start from an emptied one"
        );
        assert_eq!(envs.get("LC_ALL"), Some(&Some("C".to_owned())));
    }
}
