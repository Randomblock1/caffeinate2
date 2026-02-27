
/// Updates the list of PIDs based on the current operation and liveness check.
///
/// # Arguments
///
/// * `pids` - The current list of PIDs from the lockfile.
/// * `current_pid` - The PID of the current process.
/// * `add` - Whether we are adding or removing the current PID.
/// * `verbose` - Whether to print verbose output.
/// * `check_alive` - A function that returns true if a PID is alive.
///
/// # Returns
///
/// A tuple containing:
/// * The updated list of PIDs.
/// * A boolean indicating if the lock state should toggle (i.e., from 0 to >0 or >0 to 0).
pub fn update_pid_list<F>(
    mut pids: Vec<i32>,
    current_pid: i32,
    add: bool,
    verbose: bool,
    check_alive: F,
) -> (Vec<i32>, bool)
where
    F: Fn(i32, bool) -> bool,
{
    // Filter out dead processes
    pids.retain(|&pid| {
        if pid == current_pid {
            return true;
        }
        check_alive(pid, verbose)
    });

    let active_count_before = pids.len();

    if add {
        if !pids.contains(&current_pid) {
            pids.push(current_pid);
        }
    } else {
        if let Some(pos) = pids.iter().position(|&x| x == current_pid) {
            pids.remove(pos);
        }
    }

    let active_count_after = pids.len();

    let should_toggle = if add {
        active_count_before == 0
    } else {
        active_count_after == 0
    };

    (pids, should_toggle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_pid_list_add_first() {
        let pids = vec![];
        let current_pid = 123;
        let add = true;
        let verbose = false;
        let check_alive = |_: i32, _: bool| true;

        let (new_pids, should_toggle) = update_pid_list(pids, current_pid, add, verbose, check_alive);

        assert_eq!(new_pids, vec![123]);
        assert!(should_toggle, "Should toggle when adding first PID");
    }

    #[test]
    fn test_update_pid_list_add_second() {
        let pids = vec![456];
        let current_pid = 123;
        let add = true;
        let verbose = false;
        let check_alive = |_: i32, _: bool| true;

        let (new_pids, should_toggle) = update_pid_list(pids, current_pid, add, verbose, check_alive);

        assert!(new_pids.contains(&123));
        assert!(new_pids.contains(&456));
        assert_eq!(new_pids.len(), 2);
        assert!(!should_toggle, "Should not toggle when adding subsequent PID");
    }

    #[test]
    fn test_update_pid_list_remove_last() {
        let pids = vec![123];
        let current_pid = 123;
        let add = false;
        let verbose = false;
        let check_alive = |_: i32, _: bool| true;

        let (new_pids, should_toggle) = update_pid_list(pids, current_pid, add, verbose, check_alive);

        assert!(new_pids.is_empty());
        assert!(should_toggle, "Should toggle when removing last PID");
    }

    #[test]
    fn test_update_pid_list_remove_intermediate() {
        let pids = vec![123, 456];
        let current_pid = 123;
        let add = false;
        let verbose = false;
        let check_alive = |_: i32, _: bool| true;

        let (new_pids, should_toggle) = update_pid_list(pids, current_pid, add, verbose, check_alive);

        assert_eq!(new_pids, vec![456]);
        assert!(!should_toggle, "Should not toggle when removing one of multiple PIDs");
    }

    #[test]
    fn test_update_pid_list_remove_dead() {
        let pids = vec![999];
        let current_pid = 123;
        let add = true;
        let verbose = false;
        // Mock 999 as dead
        let check_alive = |pid: i32, _: bool| pid != 999;

        let (new_pids, should_toggle) = update_pid_list(pids, current_pid, add, verbose, check_alive);

        assert_eq!(new_pids, vec![123]);
        // Before adding current, active count was 0 (since 999 was removed)
        assert!(should_toggle, "Should toggle because active count before add was effectively 0");
    }

    #[test]
    fn test_update_pid_list_current_pid_protection() {
        // Even if check_alive says current_pid is dead (which shouldn't happen in reality but good to test logic),
        // it should not be removed by the liveness check.
        let pids = vec![123];
        let current_pid = 123;
        let add = true; // Trying to add existing
        let verbose = false;
        let check_alive = |_: i32, _: bool| false; // Says everyone is dead

        let (new_pids, _) = update_pid_list(pids, current_pid, add, verbose, check_alive);

        assert_eq!(new_pids, vec![123]);
    }
}
