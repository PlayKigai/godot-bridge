use std::fmt::Arguments;
use std::io::Write;
use std::sync::LazyLock;

pub const ERROR: u8 = 0;
pub const WARN: u8 = 1;
pub const INFO: u8 = 2;
pub const DEBUG: u8 = 3;

static MAX_LEVEL: LazyLock<u8> =
    LazyLock::new(|| match std::env::var("GODOT_BRIDGE_LOG").as_deref() {
        Ok("error") => ERROR,
        Ok("warn") => WARN,
        Ok("debug") => DEBUG,
        _ => INFO,
    });

pub fn write(level: u8, message: Arguments) {
    if level > *MAX_LEVEL {
        return;
    }
    let label = ["ERROR", "WARN", "INFO", "DEBUG"][level as usize];
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
