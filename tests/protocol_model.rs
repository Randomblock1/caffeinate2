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
//! The single safety property is the same convergence oracle the property tests
//! use: once a reconcile runs to completion, the kernel's SleepDisabled bit must
//! equal "there is at least one live holder".
//!
//! A second model, [`CrossProcess`], covers the CLI-fallback deployment: with
//! no helper installed, two root CLI *processes* share the lockfile and the
//! `ops` mutex is per-process, so the decomposed steps of their operations
//! interleave freely (each flock'd lockfile mutation stays atomic, but one
//! process's kernel toggle and follow-up marker write may land between the
//! other's mutations). It compares the two post-toggle marker-write designs:
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
//! sequences are a strict subset of reconcile's. The cross-process model in
//! turn omits the startup-only legacy `had_entries` fallback (it only widens
//! the Enable condition). Mid-operation process death is covered by `Crash`:
//! a CLI-fallback process SIGKILLed between the atomic steps of its operation
//! — including while the other process is also mid-operation — abandons its
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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Pending {
    None,
    /// hold(): holder+marker written; the kernel disable is still to come.
    HoldToggle {
        first: bool,
    },
    /// release(): holder removed (marker left set); the kernel enable is still
    /// to come.
    ReleaseToggle {
        enable: bool,
    },
    /// release(): kernel re-enabled; clearing the marker is still to come.
    ReleaseClear {
        enable: bool,
    },
    /// reconcile(): pruned and decided; the kernel toggle is still to come.
    ReconcileToggle {
        effect: Effect,
    },
    /// reconcile(): kernel toggled; persisting the marker is still to come.
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
            needs_startup: true,
        }
    }
}

/// Live holders: lockfile entries whose process is still alive.
fn live_count(s: &State) -> usize {
    s.lockfile.iter().filter(|p| s.alive.contains(p)).count()
}

/// Prune dead holders (the model's equivalent of `prune_stale_holders`): retain
/// only entries whose process is alive.
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
    /// Helper SIGKILL: abandon any in-flight op (and, for Buggy, lose the cache).
    Crash,
}

struct Coordinator {
    variant: Variant,
    pids: Vec<Pid>,
}

const CONVERGES: &str = "reconcile converges sleep to live holders";

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
        }
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut s = last.clone();
        // Any action other than completing a reconcile leaves us in a
        // not-just-reconciled state; the reconcile-completion step sets it true.
        s.just_reconciled = false;

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
                // acquire(): the caller is by definition alive.
                s.alive.insert(p);
                prune(&mut s);
                let first = s.lockfile.is_empty();
                s.lockfile.insert(p);
                if first && self.variant == Variant::Fixed {
                    // Marker written in the SAME write that adds the holder,
                    // before the disable below.
                    s.marker = true;
                }
                s.pending = Pending::HoldToggle { first };
            }

            Action::Release(p) => {
                prune(&mut s);
                let enable = match self.variant {
                    // Fixed: re-enable when no live holders remain and we own the
                    // disable — regardless of whether this caller held.
                    Variant::Fixed => {
                        s.lockfile.remove(&p);
                        s.lockfile.is_empty() && s.marker
                    }
                    // Buggy: only when this caller removed a live holder and none
                    // remain.
                    Variant::Buggy => {
                        let removed = s.lockfile.remove(&p);
                        removed && s.lockfile.is_empty()
                    }
                };
                s.pending = Pending::ReleaseToggle { enable };
            }

            Action::StartReconcile | Action::StartStartup => {
                let startup = action == Action::StartStartup;
                if startup {
                    // Boot-time recovery has now run; the periodic reaper may run
                    // from here (until the next crash).
                    s.needs_startup = false;
                }
                let had_entries = !s.lockfile.is_empty();
                prune(&mut s);
                let live = s.lockfile.len();
                let owns = match self.variant {
                    Variant::Fixed => s.marker,
                    Variant::Buggy => s.cache_owns,
                };
                let effect = if live > 0 {
                    Effect::Disable
                } else if owns || (startup && had_entries) {
                    // `had_entries` is the legacy startup-only fallback for
                    // lockfiles written before the marker existed.
                    Effect::Enable
                } else {
                    Effect::Nothing
                };
                s.pending = Pending::ReconcileToggle { effect };
            }

            Action::StepOp => match last.pending {
                Pending::None => return None,

                Pending::HoldToggle { first } => {
                    if first {
                        s.kernel_disabled = true;
                        if self.variant == Variant::Buggy {
                            s.cache_owns = true;
                        }
                    }
                    s.pending = Pending::None;
                }

                Pending::ReleaseToggle { enable } => {
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
                        Variant::Buggy => Pending::None,
                    };
                }

                Pending::ReleaseClear { enable } => {
                    if enable {
                        s.marker = false;
                    }
                    s.pending = Pending::None;
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
        vec![Property::<Self>::always(CONVERGES, |_, s| {
            // Only meaningful once a reconcile has completed. Mid-operation and
            // post-crash windows are transient and healed by the next reconcile.
            if !s.just_reconciled {
                return true;
            }
            s.kernel_disabled == (live_count(s) > 0)
        })]
    }
}

