use crate::entirely::coordinator;
use crate::entirely::error::InstallError;
use crate::entirely::helper_ipc;
use crate::sleep::power_management;
use crate::util::fs_util;
use libc::{S_IFDIR, S_IFMT};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

// /Library/PrivilegedHelperTools is the canonical location for root LaunchDaemon
// helpers. Unlike /usr/local (owned by the installing user on Homebrew-Intel), it
// and its parent /Library are root-owned and not user-writable by default on both
// Intel and Apple Silicon, so a non-root user cannot replace the daemon binary or
// redirect the path via a writable ancestor.
pub const HELPER_INSTALL_PATH: &str =
    "/Library/PrivilegedHelperTools/com.randomblock1.caffeinate2.helper";
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

/// Escape the five XML metacharacters so a path containing `&`, `<`, `>`, or
/// quotes can't break (or inject into) the surrounding plist `<string>`.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[must_use]
pub fn helper_plist_content(helper_path: &Path) -> String {
    HELPER_PLIST_TEMPLATE.replace(
        "__HELPER_PATH__",
        &xml_escape(&helper_path.display().to_string()),
    )
}

#[must_use]
pub fn tray_launch_agent_plist(tray_path: &Path) -> String {
    TRAY_PLIST_TEMPLATE.replace(
        "__TRAY_PATH__",
        &xml_escape(&tray_path.display().to_string()),
    )
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
    let parent = dest
        .parent()
        .ok_or_else(|| InstallError::msg("helper install path has no parent directory"))?;

    // Defense in depth around HELPER_INSTALL_PATH: if any ancestor of the install
    // directory were attacker-owned or group/world-writable, that user could
    // replace the root-owned daemon binary (or redirect the path via a symlinked
    // ancestor) and gain root code execution the next time launchd starts the
    // service. /Library/PrivilegedHelperTools is root-owned by default, but a
    // misconfigured system (or a custom HELPER_INSTALL_PATH) could still expose
    // this, so validate the whole chain is root-owned and non-writable by others
    // and create any missing components ourselves as root:wheel 0o755. launchd
    // then only ever executes a binary in a tree no non-root user controls.
    ensure_secure_install_dir(parent)?;

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
    // Copy with O_NOFOLLOW|O_EXCL and pin root:wheel 0o755 on the open fd, so a
    // symlink planted at the destination cannot redirect the privileged write
    // and the installed binary is never momentarily owned or writable by a
    // non-root user.
    install_helper_binary(source_helper, &dest)?;

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

/// Validate that every ancestor of `dir` (and `dir` itself) is a real directory
/// owned by root and not writable by group or other, creating any missing
/// components as root-owned 0o755. Refuses rather than proceeding when a
/// component is attacker-controllable: writing the root daemon binary beneath a
/// directory a non-root user can modify (or symlink away) is a local root
/// privilege-escalation vector on any user-writable prefix.
fn ensure_secure_install_dir(dir: &Path) -> Result<(), InstallError> {
    use std::path::Component;

    if !dir.is_absolute() {
        return Err(InstallError::msg(format!(
            "refusing to install: helper directory {} is not an absolute path",
            dir.display()
        )));
    }

    // Walk top-down from the filesystem root. Each component is validated (or
    // created) only after its parent has been confirmed root-owned and
    // non-writable by others, which closes the TOCTOU window: a non-root user
    // cannot create or symlink-swap an entry inside a directory they can't write.
    let mut current = PathBuf::from("/");
    verify_secure_existing_dir(&current)?;
    for component in dir.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current.push(name);
                match nix::sys::stat::lstat(&current) {
                    Ok(st) => verify_secure_dir_stat(&current, st.st_mode, st.st_uid)?,
                    Err(nix::errno::Errno::ENOENT) => create_secure_dir(&current)?,
                    Err(e) => {
                        return Err(InstallError::msg(format!(
                            "could not inspect {}: {e}",
                            current.display()
                        )));
                    }
                }
            }
            // `.`/`..`/prefix shouldn't appear in the normalized absolute
            // constant; reject rather than silently traversing them.
            _ => {
                return Err(InstallError::msg(format!(
                    "refusing to install: helper directory {} is not normalized",
                    dir.display()
                )));
            }
        }
    }
    Ok(())
}

fn verify_secure_existing_dir(path: &Path) -> Result<(), InstallError> {
    let st = nix::sys::stat::lstat(path)
        .map_err(|e| InstallError::msg(format!("could not inspect {}: {e}", path.display())))?;
    verify_secure_dir_stat(path, st.st_mode, st.st_uid)
}

fn verify_secure_dir_stat(
    path: &Path,
    st_mode: libc::mode_t,
    st_uid: u32,
) -> Result<(), InstallError> {
    if dir_stat_is_secure(st_mode, st_uid) {
        return Ok(());
    }
    Err(InstallError::msg(format!(
        "refusing to install helper: {} must be a directory owned by root and not writable by \
         other users (found uid {st_uid}, mode {:o}); a non-root-owned or world/group-writable \
         ancestor would let a non-root user replace the root daemon binary. Fix the \
         ownership/permissions of that path before installing",
        path.display(),
        st_mode & 0o7777,
    )))
}

/// A directory is safe to host the root daemon only when it is a real directory
/// (not a symlink), owned by root, and not writable by group or other.
fn dir_stat_is_secure(st_mode: libc::mode_t, st_uid: u32) -> bool {
    (st_mode & S_IFMT) == S_IFDIR && st_uid == 0 && (st_mode & 0o022) == 0
}

