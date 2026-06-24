use crate::entirely::coordinator;
use crate::entirely::error::InstallError;
use crate::entirely::helper_ipc;
use crate::sleep::power_management;
use crate::util::fs_util;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const HELPER_INSTALL_PATH: &str = "/usr/local/libexec/caffeinate2/caffeinate2-helper";
pub const HELPER_PLIST_PATH: &str =
    "/Library/LaunchDaemons/com.randomblock1.caffeinate2.helper.plist";
pub const HELPER_PLIST_LABEL: &str = "com.randomblock1.caffeinate2.helper";
pub const TRAY_LAUNCH_AGENT_LABEL: &str = "com.randomblock1.caffeinate2-tray";

const NEWSYSLOG_CONF_PATH: &str = "/etc/newsyslog.d/com.randomblock1.caffeinate2.helper.conf";

const HELPER_PLIST_TEMPLATE: &str =
    include_str!("../../resources/com.randomblock1.caffeinate2.helper.plist");
const TRAY_PLIST_TEMPLATE: &str =
    include_str!("../../resources/com.randomblock1.caffeinate2-tray.plist");
const NEWSYSLOG_CONF_TEMPLATE: &str =
    include_str!("../../resources/newsyslog/com.randomblock1.caffeinate2.helper.conf");

const HELPER_BINARY_HINT: &str = "install caffeinate2 with --features full or helper-bin, \
    use the GitHub release bundle, or place caffeinate2-helper in the same directory as caffeinate2";

const CLI_BINARY_HINT: &str = "install caffeinate2 with --features full, use the GitHub release \
    bundle, or place caffeinate2 in the same directory as caffeinate2-helper";

fn resolve_sibling_binary(binary_name: &str, build_hint: &str) -> Result<PathBuf, InstallError> {
    let exe = std::env::current_exe().map_err(InstallError::from)?;
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == binary_name {
        return Ok(exe);
    }
    let sibling = exe
        .parent()
        .map(|p| p.join(binary_name))
        .ok_or_else(|| InstallError::msg(format!("could not resolve {binary_name} path")))?;
    if sibling.exists() {
        Ok(sibling)
    } else {
        Err(InstallError::msg(format!(
            "{binary_name} not found next to {}; {build_hint}",
            exe.display()
        )))
    }
}

///
/// # Errors
///
/// Returns an error if the CLI binary cannot be resolved.
pub fn resolve_cli_binary() -> Result<PathBuf, InstallError> {
    resolve_sibling_binary("caffeinate2", CLI_BINARY_HINT)
}

///
/// # Errors
///
/// Returns an error if the helper binary cannot be resolved.
pub fn resolve_helper_source() -> Result<PathBuf, InstallError> {
    resolve_sibling_binary("caffeinate2-helper", HELPER_BINARY_HINT)
}

#[must_use]
pub fn helper_plist_content(helper_path: &Path) -> String {
    HELPER_PLIST_TEMPLATE.replace("__HELPER_PATH__", &helper_path.display().to_string())
}

#[must_use]
pub fn tray_launch_agent_plist(tray_path: &Path) -> String {
    TRAY_PLIST_TEMPLATE.replace("__TRAY_PATH__", &tray_path.display().to_string())
}

///
/// # Errors
///
/// Returns an error if `HOME` is not set.
pub fn tray_launch_agent_path() -> Result<PathBuf, InstallError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| InstallError::msg("HOME is not set"))?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{TRAY_LAUNCH_AGENT_LABEL}.plist")))
}

