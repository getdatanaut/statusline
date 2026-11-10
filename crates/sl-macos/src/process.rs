//! The process's own resource counters.

/// Bytes this process has read from and written to disk since it started.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiskIo {
    pub read: u64,
    pub written: u64,
}

/// This process's disk counters, the numbers Activity Monitor shows; zero
/// when the kernel will not say.
pub fn disk_io() -> DiskIo {
    // SAFETY: `rusage_info_v2` is plain integers, so all-zero is a valid value.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: `proc_pid_rusage` writes at most one `RUSAGE_INFO_V2`-sized
    // struct into the buffer it is handed, which is exactly `info`. (Its
    // parameter is typed as a pointer to `rusage_info_t`, itself a `void *`;
    // the kernel treats it as the struct's address.)
    let status = unsafe {
        libc::proc_pid_rusage(
            std::process::id().cast_signed(),
            libc::RUSAGE_INFO_V2,
            (&raw mut info).cast::<libc::rusage_info_t>(),
        )
    };
    if status != 0 {
        return DiskIo::default();
    }
    DiskIo {
        read: info.ri_diskio_bytesread,
        written: info.ri_diskio_byteswritten,
    }
}

/// When process `pid` started, in whole seconds since the epoch; `None` when
/// there is no such process, or it is not this user's to ask about.
pub fn started_at(pid: u32) -> Option<u64> {
    let pid = i32::try_from(pid).ok().filter(|&pid| pid > 0)?;
    // SAFETY: `proc_bsdinfo` is plain integers and byte arrays, so all-zero
    // is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `PROC_PIDTBSDINFO` writes at most `size` bytes, one
    // `proc_bsdinfo`, into the buffer it is handed, which is exactly `info`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast::<libc::c_void>(),
            size,
        )
    };
    (written == size).then_some(info.pbi_start_tvsec)
}

#[cfg(test)]
mod tests {
    use super::started_at;

    #[test]
    fn this_process_started_in_the_past_and_a_missing_one_never() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mine = started_at(std::process::id()).expect("our own start time");
        assert!(
            mine <= now && mine + 3600 > now,
            "started at {mine}, now {now}"
        );
        assert_eq!(
            started_at(0),
            None,
            "pid 0 is not a process we can ask about"
        );
        assert_eq!(started_at(u32::MAX), None);
        assert_eq!(started_at(99_999_999), None, "beyond the largest pid");
    }
}
