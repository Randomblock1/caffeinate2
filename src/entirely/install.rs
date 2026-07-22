use crate::entirely::coordinator;
use crate::entirely::error::InstallError;
use crate::entirely::helper_ipc;
use crate::entirely::lockfile;
use crate::entirely::process_util;
use crate::sleep::power_management;
use crate::util::fs_util;
use crate::util::shell_quote::sh_single_quote;
use libc::{S_IFDIR, S_IFMT};
use serde::Serialize;
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
// Pre-migration versions installed the root-owned helper here; nothing writes
// this path anymore, so install/uninstall clean it up rather than orphaning a
// stale privileged binary.
const LEGACY_HELPER_INSTALL_PATH: &str = "/usr/local/libexec/caffeinate2/caffeinate2-helper";
pub const HELPER_PLIST_PATH: &str =
    "/Library/LaunchDaemons/com.randomblock1.caffeinate2.helper.plist";
pub const HELPER_PLIST_LABEL: &str = "com.randomblock1.caffeinate2.helper";
pub const TRAY_LAUNCH_AGENT_LABEL: &str = "com.randomblock1.caffeinate2-tray";

const NEWSYSLOG_CONF_PATH: &str = "/etc/newsyslog.d/com.randomblock1.caffeinate2.helper.conf";

// Must match the path in resources/newsyslog/…helper.conf so newsyslog rotates
// the file the daemon writes.
const HELPER_LOG_PATH: &str = "/var/log/caffeinate2-helper.log";

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

// launchd LaunchDaemon definition for the root helper. serde serializes the
// fields to a plist `<dict>` in declaration order, so the field order below is
// the on-disk key order. Renames map each field to the exact launchd key.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct HelperLaunchDaemon {
    label: String,
    program_arguments: Vec<String>,
    run_at_load: bool,
    keep_alive: bool,
    /// `Interactive` keeps the daemon responsive to RPCs and exempt from the
    /// aggressive throttling applied to background ProcessTypes; it still runs
    /// as root with full privileges, so this does not weaken helper operation.
    process_type: String,
    standard_error_path: String,
    standard_out_path: String,
}

// launchd LaunchAgent definition for the per-user tray. Field order is the
// on-disk key order, as above.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct TrayLaunchAgent {
    label: String,
    program_arguments: Vec<String>,
    run_at_load: bool,
    keep_alive: bool,
}

// The plist XML writer escapes `<string>` values, so a path containing `&`,
// `<`, `>`, or quotes can't break out of or inject into the surrounding
// element. Serialization into a `Vec<u8>` is infallible.
fn to_plist_xml<T: Serialize>(value: &T) -> String {
    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, value).expect("plist serialization into a Vec cannot fail");
    String::from_utf8(buf).expect("plist XML writer emits UTF-8")
}

#[must_use]
pub fn helper_plist_content(helper_path: &Path) -> String {
    to_plist_xml(&HelperLaunchDaemon {
        label: HELPER_PLIST_LABEL.to_string(),
        program_arguments: vec![helper_path.display().to_string()],
        run_at_load: true,
        keep_alive: true,
        process_type: "Interactive".to_string(),
        standard_error_path: HELPER_LOG_PATH.to_string(),
        standard_out_path: HELPER_LOG_PATH.to_string(),
    })
}

