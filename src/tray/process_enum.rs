//! Full process-tree enumeration for the "Wait for apps…" picker, plus the
//! running-target checks the watch loop uses.
//!
//! The picker can show every process (not just `NSRunningApplication`s), group
//! helper PIDs under their parent `.app`, and hide system processes by default.
//! Watch targets are keyed on something stable — a bundle id for apps, an
//! executable path for non-bundle programs — never a PID, which the OS recycles.

use crate::tray::app_target::WatchTarget;
use crate::tray::macos_apps;
use libproc::bsd_info::BSDInfo;
use libproc::proc_pid::{pidinfo, pidpath};
use libproc::processes::{ProcFilter, pids_by_type};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long to reuse a process-tree scan on the main thread before refreshing.
const PROC_SCAN_CACHE_TTL: Duration = Duration::from_secs(3);

/// Cached live-process executable paths. `raw` is the cheap `proc_pidpath`
/// scan; `canonical` is filled in lazily the first time a watch check has to
/// fall back to comparing realpaths (see [`exec_paths_match`]).
struct LiveExecPaths {
    stamp: Instant,
    raw: Vec<String>,
    canonical: Option<Vec<String>>,
}

static EXEC_PATH_CACHE: OnceLock<Mutex<Option<LiveExecPaths>>> = OnceLock::new();

fn exec_path_cache() -> &'static Mutex<Option<LiveExecPaths>> {
    EXEC_PATH_CACHE.get_or_init(|| Mutex::new(None))
}

/// A single running process, with just enough info to group and label it.
#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: i32,
    pub uid: u32,
    /// Full executable path (`proc_pidpath`); empty if it could not be read.
    pub exec_path: String,
    /// `pbi_name`/`pbi_comm` fallback label (truncated by the kernel).
    pub comm: String,
}

/// The owning `.app` of a process, resolved once per app path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRef {
    pub bundle_id: String,
    pub name: String,
    /// Outermost `.app` path, used for `NSWorkspace::iconForFile:`.
    pub app_path: String,
}

/// One process within a program group, for the picker's helper child rows.
#[derive(Debug, Clone)]
pub struct ChildProc {
    pub pid: i32,
    pub name: String,
}

/// One selectable row in the picker: a top-level program with every helper PID
/// collapsed under it.
#[derive(Debug, Clone)]
pub struct ProgramRow {
    pub name: String,
    pub bundle: Option<BundleRef>,
    pub is_system: bool,
    /// Path to fetch the row icon from (`.app` path for bundles, exec path
    /// otherwise).
    pub icon_path: String,
    /// The stable target persisted when this row is checked.
    pub target: WatchTarget,
    /// Every process in the group (the program itself plus any helpers).
    pub procs: Vec<ChildProc>,
}

/// All readable PIDs, or the enumeration error when the underlying
/// `pids_by_type` syscall fails. The picker path uses this so it can tell a
/// genuine "nothing running" from a failed scan.
fn all_pids_checked() -> std::io::Result<Vec<i32>> {
    pids_by_type(ProcFilter::All).map(|pids| {
        pids.into_iter()
            .filter(|&pid| pid > 0)
            .map(|pid| pid as i32)
            .collect()
    })
}

/// All readable PIDs, empty on a transient enumeration failure. The watch-loop
/// path uses this and tolerates the empty result by refusing to cache it (see
/// [`running_executable_paths_raw`]); the picker path instead goes through
/// [`all_pids_checked`] so the failure stays observable.
fn all_pids() -> Vec<i32> {
    all_pids_checked().unwrap_or_default()
}

/// Executable path for `pid`, or empty if `proc_pidpath` fails (kernel/protected
/// processes return EPERM).
fn proc_path(pid: i32) -> String {
    pidpath(pid).unwrap_or_default()
}

/// Read a NUL-terminated fixed C-char array (`pbi_name`/`pbi_comm`).
fn cstr_field(bytes: &[i8]) -> String {
    let bytes = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), bytes.len()) };
    let len = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

