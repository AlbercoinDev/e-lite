//! A single emission format for debug/trace records in every crate.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

fn utc_timestamp(at: SystemTime) -> Option<String> {
    let seconds = libc::time_t::try_from(at.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()?;
    // gmtime_r uses caller-owned storage and works across concurrent loggers.
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    if unsafe { libc::gmtime_r(&seconds, tm.as_mut_ptr()) }.is_null() {
        return None;
    }
    let tm = unsafe { tm.assume_init() };
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    ))
}

#[doc(hidden)]
pub fn diagnostic(level: &str, args: fmt::Arguments<'_>) {
    let at = utc_timestamp(SystemTime::now());
    // Preserve one write per event, before the console/file tee. Do not
    // label the event with an invented date if the system clock is invalid.
    match at {
        Some(timestamp) => eprintln!("{timestamp} [e-lite] {level}: {args}"),
        None => eprintln!("[e-lite] {level}: {args}"),
    }
}

/// Emit a debug event. The caller owns log-level gating.
#[macro_export]
macro_rules! debug_event {
    ($($arg:tt)*) => {
        $crate::logging::diagnostic("debug", ::std::format_args!($($arg)*))
    };
}

/// Emit a trace event. The caller owns log-level gating.
#[macro_export]
macro_rules! trace_event {
    ($($arg:tt)*) => {
        $crate::logging::diagnostic("trace", ::std::format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamp_uses_utc_and_seconds() {
        let time = UNIX_EPOCH + Duration::from_millis(1_704_067_259_999);
        assert_eq!(utc_timestamp(time).as_deref(), Some("2024-01-01T00:00:59Z"));
        assert!(utc_timestamp(UNIX_EPOCH - Duration::from_secs(1)).is_none());
    }
}
