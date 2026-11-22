//! Local wall-clock time for save subjects, history previews and temporary
//! ref names.
//!
//! Jiff's system time zone lookup covers almost every setup. This fills its
//! two gaps so kite shows the same time as the user's clock:
//!
//! - `TZ` set to an absolute TZif path reads that file. Jiff matches any path
//!   containing `zoneinfo/` against the system database by name, which picks
//!   the wrong zone when the file differs from its namesake.
//! - On Windows with `TZ` unset, Windows itself converts each instant. Jiff
//!   maps the Windows zone to its IANA zone and applies the IANA rules, so it
//!   misses Windows settings such as daylight saving turned off or custom
//!   transition dates.

use std::ffi::OsStr;
use std::path::Path;

use jiff::{Timestamp, ZonedDateTime, tz::TimeZone};

/// The current local time.
pub(crate) fn now() -> ZonedDateTime {
    local(Timestamp::now())
}

/// `time` in local time.
pub(crate) fn local(time: Timestamp) -> ZonedDateTime {
    let zone = match std::env::var_os("TZ") {
        Some(tz) => tzif_file(&tz),
        None => windows_offset(time).map(TimeZone::fixed),
    };
    time.to_zoned_datetime(zone.unwrap_or_else(TimeZone::system))
}

/// The zone in the TZif file `TZ` names by absolute path, with or without the
/// leading `:`. Names, POSIX rules and relative paths are left to Jiff.
fn tzif_file(tz: &OsStr) -> Option<TimeZone> {
    let tz = tz.to_str()?;
    let path = tz.strip_prefix(':').unwrap_or(tz);
    if !Path::new(path).is_absolute() {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    TimeZone::tzif(path, &data).ok()
}

/// The UTC offset the Windows clock uses at `time`, under the current Windows
/// time zone settings.
#[cfg(windows)]
fn windows_offset(time: Timestamp) -> Option<jiff::tz::Offset> {
    use windows_sys::Win32::System::Time::{
        DYNAMIC_TIME_ZONE_INFORMATION, GetDynamicTimeZoneInformation, TIME_ZONE_ID_INVALID,
    };

    let mut zone = DYNAMIC_TIME_ZONE_INFORMATION::default();
    // SAFETY: `zone` is a valid, writable value of the type the call fills in.
    if unsafe { GetDynamicTimeZoneInformation(&mut zone) } == TIME_ZONE_ID_INVALID {
        return None;
    }
    windows_offset_in(&zone, time)
}

/// The UTC offset Windows gives `time` in `zone`.
#[cfg(windows)]
fn windows_offset_in(
    zone: &windows_sys::Win32::System::Time::DYNAMIC_TIME_ZONE_INFORMATION,
    time: Timestamp,
) -> Option<jiff::tz::Offset> {
    use jiff::civil::DateTime;
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::SystemTimeToTzSpecificLocalTimeEx;

    let utc = Timestamp::from_second(time.as_second())
        .ok()?
        .to_zoned_datetime(TimeZone::UTC)
        .datetime();
    let universal = SYSTEMTIME {
        wYear: u16::try_from(utc.year()).ok()?,
        wMonth: u16::try_from(utc.month()).ok()?,
        wDay: u16::try_from(utc.day()).ok()?,
        wHour: u16::try_from(utc.hour()).ok()?,
        wMinute: u16::try_from(utc.minute()).ok()?,
        wSecond: u16::try_from(utc.second()).ok()?,
        ..SYSTEMTIME::default()
    };
    let mut local = SYSTEMTIME::default();
    // SAFETY: every pointer is to a valid value of the type the call reads or
    // fills in, and `local` is only read after a successful conversion.
    if unsafe { SystemTimeToTzSpecificLocalTimeEx(zone, &universal, &mut local) } == 0 {
        return None;
    }
    let local = DateTime::new(
        i16::try_from(local.wYear).ok()?,
        i8::try_from(local.wMonth).ok()?,
        i8::try_from(local.wDay).ok()?,
        i8::try_from(local.wHour).ok()?,
        i8::try_from(local.wMinute).ok()?,
        i8::try_from(local.wSecond).ok()?,
        0,
    )
    .ok()?;
    let seconds = local.duration_since(utc).as_secs();
    jiff::tz::Offset::from_seconds(i32::try_from(seconds).ok()?).ok()
}

#[cfg(not(windows))]
fn windows_offset(_time: Timestamp) -> Option<jiff::tz::Offset> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    /// 2026-08-02 12:00 UTC, during Pacific daylight saving time.
    const SUMMER_NOON_UTC: i64 = 1_785_672_000;

    fn at(second: i64, zone: TimeZone) -> String {
        Timestamp::from_second(second)
            .unwrap()
            .to_zoned_datetime(zone)
            .strftime("%b %-d %H:%M %:z")
            .to_string()
    }

    /// A minimal TZif v1 file with one UTC-08:00 local time type.
    fn utc_minus_eight_tzif() -> Vec<u8> {
        let mut data = b"TZif".to_vec();
        data.extend([0; 16]);
        // isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt
        for count in [0u32, 0, 0, 0, 1, 4] {
            data.extend(count.to_be_bytes());
        }
        data.extend((-8 * 3600i32).to_be_bytes());
        data.extend([0, 0]); // not daylight saving, abbreviation index 0
        data.extend(b"-08\0");
        data
    }

    #[test]
    fn explicit_tzif_path_wins_over_a_database_name_match() {
        let root = TempDir::new("kite-clock-tzif");
        let zoneinfo = root.path.join("zoneinfo");
        std::fs::create_dir_all(&zoneinfo).unwrap();
        let file = zoneinfo.join("UTC");
        std::fs::write(&file, utc_minus_eight_tzif()).unwrap();
        let path = file.to_str().unwrap();

        for tz in [format!(":{path}"), path.to_string()] {
            let zone = tzif_file(OsStr::new(&tz)).expect("TZif path should be read");
            assert_eq!(at(SUMMER_NOON_UTC, zone), "Aug 2 04:00 -08:00");
        }
    }

    #[test]
    fn names_rules_and_missing_files_fall_through_to_jiff() {
        let root = TempDir::new("kite-clock-missing");
        let missing = root.path.join("zoneinfo").join("UTC");
        for tz in [
            "America/Los_Angeles",
            ":America/Los_Angeles",
            "PST8PDT,M3.2.0,M11.1.0",
            "../zoneinfo/UTC",
            missing.to_str().unwrap(),
        ] {
            assert!(tzif_file(OsStr::new(tz)).is_none(), "{tz}");
        }
    }

    /// 2026-01-02 12:00 UTC, during Pacific standard time.
    #[cfg(windows)]
    const WINTER_NOON_UTC: i64 = 1_767_355_200;

    #[cfg(windows)]
    #[test]
    fn windows_offsets_keep_the_instant() {
        for second in [SUMMER_NOON_UTC, WINTER_NOON_UTC] {
            let time = Timestamp::from_second(second).unwrap();
            let offset = windows_offset(time).expect("Windows should convert the time");
            assert!(offset.seconds().abs() <= 14 * 3600, "{offset}");
            assert_eq!(local(time).timestamp(), time);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_offsets_follow_custom_zone_settings() {
        use windows_sys::Win32::Foundation::SYSTEMTIME;
        use windows_sys::Win32::System::Time::DYNAMIC_TIME_ZONE_INFORMATION;

        let offset = |zone: &DYNAMIC_TIME_ZONE_INFORMATION, second| {
            let time = Timestamp::from_second(second).unwrap();
            windows_offset_in(zone, time).unwrap().seconds() / 3600
        };
        let date = |year, month, day| SYSTEMTIME {
            wYear: year,
            wMonth: month,
            wDay: day,
            wHour: 2,
            ..SYSTEMTIME::default()
        };

        // Pacific time with daylight saving turned off: standard time all year.
        let mut zone = DYNAMIC_TIME_ZONE_INFORMATION {
            Bias: 480,
            DaylightBias: -60,
            DynamicDaylightTimeDisabled: true,
            ..DYNAMIC_TIME_ZONE_INFORMATION::default()
        };
        assert_eq!(offset(&zone, SUMMER_NOON_UTC), -8);
        assert_eq!(offset(&zone, WINTER_NOON_UTC), -8);

        // One-time transitions on 2026-03-08 and 2026-11-01.
        zone.DaylightDate = date(2026, 3, 8);
        zone.StandardDate = date(2026, 11, 1);
        assert_eq!(offset(&zone, SUMMER_NOON_UTC), -7);
        assert_eq!(offset(&zone, WINTER_NOON_UTC), -8);
    }
}