/// Every live process with pid/uid and its executable path, or the enumeration
/// error when the process list can't be read — so the picker can distinguish a
/// genuine empty result from a failed scan instead of silently rendering an
/// empty list. Drops PIDs whose `proc_pidinfo` fails (exited mid-scan, or
/// protected) — those are never watchable targets.
pub fn list_processes() -> std::io::Result<Vec<ProcInfo>> {
    let mut out = Vec::new();
    for pid in all_pids_checked()? {
        let info = match pidinfo::<BSDInfo>(pid, 0) {
            Ok(info) => info,
            Err(_) => continue,
        };
        out.push(ProcInfo {
            pid,
            uid: info.pbi_uid,
            exec_path: proc_path(pid),
            comm: cstr_field(&info.pbi_name),
        });
    }
    Ok(out)
}

/// Raw executable paths of all live processes (`proc_pidpath`, not realpath) —
/// the lightweight scan the watch loop uses to test `Executable` targets (no
/// `proc_pidinfo`, just paths). The result is cached so repeated main-thread
/// polls don't re-walk the process tree every tick; canonicalization is
/// deferred to [`canonical_live_paths`] and only paid when a target fails to
/// match a raw path.
fn running_executable_paths_raw() -> Vec<String> {
    let cache = exec_path_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = cache.as_ref()
        && entry.stamp.elapsed() < PROC_SCAN_CACHE_TTL
    {
        return entry.raw.clone();
    }
    drop(cache);

    let raw: Vec<String> = all_pids()
        .into_iter()
        .map(proc_path)
        .filter(|path| !path.is_empty())
        .collect();

    // Only cache a non-empty scan. `all_pids()` returns an empty Vec when the
    // underlying `pids_by_type` syscall transiently fails; caching that would
    // report every `Executable` watch target as not-running for the full TTL
    // and could prematurely end a watch session. Returning the empty result for
    // this one tick (without caching it) self-corrects on the next poll, which
    // matches the pre-caching behaviour.
    if !raw.is_empty() {
        *exec_path_cache().lock().unwrap_or_else(|e| e.into_inner()) = Some(LiveExecPaths {
            stamp: Instant::now(),
            raw: raw.clone(),
            canonical: None,
        });
    }
    raw
}

/// Realpath-canonicalized live executable paths, memoized on the current cache
/// entry so a watch check that has to compare realpaths (a symlinked live
/// process, or a not-yet-running target) canonicalizes the process table at
/// most once per [`PROC_SCAN_CACHE_TTL`] rather than every poll. Falls back to
/// canonicalizing `raw_live` directly when no fresh entry exists (an empty scan
/// is never cached).
fn canonical_live_paths(raw_live: &[String]) -> Vec<String> {
    let mut cache = exec_path_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = cache.as_mut()
        && entry.stamp.elapsed() < PROC_SCAN_CACHE_TTL
    {
        if entry.canonical.is_none() {
            entry.canonical = Some(entry.raw.iter().map(|p| canonical_exec_path(p)).collect());
        }
        return entry.canonical.clone().unwrap_or_default();
    }
    drop(cache);
    raw_live.iter().map(|p| canonical_exec_path(p)).collect()
}

