use crate::entirely::lockfile::ProcessStartTime;
use libproc::bsd_info::BSDInfo;
use libproc::proc_pid::pidinfo;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::time::Duration;

/// How many times to retry reading a live process's start time before giving
/// up and failing open. `proc_pidinfo` can fail transiently (e.g. EBUSY) for a
/// process that is very much alive, and dropping a live holder would wrongly
/// re-enable sleep, so we retry briefly before deciding.
const START_TIME_READ_ATTEMPTS: u32 = 3;
const START_TIME_RETRY_DELAY: Duration = Duration::from_millis(5);

#[must_use]
pub fn get_process_start_time(pid: i32) -> Option<ProcessStartTime> {
    pidinfo::<BSDInfo>(pid, 0).ok().map(|info| ProcessStartTime {
        seconds: info.pbi_start_tvsec,
        microseconds: info.pbi_start_tvusec,
    })
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

    // Retry a few times so a single transient proc_pidinfo failure doesn't
    // force a fail-open decision. A real start-time read settles recycled-pid
    // detection definitively; only persistent failure falls open.
    for attempt in 0..START_TIME_READ_ATTEMPTS {
        if let Some(actual_start_time) = get_process_start_time(pid) {
            return actual_start_time == start_time;
        }
        if attempt + 1 < START_TIME_READ_ATTEMPTS {
            std::thread::sleep(START_TIME_RETRY_DELAY);
        }
    }

    // Tradeoff: kill(pid, 0) already showed a live process, and dropping a live
    // holder on a transient proc_pidinfo failure would incorrectly re-enable
    // sleep. So after exhausting retries we fail open (keep the holder) and
    // accept weaker recycled-pid detection until the next reconcile, rather
    // than risk re-enabling sleep under an active hold.
    tracing::warn!(
        "could not read start time for live pid {pid} after \
         {START_TIME_READ_ATTEMPTS} attempts; keeping holder for now"
    );
    true
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
