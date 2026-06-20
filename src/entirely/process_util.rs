use crate::entirely::lockfile::ProcessStartTime;
use libc::{PROC_PIDTBSDINFO, proc_bsdinfo, proc_pidinfo};
use nix::sys::signal::kill;
use nix::unistd::Pid;

#[must_use]
pub fn get_process_start_time(pid: i32) -> Option<ProcessStartTime> {
    unsafe {
        let mut info = std::mem::zeroed::<proc_bsdinfo>();
        let size = i32::try_from(std::mem::size_of::<proc_bsdinfo>()).unwrap_or(i32::MAX);
        let ret = proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size);
        if ret == size {
            Some(ProcessStartTime {
                seconds: info.pbi_start_tvsec,
                microseconds: info.pbi_start_tvusec,
            })
        } else {
            None
        }
    }
}

#[must_use]
pub fn default_process_checker(pid: i32, start_time: ProcessStartTime) -> bool {
    let is_alive = !matches!(
        kill(Pid::from_raw(pid), None),
        Err(nix::errno::Errno::ESRCH)
    );

    if !is_alive {
        return false;
    }

    get_process_start_time(pid).map_or_else(
        || {
            // Fail open: kill(pid, 0) already showed a live process, and
            // dropping a live holder on a transient proc_pidinfo failure would
            // incorrectly re-enable sleep. This only weakens recycled-pid
            // detection until the next reconcile.
            eprintln!(
                "warning: could not read start time for live pid {pid}; keeping holder for now"
            );
            true
        },
        |actual_start_time| actual_start_time == start_time,
    )
}

///
/// # Errors
///
/// Returns an error if the process start time cannot be determined.
pub fn process_id_from_pid(
    pid: i32,
) -> Result<crate::entirely::lockfile::ProcessId, std::io::Error> {
    let start_time = get_process_start_time(pid)
        .ok_or_else(|| std::io::Error::other("Failed to determine process start time"))?;
    Ok(crate::entirely::lockfile::ProcessId { pid, start_time })
}
