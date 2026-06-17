use crate::lockfile::ProcessStartTime;
use libc::{PROC_PIDTBSDINFO, proc_bsdinfo, proc_pidinfo};
use nix::sys::signal::kill;
use nix::unistd::Pid;

pub fn get_process_start_time(pid: i32) -> Option<ProcessStartTime> {
    unsafe {
        let mut info = std::mem::zeroed::<proc_bsdinfo>();
        let size = std::mem::size_of::<proc_bsdinfo>() as i32;
        let ret = proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut _,
            size,
        );
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

pub fn default_process_checker(pid: i32, start_time: ProcessStartTime) -> bool {
    let is_alive = match kill(Pid::from_raw(pid), None) {
        Ok(_) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => true,
    };

    if !is_alive {
        return false;
    }

    match get_process_start_time(pid) {
        Some(actual_start_time) => actual_start_time == start_time,
        None => {
            // Fail open: kill(pid, 0) already showed a live process, and
            // dropping a live holder on a transient proc_pidinfo failure would
            // incorrectly re-enable sleep. This only weakens recycled-pid
            // detection until the next reconcile.
            eprintln!(
                "warning: could not read start time for live pid {pid}; keeping holder for now"
            );
            true
        }
    }
}

pub fn process_id_from_pid(pid: i32) -> Result<crate::lockfile::ProcessId, std::io::Error> {
    let start_time = get_process_start_time(pid)
        .ok_or_else(|| std::io::Error::other("Failed to determine process start time"))?;
    Ok(crate::lockfile::ProcessId { pid, start_time })
}
