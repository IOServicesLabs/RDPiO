//! The client's time zone, sent at logon so the remote session shows local
//! time (RDP time zone redirection). The host applies it when its "Allow time
//! zone redirection" policy is on, which Windows 365 / AVD hosts typically
//! enable.

use rdp_pdu::security::TimeZone;

/// The local time zone, or `None` if Windows won't say.
#[cfg(windows)]
pub fn local_time_zone() -> Option<TimeZone> {
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::System::Time::{
        GetDynamicTimeZoneInformation, DYNAMIC_TIME_ZONE_INFORMATION, TIME_ZONE_ID_INVALID,
    };

    let mut tz = DYNAMIC_TIME_ZONE_INFORMATION::default();
    // SAFETY: `tz` is a valid, writable DYNAMIC_TIME_ZONE_INFORMATION.
    if unsafe { GetDynamicTimeZoneInformation(&mut tz) } == TIME_ZONE_ID_INVALID {
        tracing::warn!("could not read the local time zone; the session keeps its own");
        return None;
    }
    let text = |w: &[u16]| {
        let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
        String::from_utf16_lossy(&w[..end])
    };
    let date = |d: &SYSTEMTIME| {
        [d.wYear, d.wMonth, d.wDayOfWeek, d.wDay, d.wHour, d.wMinute, d.wSecond, d.wMilliseconds]
    };
    let zone = TimeZone {
        bias: tz.Bias,
        standard_name: text(&tz.StandardName),
        standard_date: date(&tz.StandardDate),
        standard_bias: tz.StandardBias,
        daylight_name: text(&tz.DaylightName),
        daylight_date: date(&tz.DaylightDate),
        daylight_bias: tz.DaylightBias,
        key_name: text(&tz.TimeZoneKeyName),
        dynamic_daylight_disabled: tz.DynamicDaylightTimeDisabled,
    };
    tracing::info!(zone = %zone.key_name, bias_minutes = zone.bias, "sending the local time zone to the session");
    Some(zone)
}

#[cfg(not(windows))]
pub fn local_time_zone() -> Option<TimeZone> {
    None
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn reads_the_machine_time_zone() {
        let tz = super::local_time_zone().expect("Windows reports a time zone");
        assert!(!tz.key_name.is_empty(), "{tz:?}");
        assert!((-14 * 60..=12 * 60).contains(&tz.bias), "{tz:?}");
    }
}
