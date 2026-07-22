use libc::{S_IFMT, S_IFREG};
use nix::fcntl::{Flock, FlockArg, OFlag};
use nix::sys::stat::{Mode, fchmod, fstat, lstat};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub(crate) const LOCK_FILE_MODE: u32 = 0o600;

/// Cap the lockfile we'll read into memory. Entries are tiny (a pid plus a
/// start time), so even thousands of live holders stay well under this; a file
/// larger than this is corrupt or hostile, so we refuse it rather than reading
/// it unbounded into a String.
const MAX_LOCKFILE_BYTES: u64 = 64 * 1024;

/// Sentinel line recording that caffeinate2 owns the current sleep disable —
/// i.e. it called `pmset disablesleep`/IOKit to disable sleep and has not yet
/// re-enabled it. It is written durably (under the same lock as the holder set)
/// so that intent survives a helper crash/restart even when the holder list has
/// been pruned to empty. Without it, an empty lockfile is ambiguous: it could
/// mean "we never disabled sleep" or "we disabled sleep but the last holder's
/// entry was already reaped", and treating the latter as the former strands the
/// machine with sleep disabled and no holders.
///
/// It deliberately does not parse as a `ProcessId` (which requires a positive
/// `pid:seconds:microseconds` triple), so older code that only understood
/// holder lines simply ignored it.
const DISABLE_MARKER: &str = "!disabled";

/// Prefix of the sentinel line persisting the ownership *generation*: a counter
/// bumped in the same locked write every time the ownership marker is set (a
/// first hold, or a reconcile re-recording ownership). A caller that decides to
/// re-enable sleep clears the marker only if the generation still matches the
/// one it captured with that decision — its kernel toggle happens outside the
/// flock, so another process sharing the lockfile may have set a *new* marker
/// in between, and clearing that would orphan the new disable. The counter is
/// persisted independently of the marker so it never regresses while the
/// lockfile exists: a stale observation can never match a recycled value. Like
/// the marker, the line does not parse as a `ProcessId`, so older versions
/// ignored it.
const GENERATION_PREFIX: &str = "!generation:";

/// The full parsed contents of the lockfile: the live holder set, whether
/// caffeinate2 owns the current sleep disable (see [`DISABLE_MARKER`]), and the
/// ownership generation (see [`GENERATION_PREFIX`]).
#[derive(Debug, Default)]
struct LockfileState {
    holders: HashSet<ProcessId>,
    owns_disable: bool,
    disable_generation: u64,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct ProcessStartTime {
    pub seconds: u64,
    pub microseconds: u64,
}

impl std::fmt::Display for ProcessStartTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.seconds, self.microseconds)
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct ProcessId {
    pub pid: i32,
    pub start_time: ProcessStartTime,
}

impl std::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.pid, self.start_time)
    }
}

impl std::str::FromStr for ProcessId {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.trim().split(':');
        let pid = parts.next().ok_or(())?.parse().map_err(|_| ())?;
        if pid <= 0 {
            return Err(());
        }
        let seconds = parts.next().ok_or(())?.parse().map_err(|_| ())?;
        let microseconds = parts.next().ok_or(())?.parse().map_err(|_| ())?;
        if microseconds >= 1_000_000 {
            return Err(());
        }
        if parts.next().is_some() {
            return Err(());
        }
        let start_time = ProcessStartTime {
            seconds,
            microseconds,
        };
        Ok(Self { pid, start_time })
    }
}

pub type ProcessChecker = dyn Fn(i32, ProcessStartTime) -> bool + Send + Sync;

