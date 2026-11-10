//! The process's own disk counters, for `SL_TRAY_DEBUG` traces.
//!
//! The same numbers Activity Monitor's "Bytes Read" / "Bytes Written" columns
//! show, read with `proc_pid_rusage` on this process -- which needs no
//! privileges -- so the cost of a refresh is measured rather than estimated.

pub use sl_macos::process::DiskIo;

/// The counters now, or zeroes if the kernel will not say.
pub fn sample() -> DiskIo {
    sl_macos::process::disk_io()
}

/// `512 B`, `14.2 KB`, `3.1 MB`, `1.20 GB`.
pub fn fmt_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / KB / KB)
    } else {
        format!("{:.2} GB", b / KB / KB / KB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counters_are_readable_and_only_go_up() {
        let before = sample();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("written"), vec![0u8; 1 << 20]).unwrap();
        let after = sample();
        assert!(after.read >= before.read && after.written >= before.written);
    }

    #[test]
    fn byte_counts_read_at_a_glance() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(14_540), "14.2 KB");
        assert_eq!(fmt_bytes(3_250_000), "3.1 MB");
        assert_eq!(fmt_bytes(1_288_490_189), "1.20 GB");
    }
}
