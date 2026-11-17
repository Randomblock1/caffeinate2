use crate::entirely::coordinator;
use crate::entirely::error::{HelperIpcErrorKind, InstallError};
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
pub const HELPER_PLIST_PATH: &str =
    "/Library/LaunchDaemons/com.randomblock1.caffeinate2.helper.plist";
pub const HELPER_PLIST_LABEL: &str = "com.randomblock1.caffeinate2.helper";
pub const TRAY_LAUNCH_AGENT_LABEL: &str = "com.randomblock1.caffeinate2-tray";

const HELPER_BINARY_HINT: &str = "install caffeinate2 with --features full or helper-bin, \
    use the GitHub release bundle, or place caffeinate2-helper in the same directory as caffeinate2";

const CLI_BINARY_HINT: &str = "install caffeinate2 with --features full, use the GitHub release \
    bundle, or place caffeinate2 in the same directory as caffeinate2-helper";

fn resolve_sibling_binary(binary_name: &str, build_hint: &str) -> Result<PathBuf, InstallError> {
    // Resolve symlinks first, so a binary reached through a symlinked bin
    // directory (Homebrew links into its Cellar) finds its real sibling, which
    // the helper copy then opens without following symlinks.
    let exe = running_binary()?;
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

/// The running executable with symlinks resolved.
fn running_binary() -> Result<PathBuf, InstallError> {
    let exe = std::env::current_exe().map_err(InstallError::from)?;
    fs::canonicalize(&exe)
        .map_err(|e| InstallError::msg(format!("could not resolve {}: {e}", exe.display())))
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
    ///
    /// Deliberately no StandardOut/ErrorPath: launchd opens such a file once
    /// and the daemon inherits the fd, so after newsyslog rotated it the
    /// writes went into the renamed archive forever and the fresh file stayed
    /// empty. The helper logs to the unified system log instead
    /// (`tracing_oslog`, subsystem com.randomblock1.caffeinate2.helper),
    /// which needs no rotation.
    process_type: String,
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

    // Validate and read the new binary before touching the installed one, so
    // a refused source leaves the working helper in place.
    let contents = read_helper_source(source_helper, running_binary_owner()?)?;

    // Reinstall path: stop any loaded helper before replacing its binary, and
    // unlink the old file so the copy gets a fresh inode. Overwriting a running
    // executable in place invalidates its code signature and the kernel kills
    // the process mid-write; bootstrap below would also no-op while the old
    // service is still loaded. bootout may fail on first install, so suppress it.
    let _ = launchctl_bootout_system(HELPER_PLIST_LABEL);
    // On a first install nothing answers and this returns on the first probe.
    // On a reinstall, an old daemon that survived the bootout keeps the
    // service loaded, so the bootstrap below silently no-ops and the old
    // binary keeps serving — warn so the apparent success isn't trusted.
    if !helper_stopped_answering() {
        tracing::warn!(
            "the old helper daemon still answers after launchctl bootout; the new binary may \
             not take over. If --status still reports a stale helper, run `sudo launchctl \
             bootout system/{HELPER_PLIST_LABEL}` and re-run --install-helper"
        );
    }
    match fs::remove_file(&dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("could not remove old helper binary: {e}"),
    }
    // Copy with O_NOFOLLOW|O_EXCL and pin root:wheel 0o755 on the open fd, so a
    // symlink planted at the destination cannot redirect the privileged write
    // and the installed binary is never momentarily owned or writable by a
    // non-root user.
    install_helper_binary(&contents, &dest)?;

    let plist = helper_plist_content(&dest);
    // launchctl refuses a system LaunchDaemon plist that is group/world-writable
    // (a permissive umask would otherwise make atomic_write produce 0666), so
    // write it with an explicit root-owned 0644.
    fs_util::atomic_write_with_mode(Path::new(HELPER_PLIST_PATH), plist.as_bytes(), 0o644)
        .map_err(InstallError::from)?;

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

/// Largest helper binary the installer will read. A release build is under
/// 2 MiB; the cap only bounds what a planted file can make root read.
const MAX_HELPER_BYTES: u64 = 64 * 1024 * 1024;

/// First four bytes of the Mach-O formats a helper build can produce: a thin
/// 64-bit image (little-endian `MH_MAGIC_64`) or a universal binary
/// (big-endian `FAT_MAGIC` / `FAT_MAGIC_64`).
const MACH_O_MAGICS: [[u8; 4]; 3] = [
    [0xcf, 0xfa, 0xed, 0xfe],
    [0xca, 0xfe, 0xba, 0xbe],
    [0xca, 0xfe, 0xba, 0xbf],
];

/// Read the helper binary the installer is about to copy into place as root.
///
/// The file sits in a user-writable directory, so its *content* cannot be
/// trusted here (see the README's Security section). What these checks stop is
/// the installer being turned into a root file reader: a planted symlink or
/// FIFO, or another account's file, cannot be copied out world-readable to
/// `/Library/PrivilegedHelperTools`. Requires a regular file, opened without
/// following a symlink, owned by root or by `expected_owner` (whoever owns the
/// running caffeinate2), at most [`MAX_HELPER_BYTES`], that starts with a
/// Mach-O header.
fn read_helper_source(source: &Path, expected_owner: u32) -> Result<Vec<u8>, InstallError> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let refuse = |why: String| {
        InstallError::msg(format!(
            "refusing to install helper from {}: {why}",
            source.display()
        ))
    };
    // O_NONBLOCK so a FIFO planted at the path can't hang the open; it is
    // then rejected as not a regular file. It changes nothing for a regular
    // file's reads.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags((nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_NONBLOCK).bits())
        .open(source)
        .map_err(|e| refuse(format!("could not open it without following symlinks: {e}")))?;
    let meta = file.metadata().map_err(InstallError::from)?;
    if !meta.file_type().is_file() {
        return Err(refuse("not a regular file".to_string()));
    }
    if meta.uid() != 0 && meta.uid() != expected_owner {
        return Err(refuse(format!(
            "owned by uid {}, expected root or uid {expected_owner} (the owner of the running \
             caffeinate2)",
            meta.uid()
        )));
    }
    if meta.len() > MAX_HELPER_BYTES {
        return Err(refuse(format!(
            "{} bytes is over the {MAX_HELPER_BYTES}-byte limit",
            meta.len()
        )));
    }
    let mut contents = Vec::new();
    // Bounded even if the file grows between the fstat and the read.
    file.take(MAX_HELPER_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(InstallError::from)?;
    if contents.len() as u64 > MAX_HELPER_BYTES {
        return Err(refuse(
            "it grew past the size limit while being read".to_string(),
        ));
    }
    if !MACH_O_MAGICS
        .iter()
        .any(|magic| contents.starts_with(magic))
    {
        return Err(refuse("not a Mach-O executable".to_string()));
    }
    Ok(contents)
}

/// Owner uid of the running binary, which the helper source must match unless
/// it is root-owned (see [`read_helper_source`]).
fn running_binary_owner() -> Result<u32, InstallError> {
    use std::os::unix::fs::MetadataExt;
    let exe = running_binary()?;
    Ok(fs::metadata(&exe).map_err(InstallError::from)?.uid())
}

/// Copy the helper binary to `dest` without following symlinks and pin it to
/// root:wheel 0o755 through the open fd, so a symlink planted at `dest` cannot
/// redirect the privileged write and the binary is never owned or writable by a
/// non-root user.
fn install_helper_binary(contents: &[u8], dest: &Path) -> Result<(), InstallError> {
    use nix::sys::stat::{Mode, fchmod};
    use nix::unistd::{Gid, Uid, fchown};
    use std::io::Write;
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt;

    // create_new => O_CREAT|O_EXCL; O_NOFOLLOW rejects a symlink at dest. The
    // caller unlinks any prior binary first, so O_EXCL gets a fresh inode.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(dest)
        .map_err(InstallError::from)?;
    file.write_all(contents).map_err(InstallError::from)?;
    file.sync_all().map_err(InstallError::from)?;
    fchown(file.as_fd(), Some(Uid::from_raw(0)), Some(Gid::from_raw(0)))
        .map_err(|e| InstallError::msg(format!("could not set helper owner: {e}")))?;
    fchmod(file.as_fd(), Mode::from_bits_truncate(0o755))
        .map_err(|e| InstallError::msg(format!("could not set helper mode: {e}")))?;
    Ok(())
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
    // A bootout "success" does not prove the daemon stopped (see
    // helper_stopped_answering). Removing the files under a daemon that kept
    // running would orphan an unreachable root process whose binary is gone —
    // and a later --install-helper's bootstrap would no-op against the
    // still-loaded service instead of starting the new binary. Refuse before
    // any file is removed and before sleep is re-enabled.
    if !helper_stopped_answering() {
        return Err(InstallError::msg(format!(
            "the helper daemon still answers after launchctl bootout; leaving its files in \
             place so a running root daemon is not orphaned. Stop it with `sudo launchctl \
             bootout system/{HELPER_PLIST_LABEL}` and re-run --uninstall-helper"
        )));
    }
    let _ = fs::remove_file(HELPER_PLIST_PATH);
    let _ = fs::remove_file(HELPER_INSTALL_PATH);
    let _ = fs::remove_file(helper_ipc::HELPER_SOCKET_PATH);

    // Once the helper is removed nobody can send Release, so re-enable sleep if
    // caffeinate2 is (or was) managing the SleepDisabled setting. That evidence
    // is the lockfile's durable ownership marker, never the file's mere
    // existence: the helper's startup reconcile creates the file, and it
    // persists in /var/db from then on, so an existence check is always true on
    // a helper machine and would clobber an unrelated manual
    // `pmset disablesleep`. (Holder entries need no separate check: an acquire
    // sets the marker in the same write, and it is cleared only with zero
    // holders, so holders imply the marker.)
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
            Ok(outcome) => outcome.owns_disable,
            Err(e) => {
                tracing::warn!("could not re-read helper lockfile during uninstall: {e}");
                // Fall back to the pre-bootout snapshot rather than skipping
                // the re-enable outright.
                lock_outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.owns_disable)
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

/// What [`install_helper_privileged`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    /// The running helper already matches the binary on disk byte for byte,
    /// so nothing was copied and no password was asked for.
    AlreadyCurrent,
}

/// Whether the installed helper is exactly what an install would produce:
/// the binary matches `wanted` byte for byte, the plist matches, and the
/// daemon answers with this binary's version. Everything read here is
/// world-readable, so the check needs no privileges.
///
/// Each install copies a user-writable file into a root daemon, so skipping
/// a reinstall that would change nothing removes one chance for a tampered
/// file to get in (and a needless password prompt).
fn helper_is_current(wanted: &[u8]) -> bool {
    let plist_current = fs::read_to_string(HELPER_PLIST_PATH)
        .is_ok_and(|plist| plist == helper_plist_content(Path::new(HELPER_INSTALL_PATH)));
    let binary_current = fs::read(HELPER_INSTALL_PATH).is_ok_and(|installed| installed == wanted);
    plist_current
        && binary_current
        && helper_ipc::HelperClient::new()
            .status()
            .is_ok_and(|status| !status.is_stale())
}

///
/// # Errors
///
/// Returns an error if privileged installation fails or is cancelled.
pub fn install_helper_privileged() -> Result<InstallOutcome, InstallError> {
    let source = resolve_helper_source()?;
    // Run the source checks here too, before any password prompt: a refused
    // source should say so up front, not after the user has authenticated.
    // Not a security boundary (the root install re-checks the file itself).
    let wanted = read_helper_source(&source, running_binary_owner()?)?;
    if helper_is_current(&wanted) {
        return Ok(InstallOutcome::AlreadyCurrent);
    }

    if nix::unistd::Uid::effective().is_root() {
        install_helper(&source)?;
        return Ok(InstallOutcome::Installed);
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
        Ok(InstallOutcome::Installed)
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

/// Wait for the helper daemon to stop listening on its socket after a bootout.
/// Returns `true` once it is gone, `false` if it is still there at the
/// deadline. Needed because bootout's exit status is not evidence the daemon
/// stopped: the legacy `launchctl unload` fallback exits 0 even on failure
/// (per launchctl(1), load/unload return non-zero only on improper usage). The
/// generous budget covers a daemon finishing an in-flight connection before
/// exiting; a dead daemon refuses the connect immediately, so the common case
/// returns on the first probe.
fn helper_stopped_answering() -> bool {
    helper_stopped_answering_within(
        &helper_ipc::HelperClient::new(),
        std::time::Duration::from_secs(5),
    )
}

fn helper_stopped_answering_within(
    client: &helper_ipc::HelperClient,
    budget: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        // Only a refused connect is evidence the listener is gone. Everything
        // else a Status probe can return comes from a live process the bootout
        // did not stop: an Error response (saturated connection slots, a
        // lockfile failure), a read timeout from a wedged daemon, or an
        // undecodable reply. Treating those as "stopped" would delete the
        // files under a running root daemon — the exact outcome this check
        // exists to prevent.
        match client.status() {
            Err(e) if e.kind() == HelperIpcErrorKind::Connect => return true,
            Ok(_) | Err(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
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
    fn probe_treats_refused_connect_as_stopped() {
        let client =
            helper_ipc::HelperClient::with_socket_path("/nonexistent/caffeinate2-test.sock");
        // Long budget on purpose: a refused connect must return on the first
        // probe, so the test hanging near this budget would itself be the bug.
        assert!(helper_stopped_answering_within(
            &client,
            std::time::Duration::from_secs(5)
        ));
    }

    #[test]
    fn probe_treats_an_error_response_as_still_alive() {
        // A daemon that ANSWERS — even with an error, as it does when its
        // connection slots are saturated — is alive, and uninstall must refuse.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("caffeinate2-probe-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind test socket");
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0_u8; 256];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    b"{\"kind\":\"error\",\"message\":\"too many concurrent clients\"}\n",
                );
            }
        });
        let client = helper_ipc::HelperClient::with_socket_path(&path.to_string_lossy());
        assert!(!helper_stopped_answering_within(
            &client,
            std::time::Duration::from_millis(300)
        ));
        drop(std::os::unix::net::UnixStream::connect(&path)); // unblock accept
        let _ = std::fs::remove_file(&path);
        drop(server); // detach; the thread dies with the test process
    }

    #[test]
    fn helper_plist_contains_path_and_keys() {
        let content = helper_plist_content(Path::new(HELPER_INSTALL_PATH));
        assert!(content.contains(HELPER_INSTALL_PATH));
        assert!(content.contains("<key>Label</key>"));
        assert!(content.contains(HELPER_PLIST_LABEL));
        assert!(content.contains("<key>ProcessType</key>"));
        assert!(content.contains("<string>Interactive</string>"));
        // No file redirection: the daemon's inherited fd would keep writing
        // into a rotated-away file (see HelperLaunchDaemon).
        assert!(!content.contains("StandardOutPath"));
        assert!(!content.contains("StandardErrorPath"));
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

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("caffeinate2_install_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn own_uid() -> u32 {
        nix::unistd::getuid().as_raw()
    }

    #[test]
    fn helper_source_accepts_an_owned_mach_o_binary() {
        // The test binary itself is a Mach-O owned by whoever built it.
        let exe = std::env::current_exe().unwrap();
        let contents = read_helper_source(&exe, own_uid()).unwrap();
        assert_eq!(contents, fs::read(&exe).unwrap());
    }

    #[test]
    fn helper_source_rejects_a_file_owned_by_someone_else() {
        let exe = std::env::current_exe().unwrap();
        let message = read_helper_source(&exe, own_uid() + 1)
            .unwrap_err()
            .to_string();
        assert!(message.contains("owned by uid"), "{message}");
    }

    #[test]
    fn helper_source_rejects_non_mach_o_content() {
        let dir = scratch_dir("text");
        let path = dir.join("caffeinate2-helper");
        fs::write(&path, "root-only secret\n").unwrap();
        let message = read_helper_source(&path, own_uid())
            .unwrap_err()
            .to_string();
        assert!(message.contains("not a Mach-O"), "{message}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn helper_source_rejects_a_symlink() {
        let dir = scratch_dir("link");
        let path = dir.join("caffeinate2-helper");
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &path).unwrap();
        let message = read_helper_source(&path, own_uid())
            .unwrap_err()
            .to_string();
        assert!(message.contains("without following symlinks"), "{message}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn helper_source_rejects_a_fifo_without_hanging() {
        let dir = scratch_dir("fifo");
        let path = dir.join("caffeinate2-helper");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let message = read_helper_source(&path, own_uid())
            .unwrap_err()
            .to_string();
        assert!(message.contains("not a regular file"), "{message}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn helper_binary_hint_mentions_packaging_options() {
        assert!(HELPER_BINARY_HINT.contains("--features full"));
        assert!(HELPER_BINARY_HINT.contains("GitHub release"));
        assert!(HELPER_BINARY_HINT.contains("caffeinate2-helper"));
    }
}