/// Create `path` as a root-owned 0o755 directory. The caller has already
/// verified the parent is root-owned and non-writable by others, so no non-root
/// user can pre-create or symlink-swap this component before we secure it.
fn create_secure_dir(path: &Path) -> Result<(), InstallError> {
    use std::os::unix::fs::DirBuilderExt;
    // mode(0o755) guarantees the directory is never group/world-writable, even
    // for an instant (umask can only tighten it); the chown and set_permissions
    // below pin the exact owner and bits regardless of umask.
    fs::DirBuilder::new()
        .mode(0o755)
        .create(path)
        .map_err(InstallError::from)?;
    std::os::unix::fs::chown(path, Some(0), Some(0)).map_err(InstallError::from)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(InstallError::from)?;
    Ok(())
}

/// Copy the helper binary to `dest` without following symlinks and pin it to
/// root:wheel 0o755 through the open fd, so a symlink planted at `dest` cannot
/// redirect the privileged write and the binary is never owned or writable by a
/// non-root user.
fn install_helper_binary(source: &Path, dest: &Path) -> Result<(), InstallError> {
    use nix::sys::stat::{Mode, fchmod};
    use nix::unistd::{Gid, Uid, fchown};
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt;

    let mut contents = Vec::new();
    fs::File::open(source)
        .map_err(InstallError::from)?
        .read_to_end(&mut contents)
        .map_err(InstallError::from)?;

    // create_new => O_CREAT|O_EXCL; O_NOFOLLOW rejects a symlink at dest. The
    // caller unlinks any prior binary first, so O_EXCL gets a fresh inode.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(dest)
        .map_err(InstallError::from)?;
    file.write_all(&contents).map_err(InstallError::from)?;
    file.sync_all().map_err(InstallError::from)?;
    fchown(file.as_fd(), Some(Uid::from_raw(0)), Some(Gid::from_raw(0)))
        .map_err(|e| InstallError::msg(format!("could not set helper owner: {e}")))?;
    fchmod(file.as_fd(), Mode::from_bits_truncate(0o755))
        .map_err(|e| InstallError::msg(format!("could not set helper mode: {e}")))?;
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
    if let Err(e) = fs_util::atomic_write_with_mode(path, NEWSYSLOG_CONF_TEMPLATE.as_bytes(), 0o644)
    {
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
        // Re-enabling sleep is the one uninstall step that must not fail
        // silently: reporting a successful uninstall while SleepDisabled stays
        // on would leave the machine permanently unable to sleep with no helper
        // left to fix it. Propagate the failure so the caller (and exit code)
        // reflect it.
        power_management::set_sleep_disabled(false, false).map_err(|code| {
            InstallError::msg(format!(
                "uninstalled helper but failed to re-enable system sleep (IOKit error {code:#x}); run `sudo pmset -a disablesleep 0` to restore it"
            ))
        })?;
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
    // Pin the mode like the helper plist instead of inheriting the caller's
    // umask: a permissive umask would otherwise leave the LaunchAgent plist
    // group/world-writable.
    fs_util::atomic_write_with_mode(&plist_path, plist.as_bytes(), 0o644)
        .map_err(InstallError::from)?;
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
        let content = helper_plist_content(Path::new(HELPER_INSTALL_PATH));
        assert!(content.contains(HELPER_INSTALL_PATH));
        assert!(!content.contains("__HELPER_PATH__"));
    }

    #[test]
    fn helper_install_path_is_root_owned_prefix() {
        // The install path must live under a directory tree that is root-owned
        // and not user-writable by default; /usr/local is user-owned on
        // Homebrew-Intel and must not be used.
        assert!(HELPER_INSTALL_PATH.starts_with("/Library/PrivilegedHelperTools/"));
        assert!(!HELPER_INSTALL_PATH.starts_with("/usr/local/"));
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
    fn dir_stat_is_secure_requires_root_owned_nonwritable_directory() {
        let dir = S_IFDIR;
        // Root-owned, not group/world-writable directory: safe.
        assert!(dir_stat_is_secure(dir | 0o755, 0));
        assert!(dir_stat_is_secure(dir | 0o700, 0));
        // Owned by a non-root user (e.g. Homebrew-Intel /usr/local): unsafe.
        assert!(!dir_stat_is_secure(dir | 0o755, 501));
        // Group- or world-writable, even when root-owned: unsafe.
        assert!(!dir_stat_is_secure(dir | 0o775, 0));
        assert!(!dir_stat_is_secure(dir | 0o757, 0));
        assert!(!dir_stat_is_secure(dir | 0o777, 0));
        // A symlink (or any non-directory) is never acceptable.
        assert!(!dir_stat_is_secure(libc::S_IFLNK | 0o755, 0));
        assert!(!dir_stat_is_secure(libc::S_IFREG | 0o755, 0));
    }

    #[test]
    fn ensure_secure_install_dir_rejects_relative_path() {
        let result = ensure_secure_install_dir(Path::new("usr/local/libexec/caffeinate2"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("absolute path"));
    }

    #[test]
    fn helper_binary_hint_mentions_packaging_options() {
        assert!(HELPER_BINARY_HINT.contains("--features full"));
        assert!(HELPER_BINARY_HINT.contains("GitHub release"));
        assert!(HELPER_BINARY_HINT.contains("caffeinate2-helper"));
    }
}
