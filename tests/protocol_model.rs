//! Exhaustive model check of the entirely-mode sleep-coordination protocol.
//!
//! This is a *design-level* check, complementary to the property tests in
//! `entirely_convergence.rs`. Those drive the real `EntirelyCoordinator` through
//! randomized sequences with crashes at operation boundaries. This models the
//! protocol abstractly and has [`stateright`] explore **every** reachable
//! interleaving by BFS — including crashes *between* the individual atomic steps
//! of a single operation (a lockfile write, the kernel toggle, the marker
//! clear). That sub-operation crash window is exactly what the durable ownership
//! marker exists to survive, and it is the one thing the operation-granularity
//! property tests cannot reach.
//!
//! The model is a separate copy of the protocol, so on its own it only shows
//! the design is sound. The [`conformance`] module ties it to the code: it runs
//! every transition of the single-process model against the real coordinator
//! and fails if the two disagree, so a change to either one that isn't made to
//! both shows up as a test failure. The cross-process model has no such check;
//! its reconcile ownership re-record is pinned by a unit test in
//! `src/entirely/coordinator.rs` instead.
//!
//! Both models ASSUME each flock'd lockfile write is atomic: a crash observes
//! either the pre-write or the post-write file, never a torn intermediate.
//! `write_state` earns that assumption with two mitigations — the ownership
//! marker is serialized first, so partial persistence of a prefix errs toward
//! an extra re-enable rather than a missed one, and a snapshot shorter than
//! the file is padded out to the file's length, so no kill window re-exposes a
//! departed holder's line from an un-truncated tail. Intra-write crash states
//! are otherwise outside the modeled state space.
//!
//! Two protocol variants are modelled:
//!
//! * [`Variant::Fixed`] — the durable-ownership-marker design (current code).
//! * [`Variant::Buggy`] — the pre-fix design that cached ownership only in
//!   memory. Kept so the checker can *demonstrate* it catches the
//!   lost-disable-intent bug rather than passing vacuously.
//!
//! Two safety properties are checked. [`CONVERGES`] is the same convergence
//! oracle the property tests use: once a reconcile runs to completion, the
//! kernel's SleepDisabled bit must equal "there is at least one live holder".
//! [`RELEASE_IMMEDIATE`] pins the release-side half of the fix at the moment a
//! release *completes*: once no live holders remain, sleep must already be
//! re-enabled — a release must not lean on a later reconcile to heal a
//! stranding its own decision caused. Without that second checkpoint, a
//! release-side regression (the pre-fix `removed && empty` decision) only
//! surfaces through the restart-shaped [`CONVERGES`] counterexample, and only
//! because reconcile checkpoints happen to follow; the release contract itself
//! would be unchecked. Both checkpoints carry `sometimes` witness properties:
//! an always-property over states that are never reached passes vacuously, so
//! the witnesses fail the suite if a refactor stops marking the checkpoints.
//!
//! Kernel toggles can also *fail* ([`Action::FailOp`] / [`CrossAction::FailOp`]
//! — a transient IOKit error): the operation errors out and runs its rollback,
//! exactly as `EntirelyCoordinator` does. This makes hold()'s two-write
//! rollback property-checked: backing out the holder, and clearing the marker
//! only when this acquire created it and the ownership generation still
//! matches. release()'s holder restore is stepped too, but only for
//! state-space coverage — these models describe a hold as a lockfile entry
//! for a live pid, so they cannot tell a fabricated entry from a genuine one,
//! and the restore's `removed` guard is not falsifiable here. The unit test
//! `stray_release_with_failed_enable_does_not_fabricate_a_holder` in
//! `src/entirely/coordinator.rs` pins that guard instead.
//!
//! A second model, [`CrossProcess`], covers the CLI-fallback deployment: with
//! no helper installed, [`CROSS_PROCS`] root CLI *processes* share the lockfile
//! and the `ops` mutex is per-process, so the decomposed steps of their
//! operations interleave freely (each flock'd lockfile mutation stays atomic,
//! but one process's kernel toggle and follow-up marker write may land between
//! another's mutations). It compares the two post-toggle marker-write designs:
//!
//! * [`MarkerWrite::Unconditional`] — the pre-fix code: release()/status()/
//!   reconcile clear the marker without re-checking the holder set, and the
//!   reconcile disable path records ownership only when the prune saw no
//!   marker. Kept so the checker demonstrates it catches both cross-process
//!   marker races (a clobbered fresh marker, and a re-applied disable owned by
//!   nobody).
//! * [`MarkerWrite::Guarded`] — the fixed code: the clear re-checks, inside the
//!   same flock mutation, that no holders remain *and* that the persisted
//!   ownership generation still matches the one captured with the re-enable
//!   decision (`clear_owns_disable_if_current`), and the reconcile disable path
//!   always re-records ownership — bumping the generation — after its toggle.
//!
//! The cross-process model includes status(): it is IPC-reachable and its
//! `holders == 0 && owns_disable` path decomposes exactly like reconcile's
//! Enable path (prune+decide, kernel toggle, marker clear), with the same
//! post-toggle marker race. The single-process model above leaves status out
//! for the same reason: under a single serialized coordinator its step
//! sequences are a strict subset of reconcile's. Mid-operation process death
//! is covered by `Crash`:
//! a CLI-fallback process SIGKILLed between the atomic steps of its operation
//! — including while other processes are also mid-operation — abandons its
//! remaining steps, and, being coordinator and holder in one, leaves its own
//! lockfile entry stale until a later locked mutation prunes it. Intra-write
//! torn states remain outside both models, per the atomic-write assumption
//! above.
//!
//! Exploring that cross-term surfaces one residual window the protocol cannot
//! close: a reconcile that re-applies the disable and is SIGKILLed before its
//! follow-up ownership record leaves — once a concurrent generation-valid
//! clear removes the marker its prune saw — a standing disable owned by
//! nobody, indistinguishable from a manual `pmset disablesleep`, which
//! reconcile deliberately refuses to override. (The reverse ordering,
//! record-then-toggle, is no fix: it lets a concurrent release validly clear
//! the freshly recorded marker before the toggle lands, orphaning the disable
//! with no crash at all.) The model tracks that window exactly
//! ([`CrossState::orphaned_disable`]), exempts it from the convergence
//! property, and proves with a `sometimes` property that the exempted window
//! is actually reached. Convergence is promised again as soon as ownership is
//! re-established (any marker write) or the disable itself is undone (any
//! enable) — in practice the next hold/release cycle heals it.

use stateright::{Checker, Model, Property};
use std::collections::BTreeSet;

type Pid = u8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Variant {
    /// Ownership recorded durably in the lockfile (the `!disabled` marker),
    /// cleared only after a confirmed re-enable.
    Fixed,
    /// Ownership cached only in the coordinator's memory; lost on restart. This
    /// is the design that stranded sleep disabled after a crash.
    Buggy,
}

/// Precomputed effect of a reconcile, decided at prune time and applied over the
/// following atomic steps (mirrors the code deciding under the lock, then acting
/// after releasing it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Effect {
    Disable,
    Enable,
    Nothing,
}

