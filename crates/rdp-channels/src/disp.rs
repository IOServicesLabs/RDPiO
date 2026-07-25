//! Display Control (MS-RDPEDISP) — client-initiated desktop resize.
//!
//! Runs as a dynamic virtual channel (`Microsoft::Windows::RDS::DisplayControl`)
//! over DRDYNVC. The server sends a `DISPLAYCONTROL_CAPS` PDU (max monitors and
//! area); the client then sends a `DISPLAYCONTROL_MONITOR_LAYOUT_PDU` whenever
//! the window is resized, and the server resizes the remote desktop to match.
//! This module is sans-I/O: parse the caps, build a single-monitor layout PDU.

/// The dynamic channel name for Display Control.
pub const DISPLAYCONTROL_CHANNEL: &str = "Microsoft::Windows::RDS::DisplayControl";

const DISPLAYCONTROL_PDU_TYPE_MONITOR_LAYOUT: u32 = 0x0000_0002;
const DISPLAYCONTROL_PDU_TYPE_CAPS: u32 = 0x0000_0005;

/// A monitor that is the primary one.
const DISPLAYCONTROL_MONITOR_PRIMARY: u32 = 0x0000_0001;
/// One `DISPLAYCONTROL_MONITOR_LAYOUT` is 40 bytes.
const MONITOR_LAYOUT_SIZE: u32 = 40;

/// Server display-control capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayCaps {
    pub max_monitors: u32,
    pub max_area_factor_a: u32,
    pub max_area_factor_b: u32,
}

#[inline]
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(o)?,
        *b.get(o + 1)?,
        *b.get(o + 2)?,
        *b.get(o + 3)?,
    ]))
}

/// Parse a `DISPLAYCONTROL_CAPS` PDU; `None` for other Display Control PDUs.
pub fn parse_caps(pdu: &[u8]) -> Option<DisplayCaps> {
    // DISPLAYCONTROL_HEADER: Type(4), Length(4), then the body.
    if u32le(pdu, 0)? != DISPLAYCONTROL_PDU_TYPE_CAPS {
        return None;
    }
    Some(DisplayCaps {
        max_monitors: u32le(pdu, 8)?,
        max_area_factor_a: u32le(pdu, 12)?,
        max_area_factor_b: u32le(pdu, 16)?,
    })
}

