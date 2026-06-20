//! Full process-tree enumeration for the "Wait for apps…" picker, plus the
//! running-target checks the watch loop uses.
//!
//! The picker can show every process (not just `NSRunningApplication`s), group
//! helper PIDs under their parent `.app`, and hide system processes by default.
//! Watch targets are keyed on something stable — a bundle id for apps, an
//! executable path for non-bundle programs — never a PID, which the OS recycles.

use crate::app_target::WatchTarget;
use crate::macos_apps;
use libc::{PROC_PIDTBSDINFO, proc_bsdinfo, proc_pidinfo};
use std::collections::HashMap;

/// A single running process, with just enough info to group and label it.
#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
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

/// All readable PIDs. The set can change between sizing and fill, so we oversize
/// the buffer and trust the returned PID count (a few transient PIDs are
/// harmless).
fn all_pids() -> Vec<i32> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    let cap = count as usize + 64;
    let mut pids = vec![0i32; cap];
    let count = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (cap * std::mem::size_of::<i32>()) as libc::c_int,
        )
    };
    if count <= 0 {
        return Vec::new();
    }
    pids.truncate(count as usize);
    pids.retain(|&pid| pid > 0);
    pids
}

/// Executable path for `pid`, or empty if `proc_pidpath` fails (kernel/protected
/// processes return EPERM).
fn proc_path(pid: i32) -> String {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf8_lossy(&buf[..len as usize]).into_owned()
}

/// Read a NUL-terminated fixed C-char array (`pbi_name`/`pbi_comm`).
fn cstr_field(bytes: &[libc::c_char]) -> String {
    let raw: Vec<u8> = bytes
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&raw).into_owned()
}

/// Every live process with pid/ppid/uid and its executable path. Drops PIDs
/// whose `proc_pidinfo` fails (exited mid-scan, or protected) — those are never
/// watchable targets.
pub fn list_processes() -> Vec<ProcInfo> {
    let mut out = Vec::new();
    for pid in all_pids() {
        let mut info = unsafe { std::mem::zeroed::<proc_bsdinfo>() };
        let size = std::mem::size_of::<proc_bsdinfo>() as libc::c_int;
        let ret = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut _,
                size,
            )
        };
        if ret != size {
            continue;
        }
        out.push(ProcInfo {
            pid,
            ppid: info.pbi_ppid as i32,
            uid: info.pbi_uid,
            exec_path: proc_path(pid),
            comm: cstr_field(&info.pbi_name),
        });
    }
    out
}

/// Executable paths of all live processes — the lightweight scan the watch loop
/// uses to test `Executable` targets (no `proc_pidinfo`, just paths).
fn running_executable_paths() -> Vec<String> {
    all_pids()
        .into_iter()
        .map(proc_path)
        .filter(|path| !path.is_empty())
        .collect()
}

/// Outermost `.app` ancestor of an executable path, so an Electron helper at
/// `…/Slack.app/Contents/Frameworks/Slack Helper (GPU).app/…` groups under
/// `Slack.app` (the earliest `.app/` match), not the inner helper bundle.
fn outermost_app_path(exec_path: &str) -> Option<&str> {
    let idx = exec_path.find(".app/")?;
    Some(&exec_path[..idx + ".app".len()])
}

/// File-stem label for a non-bundle executable path.
fn file_stem(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
}

/// Resolve (and cache) the bundle for an `.app` path. `NSBundle` does disk I/O,
/// so each unique `.app` is read at most once per scan.
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
/// Apple `.app`s (Finder, Dock, …) and OS daemons are system; Homebrew binaries
/// under `/usr/local` or `/opt` and user apps are not.
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

/// Build the picker's program rows, grouping helper PIDs under their parent
/// `.app`. `app_only` keeps only `.app`-backed programs; `include_system`
/// reveals system processes (off by default).
pub fn program_rows(app_only: bool, include_system: bool) -> Vec<ProgramRow> {
    let mut bundle_cache: HashMap<String, Option<BundleRef>> = HashMap::new();
    let mut groups: HashMap<String, ProgramRow> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for proc in list_processes() {
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
        match groups.get_mut(&group_key) {
            Some(row) => row.procs.push(child),
            None => {
                order.push(group_key.clone());
                groups.insert(group_key, new_row(&proc, bundle, child));
            }
        }
    }

    let mut rows: Vec<ProgramRow> = order
        .into_iter()
        .filter_map(|key| groups.remove(&key))
        .filter(|row| (!app_only || row.bundle.is_some()) && (include_system || !row.is_system))
        .collect();
    rows.sort_by_key(|row| row.name.to_lowercase());
    rows
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
    let (name, icon_path, target) = match &bundle {
        Some(b) => (
            b.name.clone(),
            b.app_path.clone(),
            WatchTarget::Bundle {
                bundle_id: b.bundle_id.clone(),
                name: b.name.clone(),
            },
        ),
        None => {
            let name = file_stem(&proc.exec_path).unwrap_or_else(|| proc.comm.clone());
            (
                name.clone(),
                proc.exec_path.clone(),
                WatchTarget::Executable {
                    path: proc.exec_path.clone(),
                    name,
                },
            )
        }
    };
    ProgramRow {
        name,
        bundle,
        is_system,
        icon_path,
        target,
        procs: vec![first],
    }
}

/// Whether a single watch target still has a live process.
pub fn target_running(target: &WatchTarget) -> bool {
    any_target_running(std::slice::from_ref(target))
}

/// Whether *any* of the targets is still running. Bundle targets use the cheap
/// `NSRunningApplication` lookup; only when an `Executable` target is present do
/// we do a single process-tree scan covering all of them.
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
    let running = running_executable_paths();
    exec_paths
        .iter()
        .any(|path| running.iter().any(|live| live == path))
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
    }

    #[test]
    fn file_stem_extracts_executable_name() {
        assert_eq!(file_stem("/usr/local/bin/foo").as_deref(), Some("foo"));
        assert_eq!(
            file_stem("/Applications/Foo.app/Contents/MacOS/Foo Helper (GPU)").as_deref(),
            Some("Foo Helper (GPU)")
        );
    }
}
