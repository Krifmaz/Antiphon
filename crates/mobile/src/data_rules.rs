//! The phone's battery and data rules: when a file transfer may run, when
//! to save data, and whether to keep listening for calls in the
//! background.
//!
//! Plain functions over plain values, kept apart from the interface and
//! the platform calls so the rules themselves are testable on any
//! machine. The phone code reads the network and battery state, hands it
//! in here, and does what comes back.

/// What the phone is connected through, as far as these rules care.
///
/// A copy of `platform::Network` rather than that type itself, because
/// the platform module only exists on a phone and these rules are tested
/// on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Link {
    /// The platform cannot tell. Treated as unmetered: a phone whose shell
    /// does not report the network would otherwise hold every transfer
    /// forever, and a rule that silently never lets anything through is
    /// worse than one that occasionally uses data.
    #[default]
    Unknown,
    /// No network at all; transfers wait.
    Offline,
    /// An unmetered network, where nothing is held.
    Wifi,
    /// Mobile data, or any network the system marks as metered.
    Metered,
}

/// The user's data settings, as the rules need them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataSettings {
    /// Hold file transfers while on a metered network.
    pub wifi_only: bool,
    /// Most mobile data file transfers may use in a calendar month, in
    /// megabytes; 0 for no limit.
    pub cellular_limit_mb: u32,
    /// Lower the call's data use and skip thumbnails while metered.
    pub data_saver: bool,
}

/// One megabyte, the unit the data limit setting is in.
const MB: u64 = 1024 * 1024;

/// The upload cap a transfer gets on a metered network with the data
/// saver on, in KB/s. Slow enough that a big file does not eat a data plan
/// in minutes, fast enough that a photo still arrives during the call.
pub const SAVER_UPLOAD_KBPS: u32 = 64;

/// Why a transfer is being held rather than sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// The phone has no network at all.
    Offline,
    /// On mobile data while "Files only on Wi-Fi" is on.
    WaitingForWifi,
    /// On mobile data with this month's file allowance used up.
    CellularLimit,
}

impl Hold {
    /// One sentence for the queue, saying what will make it move again.
    pub fn explain(self) -> &'static str {
        match self {
            Hold::Offline => "Waiting for a connection.",
            Hold::WaitingForWifi => "Waiting for Wi-Fi. Turn off \"Files only on Wi-Fi\" in Settings to send over mobile data.",
            Hold::CellularLimit => "This month's mobile data limit for files is used up. It resets next month, or raise it in Settings.",
        }
    }
}

/// Whether file transfers must wait, and why. `used_bytes` is what
/// transfers have already used on mobile data this month.
pub fn transfer_hold(settings: DataSettings, link: Link, used_bytes: u64) -> Option<Hold> {
    match link {
        Link::Offline => Some(Hold::Offline),
        Link::Metered if settings.wifi_only => Some(Hold::WaitingForWifi),
        Link::Metered if settings.cellular_limit_mb > 0 && used_bytes >= settings.cellular_limit_mb as u64 * MB => {
            Some(Hold::CellularLimit)
        }
        _ => None,
    }
}

/// Whether to save data right now: drop the spare audio copy, skip
/// thumbnails, cap uploads.
pub fn data_saver_active(settings: DataSettings, link: Link) -> bool {
    settings.data_saver && link == Link::Metered
}

/// The file upload cap for now, in KB/s, or 0 for none of our own (the
/// in-call throttle from `transfer_limits` still applies on top).
pub fn upload_cap_kbps(settings: DataSettings, link: Link) -> u32 {
    if data_saver_active(settings, link) {
        SAVER_UPLOAD_KBPS
    } else {
        0
    }
}

/// Whether to keep waiting for calls once the app is in the background.
///
/// Listening holds a socket open and keeps the phone from sleeping
/// properly, which is exactly what battery saver asks apps to stop doing;
/// so saver wins unless the user said otherwise for this app.
pub fn listen_in_background(keep_listening: bool, battery_saver: bool, ignore_saver: bool) -> bool {
    keep_listening && (!battery_saver || ignore_saver)
}

/// Mobile data used by transfers in the current calendar month.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CellularMeter {
    /// The month counted, as `YYYYMM`.
    pub month: u32,
    /// Bytes counted so far in `month`.
    pub used_bytes: u64,
}

impl CellularMeter {
    /// Bytes used this month; a count from an earlier month reads as zero.
    pub fn used(&self, now_secs: u64) -> u64 {
        if self.month == month_of(now_secs) {
            self.used_bytes
        } else {
            0
        }
    }

    /// Counts `bytes` of mobile data against the month `now_secs` (Unix
    /// seconds) falls in, starting the count afresh when the month has
    /// changed since the last call.
    pub fn add(&mut self, bytes: u64, now_secs: u64) {
        let month = month_of(now_secs);
        if self.month != month {
            *self = Self { month, used_bytes: 0 };
        }
        self.used_bytes = self.used_bytes.saturating_add(bytes);
    }
}