/// Build a `DISPLAYCONTROL_MONITOR_LAYOUT_PDU` from a client monitor layout.
///
/// `monitors` is the full `Vec<MonitorDef>` advertised in the GCC block (primary
/// at `(0,0)`, other monitors possibly negative). Width and height are rounded
/// down to even values and floored at 200 per the spec's constraints. The
/// returned PDU tells the server the new desktop topology to use.
pub fn monitor_layout(monitors: &[rdp_pdu::gcc::MonitorDef]) -> Vec<u8> {
    let mut monitors = monitors.to_vec();
    if monitors.is_empty() {
        // Fall back to a safe single-monitor layout so callers never emit an
        // empty/invalid PDU.
        monitors.push(rdp_pdu::gcc::MonitorDef {
            left: 0,
            top: 0,
            right: 1919,
            bottom: 1079,
            primary: true,
        });
    }

    // DISPLAYCONTROL_MONITOR_LAYOUT_PDU body: MonitorLayoutSize, NumMonitors,
    // then one 40-byte DISPLAYCONTROL_MONITOR_LAYOUT per monitor.
    let mut body = Vec::with_capacity(8 + monitors.len() * MONITOR_LAYOUT_SIZE as usize);
    body.extend_from_slice(&MONITOR_LAYOUT_SIZE.to_le_bytes()); // MonitorLayoutSize
    body.extend_from_slice(&(monitors.len() as u32).to_le_bytes()); // NumMonitors

    for m in &monitors {
        let raw_w = (m.right - m.left + 1) as u32;
        let raw_h = (m.bottom - m.top + 1) as u32;
        let w = (raw_w & !1).max(200);
        let h = (raw_h & !1).max(200);
        let flags = if m.primary {
            DISPLAYCONTROL_MONITOR_PRIMARY
        } else {
            0
        };

        body.extend_from_slice(&flags.to_le_bytes());
        body.extend_from_slice(&m.left.to_le_bytes());
        body.extend_from_slice(&m.top.to_le_bytes());
        body.extend_from_slice(&w.to_le_bytes());
        body.extend_from_slice(&h.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // PhysicalWidth
        body.extend_from_slice(&0u32.to_le_bytes()); // PhysicalHeight
        body.extend_from_slice(&0u32.to_le_bytes()); // Orientation
        body.extend_from_slice(&100u32.to_le_bytes()); // DesktopScaleFactor
        body.extend_from_slice(&100u32.to_le_bytes()); // DeviceScaleFactor
    }

    let mut pdu = Vec::with_capacity(8 + body.len());
    pdu.extend_from_slice(&DISPLAYCONTROL_PDU_TYPE_MONITOR_LAYOUT.to_le_bytes()); // Type
    pdu.extend_from_slice(&((8 + body.len()) as u32).to_le_bytes()); // Length
    pdu.extend_from_slice(&body);
    pdu
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_pdu(max: u32, a: u32, b: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&DISPLAYCONTROL_PDU_TYPE_CAPS.to_le_bytes());
        v.extend_from_slice(&20u32.to_le_bytes()); // Length
        v.extend_from_slice(&max.to_le_bytes());
        v.extend_from_slice(&a.to_le_bytes());
        v.extend_from_slice(&b.to_le_bytes());
        v
    }

    fn primary(w: i32, h: i32) -> rdp_pdu::gcc::MonitorDef {
        rdp_pdu::gcc::MonitorDef {
            left: 0,
            top: 0,
            right: w - 1,
            bottom: h - 1,
            primary: true,
        }
    }

    #[test]
    fn parses_caps() {
        assert_eq!(
            parse_caps(&caps_pdu(16, 0x1000, 0x0100)),
            Some(DisplayCaps {
                max_monitors: 16,
                max_area_factor_a: 0x1000,
                max_area_factor_b: 0x0100,
            })
        );
        // A monitor-layout PDU is not caps.
        assert_eq!(parse_caps(&monitor_layout(&[primary(800, 600)])), None);
    }

    #[test]
    fn monitor_layout_is_well_formed() {
        let pdu = monitor_layout(&[primary(1366, 769)]);
        assert_eq!(u32::from_le_bytes([pdu[0], pdu[1], pdu[2], pdu[3]]), DISPLAYCONTROL_PDU_TYPE_MONITOR_LAYOUT);
        // Length == buffer length.
        assert_eq!(u32::from_le_bytes([pdu[4], pdu[5], pdu[6], pdu[7]]) as usize, pdu.len());
        assert_eq!(pdu.len(), 8 + 8 + 40);
        // NumMonitors = 1.
        assert_eq!(u32::from_le_bytes([pdu[12], pdu[13], pdu[14], pdu[15]]), 1);
        // Width rounded down to even (1366), height floored even (768).
        assert_eq!(u32::from_le_bytes([pdu[28], pdu[29], pdu[30], pdu[31]]), 1366);
        assert_eq!(u32::from_le_bytes([pdu[32], pdu[33], pdu[34], pdu[35]]), 768);
    }

    #[test]
    fn clamps_tiny_dimensions() {
        let pdu = monitor_layout(&[primary(10, 10)]);
        assert_eq!(u32::from_le_bytes([pdu[28], pdu[29], pdu[30], pdu[31]]), 200);
        assert_eq!(u32::from_le_bytes([pdu[32], pdu[33], pdu[34], pdu[35]]), 200);
    }

    #[test]
    fn multi_monitor_layout_has_two_entries() {
        let monitors = vec![
            rdp_pdu::gcc::MonitorDef {
                left: 0,
                top: 0,
                right: 1919,
                bottom: 1079,
                primary: true,
            },
            rdp_pdu::gcc::MonitorDef {
                left: 1920,
                top: 0,
                right: 3839,
                bottom: 1079,
                primary: false,
            },
        ];
        let pdu = monitor_layout(&monitors);
        assert_eq!(u32::from_le_bytes([pdu[0], pdu[1], pdu[2], pdu[3]]), DISPLAYCONTROL_PDU_TYPE_MONITOR_LAYOUT);
        assert_eq!(u32::from_le_bytes([pdu[12], pdu[13], pdu[14], pdu[15]]), 2);
        assert_eq!(pdu.len(), 8 + 8 + 2 * 40);
        // First monitor flags == PRIMARY, second == 0.
        assert_eq!(u32::from_le_bytes([pdu[16], pdu[17], pdu[18], pdu[19]]), DISPLAYCONTROL_MONITOR_PRIMARY);
        assert_eq!(u32::from_le_bytes([pdu[56], pdu[57], pdu[58], pdu[59]]), 0);
        // Second monitor left offset is 1920.
        assert_eq!(i32::from_le_bytes([pdu[60], pdu[61], pdu[62], pdu[63]]), 1920);
    }
}
