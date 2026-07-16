//! Model-based (property) tests for the entirely-mode sleep coordinator.
//!
//! The coordinator's real nondeterminism is cross-process and crash-driven:
//! holder processes get SIGKILLed at arbitrary points, the helper itself can
//! crash between writing the lockfile and toggling the kernel's `SleepDisabled`
//! bit, and external actors (`pmset`, a sleep/wake cycle) can flip that bit
//! underneath us. These tests drive the *real* `EntirelyCoordinator` through
//! randomized sequences of those events against a simulated kernel bit and
//! process table, checking the convergence invariants the module's comments
//! reason about informally.
//!
//! Two injectable seams make this possible without touching real IOKit or the
//! OS process table: `SleepDisabler` (writes the kernel bit) and
//! `ProcessChecker` (decides which pids are alive). The lockfile is real, on a
//! per-case temp path. The disabler can also be armed to fail its next toggle
//! (an IOKit error), composing the hold/release/reconcile rollback paths with
//! kills, restarts, and reconciles under the same oracle.
//!
//! The oracle is deliberately independent of the coordinator's implementation:
//! it tracks only the set of processes that are currently holding and alive, and
//! asserts the kernel bit converges to `holders > 0` after a reconcile. If the
//! oracle and the code ever disagree, one of them is wrong — that is the signal.

#![cfg(target_os = "macos")]

use caffeinate2::entirely::coordinator::{EntirelyCoordinator, SleepDisabler};
use caffeinate2::entirely::lockfile::{ProcessChecker, ProcessId, ProcessStartTime};
use proptest::prelude::*;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Candidate holder identities. `(1,10)` and `(1,20)` share pid 1 with distinct
/// start times, exercising the pid-reuse guard (a recycled pid must not be
/// mistaken for the original holder).
fn pool() -> Vec<ProcessId> {
    [(1, 10), (2, 10), (3, 10), (1, 20)]
        .into_iter()
        .map(|(pid, seconds)| ProcessId {
            pid,
            start_time: ProcessStartTime {
                seconds,
                microseconds: 0,
            },
        })
        .collect()
}
const POOL_LEN: usize = 4;

/// The simulated outside world shared between the injected seams: the kernel's
/// `SleepDisabled` bit and the set of live processes.
#[derive(Clone)]
struct World {
    kernel_disabled: Arc<AtomicBool>,
    live: Arc<Mutex<HashSet<ProcessId>>>,
    /// When armed, the next toggle through the injected disabler fails with an
    /// IOKit-style error (and disarms), leaving the kernel bit untouched.
    fail_next_toggle: Arc<AtomicBool>,
}

impl World {
    fn new() -> Self {
        Self {
            kernel_disabled: Arc::new(AtomicBool::new(false)),
            live: Arc::new(Mutex::new(HashSet::new())),
            fail_next_toggle: Arc::new(AtomicBool::new(false)),
        }
    }

    fn kernel(&self) -> bool {
        self.kernel_disabled.load(Ordering::SeqCst)
    }
}

/// Build a fresh coordinator bound to `world`. Crucially, each call constructs
/// the coordinator with no in-memory state (the first argument is
/// `verbose = false`) — modelling a helper (re)start, where the coordinator
/// must rediscover intent from the lockfile's durable ownership marker.
fn make_coord(world: &World, lock_path: &std::path::Path) -> EntirelyCoordinator {
    let kernel = world.kernel_disabled.clone();
    let fail_next = world.fail_next_toggle.clone();
    let disabler: SleepDisabler = Arc::new(move |state, _verbose| {
        if fail_next.swap(false, Ordering::SeqCst) {
            return Err(0xE000_02C1u32);
        }
        kernel.store(state, Ordering::SeqCst);
        Ok(())
    });
    let live = world.live.clone();
    let checker: Arc<ProcessChecker> = Arc::new(move |pid, start_time| {
        live.lock()
            .unwrap()
            .contains(&ProcessId { pid, start_time })
    });
    EntirelyCoordinator::with_options(false, lock_path.to_path_buf(), disabler, checker)
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_lock_path() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "caffeinate2_converge_{}_{}.lock",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    path
}

#[derive(Debug, Clone)]
enum Op {
    /// Start (or repeat) a hold for pool[i]; the holder is, by definition, alive.
    Hold(usize),
    /// The holder voluntarily releases pool[i].
    Release(usize),
    /// pool[i] is SIGKILLed: it vanishes from the process table without ever
    /// releasing. Its lockfile entry becomes stale until reaped.
    Kill(usize),
    /// The periodic reaper runs.
    Reconcile,
    /// The helper crashes and restarts: fresh coordinator + startup reconcile.
    Restart,
    /// An external actor disables sleep (e.g. manual `pmset disablesleep`).
    ExternalDisable,
    /// An external actor re-enables sleep (manual `pmset enablesleep`, or a
    /// sleep/wake cycle clearing the bit).
    ExternalEnable,
    /// Arm the disabler to fail its next toggle (transient IOKit error),
    /// driving the hold/release/reconcile rollback paths.
    DisablerFailNext,
}