/// The in-flight operation, if any. Only one runs at a time (the coordinator's
/// `ops` mutex), but a crash can abandon it and a holder can be killed while it
/// is pending.
///
/// Each variant names the real atomic step it mirrors (all in
/// `src/entirely/coordinator.rs` / `src/entirely/lockfile.rs`); those functions
/// carry matching "modeled by" pointers back here — keep the two in sync.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Pending {
    None,
    /// hold(): holder+marker written (`lockfile::acquire`); the kernel disable
    /// (`sleep_disabler(true)` in `EntirelyCoordinator::hold`) is still to come.
    /// `p` and `prior_owned` (the marker state before the acquire) are what the
    /// rollback needs if that disable fails.
    HoldToggle {
        p: Pid,
        first: bool,
        prior_owned: bool,
    },
    /// release(): holder removed, marker left set (`lockfile::release`); the
    /// kernel enable (`sleep_disabler(false)` in `release_under_ops`) is still
    /// to come. `removed` is whether this release actually removed the caller's
    /// entry — the rollback restores only an entry that existed.
    ReleaseToggle {
        p: Pid,
        enable: bool,
        removed: bool,
    },
    /// release(): kernel re-enabled; clearing the marker
    /// (`lockfile::clear_owns_disable_if_current`) is still to come.
    ReleaseClear {
        enable: bool,
    },
    /// reconcile(): pruned and decided (`lockfile::prune_lockfile` + the effect
    /// decision in `reconcile_locked`); the kernel toggle is still to come.
    ReconcileToggle {
        effect: Effect,
    },
    /// reconcile(): kernel toggled; persisting the marker
    /// (`lockfile::record_owns_disable` on the disable path,
    /// `clear_owns_disable_if_current` on the enable path) is still to come.
    ReconcileMarker {
        effect: Effect,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct State {
    /// Durable holder entries (may include dead pids not yet pruned).
    lockfile: BTreeSet<Pid>,
    /// Which holder processes are currently alive.
    alive: BTreeSet<Pid>,
    /// The kernel SleepDisabled bit. Persists across a helper crash.
    kernel_disabled: bool,
    /// Durable ownership marker (Fixed variant).
    marker: bool,
    /// In-memory ownership cache (Buggy variant); reset to false on crash.
    cache_owns: bool,
    pending: Pending,
    /// True in exactly the states reached by completing a reconcile, so the
    /// convergence property is asserted only where it should hold.
    just_reconciled: bool,
    /// True in exactly the states reached by completing a release (its final
    /// atomic step ran; a crash-abandoned release never sets it), so the
    /// release-immediate property is asserted only at release completions.
    just_released: bool,
    /// A (re)started coordinator must run `reconcile_startup` before the periodic
    /// reaper — both the helper daemon and the CLI fallback do this at boot. So
    /// the first reconcile after a crash is always a startup one; modelling that
    /// keeps the buggy counterexample realistic (a periodic reconcile can't be
    /// the first thing to run after a crash and quietly skip startup recovery).
    needs_startup: bool,
}

impl State {
    fn initial() -> Self {
        State {
            lockfile: BTreeSet::new(),
            alive: BTreeSet::new(),
            kernel_disabled: false,
            marker: false,
            cache_owns: false,
            pending: Pending::None,
            just_reconciled: false,
            just_released: false,
            needs_startup: true,
        }
    }
}

/// Live holders: lockfile entries whose process is still alive.
fn live_count(s: &State) -> usize {
    s.lockfile.iter().filter(|p| s.alive.contains(p)).count()
}

/// Prune dead holders (mirrors `lockfile::prune_stale_holders`, which every
/// locked mutation in `src/entirely/lockfile.rs` runs first): retain only
/// entries whose process is alive.
fn prune(s: &mut State) {
    let alive = &s.alive;
    s.lockfile.retain(|p| alive.contains(p));
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Action {
    Hold(Pid),
    Release(Pid),
    Kill(Pid),
    /// Start the periodic reaper (reconcile()).
    StartReconcile,
    /// Start the boot-time reconcile (reconcile_startup()).
    StartStartup,
    /// Advance the in-flight operation by one atomic step.
    StepOp,
    /// The in-flight operation's kernel toggle fails (transient IOKit error):
    /// the operation returns an error after running its rollback. Offered only
    /// for steps that actually call the kernel.
    FailOp,
    /// Helper SIGKILL: abandon any in-flight op (and, for Buggy, lose the cache).
    Crash,
}

struct Coordinator {
    variant: Variant,
    pids: Vec<Pid>,
}

const CONVERGES: &str = "reconcile converges sleep to live holders";
/// The release-side contract of the durable-marker fix, checked at the moment a
/// release completes (not deferred to the next reconcile): once no live holders
/// remain, sleep must already be re-enabled. Shared by both models.
const RELEASE_IMMEDIATE: &str = "a completed release leaves no holderless disable";
/// Witnesses that the checkpoint states the always-properties are guarded on
/// are actually reached: without them, deleting the code that marks a
/// checkpoint would leave the corresponding always-property vacuously green.
/// Shared by both models.
const RECONCILE_CHECKPOINTED: &str = "a reconcile-completion checkpoint is reached";
const RELEASE_CHECKPOINTED: &str = "a release-completion checkpoint is reached";

impl Model for Coordinator {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![State::initial()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        // A crash can happen at any point (idle or mid-operation).
        actions.push(Action::Crash);

        if state.pending == Pending::None {
            // Quiescent: any operation may start, and live holders may die.
            for &p in &self.pids {
                actions.push(Action::Hold(p));
                actions.push(Action::Release(p)); // includes stray/non-holder releases
                if state.alive.contains(&p) {
                    actions.push(Action::Kill(p));
                }
            }
            // The periodic reaper only runs once boot-time startup recovery has
            // happened; startup recovery is always available.
            if !state.needs_startup {
                actions.push(Action::StartReconcile);
            }
            actions.push(Action::StartStartup);
        } else {
            // An operation is in flight; only its own steps advance it. (Kills
            // are restricted to quiescent states: a kill mid-operation is
            // equivalent to one just after it for these properties, since the
            // operation's decisions are already locked in.)
            actions.push(Action::StepOp);
            // Steps that actually call the kernel can fail; the marker/clear
            // steps are flock'd lockfile writes and stay reliable per the
            // atomic-write assumption.
            if matches!(
                state.pending,
                Pending::HoldToggle { first: true, .. }
                    | Pending::ReleaseToggle { enable: true, .. }
                    | Pending::ReconcileToggle {
                        effect: Effect::Disable | Effect::Enable
                    }
            ) {
                actions.push(Action::FailOp);
            }
        }
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut s = last.clone();
        // Any action other than completing a reconcile/release leaves us in a
        // not-just-checkpointed state; the completion steps set these true.
        s.just_reconciled = false;
        s.just_released = false;

        match action {
            Action::Crash => {
                s.pending = Pending::None;
                // The restarted coordinator must run startup recovery first.
                s.needs_startup = true;
                if self.variant == Variant::Buggy {
                    // A fresh coordinator has no in-memory ownership.
                    s.cache_owns = false;
                }
                // kernel/marker/lockfile/alive are durable and survive the crash.
            }

            Action::Kill(p) => {
                s.alive.remove(&p); // lockfile entry left stale until pruned
            }

            Action::Hold(p) => {
                // Mirrors `lockfile::acquire`: the caller is by definition
                // alive; prune, insert the holder, and — for the first holder —
                // set the ownership marker in the SAME flock'd write, before
                // the disable below.
                s.alive.insert(p);
                prune(&mut s);
                let first = s.lockfile.is_empty();
                let prior_owned = s.marker;
                s.lockfile.insert(p);
                if first && self.variant == Variant::Fixed {
                    s.marker = true;
                }
                s.pending = Pending::HoldToggle {
                    p,
                    first,
                    prior_owned,
                };
            }

            Action::Release(p) => {
                // Mirrors `lockfile::release`, which pins the caller: its own
                // entry survives the prune even when its process is dead (the
                // helper's failed-response rollback, or a client that exited
                // after sending Release), so it counts as removed.
                s.lockfile.retain(|q| *q == p || s.alive.contains(q));
                let removed = s.lockfile.remove(&p);
                let enable = match self.variant {
                    // Fixed: mirrors `lockfile::release`'s `should_enable:
                    // holders.is_empty() && owns_disable` — re-enable when no
                    // live holders remain and we own the disable, regardless of
                    // whether this caller held.
                    Variant::Fixed => s.lockfile.is_empty() && s.marker,
                    // Buggy: the pre-fix `removed && empty` decision — only
                    // when this caller removed a live holder and none remain.
                    Variant::Buggy => removed && s.lockfile.is_empty(),
                };
                s.pending = Pending::ReleaseToggle { p, enable, removed };
            }

            Action::StartReconcile | Action::StartStartup => {
                // Mirrors `EntirelyCoordinator::reconcile_locked`'s decision:
                // `lockfile::prune_lockfile`, then holders>0 => disable;
                // owns_disable => enable; else nothing. Startup and periodic
                // reconciles share the decision; startup only unlocks the
                // periodic reaper (the boot ordering the real daemons follow).
                if action == Action::StartStartup {
                    s.needs_startup = false;
                }
                prune(&mut s);
                let live = s.lockfile.len();
                let owns = match self.variant {
                    Variant::Fixed => s.marker,
                    Variant::Buggy => s.cache_owns,
                };
                let effect = if live > 0 {
                    Effect::Disable
                } else if owns {
                    Effect::Enable
                } else {
                    Effect::Nothing
                };
                s.pending = Pending::ReconcileToggle { effect };
            }

            Action::FailOp => match last.pending {
                // FailOp is only offered for kernel-toggle steps (see actions).
                Pending::None
                | Pending::ReleaseClear { .. }
                | Pending::ReconcileMarker { .. }
                | Pending::HoldToggle { first: false, .. }
                | Pending::ReleaseToggle { enable: false, .. }
                | Pending::ReconcileToggle {
                    effect: Effect::Nothing,
                } => return None,

                // hold()'s rollback: back out the holder entry; clear the
                // marker only if this acquire created it (mirrors the
                // `prior_owns_disable` guard, with the generation trivially
                // still current under the serialized single coordinator).
                Pending::HoldToggle {
                    p,
                    first: true,
                    prior_owned,
                } => {
                    prune(&mut s);
                    s.lockfile.remove(&p);
                    if self.variant == Variant::Fixed && !prior_owned {
                        s.marker = false;
                    }
                    s.pending = Pending::None;
                }

                // release()'s rollback: restore the holder entry — but only
                // when this release actually removed one (a stray release that
                // merely pruned must not fabricate a hold). The marker was
                // left set by the release, so nothing else changes.
                Pending::ReleaseToggle {
                    p,
                    enable: true,
                    removed,
                } => {
                    prune(&mut s);
                    if removed {
                        s.lockfile.insert(p);
                    }
                    s.pending = Pending::None;
                }

                // reconcile() propagates the toggle error; neither the marker
                // step nor the completion checkpoint is reached.
                Pending::ReconcileToggle {
                    effect: Effect::Disable | Effect::Enable,
                } => {
                    s.pending = Pending::None;
                }
            },

            Action::StepOp => match last.pending {
                Pending::None => return None,

                Pending::HoldToggle { first, .. } => {
                    if first {
                        s.kernel_disabled = true;
                        if self.variant == Variant::Buggy {
                            s.cache_owns = true;
                        }
                    }
                    s.pending = Pending::None;
                }

                Pending::ReleaseToggle { enable, .. } => {
                    if enable {
                        s.kernel_disabled = false;
                        if self.variant == Variant::Buggy {
                            s.cache_owns = false;
                        }
                    }
                    s.pending = match self.variant {
                        // Fixed clears the marker in a later step, after the
                        // enable is confirmed.
                        Variant::Fixed => Pending::ReleaseClear { enable },
                        Variant::Buggy => {
                            // Buggy has no marker to clear: the release is
                            // complete here, so it is a release-immediate
                            // checkpoint.
                            s.just_released = true;
                            Pending::None
                        }
                    };
                }

                Pending::ReleaseClear { enable } => {
                    if enable {
                        s.marker = false;
                    }
                    s.pending = Pending::None;
                    s.just_released = true;
                }

                Pending::ReconcileToggle { effect } => {
                    match effect {
                        Effect::Disable => s.kernel_disabled = true,
                        Effect::Enable => s.kernel_disabled = false,
                        Effect::Nothing => {}
                    }
                    s.pending = Pending::ReconcileMarker { effect };
                }

                Pending::ReconcileMarker { effect } => {
                    let owned = match effect {
                        Effect::Disable => true,
                        Effect::Enable => false,
                        Effect::Nothing => match self.variant {
                            Variant::Fixed => s.marker,
                            Variant::Buggy => s.cache_owns,
                        },
                    };
                    match self.variant {
                        Variant::Fixed => s.marker = owned,
                        Variant::Buggy => s.cache_owns = owned,
                    }
                    s.pending = Pending::None;
                    s.just_reconciled = true;
                }
            },
        }

        Some(s)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always(CONVERGES, |_, s| {
                // Only meaningful once a reconcile has completed. Mid-operation
                // and post-crash windows are transient and healed by the next
                // reconcile.
                if !s.just_reconciled {
                    return true;
                }
                s.kernel_disabled == (live_count(s) > 0)
            }),
            // The release-immediate checkpoint (the model-level twin of the
            // per-Release assertion in entirely_convergence.rs): a *completed*
            // release must not leave sleep disabled with zero live holders —
            // stranding is not allowed to lean on the reaper. Legitimate
            // deferrals never reach this checkpoint: a crash abandons the
            // release before its final step (`just_released` stays false), and
            // a SIGKILLed holder's stale entry was pruned by the release's own
            // locked mutation. Only the enable direction is asserted — a
            // release with live holders remaining rightly leaves the kernel bit
            // wherever the (possibly crash-interrupted) hold left it, which the
            // next reconcile converges.
            Property::<Self>::always(RELEASE_IMMEDIATE, |_, s| {
                if !s.just_released {
                    return true;
                }
                !(s.kernel_disabled && live_count(s) == 0)
            }),
            // Witnesses: the checkpoints the two always-properties are guarded
            // on must actually be reached (see the const docs).
            Property::<Self>::sometimes(RECONCILE_CHECKPOINTED, |_, s| s.just_reconciled),
            Property::<Self>::sometimes(RELEASE_CHECKPOINTED, |_, s| s.just_released),
        ]
    }
}

/// The fixed protocol must satisfy convergence *and* the release-immediate
/// contract across *every* interleaving, including crashes between the write,
/// the kernel toggle, and the marker clear.
#[test]
fn fixed_protocol_converges_under_all_interleavings() {
    let checker = Coordinator {
        variant: Variant::Fixed,
        pids: vec![1, 2, 3],
    }
    .checker()
    .spawn_bfs()
    .join();
    // Panics with a counterexample path if any property is ever violated.
    checker.assert_properties();
    eprintln!(
        "Fixed protocol: {} states explored, no property violation.",
        checker.unique_state_count()
    );
}

/// Sanity check that the checker has teeth: the pre-fix protocol must produce a
/// convergence counterexample *demonstrating the lost-disable-intent bug*, and
/// a release-immediate counterexample convicting the release decision on its
/// own. If either discovery disappears, or the convergence counterexample stops
/// matching the historical bug's shape, the model has drifted.
#[test]
fn buggy_protocol_is_caught_by_the_checker() {
    let checker = Coordinator {
        variant: Variant::Buggy,
        pids: vec![1, 2, 3],
    }
    .checker()
    .spawn_bfs()
    .join();
    let path = checker.discovery(CONVERGES).expect(
        "the checker should find the lost-disable-intent counterexample in the buggy protocol",
    );
    eprintln!(
        "Buggy protocol counterexample ({} actions):\n{:#?}",
        path.clone().into_actions().len(),
        path.clone().into_actions()
    );

    // Pin the counterexample to the *shape* of the historical bug rather than
    // an exact action list, so BFS tie-breaking (which pid, which equal-length
    // interleaving) cannot flake the test:
    let steps = path.into_vec(); // (state, action taken FROM that state) pairs

    // 1. The violating final state is the stranded machine: a completed
    //    reconcile with sleep still disabled, no live holders, and an empty
    //    (already-pruned) lockfile.
    let (last, _) = steps.last().expect("counterexample path cannot be empty");
    assert!(
        last.just_reconciled && last.kernel_disabled && live_count(last) == 0,
        "final state must be a completed reconcile stranded with sleep \
         disabled and no live holders, got {last:?}"
    );
    assert!(
        last.lockfile.is_empty(),
        "the final reconcile pruned the lockfile, so no entry may remain: {last:?}"
    );

    // 2. The disable was once legitimately held: some earlier state has sleep
    //    disabled with a live holder.
    let held_at = steps
        .iter()
        .position(|(s, _)| s.kernel_disabled && live_count(s) > 0)
        .expect("the disable must have been legitimately held at some point");

    // 3. The intent is lost *across a restart*: a Release empties the holder
    //    set without the marker's protection, and a later crash wipes the
    //    in-memory ownership cache, leaving the final reconcile nothing to act
    //    on. Require a Release after the hold took effect and a Crash after
    //    that Release.
    let release_at = steps
        .iter()
        .skip(held_at)
        .position(|(_, a)| matches!(a, Some(Action::Release(_))))
        .map(|offset| held_at + offset)
        .expect("a Release must empty the holder set while sleep is disabled");
    assert!(
        steps[release_at..]
            .iter()
            .any(|(_, a)| matches!(a, Some(Action::Crash))),
        "a crash/restart after the Release must be what loses the in-memory \
         ownership cache (the lost-disable-intent shape)"
    );

    // The release-immediate property must convict the buggy release decision
    // directly, with no restart involved: its counterexample ends at a
    // completed release that left sleep disabled with zero live holders.
    let quick = checker
        .discovery(RELEASE_IMMEDIATE)
        .expect("the buggy release decision should violate the release-immediate contract");
    let quick_last = quick.last_state();
    assert!(
        quick_last.just_released && quick_last.kernel_disabled && live_count(quick_last) == 0,
        "release-immediate counterexample must end at a completed release \
         stranding sleep disabled, got {quick_last:?}"
    );
}

/// Which post-toggle marker-write design the cross-process model checks (see
/// the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum MarkerWrite {
    /// Pre-fix: clear without re-checking anything; record ownership on a
    /// reconcile disable only when the prune saw no marker.
    Unconditional,
    /// Fixed: `clear_owns_disable_if_current` semantics — the clear re-checks,
    /// inside the same flock write, that no holders remain and that the
    /// ownership generation still matches the one captured with the re-enable
    /// decision — and the reconcile disable path always re-records ownership
    /// (bumping the generation) after its toggle.
    Guarded,
}

