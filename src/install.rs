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
    resolve_sibling_binary(
        "caffeinate2",
        "install caffeinate2 with --features full",
    )
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
    Ok(home.join("Library/LaunchAgents").join(format!(
        "{TRAY_LAUNCH_AGENT_LABEL}.plist"
    )))
}

pub fn install_helper(source_helper: &Path) -> Result<(), String> {
    if !nix::unistd::Uid::effective().is_root() {
        return Err("install-helper must run as root".to_string());
    }

    let dest = PathBuf::from(HELPER_INSTALL_PATH);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::copy(source_helper, &dest).map_err(|e| e.to_string())?;
    let mut perms = fs::metadata(&dest).map_err(|e| e.to_string())?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&dest, perms).map_err(|e| e.to_string())?;

    let plist = helper_plist_content(&dest);
    fs::write(HELPER_PLIST_PATH, plist).map_err(|e| e.to_string())?;

    launchctl_bootstrap_system(HELPER_PLIST_PATH)?;
    Ok(())
}

pub fn uninstall_helper() -> Result<(), String> {
    if !nix::unistd::Uid::effective().is_root() {
        return Err("uninstall-helper must run as root".to_string());
    }

    let _ = launchctl_bootout_system(HELPER_PLIST_LABEL);
    let _ = fs::remove_file(HELPER_PLIST_PATH);
    let _ = fs::remove_file(HELPER_INSTALL_PATH);
    let _ = fs::remove_file("/var/run/caffeinate2.sock");
    Ok(())
}

pub fn install_helper_privileged() -> Result<(), String> {
    if nix::unistd::Uid::effective().is_root() {
        let source = resolve_helper_source()?;
        return install_helper(&source);
    }

    let cli = resolve_cli_binary()?;
    let cli_escaped = shell_escape(&cli.display().to_string());
    let script = format!(
        "do shell script \"{cli_escaped} install-helper-internal\" with administrator privileges"
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

fn gui_launchctl_domain() -> String {
    format!("gui/{}", nix::unistd::getuid().as_raw())
}

pub fn install_tray_launch_agent(tray_path: &Path) -> Result<(), String> {
    let plist_path = tray_launch_agent_path()?;
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&plist_path, tray_launch_agent_plist(tray_path)).map_err(|e| e.to_string())?;

    let domain = gui_launchctl_domain();
    run_launchctl(&["bootstrap", &domain, &plist_path.display().to_string()])?;
    Ok(())
}

pub fn uninstall_tray_launch_agent() -> Result<(), String> {
    let plist_path = tray_launch_agent_path()?;
    let domain = gui_launchctl_domain();
    let _ = run_launchctl(&["bootout", &domain, TRAY_LAUNCH_AGENT_LABEL]);
    let _ = fs::remove_file(plist_path);
    Ok(())
}

pub fn tray_launch_agent_installed() -> bool {
    tray_launch_agent_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

fn shell_escape(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

fn launchctl_bootstrap_system(plist_path: &str) -> Result<(), String> {
    if run_launchctl(&["bootstrap", "system", plist_path]).is_ok() {
        return Ok(());
    }
    run_launchctl(&["load", "-w", plist_path])
}

fn launchctl_bootout_system(label: &str) -> Result<(), String> {
    if run_launchctl(&["bootout", "system", label]).is_ok() {
        return Ok(());
    }
    run_launchctl(&["unload", "-w", &format!("/Library/LaunchDaemons/{label}.plist")])
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
        let content = helper_plist_content(Path::new("/usr/local/libexec/caffeinate2/caffeinate2-helper"));
        assert!(content.contains("/usr/local/libexec/caffeinate2/caffeinate2-helper"));
        assert!(!content.contains("__HELPER_PATH__"));
    }

    #[test]
    fn tray_plist_substitutes_path() {
        let content = tray_launch_agent_plist(Path::new("/Users/test/.cargo/bin/caffeinate2-tray"));
        assert!(content.contains("/Users/test/.cargo/bin/caffeinate2-tray"));
        assert!(content.contains(TRAY_LAUNCH_AGENT_LABEL));
        assert!(!content.contains("__TRAY_PATH__"));
    }
}