/// The fixed protocol must satisfy convergence across *every* interleaving,
/// including crashes between the write, the kernel toggle, and the marker clear.
#[test]
fn fixed_protocol_converges_under_all_interleavings() {
    let checker = Coordinator {
        variant: Variant::Fixed,
        pids: vec![1, 2, 3],
    }
    .checker()
    .spawn_bfs()
    .join();
    // Panics with a counterexample path if convergence is ever violated.
    checker.assert_properties();
    eprintln!(
        "Fixed protocol: {} states explored, no convergence violation.",
        checker.unique_state_count()
    );
}

/// Sanity check that the checker has teeth: the pre-fix protocol must produce a
/// convergence counterexample (the lost-disable-intent bug). If this ever stops
/// finding one, the model has drifted into vacuously passing.
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
        path.into_actions()
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
/// step, and the follow-up marker mutation is a second lockfile write.
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
    HoldToggle,
    /// release(): holder removed, re-enable decided; the kernel enable is to come.
    ReleaseToggle {
        gen_valid: bool,
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
    pending: [ProcPending; 2],
    /// interfered[i]: the other process mutated shared state while process i's
    /// current operation was in flight, so i's locked-in decision may be stale
    /// and its completion is not a convergence checkpoint.
    interfered: [bool; 2],
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
    /// interference-free and with the other process idle. Only there is
    /// convergence promised — a reconcile finishing while the other process is
    /// mid-operation may observe a state that op's remaining steps (e.g. its
    /// pending marker write) are about to repair.
    just_reconciled: bool,
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

fn cross_live_count(s: &CrossState) -> usize {
    s.lockfile.iter().filter(|p| s.alive.contains(p)).count()
}

fn cross_prune(s: &mut CrossState) {
    let alive = &s.alive;
    s.lockfile.retain(|p| alive.contains(p));
}

/// A marker set bumps the persisted ownership generation, invalidating every
/// in-flight re-enable decision that captured the previous one.
fn invalidate_generations(s: &mut CrossState) {
    for pending in &mut s.pending {
        match pending {
            ProcPending::ReleaseToggle { gen_valid }
            | ProcPending::ReleaseClear { gen_valid }
            | ProcPending::StatusToggle { gen_valid }
            | ProcPending::StatusClear { gen_valid }
            | ProcPending::ReconcileToggle { gen_valid, .. }
            | ProcPending::ReconcileMarker { gen_valid, .. } => *gen_valid = false,
            ProcPending::None | ProcPending::HoldToggle => {}
        }
    }
}

impl CrossProcess {
    /// The post-toggle marker clear. Guarded re-checks the holder set and the
    /// ownership generation inside the same flock mutation; Unconditional is
    /// the pre-fix clear.
    fn clear_marker(&self, s: &mut CrossState, gen_valid: bool) {
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
            pending: [ProcPending::None; 2],
            interfered: [false; 2],
            orphaned_disable: false,
            just_reconciled: false,
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        for who in 0..2 {
            if state.pending[who] == ProcPending::None {
                let who = u8::try_from(who).unwrap();
                actions.push(CrossAction::Hold(who));
                actions.push(CrossAction::Release(who));
                actions.push(CrossAction::Reconcile(who));
                actions.push(CrossAction::Status(who));
            } else {
                let who = u8::try_from(who).unwrap();
                actions.push(CrossAction::StepOp(who));
                actions.push(CrossAction::Crash(who));
            }
        }
        if state.pending.iter().all(|p| *p == ProcPending::None) {
            for who in 0..2 {
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

        // Any move by one process while the other has an operation in flight
        // taints that operation's locked-in decision. Over-approximate (taint
        // on every move, mutating or not): a skipped assertion is sound, and a
        // later solo reconcile still has to converge from whatever state the
        // interleaving produced.
        let taint_other_of = |s: &mut CrossState, who: usize| {
            let other = 1 - who;
            if s.pending[other] != ProcPending::None {
                s.interfered[other] = true;
            }
        };

        match action {
            CrossAction::Kill(p) => {
                // Only offered when both processes are quiescent.
                s.alive.remove(&p);
            }

            CrossAction::Crash(who) => {
                // Only offered while `who`'s operation is in flight. Its
                // remaining steps die with the process (no marker set, no
                // generation bump), and its own lockfile entry goes stale.
                // The survivor's in-flight decision now rests on stale
                // liveness, so it is tainted like any other concurrent move.
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
                taint_other_of(&mut s, who);
            }

            CrossAction::Hold(who) => {
                let who = usize::from(who);
                let p = pid_of(who);
                s.alive.insert(p);
                cross_prune(&mut s);
                let first = s.lockfile.is_empty();
                s.lockfile.insert(p);
                if first {
                    // Marker written (and the generation bumped) in the same
                    // flock write that adds the holder; the kernel disable is a
                    // later step. The fresh record owns any standing disable,
                    // ending an orphaned-disable window.
                    s.marker = true;
                    invalidate_generations(&mut s);
                    s.orphaned_disable = false;
                    s.pending[who] = ProcPending::HoldToggle;
                    s.interfered[who] = false;
                }
                taint_other_of(&mut s, who);
            }

            CrossAction::Release(who) => {
                let who = usize::from(who);
                cross_prune(&mut s);
                s.lockfile.remove(&pid_of(who));
                if s.lockfile.is_empty() && s.marker {
                    s.pending[who] = ProcPending::ReleaseToggle { gen_valid: true };
                    s.interfered[who] = false;
                }
                taint_other_of(&mut s, who);
            }

            CrossAction::Reconcile(who) => {
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
                    // if the other process is idle.
                    s.just_reconciled = s.pending[1 - who] == ProcPending::None;
                }
                taint_other_of(&mut s, who);
            }

            CrossAction::Status(who) => {
                let who = usize::from(who);
                cross_prune(&mut s);
                if s.lockfile.is_empty() && s.marker {
                    s.pending[who] = ProcPending::StatusToggle { gen_valid: true };
                    s.interfered[who] = false;
                }
                taint_other_of(&mut s, who);
            }

            CrossAction::StepOp(who) => {
                let who = usize::from(who);
                match last.pending[who] {
                    ProcPending::None => return None,

                    ProcPending::HoldToggle => {
                        s.kernel_disabled = true;
                        s.pending[who] = ProcPending::None;
                    }

                    ProcPending::ReleaseToggle { gen_valid } => {
                        s.kernel_disabled = false;
                        s.pending[who] = ProcPending::ReleaseClear { gen_valid };
                    }

                    ProcPending::ReleaseClear { gen_valid } => {
                        self.clear_marker(&mut s, gen_valid);
                        s.pending[who] = ProcPending::None;
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
                        s.just_reconciled =
                            !s.interfered[who] && s.pending[1 - who] == ProcPending::None;
                    }
                }
                taint_other_of(&mut s, who);
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
            // Teeth for the exemption above: the orphaned-disable window must
            // actually be reached, or the carve-out is dead weight and the
            // convergence check quietly weaker than it claims. (This also
            // fails the suite if the Crash action is ever dropped, instead of
            // silently losing the crash coverage.)
            Property::<Self>::sometimes(ORPHANED, |_, s| s.orphaned_disable),
        ]
    }
}

/// The fixed cross-process protocol (guarded marker writes) must converge under
/// every interleaving of two coordinator processes sharing the lockfile,
/// including mid-operation crashes — outside the documented orphaned-disable
/// window, whose reachability the [`ORPHANED`] `sometimes` property pins down.
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
        "Cross-process fixed protocol: {} states explored, no convergence violation.",
        checker.unique_state_count()
    );
}

/// Teeth: the pre-fix marker writes must produce a cross-process
/// counterexample — e.g. one process's post-enable marker clear destroying the
/// marker a concurrent first hold just set, leaving that hold's eventual
/// release with no evidence of ownership and sleep stranded disabled with zero
/// holders.
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
        path.into_actions()
    );
}