/// The calendar month a Unix time falls in, as `YYYYMM`, in UTC.
///
/// UTC and calendar months rather than the carrier's billing cycle, which
/// the app has no way to know; the setting says "per month" and means it
/// roughly, as a guard against surprises rather than a bill.
pub fn month_of(unix_secs: u64) -> u32 {
    // Howard Hinnant's days-to-civil algorithm, for dates after 1970.
    let days = (unix_secs / 86_400) as i64 + 719_468;
    let era = days / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year * 100 + month) as u32
}

/// How long ago something happened, in the words a phone uses.
pub fn ago(now_secs: u64, then_secs: u64) -> String {
    let secs = now_secs.saturating_sub(then_secs);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{} min ago", secs / 60),
        3_600..=86_399 => format!("{} h ago", secs / 3_600),
        86_400..=172_799 => "yesterday".to_string(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// A call's length as `m:ss`, or `h:mm:ss` past an hour.
pub fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3_600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A byte count for people: "820 KB", "4.2 MB".
pub fn size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{} KB", bytes / 1024)
    } else if bytes < 1024 * MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.2} GB", bytes as f64 / (1024 * MB) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ON: DataSettings = DataSettings { wifi_only: true, cellular_limit_mb: 10, data_saver: true };

    #[test]
    fn wifi_only_holds_on_mobile_data_and_nowhere_else() {
        assert_eq!(transfer_hold(ALL_ON, Link::Metered, 0), Some(Hold::WaitingForWifi));
        assert_eq!(transfer_hold(ALL_ON, Link::Wifi, 0), None);
        // A platform that cannot tell must not hold everything forever.
        assert_eq!(transfer_hold(ALL_ON, Link::Unknown, 0), None);
        assert_eq!(transfer_hold(ALL_ON, Link::Offline, 0), Some(Hold::Offline));
    }

    #[test]
    fn the_cellular_limit_applies_only_when_mobile_data_is_allowed() {
        let settings = DataSettings { wifi_only: false, ..ALL_ON };
        assert_eq!(transfer_hold(settings, Link::Metered, 10 * MB - 1), None);
        assert_eq!(transfer_hold(settings, Link::Metered, 10 * MB), Some(Hold::CellularLimit));
        // Wi-Fi does not count against it.
        assert_eq!(transfer_hold(settings, Link::Wifi, 100 * MB), None);
        // 0 means no limit.
        let unlimited = DataSettings { cellular_limit_mb: 0, ..settings };
        assert_eq!(transfer_hold(unlimited, Link::Metered, u64::MAX), None);
    }

    #[test]
    fn the_data_saver_only_acts_on_metered_networks() {
        assert!(data_saver_active(ALL_ON, Link::Metered));
        assert!(!data_saver_active(ALL_ON, Link::Wifi));
        assert!(!data_saver_active(DataSettings { data_saver: false, ..ALL_ON }, Link::Metered));
        assert_eq!(upload_cap_kbps(ALL_ON, Link::Metered), SAVER_UPLOAD_KBPS);
        assert_eq!(upload_cap_kbps(ALL_ON, Link::Wifi), 0);
    }

    #[test]
    fn battery_saver_stops_background_listening_unless_overridden() {
        assert!(listen_in_background(true, false, false));
        assert!(!listen_in_background(true, true, false));
        assert!(listen_in_background(true, true, true));
        assert!(!listen_in_background(false, false, true));
    }

    #[test]
    fn months_are_counted_in_utc_calendar_months() {
        assert_eq!(month_of(0), 197_001);
        // 2024-02-29 12:00 UTC, a leap day.
        assert_eq!(month_of(1_709_208_000), 202_402);
        // 2024-03-01 00:00 UTC.
        assert_eq!(month_of(1_709_251_200), 202_403);
        // 2026-12-31 23:59:59 UTC, then the next second.
        assert_eq!(month_of(1_798_761_599), 202_612);
        assert_eq!(month_of(1_798_761_600), 202_701);
    }

    #[test]
    fn the_meter_starts_again_each_month() {
        let feb = 1_709_208_000;
        let march = 1_709_251_200;
        let mut meter = CellularMeter::default();
        meter.add(500, feb);
        meter.add(500, feb);
        assert_eq!(meter.used(feb), 1000);
        assert_eq!(meter.used(march), 0);
        meter.add(7, march);
        assert_eq!(meter.used(march), 7);
    }

    #[test]
    fn times_and_sizes_read_naturally() {
        assert_eq!(ago(1000, 990), "just now");
        assert_eq!(ago(10_000, 10_000 - 125), "2 min ago");
        assert_eq!(ago(100_000, 100_000 - 7_200), "2 h ago");
        assert_eq!(ago(1_000_000, 1_000_000 - 90_000), "yesterday");
        assert_eq!(ago(1_000_000, 1_000_000 - 3 * 86_400), "3 days ago");
        // A clock that moved backwards is not "in the future".
        assert_eq!(ago(0, 50), "just now");
        assert_eq!(clock(65), "1:05");
        assert_eq!(clock(3_723), "1:02:03");
        assert_eq!(size(900), "900 B");
        assert_eq!(size(2048), "2 KB");
        assert_eq!(size(5 * MB / 2), "2.5 MB");
    }
}