///
/// # Errors
///
/// Returns an error if installation fails or the process is not root.
pub fn install_helper(source_helper: &Path) -> Result<(), InstallError> {
    if !nix::unistd::Uid::effective().is_root() {
        return Err(InstallError::msg("--install-helper must run as root"));
    }

    let dest = PathBuf::from(HELPER_INSTALL_PATH);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(InstallError::from)?;
    }
    // Reinstall path: stop any loaded helper before replacing its binary, and
    // unlink the old file so the copy gets a fresh inode. Overwriting a running
    // executable in place invalidates its code signature and the kernel kills
    // the process mid-write; bootstrap below would also no-op while the old
    // service is still loaded. bootout may fail on first install, so suppress it.
    let _ = launchctl_bootout_system(HELPER_PLIST_LABEL);
    match fs::remove_file(&dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("could not remove old helper binary: {e}"),
    }
    fs::copy(source_helper, &dest).map_err(InstallError::from)?;
    let mut perms = fs::metadata(&dest)
        .map_err(InstallError::from)?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&dest, perms).map_err(InstallError::from)?;

    let plist = helper_plist_content(&dest);
    // launchctl refuses a system LaunchDaemon plist that is group/world-writable
    // (a permissive umask would otherwise make atomic_write produce 0666), so
    // write it with an explicit root-owned 0644.
    fs_util::atomic_write_with_mode(Path::new(HELPER_PLIST_PATH), plist.as_bytes(), 0o644)
        .map_err(InstallError::from)?;
    install_newsyslog_conf();

    // Best-effort: create the grant group so administrators can allow
    // standard accounts with a single dseditgroup -o edit command. The
    // helper authorizes admins regardless, so a failure here only matters
    // for that workflow.
    ensure_grant_group();

    launchctl_bootstrap_system(HELPER_PLIST_PATH)?;
    Ok(())
}

fn install_newsyslog_conf() {
    let path = Path::new(NEWSYSLOG_CONF_PATH);
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        tracing::warn!("could not create newsyslog config directory: {e}");
        return;
    }
    // newsyslog also expects a root-owned, non-world-writable config.
    if let Err(e) = fs_util::atomic_write_with_mode(path, NEWSYSLOG_CONF_TEMPLATE.as_bytes(), 0o644) {
        tracing::warn!("could not install newsyslog config: {e}");
    }
}

fn ensure_grant_group() {
    let group = crate::entirely::authz::GRANT_GROUP;
    let exists = Command::new("dseditgroup")
        .args(["-o", "read", group])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if exists {
        return;
    }
    match Command::new("dseditgroup")
        .args(["-o", "create", group])
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!(
            "could not create '{group}' group: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(e) => tracing::warn!("could not create '{group}' group: {e}"),
    }
}

///
/// # Errors
///
/// Returns an error if uninstallation fails or the process is not root.
pub fn uninstall_helper() -> Result<(), InstallError> {
    if !nix::unistd::Uid::effective().is_root() {
        return Err(InstallError::msg("--uninstall-helper must run as root"));
    }

    let _ = launchctl_bootout_system(HELPER_PLIST_LABEL);
    let _ = fs::remove_file(HELPER_PLIST_PATH);
    let _ = fs::remove_file(NEWSYSLOG_CONF_PATH);
    let _ = fs::remove_file(HELPER_INSTALL_PATH);
    let _ = fs::remove_file(helper_ipc::HELPER_SOCKET_PATH);

    // The lockfile is caffeinate2's marker that it is (or was) managing the
    // global SleepDisabled setting. Once the helper is removed, nobody can send
    // Release, so if that marker is present we re-enable sleep to avoid leaving
    // SleepDisabled stuck on. If there is no lockfile we have no evidence that
    // caffeinate2 disabled sleep, so we leave the setting untouched rather than
    // clobber an unrelated manual `pmset disablesleep`.
    let caffeinate2_managed_sleep = Path::new(coordinator::HELPER_LOCK_PATH).exists();
    let _ = fs::remove_file(coordinator::HELPER_LOCK_PATH);
    if caffeinate2_managed_sleep {
        let _ = power_management::set_sleep_disabled(false, false);
    }
    Ok(())
}

///
/// # Errors
///
/// Returns an error if privileged installation fails or is cancelled.
pub fn install_helper_privileged() -> Result<(), InstallError> {
    if nix::unistd::Uid::effective().is_root() {
        let source = resolve_helper_source()?;
        return install_helper(&source);
    }

    let cli = resolve_cli_binary()?;
    // The path is embedded in a shell command inside an AppleScript string, so
    // it needs both layers of quoting or paths with spaces break.
    let cli_escaped = applescript_escape(&sh_single_quote(&cli.display().to_string()));
    let script = format!(
        "do shell script \"{cli_escaped} --install-helper-internal\" with administrator privileges"
    );
    let status = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()
        .map_err(InstallError::from)?;
    if status.success() {
        Ok(())
    } else {
        Err(InstallError::msg(
            "administrator authorization failed or was cancelled",
        ))
    }
}

