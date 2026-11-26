//! Whether a process is still the one a caller named. A process is
//! identified by its ID and its start time, so a later process that reuses
//! the ID is not mistaken for it.

/// The start time of process `pid` as an opaque token, or `None` when there
/// is no such process or it cannot be inspected. Two calls for the same
/// running process give the same token.
#[cfg(target_os = "linux")]
pub(crate) fn start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name, the second field, is in parentheses and may hold
    // spaces or parentheses itself; the fields after the last `)` are plain.
    let after = &stat[stat.rfind(')')? + 1..];
    let mut fields = after.split_ascii_whitespace();
    // `state` is field 3; the start time is field 22.
    let state = fields.next()?;
    if state == "Z" || state == "X" {
        return None;
    }
    fields.nth(18).map(str::to_owned)
}

#[cfg(target_os = "macos")]
pub(crate) fn start_time(pid: u32) -> Option<String> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: `info` is a writable buffer of `size` bytes for the call.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if written <= 0 {
        // Another user's process cannot be inspected, but it is running:
        // as elsewhere, only whether the ID is in use can be told.
        let denied = std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        return denied.then(String::new);
    }
    if written != size || info.pbi_status == libc::SZOMB {
        return None;
    }
    Some(format!(
        "{}.{:06}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}

#[cfg(windows)]
pub(crate) fn start_time(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the handle is checked before use and closed once.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let empty = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (empty, empty, empty, empty);
        let mut code = 0u32;
        let running = GetExitCodeProcess(handle, &mut code) != 0 && code == STILL_ACTIVE as u32;
        let timed = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        CloseHandle(handle);
        (running && timed).then(|| {
            (u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime)).to_string()
        })
    }
}

/// Elsewhere only whether the ID is in use can be told.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(crate) fn start_time(pid: u32) -> Option<String> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: signal 0 only checks that the process exists.
    let exists = unsafe { libc::kill(pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    exists.then(String::new)
}

/// Whether process `pid` is running and started at `start`.
pub(crate) fn alive(pid: u32, start: &str) -> bool {
    start_time(pid).is_some_and(|now| now == start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_a_finished_one_is_not() {
        let me = std::process::id();
        let start = start_time(me).expect("this process can be inspected");
        assert_eq!(start_time(me).as_deref(), Some(start.as_str()));
        assert!(alive(me, &start));
        assert!(!alive(me, "not the start time"));

        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                &["/C", "exit"][..]
            } else {
                &[][..]
            })
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        // Reaped, so the ID names no process unless the system reused it.
        if let Some(start) = start_time(pid) {
            assert!(!alive(pid, &format!("{start}-earlier")));
        }
        assert_eq!(start_time(u32::MAX), None);
    }

    /// Process 1, the system's first process, runs as the superuser. When
    /// the tests do not, it is another user's process, which may not be
    /// inspectable but is still running.
    #[cfg(target_os = "macos")]
    #[test]
    fn another_user_s_process_is_alive() {
        let start = start_time(1).expect("process 1 is running");
        assert!(alive(1, &start));
    }
}