fn open_validated_lockfile(path: &Path) -> Result<Flock<File>, std::io::Error> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(LOCK_FILE_MODE)
        .custom_flags(OFlag::O_NOFOLLOW.bits())
        .open(path)?;

    let file = match Flock::lock(file, FlockArg::LockExclusive) {
        Ok(f) => f,
        Err((_, e)) => return Err(std::io::Error::other(e)),
    };

    let file_stat = fstat(file.as_fd()).map_err(std::io::Error::other)?;
    if (file_stat.st_mode & S_IFMT) != S_IFREG {
        return Err(std::io::Error::other("Lockfile is not a regular file"));
    }

    let current_uid = nix::unistd::getuid().as_raw();
    if file_stat.st_uid != current_uid {
        return Err(std::io::Error::other(
            "Lockfile is not owned by current user",
        ));
    }

    let path_stat = lstat(path).map_err(|e| {
        std::io::Error::other(format!("Lockfile path disappeared during acquisition: {e}"))
    })?;
    if (path_stat.st_mode & S_IFMT) != S_IFREG
        || file_stat.st_dev != path_stat.st_dev
        || file_stat.st_ino != path_stat.st_ino
    {
        return Err(std::io::Error::other(
            "Lockfile was replaced during acquisition",
        ));
    }

    fchmod(
        file.as_fd(),
        Mode::from_bits_truncate(libc::mode_t::try_from(LOCK_FILE_MODE).unwrap_or(0)),
    )
    .map_err(std::io::Error::other)?;

    Ok(file)
}

fn read_state(file: &mut Flock<File>) -> Result<LockfileState, std::io::Error> {
    let size = file.seek(SeekFrom::End(0))?;
    if size > MAX_LOCKFILE_BYTES {
        return Err(std::io::Error::other(format!(
            "lockfile is {size} bytes, exceeding the {MAX_LOCKFILE_BYTES}-byte limit; refusing to read"
        )));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let mut state = LockfileState::default();
    for line in content.lines() {
        let line = line.trim();
        if line == DISABLE_MARKER {
            state.owns_disable = true;
        } else if let Some(generation) = line.strip_prefix(GENERATION_PREFIX) {
            if let Ok(generation) = generation.parse::<u64>() {
                state.disable_generation = generation;
            }
        } else if let Ok(pid) = line.parse::<ProcessId>() {
            state.holders.insert(pid);
        }
    }
    Ok(state)
}

fn write_state(file: &mut Flock<File>, state: &LockfileState) -> Result<(), std::io::Error> {
    let mut content = String::new();
    // The ownership marker leads the snapshot so that partial persistence of
    // only the leading bytes (a power loss mid-writeback) still records the
    // "we disabled sleep" fact — the conservative direction: at worst an extra
    // reconcile re-enable. The opposite direction — a released holder's line
    // surviving in the old tail and suppressing a re-enable — is closed by the
    // padded overwrite below.
    if state.owns_disable {
        content.push_str(DISABLE_MARKER);
        content.push('\n');
    }
    // The generation outlives the marker (see [`GENERATION_PREFIX`]): keep
    // persisting it after a clear so a later re-set can never recycle a value
    // a stale observer captured.
    if state.disable_generation > 0 {
        content.push_str(&format!(
            "{GENERATION_PREFIX}{}\n",
            state.disable_generation
        ));
    }
    for p in &state.holders {
        content.push_str(&format!("{p}\n"));
    }
    // In-place overwrite: the flock is pinned to this inode (see
    // open_validated_lockfile), so the atomic temp-file+rename idiom is not
    // available. A shrinking snapshot is instead padded with blank lines —
    // which read_state skips — out to the file's current length, so the single
    // write_all covers every previously-live byte. A writer killed before the
    // write leaves the old snapshot; killed anywhere after it leaves the
    // padded new one (the kernel completes an issued write even if the writer
    // dies). Neither state re-exposes a departed holder's line from an
    // un-truncated tail, where it would suppress a re-enable for as long as
    // that process lived. A power loss mid-writeback can persist any byte
    // subset, but every holder pid recorded before the loss is dead after
    // reboot and the next locked mutation prunes it.
    let current_len = file.seek(SeekFrom::End(0))?;
    let mut bytes = content.into_bytes();
    if (bytes.len() as u64) < current_len {
        // read_state has already capped the file at MAX_LOCKFILE_BYTES, so the
        // length fits in usize.
        bytes.resize(current_len as usize, b'\n');
    }
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&bytes)?;
    file.set_len(bytes.len() as u64)?;
    file.sync_all()?;
    Ok(())
}