///
/// # Errors
///
/// Returns an error if the launch agent plist cannot be written.
pub fn install_tray_launch_agent(tray_path: &Path) -> Result<(), InstallError> {
    let plist_path = tray_launch_agent_path()?;
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent).map_err(InstallError::from)?;
    }
    let plist = tray_launch_agent_plist(tray_path);
    fs_util::atomic_write(&plist_path, plist.as_bytes()).map_err(InstallError::from)?;
    // Don't bootstrap the agent now: RunAtLoad would immediately launch a
    // second tray instance next to the one the user is clicking in. launchd
    // picks up ~/Library/LaunchAgents plists at the next login.
    Ok(())
}

///
/// # Errors
///
/// Returns an error if the launch agent plist cannot be removed.
pub fn uninstall_tray_launch_agent() -> Result<(), InstallError> {
    let plist_path = tray_launch_agent_path()?;
    // Only remove the plist; launchd won't start the agent at the next login.
    // Do NOT boot out the loaded service: when the tray was started by launchd
    // (the common case after enabling start-at-login), the current process *is*
    // that service, and bootout would terminate the running app the instant the
    // user unchecks the menu item.
    match fs::remove_file(&plist_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(InstallError::msg(format!(
            "failed to remove {}: {e}",
            plist_path.display()
        ))),
    }
}

#[must_use]
pub fn tray_launch_agent_installed() -> bool {
    tray_launch_agent_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn launchctl_bootstrap_system(plist_path: &str) -> Result<(), InstallError> {
    if run_launchctl(&["bootstrap", "system", plist_path]).is_ok() {
        return Ok(());
    }
    run_launchctl(&["load", "-w", plist_path])
}

fn launchctl_bootout_system(label: &str) -> Result<(), InstallError> {
    // bootout takes a service target (`system/<label>`), not a bare label.
    if run_launchctl(&["bootout", &format!("system/{label}")]).is_ok() {
        return Ok(());
    }
    run_launchctl(&[
        "unload",
        "-w",
        &format!("/Library/LaunchDaemons/{label}.plist"),
    ])
}

fn run_launchctl(args: &[&str]) -> Result<(), InstallError> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .map_err(InstallError::from)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(InstallError::msg(format!(
            "launchctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_plist_substitutes_path() {
        let content = helper_plist_content(Path::new(
            "/usr/local/libexec/caffeinate2/caffeinate2-helper",
        ));
        assert!(content.contains("/usr/local/libexec/caffeinate2/caffeinate2-helper"));
        assert!(!content.contains("__HELPER_PATH__"));
    }

    #[test]
    fn privileged_install_quoting_survives_spaces_and_quotes() {
        assert_eq!(
            sh_single_quote("/Users/a b/caffeinate2"),
            "'/Users/a b/caffeinate2'"
        );
        assert_eq!(sh_single_quote("it's"), r"'it'\''s'");
        assert_eq!(applescript_escape(r"'it'\''s'"), r"'it'\\''s'");
        assert_eq!(applescript_escape("say \"hi\""), r#"say \"hi\""#);
    }

    #[test]
    fn tray_plist_substitutes_path() {
        let content = tray_launch_agent_plist(Path::new("/Users/test/.cargo/bin/caffeinate2-tray"));
        assert!(content.contains("/Users/test/.cargo/bin/caffeinate2-tray"));
        assert!(content.contains(TRAY_LAUNCH_AGENT_LABEL));
        assert!(!content.contains("__TRAY_PATH__"));
    }

    #[test]
    fn helper_binary_hint_mentions_packaging_options() {
        assert!(HELPER_BINARY_HINT.contains("--features full"));
        assert!(HELPER_BINARY_HINT.contains("GitHub release"));
        assert!(HELPER_BINARY_HINT.contains("caffeinate2-helper"));
    }
}
