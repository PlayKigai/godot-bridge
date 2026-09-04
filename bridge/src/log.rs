use std::fmt::Arguments;
use std::io::Write;
use std::sync::LazyLock;

pub const ERROR: u8 = 0;
pub const WARN: u8 = 1;
pub const DEBUG: u8 = 2;

static MAX_LEVEL: LazyLock<u8> =
    LazyLock::new(|| match std::env::var("GODOT_BRIDGE_LOG").as_deref() {
        Ok("error") => ERROR,
        Ok("debug") => DEBUG,
        _ => WARN,
    });

pub fn write(level: u8, message: Arguments) {
    if level > *MAX_LEVEL {
        return;
    }
    let label = match level {
        ERROR => "ERROR",
        WARN => "WARN",
        _ => "DEBUG",
    };
    let _ = writeln!(std::io::stderr(), "{label} {message}");
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::ERROR, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::WARN, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::DEBUG, format_args!($($arg)*))
    };
}