/// One process's in-flight operation, decomposed at its atomicity boundaries:
/// each lockfile mutation is one flock'd write, the kernel toggle is a separate
/// step, and the follow-up marker mutation is a second lockfile write. The
/// real steps mirrored are the same as in the single-process [`Pending`] (see
/// its per-variant pointers into `src/entirely/{coordinator,lockfile}.rs`),
/// plus status()'s enable path from `EntirelyCoordinator::status`.
/// Operations whose initial lockfile mutation is their only effect (a non-first
/// hold, a release/status that decides against toggling, a reconcile that
/// decides `Nothing`) complete atomically at start and never appear here.
///
/// `gen_valid` abstracts the persisted ownership generation exactly: the code
/// compares "generation captured with the re-enable decision" against the
/// current one, and the counter is monotonic (never recycled), so the
/// comparison is precisely "has any marker set happened since the decision".
/// Every marker set flips the token to false via [`invalidate_generations`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ProcPending {
    None,
    /// hold(): first holder + marker written; the kernel disable is to come.
    /// The extra fields serve the rollback of a failed disable: `prior_owned`
    /// is the marker state before this acquire (a pre-existing marker must
    /// survive the rollback), `gen_valid` its captured ownership generation,
    /// and `was_orphaned` whether the acquire's marker write covered a
    /// standing orphaned disable (which the rollback then un-covers).
    HoldToggle {
        prior_owned: bool,
        gen_valid: bool,
        was_orphaned: bool,
    },
    /// release(): holder removed, re-enable decided; the kernel enable is to
    /// come. `removed` is whether the caller's entry was actually present —
    /// the rollback of a failed enable restores only an entry that existed.
    ReleaseToggle {
        gen_valid: bool,
        removed: bool,
    },
    /// release(): kernel re-enabled; the marker clear is to come.
    ReleaseClear {
        gen_valid: bool,
    },
    /// reconcile(): pruned and decided; the kernel toggle is to come. `owned`
    /// is the marker as seen at prune time (the Unconditional disable path
    /// consults it).
    ReconcileToggle {
        effect: Effect,
        owned: bool,
        gen_valid: bool,
    },
    /// reconcile(): kernel toggled; the marker write is to come.
    ReconcileMarker {
        effect: Effect,
        owned: bool,
        gen_valid: bool,
    },
    /// status(): pruned to zero holders while owning the disable; the kernel
    /// enable is to come.
    StatusToggle {
        gen_valid: bool,
    },
    /// status(): kernel re-enabled; the marker clear is to come.
    StatusClear {
        gen_valid: bool,
    },
}