#[must_use]
pub fn tray_launch_agent_plist(tray_path: &Path) -> String {
    to_plist_xml(&TrayLaunchAgent {
        label: TRAY_LAUNCH_AGENT_LABEL.to_string(),
        program_arguments: vec![tray_path.display().to_string()],
        run_at_load: true,
        keep_alive: false,
    })
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
    remove_legacy_helper();
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

/// Best-effort removal of the pre-migration helper binary (and the caffeinate2
/// directory that existed only to hold it). Errors are ignored: the path may be
/// absent, or the directory non-empty because something else was placed there.
///
/// Invariant: this runs as root, so it must never follow a symlink while
/// removing. The parent chain is opened component-by-component with
/// `O_DIRECTORY|O_NOFOLLOW` and the leaf file/directory are removed with
/// `unlinkat` relative to those dirfds, so a symlink swapped in for any
/// intermediate component — or for the leaf itself — cannot redirect a
/// privileged unlink outside the intended tree (the same discipline as
/// `ensure_secure_install_dir` and the `O_NOFOLLOW|O_EXCL` binary/plist writes).
/// A no-op when the path does not exist.
fn remove_legacy_helper() {
    use std::os::fd::AsFd;

    let legacy = Path::new(LEGACY_HELPER_INSTALL_PATH);
    // Remove the file relative to a dirfd on its parent directory, opened
    // without ever traversing a symlink.
    if let (Some(dir), Some(file_name)) = (legacy.parent(), legacy.file_name())
        && let Some(dir_fd) = open_dir_chain_nofollow(dir)
    {
        let _ = unlinkat_name(dir_fd.as_fd(), file_name, 0);
        // Then remove the now-empty directory relative to a dirfd on *its*
        // parent, again symlink-free. rmdir no-ops if the directory is missing
        // or non-empty.
        if let (Some(grandparent), Some(dir_name)) = (dir.parent(), dir.file_name())
            && let Some(parent_fd) = open_dir_chain_nofollow(grandparent)
        {
            let _ = unlinkat_name(parent_fd.as_fd(), dir_name, libc::AT_REMOVEDIR);
        }
    }
}

/// Open `dir` as a directory fd by walking its absolute path from the
/// filesystem root, opening each component with `O_DIRECTORY|O_NOFOLLOW` so no
/// intermediate symlink is ever traversed. Best effort: returns `None` if `dir`
/// is not absolute/normalized, or any component is missing, not a directory, or
/// a symlink.
fn open_dir_chain_nofollow(dir: &Path) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::AsFd;
    use std::path::Component;

    if !dir.is_absolute() {
        return None;
    }
    // Anchor the walk at "/": it is never a symlink, so O_NOFOLLOW opens it
    // safely and it becomes the dirfd for the first component.
    let root = std::ffi::CString::new("/").ok()?;
    let mut current = open_dir_at_nofollow(None, &root)?;
    for component in dir.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                use std::os::unix::ffi::OsStrExt;
                let c_name = std::ffi::CString::new(name.as_bytes()).ok()?;
                current = open_dir_at_nofollow(Some(current.as_fd()), &c_name)?;
            }
            // A normalized absolute path has no `.`/`..`/prefix components;
            // refuse to traverse them rather than walk outside the chain.
            _ => return None,
        }
    }
    Some(current)
}