/// Drop holder entries whose process is no longer alive (`pin` survives
/// regardless). Runs at the start of every locked mutation. Modeled by
/// `tests/protocol_model.rs` `prune`/`cross_prune`; keep the model in sync.
fn prune_stale_holders(
    pids: &mut HashSet<ProcessId>,
    verbose: bool,
    process_checker: &ProcessChecker,
    pin: Option<&ProcessId>,
) {
    pids.retain(|p| {
        if pin == Some(p) {
            return true;
        }
        if process_checker(p.pid, p.start_time) {
            true
        } else {
            if verbose {
                tracing::debug!(
                    "Removing stale process {}:{} from lockfile",
                    p.pid,
                    p.start_time
                );
            }
            false
        }
    });
}

fn mutate_lockfile<R>(
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
    pin: Option<&ProcessId>,
    mutate: impl FnOnce(&mut LockfileState) -> Result<R, std::io::Error>,
) -> Result<R, std::io::Error> {
    let mut file = open_validated_lockfile(path)?;
    let mut state = read_state(&mut file)?;
    prune_stale_holders(&mut state.holders, verbose, process_checker, pin);
    let result = mutate(&mut state)?;
    write_state(&mut file, &state)?;
    Ok(result)
}

/// Outcome of registering a holder under the exclusive lock.
#[derive(Debug)]
pub(crate) struct AcquireOutcome {
    /// This is the first holder, so the caller must disable sleep.
    pub first_holder: bool,
    /// Whether the ownership marker was already set *before* this acquire. A
    /// rollback of a failed disable must restore this value rather than force
    /// the marker off: a pre-existing marker (e.g. left by an earlier failed
    /// re-enable) records a real disable that reconcile still has to see.
    pub prior_owns_disable: bool,
    /// Ownership generation as of this acquire (post-bump when this is the
    /// first holder). A rollback of a failed disable passes it to
    /// [`clear_owns_disable_if_current`] so it can only clear the marker
    /// instance this acquire wrote.
    pub disable_generation: u64,
    /// Whether this acquire created the holder entry, as opposed to
    /// re-asserting an entry the same process already had. Lets a caller that
    /// must undo the acquire (e.g. the helper after a failed response write)
    /// remove exactly what it added and nothing more.
    pub newly_inserted: bool,
}

/// Register `current_proc` as a holder.
///
/// When it is the first holder, the ownership marker is set in the *same* write
/// that adds the holder — before the caller toggles sleep — so a crash between
/// this write and the actual disable still leaves durable evidence to reconcile
/// against (the safe direction).
///
/// Modeled by `tests/protocol_model.rs` (`Action::Hold` / `CrossAction::Hold`:
/// prune + insert + first-holder marker set and generation bump as one atomic
/// step); keep the model in sync with this write.
pub(crate) fn acquire(
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
    current_proc: &ProcessId,
) -> Result<AcquireOutcome, std::io::Error> {
    mutate_lockfile(
        verbose,
        path,
        process_checker,
        Some(current_proc),
        |state| {
            let first_holder = state.holders.is_empty();
            let prior_owns_disable = state.owns_disable;
            let newly_inserted = state.holders.insert(*current_proc);
            if first_holder {
                state.owns_disable = true;
                state.disable_generation += 1;
            }
            Ok(AcquireOutcome {
                first_holder,
                prior_owns_disable,
                disable_generation: state.disable_generation,
                newly_inserted,
            })
        },
    )
}

/// Outcome of removing a holder under the exclusive lock.
pub(crate) struct ReleaseOutcome {
    /// No live holders remain *and* caffeinate2 owns the disable, i.e. the
    /// caller must re-enable sleep.
    pub should_enable: bool,
    /// Whether `current_proc` actually held (its entry was present and
    /// removed). A rollback of a failed re-enable must only restore an entry
    /// that existed: re-adding a stray non-holder would fabricate a hold.
    pub removed: bool,
    /// Ownership generation observed by this decision, passed back to
    /// [`clear_owns_disable_if_current`] after the re-enable so a marker
    /// (re)set by another process since this write is never clobbered.
    pub disable_generation: u64,
}

