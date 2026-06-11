use crate::entirely;
use crate::helper_ipc;
use crate::power_management;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const HELPER_INSTALL_PATH: &str = "/usr/local/libexec/caffeinate2/caffeinate2-helper";
pub const HELPER_PLIST_PATH: &str =
    "/Library/LaunchDaemons/com.randomblock1.caffeinate2.helper.plist";
pub const HELPER_PLIST_LABEL: &str = "com.randomblock1.caffeinate2.helper";
pub const TRAY_LAUNCH_AGENT_LABEL: &str = "com.randomblock1.caffeinate2-tray";

const HELPER_PLIST_TEMPLATE: &str =
    include_str!("../resources/com.randomblock1.caffeinate2.helper.plist");
const TRAY_PLIST_TEMPLATE: &str =
    include_str!("../resources/com.randomblock1.caffeinate2-tray.plist");

fn resolve_sibling_binary(binary_name: &str, build_hint: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == binary_name {
        return Ok(exe);
    }
    let sibling = exe
        .parent()
        .map(|p| p.join(binary_name))
        .ok_or_else(|| format!("could not resolve {binary_name} path"))?;
    if sibling.exists() {
        Ok(sibling)
    } else {
        Err(format!(
            "{binary_name} not found next to {}; {build_hint}",
            exe.display()
        ))
    }
}

pub fn resolve_cli_binary() -> Result<PathBuf, String> {
    resolve_sibling_binary("caffeinate2", "install caffeinate2 with --features full")
}

pub fn resolve_helper_source() -> Result<PathBuf, String> {
    resolve_sibling_binary("caffeinate2-helper", "build with --features helper-bin")
}

pub fn helper_plist_content(helper_path: &Path) -> String {
    HELPER_PLIST_TEMPLATE.replace("__HELPER_PATH__", &helper_path.display().to_string())
}

pub fn tray_launch_agent_plist(tray_path: &Path) -> String {
    TRAY_PLIST_TEMPLATE.replace("__TRAY_PATH__", &tray_path.display().to_string())
}

pub fn tray_launch_agent_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{TRAY_LAUNCH_AGENT_LABEL}.plist")))
}

pub fn install_helper(source_helper: &Path) -> Result<(), String> {
    if !nix::unistd::Uid::effective().is_root() {
        return Err("--install-helper must run as root".to_string());
    }

    let dest = PathBuf::from(HELPER_INSTALL_PATH);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Reinstall path: stop any loaded helper before replacing its binary, and
    // unlink the old file so the copy gets a fresh inode. Overwriting a running
    // executable in place invalidates its code signature and the kernel kills
    // the process mid-write; bootstrap below would also no-op while the old
    // service is still loaded.
    let _ = launchctl_bootout_system(HELPER_PLIST_LABEL);
    let _ = fs::remove_file(&dest);
    fs::copy(source_helper, &dest).map_err(|e| e.to_string())?;
    let mut perms = fs::metadata(&dest)
        .map_err(|e| e.to_string())?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&dest, perms).map_err(|e| e.to_string())?;

    let plist = helper_plist_content(&dest);
    fs::write(HELPER_PLIST_PATH, plist).map_err(|e| e.to_string())?;

    // Best-effort: create the grant group so administrators can allow
    // standard accounts with a single dseditgroup -o edit command. The
    // helper authorizes admins regardless, so a failure here only matters
    // for that workflow.
    ensure_grant_group();

    launchctl_bootstrap_system(HELPER_PLIST_PATH)?;
    Ok(())
}

fn ensure_grant_group() {
    let group = crate::authz::GRANT_GROUP;
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
        Ok(output) => eprintln!(
            "warning: could not create '{group}' group: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(e) => eprintln!("warning: could not create '{group}' group: {e}"),
    }
}

pub fn uninstall_helper() -> Result<(), String> {
    if !nix::unistd::Uid::effective().is_root() {
        return Err("--uninstall-helper must run as root".to_string());
    }

    let _ = launchctl_bootout_system(HELPER_PLIST_LABEL);
    let _ = fs::remove_file(HELPER_PLIST_PATH);
    let _ = fs::remove_file(HELPER_INSTALL_PATH);
    let _ = fs::remove_file(helper_ipc::HELPER_SOCKET_PATH);
    // Nobody can send Release to the removed helper; drop any holder state and
    // make sure the persistent SleepDisabled setting isn't left on.
    let _ = fs::remove_file(entirely::HELPER_LOCK_PATH);
    let _ = power_management::set_sleep_disabled(false, false);
    Ok(())
}

pub fn install_helper_privileged() -> Result<(), String> {
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
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("administrator authorization failed or was cancelled".to_string())
    }
}

pub fn install_tray_launch_agent(tray_path: &Path) -> Result<(), String> {
    let plist_path = tray_launch_agent_path()?;
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&plist_path, tray_launch_agent_plist(tray_path)).map_err(|e| e.to_string())?;
    // Don't bootstrap the agent now: RunAtLoad would immediately launch a
    // second tray instance next to the one the user is clicking in. launchd
    // picks up ~/Library/LaunchAgents plists at the next login.
    Ok(())
}

pub fn uninstall_tray_launch_agent() -> Result<(), String> {
    let plist_path = tray_launch_agent_path()?;
    // Only remove the plist; launchd won't start the agent at the next login.
    // Do NOT boot out the loaded service: when the tray was started by launchd
    // (the common case after enabling start-at-login), the current process *is*
    // that service, and bootout would terminate the running app the instant the
    // user unchecks the menu item.
    match fs::remove_file(&plist_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove {}: {e}", plist_path.display())),
    }
}

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

fn launchctl_bootstrap_system(plist_path: &str) -> Result<(), String> {
    if run_launchctl(&["bootstrap", "system", plist_path]).is_ok() {
        return Ok(());
    }
    run_launchctl(&["load", "-w", plist_path])
}

fn launchctl_bootout_system(label: &str) -> Result<(), String> {
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

fn run_launchctl(args: &[&str]) -> Result<(), String> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "launchctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ))
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
        assert_eq!(applescript_escape(r"'it'\''s'"), r#"'it'\\''s'"#);
        assert_eq!(applescript_escape("say \"hi\""), r#"say \"hi\""#);
    }

    #[test]
    fn tray_plist_substitutes_path() {
        let content = tray_launch_agent_plist(Path::new("/Users/test/.cargo/bin/caffeinate2-tray"));
        assert!(content.contains("/Users/test/.cargo/bin/caffeinate2-tray"));
        assert!(content.contains(TRAY_LAUNCH_AGENT_LABEL));
        assert!(!content.contains("__TRAY_PATH__"));
    }
}