/// Drop the cached process scan so the next [`running_executable_paths_raw`]
/// call does a fresh walk. Called when the watch set or enable state changes, where
/// a scan cached up to [`PROC_SCAN_CACHE_TTL`] ago could predate a just-launched
/// target and make the first "seen running" decision miss a short-lived process.
pub fn invalidate_exec_path_cache() {
    *exec_path_cache().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Canonical path for stable comparison when checking executable targets.
fn canonical_exec_path(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Outermost `.app` ancestor of an executable path, so an Electron helper at
/// `…/Slack.app/Contents/Frameworks/Slack Helper (GPU).app/…` groups under
/// `Slack.app` (the earliest `.app/` match), not the inner helper bundle.
fn outermost_app_path(exec_path: &str) -> Option<&str> {
    let idx = exec_path.find(".app/")?;
    Some(&exec_path[..idx + ".app".len()])
}

/// File-stem of a filesystem path — the label shown for a non-bundle program,
/// and the app-name fallback in [`macos_apps::bundle_from_app_path`].
pub(crate) fn file_stem(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
}

/// Resolve (and cache) the bundle for an `.app` path. `NSBundle` does disk I/O,
/// so each unique `.app` is read at most once per `cache` — one scan for the
/// picker, the whole process lifetime for the watcher's
/// [`HOLDER_BUNDLE_CACHE`].
fn bundle_for_app_path(
    app_path: &str,
    cache: &mut HashMap<String, Option<BundleRef>>,
) -> Option<BundleRef> {
    if let Some(cached) = cache.get(app_path) {
        return cached.clone();
    }
    let resolved = macos_apps::bundle_from_app_path(app_path).map(|app| BundleRef {
        bundle_id: app.bundle_id,
        name: app.name,
        app_path: app_path.to_string(),
    });
    cache.insert(app_path.to_string(), resolved.clone());
    resolved
}

/// Whether a process should be hidden by the default "hide system apps" filter.
/// Apple `.app`s (Finder, Dock, …) and OS daemons are system, as is any
/// root-owned process outside `/Applications` regardless of path (so a
/// root-run Homebrew daemon counts as system). Non-root Homebrew binaries under
/// `/usr/local` or `/opt` and user apps are not.
fn is_system_process(exec_path: &str, uid: u32, bundle: Option<&BundleRef>) -> bool {
    if let Some(bundle) = bundle {
        return bundle.bundle_id.starts_with("com.apple.");
    }
    exec_path.is_empty()
        || exec_path.starts_with("/System/")
        || exec_path.starts_with("/sbin/")
        || exec_path.starts_with("/bin/")
        || (exec_path.starts_with("/usr/") && !exec_path.starts_with("/usr/local/"))
        || exec_path.starts_with("/Library/Apple/")
        || (uid == 0 && !exec_path.starts_with("/Applications/"))
}

/// Bundles resolved for the upgrade watcher's per-PID system check, keyed by
/// `.app` path. `NSBundle` does disk I/O and the watcher re-checks the same
/// holders every poll, so each `.app` is read at most once per process
/// lifetime. Bounded by the number of distinct `.app`s that ever hold a sleep
/// assertion (a handful in practice).
static HOLDER_BUNDLE_CACHE: OnceLock<Mutex<HashMap<String, Option<BundleRef>>>> = OnceLock::new();

fn holder_bundle_cache() -> &'static Mutex<HashMap<String, Option<BundleRef>>> {
    HOLDER_BUNDLE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether the program behind `pid` belongs to the operating system rather than
/// to the user — the test the upgrade watcher applies to every sleep-assertion
/// holder, so it upgrades only programs the user runs.
///
/// A PID that can't be inspected (exited mid-poll, or a protected process whose
/// `proc_pidinfo` returns EPERM) counts as system: an assertion we cannot
/// attribute to one of the user's own programs is not one to take a stronger
/// hold for.
#[must_use]
pub fn is_system_pid(pid: i32) -> bool {
    let Ok(info) = pidinfo::<BSDInfo>(pid, 0) else {
        return true;
    };
    let exec_path = proc_path(pid);
    let bundle = outermost_app_path(&exec_path).and_then(|app_path| {
        let mut cache = holder_bundle_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        bundle_for_app_path(app_path, &mut cache)
    });
    is_system_holder(&exec_path, info.pbi_uid, current_uid(), bundle.as_ref())
}

/// Effective uid of this process — the "user" whose programs the watcher upgrades.
fn current_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

/// Where macOS keeps its own daemons and per-user agents — the background
/// processes that hold sleep assertions on the OS's behalf.
///
/// `/usr/bin` and `/bin` are deliberately absent, which is where this rule parts
/// company with [`is_system_process`]: the picker hides Apple's command-line
/// tools as noise, but a `caffeinate -i` (or `rsync`, or `ssh`) holding an
/// assertion from `/usr/bin` is the user asking for the Mac to stay awake, and
/// upgrading that is the whole point of the watcher. The list is exactly the one
/// the README documents under "What counts as a system program".
const SYSTEM_PROGRAM_DIRS: &[&str] = &[
    "/System/",
    "/usr/libexec/",
    "/usr/sbin/",
    "/sbin/",
    "/Library/Apple/",
];

/// The pure decision behind [`is_system_pid`]. A holder is the operating
/// system's rather than the user's when it runs as somebody else (daemons run as
/// root or as a service account), when it is one of Apple's own apps or agents,
/// or when its executable can't be read at all. Split out so it can be
/// unit-tested without live PIDs.
fn is_system_holder(
    exec_path: &str,
    uid: u32,
    current_uid: u32,
    bundle: Option<&BundleRef>,
) -> bool {
    if uid != current_uid {
        return true;
    }
    if let Some(bundle) = bundle {
        return bundle.bundle_id.starts_with("com.apple.");
    }
    exec_path.is_empty()
        || SYSTEM_PROGRAM_DIRS
            .iter()
            .any(|dir| exec_path.starts_with(dir))
}

/// Build the picker's program rows, grouping helper PIDs under their parent
/// `.app`. `app_only` keeps only `.app`-backed programs; `include_system`
/// reveals system processes (off by default). Returns the enumeration error
/// rather than an empty list when the process scan fails, so the picker can tell
/// the user to retry instead of silently showing nothing.
pub fn program_rows(app_only: bool, include_system: bool) -> std::io::Result<Vec<ProgramRow>> {
    let mut bundle_cache: HashMap<String, Option<BundleRef>> = HashMap::new();
    let mut groups: HashMap<String, ProgramRow> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for proc in list_processes()? {
        let bundle = outermost_app_path(&proc.exec_path)
            .and_then(|app_path| bundle_for_app_path(app_path, &mut bundle_cache));

        // Group by outermost-.app path (apps) or exec path (everything else).
        // The ppid chain is unreliable — helpers reparent to launchd — so the
        // shared .app path is what actually unites a program's processes.
        // A process with neither a bundle nor a readable path can't be watched.
        let group_key = match &bundle {
            Some(b) => b.app_path.clone(),
            None if proc.exec_path.is_empty() => continue,
            None => proc.exec_path.clone(),
        };

        let child = ChildProc {
            pid: proc.pid,
            name: child_name(&proc, bundle.as_ref()),
        };
        if let Some(row) = groups.get_mut(&group_key) {
            row.procs.push(child)
        } else {
            order.push(group_key.clone());
            groups.insert(group_key, new_row(&proc, bundle, child));
        }
    }

    let mut rows: Vec<ProgramRow> = order
        .into_iter()
        .filter_map(|key| groups.remove(&key))
        .filter(|row| (!app_only || row.bundle.is_some()) && (include_system || !row.is_system))
        .collect();
    rows.sort_by_key(|row| row.name.to_lowercase());
    Ok(rows)
}

/// Per-process label for a helper child row.
fn child_name(proc: &ProcInfo, bundle: Option<&BundleRef>) -> String {
    file_stem(&proc.exec_path).unwrap_or_else(|| {
        if !proc.comm.is_empty() {
            proc.comm.clone()
        } else if let Some(b) = bundle {
            b.name.clone()
        } else {
            format!("pid {}", proc.pid)
        }
    })
}

fn new_row(proc: &ProcInfo, bundle: Option<BundleRef>, first: ChildProc) -> ProgramRow {
    let is_system = is_system_process(&proc.exec_path, proc.uid, bundle.as_ref());
    let (name, icon_path, target) = bundle.as_ref().map_or_else(
        || {
            let name = file_stem(&proc.exec_path).unwrap_or_else(|| proc.comm.clone());
            (
                name.clone(),
                proc.exec_path.clone(),
                WatchTarget::Executable {
                    path: proc.exec_path.clone(),
                    name,
                },
            )
        },
        |b| {
            (
                b.name.clone(),
                b.app_path.clone(),
                WatchTarget::Bundle {
                    bundle_id: b.bundle_id.clone(),
                    name: b.name.clone(),
                },
            )
        },
    );
    ProgramRow {
        name,
        bundle,
        is_system,
        icon_path,
        target,
        procs: vec![first],
    }
}

/// Whether *any* of the targets is still running.
///
/// Bundle targets use the cheap `NSRunningApplication` lookup; only when an
/// `Executable` target is present do we do a single process-tree scan covering
/// all of them.
#[must_use]
pub fn any_target_running(targets: &[WatchTarget]) -> bool {
    let mut exec_paths: Vec<&str> = Vec::new();
    for target in targets {
        match target {
            WatchTarget::Bundle { bundle_id, .. } => {
                if macos_apps::is_bundle_running(bundle_id) {
                    return true;
                }
            }
            WatchTarget::Executable { path, .. } => exec_paths.push(path),
        }
    }
    if exec_paths.is_empty() {
        return false;
    }
    let raw_live = running_executable_paths_raw();
    exec_paths_match(&exec_paths, &raw_live, || canonical_live_paths(&raw_live))
}

/// Raw-first match of executable `targets` against the live process paths.
///
/// A target counts as running if its raw path — or its realpath, for a
/// symlinked target — equals a live process' raw path. Only when nothing
/// matches do we canonicalize the live paths (via `canon_live`, evaluated
/// lazily) to also catch a live process whose own path is a symlink resolving
/// to the target. This keeps the common case off `realpath` entirely while
/// preserving the symlinked-target-vs-symlinked-process matching the watch loop
/// relies on. Split out from the scan/cache so it is unit-testable.
fn exec_paths_match(
    targets: &[&str],
    raw_live: &[String],
    canon_live: impl FnOnce() -> Vec<String>,
) -> bool {
    let raw_set: HashSet<&str> = raw_live.iter().map(String::as_str).collect();
    let canon_targets: Vec<String> = targets
        .iter()
        .map(|path| canonical_exec_path(path))
        .collect();
    let raw_hit = targets
        .iter()
        .zip(&canon_targets)
        .any(|(raw, canon)| raw_set.contains(*raw) || raw_set.contains(canon.as_str()));
    if raw_hit {
        return true;
    }
    let canon_live = canon_live();
    let canon_set: HashSet<&str> = canon_live.iter().map(String::as_str).collect();
    canon_targets
        .iter()
        .any(|canon| canon_set.contains(canon.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outermost_app_path_picks_top_level_bundle() {
        assert_eq!(
            outermost_app_path(
                "/Applications/Slack.app/Contents/Frameworks/Slack Helper (GPU).app/Contents/MacOS/Slack Helper (GPU)"
            ),
            Some("/Applications/Slack.app")
        );
        assert_eq!(
            outermost_app_path("/Applications/Foo.app/Contents/MacOS/Foo"),
            Some("/Applications/Foo.app")
        );
        assert_eq!(outermost_app_path("/usr/local/bin/foo"), None);
        assert_eq!(outermost_app_path(""), None);
    }

    #[test]
    fn is_system_process_classifies_apple_and_os_paths() {
        let apple = BundleRef {
            bundle_id: "com.apple.finder".into(),
            name: "Finder".into(),
            app_path: "/System/Library/CoreServices/Finder.app".into(),
        };
        let third_party = BundleRef {
            bundle_id: "com.tinyspeck.slackmacgap".into(),
            name: "Slack".into(),
            app_path: "/Applications/Slack.app".into(),
        };
        assert!(is_system_process(
            "/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder",
            501,
            Some(&apple)
        ));
        assert!(!is_system_process(
            "/Applications/Slack.app/Contents/MacOS/Slack",
            501,
            Some(&third_party)
        ));
        // Non-bundle processes: OS paths and root daemons are system; Homebrew
        // and user binaries are not.
        assert!(is_system_process("/usr/libexec/secd", 0, None));
        assert!(is_system_process("/sbin/launchd", 0, None));
        assert!(!is_system_process("/usr/local/bin/node", 501, None));
        assert!(!is_system_process("/opt/homebrew/bin/python3", 501, None));
        // A root-owned process outside /Applications is system even under a
        // Homebrew prefix: the uid==0 clause overrides the path allowance.
        assert!(is_system_process("/usr/local/bin/node", 0, None));
    }

    #[test]
    fn is_system_holder_keeps_only_the_current_users_programs() {
        let user = 501;
        let third_party = BundleRef {
            bundle_id: "com.tinyspeck.slackmacgap".into(),
            name: "Slack".into(),
            app_path: "/Applications/Slack.app".into(),
        };
        let apple = BundleRef {
            bundle_id: "com.apple.Music".into(),
            name: "Music".into(),
            app_path: "/System/Applications/Music.app".into(),
        };
        // The user's own app or CLI tool is the only thing worth upgrading.
        assert!(!is_system_holder(
            "/Applications/Slack.app/Contents/MacOS/Slack",
            user,
            user,
            Some(&third_party)
        ));
        assert!(!is_system_holder(
            "/opt/homebrew/bin/node",
            user,
            user,
            None
        ));
        // Apple ships it, but the user ran it: `caffeinate -i` is exactly the
        // kind of hold the watcher exists to upgrade.
        assert!(!is_system_holder("/usr/bin/caffeinate", user, user, None));
        assert!(!is_system_holder("/usr/local/bin/agent", user, user, None));
        // Apple's own apps are system even when the user launched them.
        assert!(is_system_holder(
            "/System/Applications/Music.app/Contents/MacOS/Music",
            user,
            user,
            Some(&apple)
        ));
        // Daemons: root, or a service account, or a daemon/agent path even when
        // the agent runs as the user.
        assert!(is_system_holder("/usr/libexec/powerd", 0, user, None));
        assert!(is_system_holder("/usr/sbin/coreaudiod", 202, user, None));
        assert!(is_system_holder("/usr/libexec/rapportd", user, user, None));
        assert!(is_system_holder(
            "/System/Library/CoreServices/ReportCrash",
            user,
            user,
            None
        ));
        // Another human user's process is not this user's program either, even
        // from a path that would otherwise pass.
        assert!(is_system_holder("/opt/homebrew/bin/node", 502, user, None));
        // An unreadable executable path (protected process) is system.
        assert!(is_system_holder("", user, user, None));
    }

    #[test]
    fn file_stem_extracts_executable_name() {
        assert_eq!(file_stem("/usr/local/bin/foo").as_deref(), Some("foo"));
        assert_eq!(
            file_stem("/Applications/Foo.app/Contents/MacOS/Foo Helper (GPU)").as_deref(),
            Some("Foo Helper (GPU)")
        );
    }

    /// A temp real file plus a symlink to it, for the realpath-matching tests.
    /// Removed on drop.
    struct SymlinkEnv {
        dir: std::path::PathBuf,
        /// Raw (non-canonical) path of the symlink to the real file.
        link: String,
        /// Canonical path of the real file the symlink points at.
        real_canon: String,
    }

    impl SymlinkEnv {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("caffeinate2_exec_{}_{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let real = dir.join("real_bin");
            std::fs::write(&real, b"").unwrap();
            let link = dir.join("link_bin");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let real_canon = std::fs::canonicalize(&real)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            Self {
                dir,
                link: link.to_string_lossy().into_owned(),
                real_canon,
            }
        }
    }

    impl Drop for SymlinkEnv {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn exec_paths_match_hits_raw_path_without_canonicalizing_live() {
        // A target whose raw path is live matches directly; the live realpath
        // fallback must not run (the closure would panic).
        let raw_live = vec!["/opt/does-not-exist/daemon".to_string()];
        assert!(exec_paths_match(
            &["/opt/does-not-exist/daemon"],
            &raw_live,
            || panic!("raw match must not canonicalize the live set"),
        ));
    }

    #[test]
    fn exec_paths_match_misses_absent_target() {
        let raw_live = vec!["/opt/does-not-exist/daemon".to_string()];
        assert!(!exec_paths_match(
            &["/opt/does-not-exist/other"],
            &raw_live,
            || vec!["/opt/does-not-exist/daemon".to_string()],
        ));
    }

    #[test]
    fn exec_paths_match_resolves_symlinked_target() {
        let env = SymlinkEnv::new("target_symlink");
        // The live process reports the real path; the watch target was stored
        // as a symlink to it. Canonicalizing the handful of targets is enough
        // to match, so the live-set fallback must not run.
        let raw_live = vec![env.real_canon.clone()];
        assert!(exec_paths_match(
            &[env.link.as_str()],
            &raw_live,
            || panic!("canonical target match must not canonicalize the live set")
        ));
    }

    #[test]
    fn exec_paths_match_resolves_symlinked_live_process() {
        let env = SymlinkEnv::new("live_symlink");
        // The live process reports a symlink path; the target is the real path.
        // Neither the raw path nor the canonical target matches, so the fallback
        // must canonicalize the live set to resolve the symlink.
        let raw_live = vec![env.link.clone()];
        assert!(exec_paths_match(
            &[env.real_canon.as_str()],
            &raw_live,
            || raw_live.iter().map(|p| canonical_exec_path(p)).collect(),
        ));
    }
}