/// `openat` a directory named `name` relative to `dirfd` (or the absolute path
/// `name` when `dirfd` is `None`), with `O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC`.
/// `None` on any failure.
fn open_dir_at_nofollow(
    dirfd: Option<std::os::fd::BorrowedFd<'_>>,
    name: &std::ffi::CStr,
) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let raw_dirfd = dirfd.map_or(libc::AT_FDCWD, |fd| fd.as_raw_fd());
    let flags = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_RDONLY;
    // SAFETY: `name` is a valid NUL-terminated C string and `raw_dirfd` is
    // either AT_FDCWD or a live borrowed fd. The returned fd is wrapped in an
    // OwnedFd so it is closed exactly once.
    let fd = unsafe { libc::openat(raw_dirfd, name.as_ptr(), flags) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is a fresh, valid, owned descriptor returned by openat.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `unlinkat(dirfd, name, flags)` — `flags` is `0` to remove a file or
/// `AT_REMOVEDIR` to remove a directory. Errors are returned for the caller to
/// ignore (best-effort legacy cleanup).
fn unlinkat_name(
    dirfd: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    flags: libc::c_int,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let c_name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `dirfd` is a live directory fd and `c_name` is NUL-terminated.
    let rc = unsafe { libc::unlinkat(dirfd.as_raw_fd(), c_name.as_ptr(), flags) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn ensure_grant_group() {
    let group = crate::entirely::authz::GRANT_GROUP;
    let exists = Command::new("/usr/sbin/dseditgroup")
        .args(["-o", "read", group])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if exists {
        return;
    }
    match Command::new("/usr/sbin/dseditgroup")
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

    // Inspect the lockfile before touching anything. Live holders are not
    // necessarily helper clients — a sudo CLI-fallback session records its
    // hold in this same lockfile and releases it in-process, never via the
    // helper — so re-enabling sleep and deleting the lockfile here would
    // silently yank the disable out from under a session that keeps running
    // believing sleep is prevented. Uninstalling with no live holders is
    // always safe, so refuse and let the user stop them first.
    let lock_path = Path::new(coordinator::HELPER_LOCK_PATH);
    let lock_outcome = lock_path
        .exists()
        .then(|| lockfile::prune_lockfile(false, lock_path, &process_util::default_process_checker))
        .and_then(|result| {
            result
                .map_err(|e| {
                    // Unreadable/invalid lockfile: no evidence caffeinate2 disabled
                    // sleep, so proceed but leave the setting untouched.
                    tracing::warn!("could not read helper lockfile during uninstall: {e}");
                })
                .ok()
        });
    if let Some(outcome) = &lock_outcome
        && outcome.live > 0
    {
        return Err(InstallError::msg(format!(
            "{} live entirely-mode session(s) still hold sleep disabled; stop them and re-run --uninstall-helper",
            outcome.live
        )));
    }

    let _ = launchctl_bootout_system(HELPER_PLIST_LABEL);
    let _ = fs::remove_file(HELPER_PLIST_PATH);
    let _ = fs::remove_file(NEWSYSLOG_CONF_PATH);
    let _ = fs::remove_file(HELPER_INSTALL_PATH);
    let _ = fs::remove_file(helper_ipc::HELPER_SOCKET_PATH);
    remove_legacy_helper();

    // Once the helper is removed nobody can send Release, so re-enable sleep if
    // caffeinate2 is (or was) managing the SleepDisabled setting. That evidence
    // is the lockfile's *contents*, never its mere existence: the helper's
    // startup reconcile creates the file on every boot, so an existence check
    // is always true on a helper machine and would clobber an unrelated manual
    // `pmset disablesleep`. The evidence mirrors `reconcile_startup`: the
    // durable ownership marker, or — for legacy pre-marker lockfiles — any
    // recorded holder entries, even ones just pruned as dead (this is the last
    // chance to converge; no future helper startup will run the legacy
    // fallback for us).
    //
    // Read the evidence from a FRESH prune, not the pre-bootout snapshot: the
    // helper kept serving between that snapshot and its death, so a hold could
    // have committed in the window. Live holders seen here (unlike in the
    // refusal above) also count as evidence — refusing is no longer possible,
    // the helper is gone, and their releases can only fail, so leaving
    // SleepDisabled on would strand it permanently.
    let caffeinate2_managed_sleep = lock_path.exists()
        && match lockfile::prune_lockfile(false, lock_path, &process_util::default_process_checker)
        {
            Ok(outcome) => outcome.owns_disable || outcome.had_entries,
            Err(e) => {
                tracing::warn!("could not re-read helper lockfile during uninstall: {e}");
                // Fall back to the pre-bootout snapshot rather than skipping
                // the re-enable outright.
                lock_outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.owns_disable || outcome.had_entries)
            }
        };
    if caffeinate2_managed_sleep {
        // Re-enabling sleep is the one uninstall step that must not fail
        // silently: reporting a successful uninstall while SleepDisabled stays
        // on would leave the machine permanently unable to sleep with no helper
        // left to fix it. Propagate the failure so the caller (and exit code)
        // reflect it — and attempt it BEFORE removing the lockfile, so a failed
        // re-enable leaves the evidence in place and a retried
        // --uninstall-helper attempts it again instead of reporting success.
        power_management::set_sleep_disabled(false, false).map_err(|code| {
            InstallError::msg(format!(
                "uninstalled helper but failed to re-enable system sleep (IOKit error {code:#x}); run `sudo pmset -a disablesleep 0` to restore it"
            ))
        })?;
    }
    let _ = fs::remove_file(coordinator::HELPER_LOCK_PATH);
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
    let status = Command::new("/usr/bin/osascript")
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
    // External tools are invoked by absolute path throughout this file: this
    // code runs privileged and macOS sudoers sets no secure_path, so resolving
    // via the inherited PATH would execute whatever the invoking user put
    // first on it.
    let output = Command::new("/bin/launchctl")
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
    fn helper_plist_contains_path_and_keys() {
        let content = helper_plist_content(Path::new(HELPER_INSTALL_PATH));
        assert!(content.contains(HELPER_INSTALL_PATH));
        assert!(content.contains("<key>Label</key>"));
        assert!(content.contains(HELPER_PLIST_LABEL));
        assert!(content.contains("<key>ProcessType</key>"));
        assert!(content.contains("<string>Interactive</string>"));
        assert!(content.contains(HELPER_LOG_PATH));
    }

    #[test]
    fn helper_plist_escapes_xml_metacharacters_in_path() {
        // Injection via the helper path must be structurally impossible: a
        // path with `&` and `<` is escaped, never emitted raw, and round-trips.
        let nasty = "/Users/a & b/<caffeinate2-helper>";
        let content = helper_plist_content(Path::new(nasty));
        assert!(content.contains("&amp;"));
        assert!(content.contains("&lt;"));
        assert!(!content.contains("a & b"));
        let parsed: plist::Value =
            plist::from_bytes(content.as_bytes()).expect("output is valid plist XML");
        let args = parsed
            .as_dictionary()
            .and_then(|d| d.get("ProgramArguments"))
            .and_then(plist::Value::as_array)
            .expect("ProgramArguments array");
        assert_eq!(args[0].as_string(), Some(nasty));
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
    fn applescript_escaping_survives_quotes_and_backslashes() {
        assert_eq!(applescript_escape(r"'it'\''s'"), r"'it'\\''s'");
        assert_eq!(applescript_escape("say \"hi\""), r#"say \"hi\""#);
    }

    #[test]
    fn tray_plist_contains_path_and_label() {
        let content = tray_launch_agent_plist(Path::new("/Users/test/.cargo/bin/caffeinate2-tray"));
        assert!(content.contains("/Users/test/.cargo/bin/caffeinate2-tray"));
        assert!(content.contains(TRAY_LAUNCH_AGENT_LABEL));
        assert!(content.contains("<key>KeepAlive</key>"));
        assert!(content.contains("<false/>"));
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