/// Remove `current_proc` from the holder set.
///
/// The ownership marker is intentionally left set here: it is cleared only once
/// the caller confirms sleep was actually re-enabled (via
/// [`clear_owns_disable_if_current`]), so a crash between this write and the
/// re-enable is recoverable. Signalling on "no live holders remain && we own
/// the disable" — rather than only when this specific caller removed the last
/// live holder — means a stale entry pruned to empty by an unrelated Release
/// still triggers the re-enable instead of stranding sleep disabled. An empty
/// lockfile with no marker (e.g. a manual `pmset disablesleep`) never signals a
/// re-enable.
///
/// Modeled by `tests/protocol_model.rs` (`Action::Release` /
/// `CrossAction::Release`: the `holders.is_empty() && owns_disable` decision,
/// with `Variant::Buggy` keeping the pre-fix `removed && empty` shape); keep
/// the model in sync with this decision.
pub(crate) fn release(
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
    current_proc: &ProcessId,
) -> Result<ReleaseOutcome, std::io::Error> {
    mutate_lockfile(
        verbose,
        path,
        process_checker,
        Some(current_proc),
        |state| {
            let removed = state.holders.remove(current_proc);
            Ok(ReleaseOutcome {
                should_enable: state.holders.is_empty() && state.owns_disable,
                removed,
                disable_generation: state.disable_generation,
            })
        },
    )
}

/// Record the ownership marker without otherwise changing the holder set (dead
/// holders are still pruned, as with every locked mutation). Recording it stamps
/// a *new* ownership generation, invalidating any in-flight clear that captured
/// the previous one. Used to record ownership when a reconcile (re-)applies the
/// disable; clearing after a confirmed re-enable goes through
/// [`clear_owns_disable_if_current`] instead.
///
/// Modeled by `tests/protocol_model.rs` (the `ReconcileMarker`
/// `Effect::Disable` step; in the cross-process model the generation bump is
/// `invalidate_generations`); keep the model in sync.
pub(crate) fn record_owns_disable(
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
) -> Result<(), std::io::Error> {
    mutate_lockfile(verbose, path, process_checker, None, |state| {
        state.owns_disable = true;
        state.disable_generation += 1;
        Ok(())
    })
}

/// Clear the ownership marker **without** the generation guard that
/// [`clear_owns_disable_if_current`] enforces. This bypass is unsound in
/// production: after the caller's unlocked kernel re-enable, another process can
/// take a first hold or a reconcile can re-record ownership, and an
/// unconditional clear would strand that fresh disable with zero holders and no
/// marker. It exists only to construct marker-cleared lockfile states in tests,
/// so it is `#[cfg(test)]`-gated and must never become production code.
/// `tests/protocol_model.rs` model-checks exactly this unsoundness as
/// `MarkerWrite::Unconditional` in `CrossProcess::clear_marker`.
#[cfg(test)]
pub(crate) fn clear_owns_disable_unguarded(
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
) -> Result<(), std::io::Error> {
    mutate_lockfile(verbose, path, process_checker, None, |state| {
        state.owns_disable = false;
        Ok(())
    })
}