/// How many CLI-fallback processes share the lockfile in the model. Two is the
/// minimum that exhibits every cross-process marker race; three additionally
/// covers three-way interleavings (e.g. a releaser, a fresh holder, and a
/// reconciler all mid-operation). Measured with the full BFS in the debug test
/// profile: 3 processes = 229,509 states (~3s for both cross-process tests,
/// toggle-failure injection included) — still cheap, so 3 is kept. Each extra
/// process multiplies the space by roughly another pending slot's variants
/// plus the pid subsets; re-measure before raising this.
const CROSS_PROCS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CrossState {
    /// Durable holder entries (may include dead pids not yet pruned).
    lockfile: BTreeSet<Pid>,
    /// Durable ownership marker.
    marker: bool,
    /// The kernel SleepDisabled bit.
    kernel_disabled: bool,
    /// Which holder pids are currently alive.
    alive: BTreeSet<Pid>,
    /// Each process's in-flight operation (its own `ops` mutex allows one).
    pending: [ProcPending; CROSS_PROCS],
    /// interfered[i]: another process mutated shared state while process i's
    /// current operation was in flight, so i's locked-in decision may be stale
    /// and its completion is not a convergence checkpoint.
    interfered: [bool; CROSS_PROCS],
    /// A standing kernel disable whose ownership record died with its process:
    /// a reconcile re-applied the disable and was SIGKILLed before the
    /// follow-up marker write. Until a new marker write covers the disable (or
    /// an enable undoes it), a generation-valid clear can leave the bit owned
    /// by nobody — indistinguishable from a manual `pmset disablesleep`, which
    /// reconcile deliberately leaves untouched — so convergence is not
    /// promised there. Set only by that specific crash, never inferred from
    /// state shape, so the exemption cannot mask the Unconditional
    /// counterexamples (whose traces are crash-free).
    orphaned_disable: bool,
    /// True in exactly the states reached by completing a *solo* reconcile:
    /// interference-free and with every other process idle. Only there is
    /// convergence promised — a reconcile finishing while another process is
    /// mid-operation may observe a state that op's remaining steps (e.g. its
    /// pending marker write) are about to repair.
    just_reconciled: bool,
    /// True in exactly the states reached by completing a *solo* release
    /// (interference-free, every other process idle): either its final marker
    /// clear ran, or its locked decision against toggling made the initial
    /// flock'd mutation the entire operation. The release-immediate property is
    /// asserted only there, under the same solo rule as `just_reconciled`.
    just_released: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum CrossAction {
    /// Process `who` takes a hold for its own pid.
    Hold(u8),
    /// Process `who` releases its own pid (possibly a stray non-holder release).
    Release(u8),
    /// Process `who` runs its periodic reconcile.
    Reconcile(u8),
    /// Process `who` serves a Status RPC.
    Status(u8),
    /// Advance process `who`'s in-flight operation by one atomic step.
    StepOp(u8),
    /// Process `who`'s pending kernel toggle fails (transient IOKit error):
    /// the operation errors out after running its rollback.
    FailOp(u8),
    /// SIGKILL the process owning `Pid` while both processes are quiescent.
    /// Mid-operation death is [`CrossAction::Crash`]; keeping the two disjoint
    /// avoids redundant states.
    Kill(Pid),
    /// SIGKILL process `who` while its operation is in flight: the remaining
    /// steps (and their locked-in decision) vanish, and — a CLI-fallback
    /// process being coordinator and holder in one — its own lockfile entry
    /// goes stale. Offered only mid-operation: a quiescent process's death is
    /// already `Kill` of its pid. No new restart action is needed either: the
    /// now-quiescent slot's ordinary operations model a fresh replacement
    /// process (pid reuse is indistinguishable at this abstraction because
    /// `alive` is the liveness oracle).
    Crash(u8),
}

struct CrossProcess {
    marker_write: MarkerWrite,
}

/// Each modelled process holds under its own pid.
fn pid_of(who: usize) -> Pid {
    u8::try_from(who).unwrap() + 1
}

/// Every process other than `who` is idle — the solo-completion condition for
/// the convergence and release-immediate checkpoints.
fn others_idle(s: &CrossState, who: usize) -> bool {
    s.pending
        .iter()
        .enumerate()
        .all(|(other, p)| other == who || *p == ProcPending::None)
}

fn cross_live_count(s: &CrossState) -> usize {
    s.lockfile.iter().filter(|p| s.alive.contains(p)).count()
}

fn cross_prune(s: &mut CrossState) {
    let alive = &s.alive;
    s.lockfile.retain(|p| alive.contains(p));
}

/// A marker set bumps the persisted ownership generation, invalidating every
/// in-flight re-enable decision that captured the previous one. Mirrors the
/// `disable_generation += 1` bump in `lockfile::acquire` and
/// `lockfile::record_owns_disable` (the counter is monotonic and persisted, so
/// "still matches" is exactly "no set since").
fn invalidate_generations(s: &mut CrossState) {
    for pending in &mut s.pending {
        match pending {
            ProcPending::HoldToggle { gen_valid, .. }
            | ProcPending::ReleaseToggle { gen_valid, .. }
            | ProcPending::ReleaseClear { gen_valid }
            | ProcPending::StatusToggle { gen_valid }
            | ProcPending::StatusClear { gen_valid }
            | ProcPending::ReconcileToggle { gen_valid, .. }
            | ProcPending::ReconcileMarker { gen_valid, .. } => *gen_valid = false,
            ProcPending::None => {}
        }
    }
}

impl CrossProcess {
    /// The post-toggle marker clear. Guarded mirrors
    /// `lockfile::clear_owns_disable_if_current`: prune (as every locked
    /// mutation does first), then re-check the holder set and the ownership
    /// generation inside the same flock mutation. Unconditional is the pre-fix
    /// clear (surviving in the tree only as the `#[cfg(test)]`-gated
    /// `lockfile::clear_owns_disable_unguarded` bypass).
    fn clear_marker(&self, s: &mut CrossState, gen_valid: bool) {
        cross_prune(s);
        match self.marker_write {
            MarkerWrite::Unconditional => s.marker = false,
            MarkerWrite::Guarded => {
                if gen_valid && s.lockfile.is_empty() {
                    s.marker = false;
                }
            }
        }
    }
}

const ORPHANED: &str = "a crash can orphan a reconcile-applied disable";

impl Model for CrossProcess {
    type State = CrossState;
    type Action = CrossAction;

    fn init_states(&self) -> Vec<Self::State> {
        vec![CrossState {
            lockfile: BTreeSet::new(),
            marker: false,
            kernel_disabled: false,
            alive: BTreeSet::new(),
            pending: [ProcPending::None; CROSS_PROCS],
            interfered: [false; CROSS_PROCS],
            orphaned_disable: false,
            just_reconciled: false,
            just_released: false,
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        for who in 0..CROSS_PROCS {
            if state.pending[who] == ProcPending::None {
                let who = u8::try_from(who).unwrap();
                actions.push(CrossAction::Hold(who));
                actions.push(CrossAction::Release(who));
                actions.push(CrossAction::Reconcile(who));
                actions.push(CrossAction::Status(who));
            } else {
                let toggle_step = matches!(
                    state.pending[who],
                    ProcPending::HoldToggle { .. }
                        | ProcPending::ReleaseToggle { .. }
                        | ProcPending::StatusToggle { .. }
                        | ProcPending::ReconcileToggle { .. }
                );
                let who = u8::try_from(who).unwrap();
                actions.push(CrossAction::StepOp(who));
                actions.push(CrossAction::Crash(who));
                // Every pending toggle variant here is a real kernel call (a
                // decision against toggling completes at start), so each can
                // fail; the marker/clear steps are flock'd lockfile writes and
                // stay reliable per the atomic-write assumption.
                if toggle_step {
                    actions.push(CrossAction::FailOp(who));
                }
            }
        }
        if state.pending.iter().all(|p| *p == ProcPending::None) {
            for who in 0..CROSS_PROCS {
                let p = pid_of(who);
                if state.alive.contains(&p) {
                    actions.push(CrossAction::Kill(p));
                }
            }
        }
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut s = last.clone();
        s.just_reconciled = false;
        s.just_released = false;

        // Any move by one process while another has an operation in flight
        // taints that operation's locked-in decision. Over-approximate (taint
        // on every move, mutating or not): a skipped assertion is sound, and a
        // later solo reconcile still has to converge from whatever state the
        // interleaving produced.
        let taint_others_of = |s: &mut CrossState, who: usize| {
            for other in 0..CROSS_PROCS {
                if other != who && s.pending[other] != ProcPending::None {
                    s.interfered[other] = true;
                }
            }
        };

        match action {
            CrossAction::Kill(p) => {
                // Only offered when every process is quiescent.
                s.alive.remove(&p);
            }

            CrossAction::Crash(who) => {
                // Only offered while `who`'s operation is in flight. Its
                // remaining steps die with the process (no marker set, no
                // generation bump), and its own lockfile entry goes stale.
                // The survivors' in-flight decisions now rest on stale
                // liveness, so they are tainted like any other concurrent move.
                let who = usize::from(who);
                if matches!(
                    last.pending[who],
                    ProcPending::ReconcileMarker {
                        effect: Effect::Disable,
                        ..
                    }
                ) && s.kernel_disabled
                {
                    // The disable this reconcile re-applied is still standing,
                    // and the record that would have owned it just died with
                    // the process (see the module docs).
                    s.orphaned_disable = true;
                }
                s.pending[who] = ProcPending::None;
                s.alive.remove(&pid_of(who));
                s.interfered[who] = false; // no op left to be tainted
                taint_others_of(&mut s, who);
            }

            CrossAction::Hold(who) => {
                // Mirrors `lockfile::acquire` (called from
                // `EntirelyCoordinator::hold`): prune, insert the holder, and —
                // for the first holder — set the marker and bump the generation
                // in the same flock write.
                let who = usize::from(who);
                let p = pid_of(who);
                s.alive.insert(p);
                cross_prune(&mut s);
                let first = s.lockfile.is_empty();
                let prior_owned = s.marker;
                s.lockfile.insert(p);
                if first {
                    // Marker written (and the generation bumped) in the same
                    // flock write that adds the holder; the kernel disable is a
                    // later step. The fresh record owns any standing disable,
                    // ending an orphaned-disable window — though a failed
                    // toggle's rollback can reopen it (`was_orphaned`).
                    let was_orphaned = s.orphaned_disable;
                    s.marker = true;
                    invalidate_generations(&mut s);
                    s.orphaned_disable = false;
                    s.pending[who] = ProcPending::HoldToggle {
                        prior_owned,
                        gen_valid: true,
                        was_orphaned,
                    };
                    s.interfered[who] = false;
                }
                taint_others_of(&mut s, who);
            }

            CrossAction::Release(who) => {
                // Mirrors `lockfile::release` (called from
                // `release_under_ops`): one flock'd mutation that prunes,
                // removes the caller, and decides `should_enable =
                // holders.is_empty() && owns_disable`.
                let who = usize::from(who);
                cross_prune(&mut s);
                let removed = s.lockfile.remove(&pid_of(who));
                if s.lockfile.is_empty() && s.marker {
                    s.pending[who] = ProcPending::ReleaseToggle {
                        gen_valid: true,
                        removed,
                    };
                    s.interfered[who] = false;
                } else {
                    // The decision against toggling makes that mutation the
                    // entire operation: the release completes here, atomically,
                    // so this is a release-immediate checkpoint (solo rule as
                    // for `just_reconciled`). This is where a release-side
                    // decision regression — the pre-fix `removed && empty`, or
                    // a marker lost to an unconditional clear — shows up as a
                    // stranded disable with zero holders.
                    s.just_released = others_idle(&s, who);
                }
                taint_others_of(&mut s, who);
            }

            CrossAction::Reconcile(who) => {
                // Mirrors `reconcile_locked`: `lockfile::prune_lockfile`, then
                // holders>0 => disable, owns_disable => enable, else nothing.
                let who = usize::from(who);
                cross_prune(&mut s);
                let owned = s.marker;
                if !s.lockfile.is_empty() {
                    s.pending[who] = ProcPending::ReconcileToggle {
                        effect: Effect::Disable,
                        owned,
                        gen_valid: false, // unused on the disable path
                    };
                    s.interfered[who] = false;
                } else if owned {
                    s.pending[who] = ProcPending::ReconcileToggle {
                        effect: Effect::Enable,
                        owned,
                        gen_valid: true,
                    };
                    s.interfered[who] = false;
                } else {
                    // Nothing to converge: the prune was the whole reconcile,
                    // completing atomically (and interference-free). Solo only
                    // if every other process is idle.
                    s.just_reconciled = others_idle(&s, who);
                }
                taint_others_of(&mut s, who);
            }

            CrossAction::Status(who) => {
                // Mirrors `EntirelyCoordinator::status`'s `holders == 0 &&
                // owns_disable` re-enable path (prune+decide, kernel toggle,
                // marker clear).
                let who = usize::from(who);
                cross_prune(&mut s);
                if s.lockfile.is_empty() && s.marker {
                    s.pending[who] = ProcPending::StatusToggle { gen_valid: true };
                    s.interfered[who] = false;
                }
                taint_others_of(&mut s, who);
            }

            CrossAction::FailOp(who) => {
                let who = usize::from(who);
                match last.pending[who] {
                    // FailOp is only offered for kernel-toggle steps.
                    ProcPending::None
                    | ProcPending::ReleaseClear { .. }
                    | ProcPending::StatusClear { .. }
                    | ProcPending::ReconcileMarker { .. } => return None,

                    // hold()'s rollback (`EntirelyCoordinator::hold`, disable
                    // failed): back out the holder entry, then clear the marker
                    // only if this acquire created it — via the same
                    // generation-guarded clear as a release, so a marker a
                    // concurrent reconcile re-recorded meanwhile survives.
                    ProcPending::HoldToggle {
                        prior_owned,
                        gen_valid,
                        was_orphaned,
                    } => {
                        cross_prune(&mut s);
                        s.lockfile.remove(&pid_of(who));
                        if !prior_owned {
                            let marker_before = s.marker;
                            self.clear_marker(&mut s, gen_valid);
                            // Rolling the marker back un-covers the standing
                            // disable the acquire had taken ownership of: the
                            // orphan window reopens exactly as it was.
                            if was_orphaned && marker_before && !s.marker {
                                s.orphaned_disable = true;
                            }
                        }
                        s.pending[who] = ProcPending::None;
                    }

                    // release()'s rollback (`release_under_ops`, enable
                    // failed): restore the removed entry via a re-acquire —
                    // including its first-holder marker set and generation
                    // bump — but never fabricate an entry for a stray release
                    // that only pruned.
                    ProcPending::ReleaseToggle { removed, .. } => {
                        cross_prune(&mut s);
                        if removed {
                            let first = s.lockfile.is_empty();
                            s.lockfile.insert(pid_of(who));
                            if first {
                                s.marker = true;
                                invalidate_generations(&mut s);
                                s.orphaned_disable = false;
                            }
                        }
                        s.pending[who] = ProcPending::None;
                    }

                    // status() and reconcile() propagate the toggle error and
                    // never reach their marker step.
                    ProcPending::StatusToggle { .. } | ProcPending::ReconcileToggle { .. } => {
                        s.pending[who] = ProcPending::None;
                    }
                }
                taint_others_of(&mut s, who);
            }

            CrossAction::StepOp(who) => {
                let who = usize::from(who);
                match last.pending[who] {
                    ProcPending::None => return None,

                    ProcPending::HoldToggle { .. } => {
                        s.kernel_disabled = true;
                        s.pending[who] = ProcPending::None;
                    }

                    ProcPending::ReleaseToggle { gen_valid, .. } => {
                        s.kernel_disabled = false;
                        s.pending[who] = ProcPending::ReleaseClear { gen_valid };
                    }

                    ProcPending::ReleaseClear { gen_valid } => {
                        self.clear_marker(&mut s, gen_valid);
                        s.pending[who] = ProcPending::None;
                        // The release is complete: a release-immediate
                        // checkpoint, under the same solo/interference rule as
                        // reconcile completions.
                        s.just_released = !s.interfered[who] && others_idle(&s, who);
                    }

                    ProcPending::StatusToggle { gen_valid } => {
                        s.kernel_disabled = false;
                        s.pending[who] = ProcPending::StatusClear { gen_valid };
                    }

                    ProcPending::StatusClear { gen_valid } => {
                        self.clear_marker(&mut s, gen_valid);
                        s.pending[who] = ProcPending::None;
                    }

                    ProcPending::ReconcileToggle {
                        effect,
                        owned,
                        gen_valid,
                    } => {
                        match effect {
                            Effect::Disable => s.kernel_disabled = true,
                            Effect::Enable => s.kernel_disabled = false,
                            Effect::Nothing => unreachable!("completes at start"),
                        }
                        s.pending[who] = ProcPending::ReconcileMarker {
                            effect,
                            owned,
                            gen_valid,
                        };
                    }

                    ProcPending::ReconcileMarker {
                        effect,
                        owned,
                        gen_valid,
                    } => {
                        // The marker write is a locked mutation, so it prunes
                        // first like every other (`record_owns_disable`;
                        // `clear_marker` prunes internally for the Enable arm).
                        cross_prune(&mut s);
                        match (effect, self.marker_write) {
                            // Pre-fix: ownership recorded only when the prune
                            // saw no marker — a concurrent release that cleared
                            // it after the prune leaves this re-applied disable
                            // owned by nobody.
                            (Effect::Disable, MarkerWrite::Unconditional) => {
                                if !owned {
                                    s.marker = true;
                                    s.orphaned_disable = false;
                                }
                            }
                            // Fixed: always re-record ownership (bumping the
                            // generation) for the disable just re-applied. The
                            // record owns any standing disable, ending an
                            // orphaned-disable window.
                            (Effect::Disable, MarkerWrite::Guarded) => {
                                s.marker = true;
                                invalidate_generations(&mut s);
                                s.orphaned_disable = false;
                            }
                            (Effect::Enable, _) => self.clear_marker(&mut s, gen_valid),
                            (Effect::Nothing, _) => unreachable!("completes at start"),
                        }
                        s.pending[who] = ProcPending::None;
                        s.just_reconciled = !s.interfered[who] && others_idle(&s, who);
                    }
                }
                taint_others_of(&mut s, who);
            }
        }

        // An enable undoes the standing disable itself, ending any
        // orphaned-disable window.
        if !s.kernel_disabled {
            s.orphaned_disable = false;
        }

        Some(s)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always(CONVERGES, |_, s| {
                // Only meaningful at solo, interference-free reconcile
                // completions, and not inside the orphaned-disable window the
                // protocol cannot close (see the module docs).
                if !s.just_reconciled || s.orphaned_disable {
                    return true;
                }
                s.kernel_disabled == (cross_live_count(s) > 0)
            }),
            // The release-immediate checkpoint (see the single-process model's
            // twin property): a solo, interference-free release completion must
            // not leave sleep disabled with zero live holders. The
            // orphaned-disable window is exempted exactly as for CONVERGES — a
            // release that finds the unowned standing disable rightly refuses
            // to touch what it cannot distinguish from a manual
            // `pmset disablesleep`.
            Property::<Self>::always(RELEASE_IMMEDIATE, |_, s| {
                if !s.just_released || s.orphaned_disable {
                    return true;
                }
                !(s.kernel_disabled && cross_live_count(s) == 0)
            }),
            // Teeth for the exemption above: the orphaned-disable window must
            // actually be reached, or the carve-out is dead weight and the
            // convergence check quietly weaker than it claims. (This also
            // fails the suite if the Crash action is ever dropped, instead of
            // silently losing the crash coverage.)
            Property::<Self>::sometimes(ORPHANED, |_, s| s.orphaned_disable),
            // Witnesses that the solo-completion checkpoints are reachable at
            // all (see the const docs): the always-properties above are
            // guarded on them and would otherwise pass vacuously.
            Property::<Self>::sometimes(RECONCILE_CHECKPOINTED, |_, s| s.just_reconciled),
            Property::<Self>::sometimes(RELEASE_CHECKPOINTED, |_, s| s.just_released),
        ]
    }
}

/// The fixed cross-process protocol (guarded marker writes) must satisfy
/// convergence and the release-immediate contract under every interleaving of
/// [`CROSS_PROCS`] coordinator processes sharing the lockfile, including
/// mid-operation crashes — outside the documented orphaned-disable window,
/// whose reachability the [`ORPHANED`] `sometimes` property pins down.
#[test]
fn cross_process_fixed_protocol_converges() {
    let checker = CrossProcess {
        marker_write: MarkerWrite::Guarded,
    }
    .checker()
    .spawn_bfs()
    .join();
    checker.assert_properties();
    eprintln!(
        "Cross-process fixed protocol ({CROSS_PROCS} processes): {} states explored, \
         no property violation.",
        checker.unique_state_count()
    );
}

/// Teeth: the pre-fix marker writes must produce a cross-process
/// counterexample — e.g. one process's post-enable marker clear destroying the
/// marker a concurrent first hold just set, leaving that hold's eventual
/// release with no evidence of ownership and sleep stranded disabled with zero
/// holders. Both properties must convict it: [`CONVERGES`] at a reconcile
/// checkpoint, and [`RELEASE_IMMEDIATE`] already at the stranding release
/// itself.
#[test]
fn cross_process_unconditional_marker_writes_are_caught() {
    let checker = CrossProcess {
        marker_write: MarkerWrite::Unconditional,
    }
    .checker()
    .spawn_bfs()
    .join();
    let path = checker.discovery(CONVERGES).expect(
        "the checker should find the cross-process marker-race counterexample in the \
         unconditional-clear protocol",
    );
    eprintln!(
        "Cross-process counterexample ({} actions):\n{:#?}",
        path.clone().into_actions().len(),
        path.clone().into_actions()
    );
    // Shape: the violating final state is the stranded machine — sleep still
    // disabled with zero live holders at a solo reconcile completion, with no
    // crash-orphaned disable to excuse it (the marker was lost to a plain
    // unconditional clear, not a SIGKILL).
    let last = path.last_state();
    assert!(
        last.just_reconciled
            && !last.orphaned_disable
            && last.kernel_disabled
            && cross_live_count(last) == 0,
        "cross-process counterexample must end stranded (sleep disabled, no \
         live holders) at a solo reconcile, got {last:?}"
    );

    // The stranding must also be caught at the release itself, with no
    // reconcile involved: the release that finds the marker already destroyed
    // completes without re-enabling.
    let quick = checker
        .discovery(RELEASE_IMMEDIATE)
        .expect("the unconditional clear should violate the release-immediate contract");
    let quick_last = quick.last_state();
    assert!(
        quick_last.just_released
            && !quick_last.orphaned_disable
            && quick_last.kernel_disabled
            && cross_live_count(quick_last) == 0,
        "release-immediate counterexample must end at a completed release \
         stranding sleep disabled, got {quick_last:?}"
    );
}

/// Conformance: replays the single-process model against the real
/// [`EntirelyCoordinator`](caffeinate2::entirely::coordinator::EntirelyCoordinator).
///
/// Everything above checks the protocol *design* on an abstract copy of it,
/// so on its own it can't notice the code drifting from the model. This
/// closes that gap: for every state the checker reaches, it replays the path
/// that led there against the real coordinator, with a real lockfile, a
/// simulated kernel bit and a simulated process table, and checks after each
/// operation that the durable state (holder entries, ownership marker, kernel
/// bit) and the number of kernel toggles match the model's.
///
/// The model's mid-operation steps map onto the injected sleep toggle, which
/// is the only point inside an operation where the real code yields:
///
/// * a completed step lets the toggle succeed;
/// * `FailOp` makes it return an IOKit-style error, so the real rollback runs;
/// * `Crash` while the toggle is pending panics in place of toggling, and
///   `Crash` after it (before the marker write) toggles and then panics. The
///   panic unwinds out of the operation, skipping its remaining steps, and the
///   coordinator is discarded and rebuilt on the same lockfile, which is a
///   helper restart;
/// * a crash at a point with no kernel step left lets the real operation
///   finish: its remaining model steps change no durable state.
#[cfg(target_os = "macos")]
mod conformance {
    use super::*;
    use caffeinate2::entirely::coordinator::{EntirelyCoordinator, SleepDisabler};
    use caffeinate2::entirely::lockfile::{ProcessChecker, ProcessId, ProcessStartTime};
    use std::panic::{self, AssertUnwindSafe};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, Once};

    /// Panic payload for a simulated helper SIGKILL, so the panic hook can
    /// stay quiet about the thousands of intentional ones.
    struct SimulatedCrash;

    fn silence_simulated_crashes() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let default_hook = panic::take_hook();
            panic::set_hook(Box::new(move |info| {
                if !info.payload().is::<SimulatedCrash>() {
                    default_hook(info);
                }
            }));
        });
    }

    /// What the injected sleep toggle does for the operation being replayed.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Toggle {
        Succeed,
        Fail,
        CrashBefore,
        CrashAfter,
    }

    /// The real coordinator plus the simulated world its seams read.
    struct Real {
        lock_path: PathBuf,
        kernel_disabled: Arc<AtomicBool>,
        alive: Arc<Mutex<BTreeSet<Pid>>>,
        toggle: Arc<Mutex<Toggle>>,
        toggle_calls: Arc<AtomicUsize>,
        coordinator: EntirelyCoordinator,
    }

    fn process_id(p: Pid) -> ProcessId {
        ProcessId {
            pid: i32::from(p),
            start_time: ProcessStartTime {
                seconds: 10,
                microseconds: 0,
            },
        }
    }

    impl Real {
        fn new(lock_path: PathBuf) -> Self {
            let kernel_disabled = Arc::new(AtomicBool::new(false));
            let alive = Arc::new(Mutex::new(BTreeSet::new()));
            let toggle = Arc::new(Mutex::new(Toggle::Succeed));
            let toggle_calls = Arc::new(AtomicUsize::new(0));
            let coordinator =
                Self::build(&lock_path, &kernel_disabled, &alive, &toggle, &toggle_calls);
            Real {
                lock_path,
                kernel_disabled,
                alive,
                toggle,
                toggle_calls,
                coordinator,
            }
        }

        fn build(
            lock_path: &std::path::Path,
            kernel_disabled: &Arc<AtomicBool>,
            alive: &Arc<Mutex<BTreeSet<Pid>>>,
            toggle: &Arc<Mutex<Toggle>>,
            toggle_calls: &Arc<AtomicUsize>,
        ) -> EntirelyCoordinator {
            let (kernel, toggle, calls) = (
                kernel_disabled.clone(),
                toggle.clone(),
                toggle_calls.clone(),
            );
            let disabler: SleepDisabler = Arc::new(move |state, _verbose| {
                calls.fetch_add(1, Ordering::SeqCst);
                let behavior = *toggle.lock().unwrap();
                match behavior {
                    Toggle::Succeed => {
                        kernel.store(state, Ordering::SeqCst);
                        Ok(())
                    }
                    Toggle::Fail => Err(0xE000_02C1u32),
                    Toggle::CrashBefore => panic::panic_any(SimulatedCrash),
                    Toggle::CrashAfter => {
                        kernel.store(state, Ordering::SeqCst);
                        panic::panic_any(SimulatedCrash)
                    }
                }
            });
            let alive = alive.clone();
            let checker: Arc<ProcessChecker> = Arc::new(move |pid, start_time| {
                start_time == process_id(0).start_time
                    && u8::try_from(pid).is_ok_and(|p| alive.lock().unwrap().contains(&p))
            });
            EntirelyCoordinator::with_options(false, lock_path.to_path_buf(), disabler, checker)
        }

        /// A helper SIGKILL and restart: the in-memory coordinator is gone and
        /// a fresh one rediscovers everything from the lockfile.
        fn restart(&mut self) {
            self.coordinator = Self::build(
                &self.lock_path,
                &self.kernel_disabled,
                &self.alive,
                &self.toggle,
                &self.toggle_calls,
            );
        }

        /// An independent copy of this world on its own lockfile, run by a
        /// fresh coordinator. The coordinator's only in-memory state is the
        /// hold epochs used by the helper's failed-response rollback, which no
        /// modeled operation reads, so the copy behaves like the original.
        fn fork(&self, lock_path: PathBuf) -> Self {
            if self.lock_path.exists() {
                std::fs::copy(&self.lock_path, &lock_path).unwrap();
            }
            let fork = Real::new(lock_path);
            fork.kernel_disabled.store(
                self.kernel_disabled.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
            fork.alive
                .lock()
                .unwrap()
                .clone_from(&self.alive.lock().unwrap());
            fork
        }

        /// (holder entries, ownership marker) as persisted in the lockfile.
        fn lockfile(&self) -> (BTreeSet<Pid>, bool) {
            let text = std::fs::read_to_string(&self.lock_path).unwrap_or_default();
            let holders = text
                .lines()
                .filter_map(|line| line.trim().parse::<ProcessId>().ok())
                .map(|id| Pid::try_from(id.pid).expect("only model pids are written"))
                .collect();
            let marker = text.lines().any(|line| line.trim() == "!disabled");
            (holders, marker)
        }
    }

    /// Whether the step out of `pending` calls the kernel (the same set the
    /// model offers `FailOp` for).
    fn calls_kernel(pending: &Pending) -> bool {
        matches!(
            pending,
            Pending::HoldToggle { first: true, .. }
                | Pending::ReleaseToggle { enable: true, .. }
                | Pending::ReconcileToggle {
                    effect: Effect::Disable | Effect::Enable
                }
        )
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Outcome {
        Ok,
        Err,
        Crashed,
    }

    /// Runs the model action at `steps[i]` against the real coordinator: a
    /// single action, or a whole operation through to the next quiescent
    /// state. Returns the index of the model state to compare against.
    fn run_action(
        real: &mut Real,
        steps: &[(State, Option<Action>)],
        i: usize,
    ) -> Result<usize, String> {
        let action = steps[i].1.clone().expect("an action to run");
        match action {
            Action::Kill(p) => {
                real.alive.lock().unwrap().remove(&p);
                Ok(i + 1)
            }
            Action::Crash => {
                real.restart();
                Ok(i + 1)
            }
            Action::StepOp | Action::FailOp => {
                Err(format!("step {i}: {action:?} with no operation in flight"))
            }
            Action::Hold(_)
            | Action::Release(_)
            | Action::StartReconcile
            | Action::StartStartup => {
                // The operation runs until the model is quiescent again; the
                // step that gets it there decides how it ended.
                let mut end = i + 1;
                while steps[end].0.pending != Pending::None {
                    end += 1;
                }
                let (last_pending_state, last_step) = &steps[end - 1];
                let last_step = last_step.clone().expect("a step ended the operation");
                let expected_calls = steps[i + 1..end]
                    .iter()
                    .filter(|(s, _)| calls_kernel(&s.pending))
                    .count();
                let toggle = match last_step {
                    Action::StepOp => Toggle::Succeed,
                    Action::FailOp => Toggle::Fail,
                    Action::Crash if calls_kernel(&last_pending_state.pending) => {
                        Toggle::CrashBefore
                    }
                    Action::Crash => Toggle::CrashAfter,
                    ref other => return Err(format!("step {i}: operation ended by {other:?}")),
                };
                *real.toggle.lock().unwrap() = toggle;
                real.toggle_calls.store(0, Ordering::SeqCst);

                let coordinator = &real.coordinator;
                let alive = &real.alive;
                let result = panic::catch_unwind(AssertUnwindSafe(|| match action {
                    Action::Hold(p) => {
                        alive.lock().unwrap().insert(p);
                        coordinator.hold(process_id(p)).map(drop)
                    }
                    Action::Release(p) => coordinator.release(process_id(p)),
                    Action::StartReconcile => coordinator.reconcile(),
                    Action::StartStartup => coordinator.reconcile_startup(),
                    _ => unreachable!(),
                }));
                let outcome = match result {
                    Ok(Ok(())) => Outcome::Ok,
                    Ok(Err(_)) => Outcome::Err,
                    Err(payload) if payload.is::<SimulatedCrash>() => Outcome::Crashed,
                    Err(payload) => panic::resume_unwind(payload),
                };
                let expected = match (&last_step, expected_calls) {
                    (Action::FailOp, _) => Outcome::Err,
                    (Action::Crash, n) if n > 0 => Outcome::Crashed,
                    _ => Outcome::Ok,
                };
                if outcome != expected {
                    return Err(format!(
                        "step {i}: {action:?} ended {outcome:?}, model expects {expected:?}"
                    ));
                }
                let calls = real.toggle_calls.load(Ordering::SeqCst);
                if calls != expected_calls {
                    return Err(format!(
                        "step {i}: {action:?} toggled the kernel {calls} time(s), \
                         model expects {expected_calls}"
                    ));
                }
                if last_step == Action::Crash {
                    real.restart();
                }
                Ok(end)
            }
        }
    }

    /// Compares the real durable state with the model's.
    fn check(real: &Real, model: &State) -> Result<(), String> {
        let (holders, marker) = real.lockfile();
        let kernel = real.kernel_disabled.load(Ordering::SeqCst);
        if holders == model.lockfile && marker == model.marker && kernel == model.kernel_disabled {
            return Ok(());
        }
        Err(format!(
            "real code has holders {holders:?}, marker {marker}, kernel disabled {kernel}; \
             model has holders {:?}, marker {}, kernel disabled {}",
            model.lockfile, model.marker, model.kernel_disabled
        ))
    }

    /// Replays a whole model path, checking after every action.
    fn replay(real: &mut Real, steps: &[(State, Option<Action>)]) -> Result<(), String> {
        let mut i = 0;
        while steps[i].1.is_some() {
            let next = run_action(real, steps, i)?;
            check(real, &steps[next].0).map_err(|why| format!("after step {i}: {why}"))?;
            i = next;
        }
        Ok(())
    }

    /// Every way the model can leave quiescent state `state`: each action,
    /// and for an operation, each way its steps can play out until the model
    /// is quiescent again. Each is a path fragment ending in that state.
    fn transitions(model: &Coordinator, state: &State) -> Vec<Vec<(State, Option<Action>)>> {
        fn extend(
            model: &Coordinator,
            path: Vec<(State, Option<Action>)>,
            state: State,
            out: &mut Vec<Vec<(State, Option<Action>)>>,
        ) {
            let mut actions = Vec::new();
            if state.pending != Pending::None {
                model.actions(&state, &mut actions);
            }
            if actions.is_empty() {
                let mut path = path;
                path.push((state, None));
                out.push(path);
                return;
            }
            for action in actions {
                if let Some(next) = model.next_state(&state, action.clone()) {
                    let mut path = path.clone();
                    path.push((state.clone(), Some(action)));
                    extend(model, path, next, out);
                }
            }
        }
        let mut out = Vec::new();
        let mut actions = Vec::new();
        model.actions(state, &mut actions);
        for action in actions {
            if let Some(next) = model.next_state(state, action.clone()) {
                extend(model, vec![(state.clone(), Some(action))], next, &mut out);
            }
        }
        out
    }

    /// Replays `steps` to a quiescent state, then runs every transition out
    /// of it on a copy of the real state. Returns how many transitions ran and
    /// a description of each divergence.
    fn check_state_and_transitions(
        model: &Coordinator,
        steps: &[(State, Option<Action>)],
    ) -> (usize, Vec<String>) {
        let describe = |path: &[(State, Option<Action>)]| -> Vec<Action> {
            path.iter().filter_map(|(_, a)| a.clone()).collect()
        };
        let mut failures = Vec::new();
        let lock_path = temp_lock_path();
        let mut real = Real::new(lock_path.clone());
        let replayed = replay(&mut real, steps);
        let mut run = 0;
        match replayed {
            Err(why) => failures.push(format!("{why}\n  path: {:?}", describe(steps))),
            Ok(()) => {
                for transition in transitions(model, &steps.last().unwrap().0) {
                    run += 1;
                    let fork_path = temp_lock_path();
                    let mut fork = real.fork(fork_path.clone());
                    let result = run_action(&mut fork, &transition, 0).and_then(|end| {
                        check(&fork, &transition[end].0)
                            .map_err(|why| format!("after the last step: {why}"))
                    });
                    let _ = std::fs::remove_file(&fork_path);
                    if let Err(why) = result {
                        let mut path = describe(steps);
                        path.extend(describe(&transition));
                        failures.push(format!("{why}\n  path: {path:?}"));
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&lock_path);
        (run, failures)
    }

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_lock_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "caffeinate2_conformance_{}_{}.lock",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Every transition of the fixed single-process model must play out the
    /// same way in the real coordinator. The checker finds every reachable
    /// state with one path to it; each quiescent state's path is replayed,
    /// then every way out of that state is run on a copy of the real state.
    /// Replaying only the recorded paths would test each state once but miss
    /// transitions into it from elsewhere, which is where a wrong release
    /// decision showed up.
    #[test]
    fn real_coordinator_follows_the_model() {
        silence_simulated_crashes();
        let model = Coordinator {
            variant: Variant::Fixed,
            pids: vec![1, 2, 3],
        };
        let paths = Arc::new(Mutex::new(Vec::new()));
        let recorder = paths.clone();
        Coordinator {
            variant: Variant::Fixed,
            pids: vec![1, 2, 3],
        }
        .checker()
        .visitor(move |path: stateright::Path<State, Action>| {
            recorder.lock().unwrap().push(path.into_vec());
        })
        .spawn_bfs()
        .join();
        let paths = std::mem::take(&mut *paths.lock().unwrap());

        let quiescent: Vec<_> = paths
            .iter()
            .filter(|steps| steps.last().unwrap().0.pending == Pending::None)
            .collect();
        let states = quiescent.len();
        // Each replay is fsync-bound, and every state uses its own lockfiles,
        // so the states are spread across threads.
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
        let (transitions_run, mut failures) = std::thread::scope(|scope| {
            let model = &model;
            let handles: Vec<_> = quiescent
                .chunks(states.div_ceil(workers).max(1))
                .map(|chunk| {
                    scope.spawn(move || {
                        let mut run = 0;
                        let mut failures = Vec::new();
                        for steps in chunk {
                            let (n, mut f) = check_state_and_transitions(model, steps);
                            run += n;
                            failures.append(&mut f);
                        }
                        (run, failures)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).fold(
                (0, Vec::new()),
                |(run, mut all), (n, mut f)| {
                    all.append(&mut f);
                    (run + n, all)
                },
            )
        });
        eprintln!(
            "Conformance: {states} model states, {transitions_run} transitions out of them \
             run against the real coordinator, {} diverged.",
            failures.len()
        );
        assert!(states > 100, "too few states reached: {states}");
        failures.sort_by_key(String::len);
        assert!(
            failures.is_empty(),
            "the real coordinator diverged from the model {} time(s); shortest:\n{}",
            failures.len(),
            failures
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