fn base_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..POOL_LEN).prop_map(Op::Hold),
        (0..POOL_LEN).prop_map(Op::Release),
        (0..POOL_LEN).prop_map(Op::Kill),
        Just(Op::Reconcile),
        Just(Op::Restart),
        Just(Op::DisablerFailNext),
    ]
}

fn adversarial_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..POOL_LEN).prop_map(Op::Hold),
        (0..POOL_LEN).prop_map(Op::Release),
        (0..POOL_LEN).prop_map(Op::Kill),
        Just(Op::Reconcile),
        Just(Op::Restart),
        Just(Op::ExternalDisable),
        Just(Op::ExternalEnable),
        Just(Op::DisablerFailNext),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// With caffeinate2 as the *only* writer of the kernel bit, after every
    /// reconcile/restart the bit must equal "there is at least one live holder".
    /// This exercises crash recovery, lazy pruning of SIGKILLed holders, and the
    /// hold/release ref-counting all at once.
    ///
    /// This originally exposed the lost-disable-intent-across-restart bug; it
    /// now passes thanks to the durable ownership marker. See
    /// `lost_disable_intent_across_restart_is_fixed` for the minimal case.
    #[test]
    fn converges_without_external_interference(
        ops in prop::collection::vec(base_op(), 0..40)
    ) {
        let world = World::new();
        let lock_path = temp_lock_path();
        let mut coord = make_coord(&world, &lock_path);
        // The oracle: processes currently holding *and* alive.
        let mut holders: HashSet<ProcessId> = HashSet::new();
        let p = pool();

        for op in &ops {
            match op {
                Op::Hold(i) => {
                    world.live.lock().unwrap().insert(p[*i]);
                    // A failed hold (armed disable failure) is rolled back:
                    // pool[i] is not holding, so the oracle is unchanged.
                    if coord.hold(p[*i]).is_ok() {
                        holders.insert(p[*i]);
                        // A fresh live hold always leaves sleep disabled.
                        prop_assert!(world.kernel(), "kernel must be disabled after Hold");
                    }
                }
                Op::Release(i) => {
                    // A failed re-enable restores the holder entry, so the
                    // oracle keeps the holder too.
                    if coord.release(p[*i]).is_ok() {
                        holders.remove(&p[*i]);
                    }
                }
                Op::Kill(i) => {
                    world.live.lock().unwrap().remove(&p[*i]);
                    holders.remove(&p[*i]);
                }
                Op::Reconcile => {
                    // The invariant is promised only by a reconcile that ran to
                    // completion; one that hit an armed toggle failure retries
                    // on a later pass.
                    if coord.reconcile().is_ok() {
                        prop_assert_eq!(world.kernel(), !holders.is_empty());
                    }
                }
                Op::Restart => {
                    coord = make_coord(&world, &lock_path);
                    if coord.reconcile_startup().is_ok() {
                        prop_assert_eq!(world.kernel(), !holders.is_empty());
                    }
                }
                Op::DisablerFailNext => {
                    world.fail_next_toggle.store(true, Ordering::SeqCst);
                }
                Op::ExternalDisable | Op::ExternalEnable => unreachable!(),
            }
        }

        // A final quiescent reconcile — with no armed failure left — must land
        // on the invariant.
        world.fail_next_toggle.store(false, Ordering::SeqCst);
        coord.reconcile().unwrap();
        prop_assert_eq!(world.kernel(), !holders.is_empty());
        let _ = std::fs::remove_file(&lock_path);
    }

    /// Under arbitrary external meddling with the kernel bit, the one property
    /// that must still hold is *holder effectiveness*: immediately after a
    /// reconcile, if any live holder remains, sleep is disabled — the reconcile
    /// re-applies the disable rather than trusting cached state.
    #[test]
    fn live_holders_stay_effective_under_interference(
        ops in prop::collection::vec(adversarial_op(), 0..40)
    ) {
        let world = World::new();
        let lock_path = temp_lock_path();
        let mut coord = make_coord(&world, &lock_path);
        let mut holders: HashSet<ProcessId> = HashSet::new();
        let p = pool();

        for op in &ops {
            match op {
                Op::Hold(i) => {
                    world.live.lock().unwrap().insert(p[*i]);
                    if coord.hold(p[*i]).is_ok() {
                        holders.insert(p[*i]);
                    }
                }
                Op::Release(i) => {
                    if coord.release(p[*i]).is_ok() {
                        holders.remove(&p[*i]);
                    }
                }
                Op::Kill(i) => {
                    world.live.lock().unwrap().remove(&p[*i]);
                    holders.remove(&p[*i]);
                }
                Op::ExternalDisable => world.kernel_disabled.store(true, Ordering::SeqCst),
                Op::ExternalEnable => world.kernel_disabled.store(false, Ordering::SeqCst),
                Op::DisablerFailNext => {
                    world.fail_next_toggle.store(true, Ordering::SeqCst);
                }
                Op::Reconcile => {
                    if coord.reconcile().is_ok() && !holders.is_empty() {
                        prop_assert!(
                            world.kernel(),
                            "live holders present but sleep re-enabled by external actor \
                             was not corrected"
                        );
                    }
                }
                Op::Restart => {
                    coord = make_coord(&world, &lock_path);
                    if coord.reconcile_startup().is_ok() && !holders.is_empty() {
                        prop_assert!(world.kernel(), "live holders must be effective after restart");
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&lock_path);
    }
}

/// Targeted: a manual `pmset disablesleep` made outside caffeinate2 (empty
/// lockfile, no holders) must be preserved — reconcile must not force-enable.
#[test]
fn reconcile_preserves_external_disable_on_empty_lockfile() {
    let world = World::new();
    let lock_path = temp_lock_path();
    let coord = make_coord(&world, &lock_path);

    // Someone runs `pmset disablesleep` by hand. caffeinate2 never held.
    world.kernel_disabled.store(true, Ordering::SeqCst);
    coord.reconcile().unwrap();
    assert!(
        world.kernel(),
        "reconcile clobbered a manual pmset disablesleep on an empty lockfile"
    );
    let _ = std::fs::remove_file(&lock_path);
}

/// Targeted: an external `pmset enablesleep` while a hold is active must be
/// corrected on the next reconcile — the holder must stay effective.
#[test]
fn reconcile_reapplies_disable_after_external_enable() {
    let world = World::new();
    let lock_path = temp_lock_path();
    let coord = make_coord(&world, &lock_path);
    let holder = pool()[0];
    world.live.lock().unwrap().insert(holder);

    coord.hold(holder).unwrap();
    assert!(world.kernel());

    // A sleep/wake cycle or manual enablesleep clears the bit underneath us.
    world.kernel_disabled.store(false, Ordering::SeqCst);
    coord.reconcile().unwrap();
    assert!(
        world.kernel(),
        "reconcile trusted stale cache instead of re-applying the disable"
    );
    let _ = std::fs::remove_file(&lock_path);
}

/// Regression test for the lost-disable-intent-across-restart bug. Minimal
/// proptest counterexample that originally failed:
/// `Hold(1:20)`, `Kill(1:20)`, `Release(1:10)`, `Restart`.
///
/// A holder is SIGKILLed, leaving a stale lockfile entry while sleep is still
/// disabled. An unrelated *non-holder* Release then prunes that stale entry as a
/// side effect, emptying the holder set. Before the fix this did not re-enable
/// sleep and, crucially, left no evidence of caffeinate2's ownership, so a
/// restart preserved the disable forever. Now the durable ownership marker means
/// the Release re-enables immediately (it sees "no live holders && we own the
/// disable"), and even if it didn't, the marker would survive to the restart.
/// Either way, sleep converges back to enabled.
#[test]
fn lost_disable_intent_across_restart_is_fixed() {
    let world = World::new();
    let lock_path = temp_lock_path();
    let holder = pool()[3]; // (1, 20)
    let non_holder = pool()[0]; // (1, 10) — never holds

    // A holder takes a hold: sleep disabled, lockfile = {holder}.
    world.live.lock().unwrap().insert(holder);
    let coord = make_coord(&world, &lock_path);
    coord.hold(holder).unwrap();
    assert!(world.kernel());

    // The holder is SIGKILLed: gone from the process table, stale in the lockfile.
    world.live.lock().unwrap().remove(&holder);

    // A non-holder Release prunes the stale entry, emptying the holder set.
    // Because caffeinate2 owns the disable (durable marker), this now re-enables
    // sleep immediately instead of stranding it.
    coord.release(non_holder).unwrap();
    assert!(
        !world.kernel(),
        "release should re-enable sleep once no live holders remain"
    );

    // The restart is then a clean no-op: the marker was cleared on re-enable.
    let coord = make_coord(&world, &lock_path);
    coord.reconcile_startup().unwrap();
    assert!(
        !world.kernel(),
        "sleep must remain enabled with no live holders after restart"
    );
    let _ = std::fs::remove_file(&lock_path);
}

/// Targeted crash-recovery: a helper that crashes while holding the disable
/// leaves a stale (now-dead) holder in the lockfile. On restart the fresh
/// coordinator must prune it and re-enable sleep, despite its cache starting
/// false.
#[test]
fn restart_reenables_after_crashed_holder_dies() {
    let world = World::new();
    let lock_path = temp_lock_path();
    let holder = pool()[0];

    // First "helper": takes a hold, disabling sleep.
    {
        world.live.lock().unwrap().insert(holder);
        let coord = make_coord(&world, &lock_path);
        coord.hold(holder).unwrap();
        assert!(world.kernel());
        // Simulate crash: drop the coordinator without releasing.
    }
    // The holder process also dies before the new helper starts.
    world.live.lock().unwrap().remove(&holder);

    // New helper starts, rediscovers state from the lockfile.
    let coord = make_coord(&world, &lock_path);
    coord.reconcile_startup().unwrap();
    assert!(
        !world.kernel(),
        "restart failed to re-enable sleep after the crashed holder died"
    );
    let _ = std::fs::remove_file(&lock_path);
}
