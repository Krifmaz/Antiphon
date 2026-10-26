//! `log` to the unified system log, through `NSLog`.
//!
//! An app SpringBoard starts has nowhere to send stderr: it is not shown
//! in Xcode's console or in Console.app. `NSLog` is, on the simulator and
//! on a device plugged into a Mac, under the process name `antiphon-ios`.

use log::{Level, LevelFilter, Log, Metadata, Record};
use objc2_foundation::{ns_string, NSString};

extern "C" {
    // Variadic, so not in objc2-foundation; Foundation is linked anyway.
    fn NSLog(format: *const NSString, ...);
}

struct NsLogger;

impl Log for NsLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = NSString::from_str(&format!("[{}] {}: {}", record.level(), record.target(), record.args()));
        // SAFETY: the format takes exactly one object argument, and the
        // message goes in as that argument rather than as the format, so
        // a `%` in it is printed rather than interpreted.
        unsafe { NSLog(ns_string!("%@"), &*line as *const NSString) };
    }

    fn flush(&self) {}
}

static LOGGER: NsLogger = NsLogger;

pub fn init() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}