/// Clear the ownership marker only if no live holders remain *and* the
/// ownership generation still matches the one the caller captured with its
/// re-enable decision — both re-checked under the same exclusive lock as the
/// write.
///
/// An unconditional clear is not safe after the caller's unlocked kernel
/// toggle: another process sharing the lockfile (the CLI fallback has no single
/// daemon) can take a first hold — re-setting the marker — between the caller's
/// re-enable decision and this write, and a concurrent reconcile that re-applies
/// the disable re-records ownership without adding any holder at all. Either
/// write bumps the generation, so this clear backs off; clobbering the fresh
/// marker would leave the new disable with no evidence of ownership, stranding
/// sleep disabled with zero holders.
///
/// Modeled by `tests/protocol_model.rs` `CrossProcess::clear_marker`
/// (`MarkerWrite::Guarded`; the generation comparison is the `gen_valid`
/// token); keep the model in sync with this guard.
pub(crate) fn clear_owns_disable_if_current(
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
    observed_generation: u64,
) -> Result<(), std::io::Error> {
    mutate_lockfile(verbose, path, process_checker, None, |state| {
        if state.holders.is_empty() && state.disable_generation == observed_generation {
            state.owns_disable = false;
        }
        Ok(())
    })
}

/// Outcome of pruning the lockfile under an exclusive lock.
#[cfg(target_os = "macos")]
pub(crate) struct PruneOutcome {
    /// Live holders remaining after pruning dead/malformed entries.
    pub live: usize,
    /// Whether the lockfile contained any parseable holders before pruning.
    /// A legacy fallback (for lockfiles written before the ownership marker
    /// existed) distinguishing "we just pruned dead holders to zero" from "the
    /// lockfile was already empty". The `owns_disable` marker supersedes it for
    /// any lockfile this version wrote.
    pub had_entries: bool,
    /// Whether caffeinate2 owns the current sleep disable (the durable marker).
    pub owns_disable: bool,
    /// Ownership generation observed by this prune; a re-enable decided from
    /// this snapshot passes it to [`clear_owns_disable_if_current`].
    pub disable_generation: u64,
}

