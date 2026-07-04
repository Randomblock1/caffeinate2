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