/// Prune stale lockfile entries under an exclusive lock and report the live
/// holder count, whether the file had any holders beforehand, and whether
/// caffeinate2 owns the current sleep disable.
///
/// Modeled by `tests/protocol_model.rs` as the atomic prune+decide start of
/// `StartReconcile`/`StartStartup` (and `CrossAction::Reconcile`/`Status`);
/// keep the model in sync.
#[cfg(target_os = "macos")]
pub(crate) fn prune_lockfile(
    verbose: bool,
    path: &Path,
    process_checker: &ProcessChecker,
) -> Result<PruneOutcome, std::io::Error> {
    let mut file = open_validated_lockfile(path)?;
    let mut state = read_state(&mut file)?;
    let had_entries = !state.holders.is_empty();
    prune_stale_holders(&mut state.holders, verbose, process_checker, None);
    let live = state.holders.len();
    let owns_disable = state.owns_disable;
    let disable_generation = state.disable_generation;
    write_state(&mut file, &state)?;
    Ok(PruneOutcome {
        live,
        had_entries,
        owns_disable,
        disable_generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_lock_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "caffeinate2_test_{}_{}.lock",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        path
    }

    fn proc(pid: i32, seconds: u64) -> ProcessId {
        ProcessId {
            pid,
            start_time: ProcessStartTime {
                seconds,
                microseconds: 0,
            },
        }
    }

    fn read_entries(path: &Path) -> Vec<ProcessId> {
        let mut entries = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            // Skip the ownership marker (and any other non-holder line).
            .filter_map(|line| line.trim().parse::<ProcessId>().ok())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.pid);
        entries
    }

    fn owns_disable(path: &Path) -> bool {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .any(|line| line.trim() == DISABLE_MARKER)
    }

    #[test]
    fn process_id_round_trips_with_microseconds() {
        let process_id = ProcessId {
            pid: 123,
            start_time: ProcessStartTime {
                seconds: 456,
                microseconds: 789,
            },
        };

        let serialized = process_id.to_string();
        assert_eq!(serialized, "123:456:789");
        assert_eq!(serialized.parse::<ProcessId>().unwrap(), process_id);
    }

    #[test]
    fn process_id_rejects_malformed_entries() {
        for entry in [
            "123",
            "123:456",
            "0:456:789",
            "-1:456:789",
            "123:456:1000000",
            "123:456:789:extra",
            "abc:456:789",
        ] {
            assert!(entry.parse::<ProcessId>().is_err(), "{entry:?}");
        }
    }

    #[test]
    fn first_instance_creates_secure_lockfile_and_requests_sleep_toggle() {
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        let outcome = acquire(false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(outcome.first_holder);
        assert!(!outcome.prior_owns_disable);
        assert_eq!(read_entries(&lock_path), vec![current_proc]);
        // Acquiring the first holder records ownership of the disable.
        assert!(owns_disable(&lock_path));
        let mode = std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, LOCK_FILE_MODE);

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn subsequent_instance_preserves_live_entries_without_toggling() {
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        let other_proc = proc(200, 456);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{other_proc}").unwrap();
        }

        let process_checker =
            |pid: i32, start_time: ProcessStartTime| pid == 200 && start_time.seconds == 456;

        let outcome = acquire(false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(!outcome.first_holder);
        assert_eq!(read_entries(&lock_path), vec![current_proc, other_proc]);

        let outcome = release(false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(!outcome.should_enable);
        assert!(outcome.removed);
        assert_eq!(read_entries(&lock_path), vec![other_proc]);

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn stale_and_malformed_entries_are_cleaned_up() {
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        let live_proc = proc(200, 456);
        let stale_proc = proc(300, 789);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{live_proc}").unwrap();
            writeln!(file, "{stale_proc}").unwrap();
            writeln!(file, "malformed").unwrap();
            writeln!(file, "0:1:0").unwrap();
            writeln!(file, "{live_proc}").unwrap();
        }

        let process_checker =
            |pid: i32, start_time: ProcessStartTime| pid == 200 && start_time.seconds == 456;

        let outcome = acquire(false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(!outcome.first_holder);
        assert_eq!(read_entries(&lock_path), vec![current_proc, live_proc]);

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn shrinking_write_pads_instead_of_exposing_a_stale_tail() {
        // Releasing one of two holders shrinks the serialized snapshot. The
        // in-place write must pad out to the previous length rather than rely
        // on truncation: a kill between write_all and set_len would otherwise
        // leave the departed holder's line alive in the un-truncated tail,
        // suppressing the eventual re-enable while that process lived.
        let lock_path = temp_lock_path();
        let keeper = proc(100, 123);
        let leaver = proc(200, 456);
        let process_checker = |pid: i32, _start_time: ProcessStartTime| pid == 100 || pid == 200;

        acquire(false, &lock_path, &process_checker, &keeper).unwrap();
        acquire(false, &lock_path, &process_checker, &leaver).unwrap();
        let two_holder_len = std::fs::metadata(&lock_path).unwrap().len();

        let outcome = release(false, &lock_path, &process_checker, &leaver).unwrap();

        assert!(outcome.removed);
        assert!(!outcome.should_enable);
        // Exactly one holder parses back: no byte of the file resurrects the
        // departed holder.
        assert_eq!(read_entries(&lock_path), vec![keeper]);
        // The file keeps its old length (blank-line padding), pinning that
        // write_all overwrote the whole previous snapshot.
        assert!(std::fs::metadata(&lock_path).unwrap().len() >= two_holder_len);

        // A later locked mutation reads the padded file cleanly: a repeat
        // release by the departed holder is a no-op, not an error.
        let outcome = release(false, &lock_path, &process_checker, &leaver).unwrap();
        assert!(!outcome.removed);
        assert_eq!(read_entries(&lock_path), vec![keeper]);

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn removing_last_entry_requests_sleep_toggle() {
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        {
            // A real lockfile with a live holder also carries the ownership
            // marker (written when that holder was acquired).
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{DISABLE_MARKER}").unwrap();
            writeln!(file, "{current_proc}").unwrap();
        }
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        let outcome = release(false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(outcome.should_enable);
        assert!(outcome.removed);
        assert!(read_entries(&lock_path).is_empty());
        // The marker is left set until the caller confirms the re-enable.
        assert!(owns_disable(&lock_path));

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn release_by_non_holder_does_not_request_sleep_toggle() {
        // A Release from a process that never held (empty lockfile) must not
        // report "last holder released": otherwise it would re-enable sleep and
        // clobber an unrelated manual `pmset disablesleep`.
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        let outcome = release(false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(!outcome.should_enable);
        assert!(!outcome.removed);
        let _ = std::fs::remove_file(&lock_path);
    }

    #[test]
    fn release_by_non_holder_with_live_others_does_not_toggle() {
        // Releasing a PID that isn't present while another live holder exists
        // must leave the live holder intact and not toggle sleep.
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        let other_proc = proc(200, 456);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{other_proc}").unwrap();
        }
        let process_checker =
            |pid: i32, start_time: ProcessStartTime| pid == 200 && start_time.seconds == 456;

        let outcome = release(false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(!outcome.should_enable);
        assert!(!outcome.removed);
        assert_eq!(read_entries(&lock_path), vec![other_proc]);
        let _ = std::fs::remove_file(&lock_path);
    }

    #[test]
    fn oversized_lockfile_is_rejected() {
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        // Write a file larger than the cap with otherwise-parseable lines.
        let line = format!("{}\n", proc(200, 456));
        let repeats = (MAX_LOCKFILE_BYTES as usize / line.len()) + 2;
        std::fs::write(&lock_path, line.repeat(repeats)).unwrap();
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| true;

        let result = acquire(false, &lock_path, &process_checker, &current_proc);

        assert!(result.is_err());
        let message = result.unwrap_err().to_string();
        assert!(message.contains("exceeding"), "{message}");

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn symlink_lockfile_is_rejected() {
        let target_path = temp_lock_path();
        let symlink_path = temp_lock_path();
        File::create(&target_path).unwrap();
        symlink(&target_path, &symlink_path).unwrap();
        let current_proc = proc(100, 123);
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        let result = acquire(false, &symlink_path, &process_checker, &current_proc);

        assert!(result.is_err());
        assert!(read_entries(&target_path).is_empty());

        std::fs::remove_file(&target_path).unwrap();
        std::fs::remove_file(&symlink_path).unwrap();
    }

    #[test]
    fn release_pruning_last_dead_holder_to_empty_toggles_when_owned() {
        // A holder was SIGKILLed (now dead) while we owned the disable, leaving a
        // stale entry. A Release from an unrelated process prunes that entry to
        // empty; because the marker says we own the disable, it must still signal
        // a re-enable rather than stranding sleep disabled.
        let lock_path = temp_lock_path();
        let dead_holder = proc(200, 456);
        let unrelated = proc(100, 123);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{DISABLE_MARKER}").unwrap();
            writeln!(file, "{dead_holder}").unwrap();
        }
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        let outcome = release(false, &lock_path, &process_checker, &unrelated).unwrap();

        assert!(outcome.should_enable);
        // The unrelated caller never held; only the pruning emptied the set.
        assert!(!outcome.removed);
        assert!(read_entries(&lock_path).is_empty());

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn release_pruning_to_empty_without_marker_does_not_toggle() {
        // Same prune-to-empty, but no ownership marker: this models a manual
        // `pmset disablesleep` (or a legacy/foreign entry) that caffeinate2 does
        // not own, so it must not re-enable sleep.
        let lock_path = temp_lock_path();
        let dead_holder = proc(200, 456);
        let unrelated = proc(100, 123);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{dead_holder}").unwrap();
        }
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        let outcome = release(false, &lock_path, &process_checker, &unrelated).unwrap();

        assert!(!outcome.should_enable);

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn clear_owns_disable_unguarded_clears_marker_without_touching_live_holders() {
        let lock_path = temp_lock_path();
        let holder = proc(100, 123);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{DISABLE_MARKER}").unwrap();
            writeln!(file, "{holder}").unwrap();
        }
        let process_checker =
            |pid: i32, start_time: ProcessStartTime| pid == 100 && start_time.seconds == 123;

        clear_owns_disable_unguarded(false, &lock_path, &process_checker).unwrap();

        assert!(!owns_disable(&lock_path));
        assert_eq!(read_entries(&lock_path), vec![holder]);

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn acquire_reports_preexisting_marker() {
        // A failed re-enable leaves the marker set with no holders; a new
        // acquire must report that the marker pre-existed so a rollback can
        // restore it instead of clearing it.
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        {
            let mut file = File::create(&lock_path).unwrap();
            writeln!(file, "{DISABLE_MARKER}").unwrap();
        }
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        let outcome = acquire(false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(outcome.first_holder);
        assert!(outcome.prior_owns_disable);

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn clear_owns_disable_if_current_clears_when_generation_matches() {
        // The normal release flow: acquire, release-to-empty, confirmed
        // re-enable, then the clear with the generation the release observed.
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        acquire(false, &lock_path, &process_checker, &current_proc).unwrap();
        let outcome = release(false, &lock_path, &process_checker, &current_proc).unwrap();
        assert!(outcome.should_enable);

        clear_owns_disable_if_current(
            false,
            &lock_path,
            &process_checker,
            outcome.disable_generation,
        )
        .unwrap();

        assert!(!owns_disable(&lock_path));

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn clear_owns_disable_if_current_keeps_marker_while_held() {
        // Models the cross-process race: a first hold (which re-set the marker)
        // landed between the caller's re-enable decision and this clear. The
        // marker belongs to that holder now and must survive — the new holder
        // fails both the empty-holders check and the generation check.
        let lock_path = temp_lock_path();
        let releaser = proc(100, 123);
        let holder = proc(200, 456);
        let process_checker =
            |pid: i32, start_time: ProcessStartTime| pid == 200 && start_time.seconds == 456;

        acquire(false, &lock_path, &process_checker, &releaser).unwrap();
        let outcome = release(false, &lock_path, &process_checker, &releaser).unwrap();
        assert!(outcome.should_enable);
        // The concurrent first hold lands before the releaser's clear.
        acquire(false, &lock_path, &process_checker, &holder).unwrap();

        clear_owns_disable_if_current(
            false,
            &lock_path,
            &process_checker,
            outcome.disable_generation,
        )
        .unwrap();

        assert!(owns_disable(&lock_path));
        assert_eq!(read_entries(&lock_path), vec![holder]);

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn clear_owns_disable_if_current_keeps_marker_on_stale_generation() {
        // Models the holder-less cross-process race: a concurrent reconcile
        // re-applied the disable and re-recorded ownership (bumping the
        // generation) after the caller's re-enable decision — no holder entry
        // involved, so only the generation check can save the marker.
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        acquire(false, &lock_path, &process_checker, &current_proc).unwrap();
        let outcome = release(false, &lock_path, &process_checker, &current_proc).unwrap();
        assert!(outcome.should_enable);
        // The concurrent reconcile's ownership re-record.
        record_owns_disable(false, &lock_path, &process_checker).unwrap();

        clear_owns_disable_if_current(
            false,
            &lock_path,
            &process_checker,
            outcome.disable_generation,
        )
        .unwrap();

        assert!(owns_disable(&lock_path));

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn generation_survives_marker_clear() {
        // The generation must be persisted past a marker clear: if it reset
        // with the marker, a later first hold could recycle a value a stale
        // observer captured and its clear would wrongly match (ABA).
        let lock_path = temp_lock_path();
        let current_proc = proc(100, 123);
        let process_checker = |_pid: i32, _start_time: ProcessStartTime| false;

        let first = acquire(false, &lock_path, &process_checker, &current_proc).unwrap();
        let outcome = release(false, &lock_path, &process_checker, &current_proc).unwrap();
        clear_owns_disable_if_current(
            false,
            &lock_path,
            &process_checker,
            outcome.disable_generation,
        )
        .unwrap();
        assert!(!owns_disable(&lock_path));

        let second = acquire(false, &lock_path, &process_checker, &current_proc).unwrap();

        assert!(second.disable_generation > first.disable_generation);

        std::fs::remove_file(&lock_path).unwrap();
    }
}
