//! MS-RDPBCGR 2.2.14 network characteristics auto-detection.
//!
//! The server periodically probes the link by sending Auto-Detect Request PDUs
//! (RTT measure, bandwidth measure start/payload/stop, network characteristics
//! result), each tagged with the `SEC_AUTODETECT_REQ` basic-security-header
//! flag. A client that advertised `RNS_UD_CS_SUPPORT_NETCHAR_AUTODETECT` echoes
//! back Auto-Detect Response PDUs reporting the measured RTT and bandwidth; the
//! server uses those numbers to keep the session on the appropriate experience
//! profile. On a fast LAN the responses report a near-zero RTT, which keeps the
//! host at its richest, lowest-latency encode instead of degrading.
//!
//! This module is pure wire-format: [`parse_request`] classifies an inbound PDU
//! and the builders produce the matching response bytes (security header
//! included), ready to send on the I/O channel. The bandwidth measurement is
//! stateful across PDUs — the caller owns the start time and byte counter (see
//! [`BandwidthMeter`]).
//!
//! Layouts are taken from MS-RDPBCGR 2.2.14 and cross-checked against FreeRDP's
//! `libfreerdp/core/autodetect.c`.

use crate::security::{SEC_AUTODETECT_REQ, SEC_AUTODETECT_RSP};

/// `headerTypeId` for a server request (`RDP_NETWORK_DETECTION_REQUEST`).
const TYPE_ID_REQUEST: u8 = 0x00;
/// `headerTypeId` for a client response (`RDP_NETWORK_DETECTION_RESPONSE`).
const TYPE_ID_RESPONSE: u8 = 0x01;

// requestType values (the server → client direction).
const RTT_REQUEST_CONNECTTIME: u16 = 0x1001;
const RTT_REQUEST_CONTINUOUS: u16 = 0x0001;
const BW_START_CONNECTTIME: u16 = 0x1014;
const BW_START_CONTINUOUS: u16 = 0x0014;
const BW_START_TUNNEL: u16 = 0x0114;
const BW_PAYLOAD: u16 = 0x0002;
const BW_STOP_CONNECTTIME: u16 = 0x002B;
const BW_STOP_CONTINUOUS: u16 = 0x0429;
const BW_STOP_TUNNEL: u16 = 0x0629;
const NETCHAR_RESULT_BASE_AVG: u16 = 0x0840; // baseRTT + averageRTT
const NETCHAR_RESULT_BW_AVG: u16 = 0x0880; // bandwidth + averageRTT
const NETCHAR_RESULT_ALL: u16 = 0x08C0; // baseRTT + bandwidth + averageRTT

// responseType values (the client → server direction).
const RTT_RESPONSE: u16 = 0x0000;
const BW_RESULTS_CONNECTTIME: u16 = 0x0003;
const BW_RESULTS_CONTINUOUS: u16 = 0x000B;

/// A classified inbound Auto-Detect Request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoDetectRequest {
    /// RTT measure: respond immediately so the server times the round trip.
    RttMeasure { sequence: u16 },
    /// Bandwidth measurement start: reset the meter and record the start time.
    BandwidthStart { sequence: u16 },
    /// A bandwidth payload of `payload_len` bytes: add it to the meter.
    BandwidthPayload { sequence: u16, payload_len: u16 },
    /// Bandwidth measurement stop: add the trailing payload, then report results.
    /// `connect_time` selects the response's `responseType`.
    BandwidthStop { sequence: u16, payload_len: u16, connect_time: bool },
    /// Network characteristics result (server's verdict): informational, no reply.
    /// `average_rtt_us` and `bandwidth` are present only when the corresponding
    /// bits are set in the requestType (BASE_AVG, BW_AVG, ALL).
    NetCharResult {
        sequence: u16,
        base_rtt_us: Option<u32>,
        bandwidth: Option<u32>,
        average_rtt_us: Option<u32>,
    },
}

#[inline]
fn u16le(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(off)?, *b.get(off + 1)?]))
}

#[inline]
fn u32le(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(off)?,
        *b.get(off + 1)?,
        *b.get(off + 2)?,
        *b.get(off + 3)?,
    ]))
}

/// Classify an inbound I/O-channel PDU as an Auto-Detect Request, or `None` if
/// it isn't one. `pdu` is the payload as seen on the TLS/enhanced-security path,
/// i.e. it begins with the 4-byte Basic Security Header. Detection is
/// conservative — it requires the `SEC_AUTODETECT_REQ` flag, a zero `flagsHi`,
/// and the request `headerTypeId` — so a normal Share PDU (whose first bytes are
/// a Share Control Header with a non-zero `pduType` where `flagsHi` would sit)
/// is never misread.
///
/// Note: on the legacy RC4 path the Basic Security Header is stripped during
/// decryption before we see the bytes, so detection there would need the flags
/// plumbed separately; auto-detect is only wired on the TLS path today.
pub fn parse_request(pdu: &[u8]) -> Option<AutoDetectRequest> {
    let flags = u16le(pdu, 0)?;
    let flags_hi = u16le(pdu, 2)?;
    if flags & SEC_AUTODETECT_REQ == 0 || flags_hi != 0 {
        return None;
    }
    // RDP_NETWORK_DETECTION_REQUEST follows the 4-byte security header.
    let header_type_id = *pdu.get(5)?;
    if header_type_id != TYPE_ID_REQUEST {
        return None;
    }
    let sequence = u16le(pdu, 6)?;
    let request_type = u16le(pdu, 8)?;
    // Optional payloadLength (u16) at offset 10 for the variants that carry it.
    let payload_len = u16le(pdu, 10).unwrap_or(0);
    Some(match request_type {
        RTT_REQUEST_CONNECTTIME | RTT_REQUEST_CONTINUOUS => {
            AutoDetectRequest::RttMeasure { sequence }
        }
        BW_START_CONNECTTIME | BW_START_CONTINUOUS | BW_START_TUNNEL => {
            AutoDetectRequest::BandwidthStart { sequence }
        }
        BW_PAYLOAD => AutoDetectRequest::BandwidthPayload { sequence, payload_len },
        BW_STOP_CONNECTTIME => {
            AutoDetectRequest::BandwidthStop { sequence, payload_len, connect_time: true }
        }
        BW_STOP_CONTINUOUS | BW_STOP_TUNNEL => {
            // Continuous/tunnel stop carries no trailing payload.
            AutoDetectRequest::BandwidthStop { sequence, payload_len: 0, connect_time: false }
        }
        NETCHAR_RESULT_BASE_AVG | NETCHAR_RESULT_BW_AVG | NETCHAR_RESULT_ALL => {
            // Payload follows the 6-byte detection header (offset 10 from the start
            // of the PDU, which already includes the 4-byte Basic Security Header).
            let base_offset = 10usize;
            let mut base_rtt_us: Option<u32> = None;
            let mut bandwidth: Option<u32> = None;
            let mut average_rtt_us: Option<u32> = None;

            let mut off = base_offset;
            if request_type == NETCHAR_RESULT_BASE_AVG || request_type == NETCHAR_RESULT_ALL {
                base_rtt_us = u32le(pdu, off);
                off += 4;
            }
            if request_type == NETCHAR_RESULT_BW_AVG || request_type == NETCHAR_RESULT_ALL {
                bandwidth = u32le(pdu, off);
                off += 4;
            }
            if request_type == NETCHAR_RESULT_BASE_AVG
                || request_type == NETCHAR_RESULT_BW_AVG
                || request_type == NETCHAR_RESULT_ALL
            {
                average_rtt_us = u32le(pdu, off);
            }

            AutoDetectRequest::NetCharResult {
                sequence,
                base_rtt_us,
                bandwidth,
                average_rtt_us,
            }
        }
        _ => return None,
    })
}

#[inline]
fn put_u16(v: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_u32(v: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Prepend the Basic Security Header (`SEC_AUTODETECT_RSP`, `flagsHi=0`) that
/// wraps every response on the enhanced-security path.
fn response_header(out: &mut Vec<u8>) {
    put_u16(SEC_AUTODETECT_RSP, out);
    put_u16(0, out); // flagsHi
}

/// Build an RTT Measure Response (MS-RDPBCGR 2.2.14.2.2) for `sequence`. The body
/// is just the 6-byte detection header; the server times the round trip, so this
/// should be sent the instant the request arrives. Returns the full I/O-channel
/// payload including the Basic Security Header.
pub fn rtt_response(sequence: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    response_header(&mut out);
    out.push(0x06); // headerLength
    out.push(TYPE_ID_RESPONSE); // headerTypeId
    put_u16(sequence, &mut out);
    put_u16(RTT_RESPONSE, &mut out);
    out
}

/// Build a Bandwidth Measure Results response (MS-RDPBCGR 2.2.14.2.4) reporting
/// `time_delta_ms` elapsed and `byte_count` bytes received between the matching
/// Start and Stop. `connect_time` selects the `responseType` (it must mirror the
/// Stop request that triggered it). Returns the full I/O-channel payload.
pub fn bandwidth_results(sequence: u16, connect_time: bool, time_delta_ms: u32, byte_count: u32) -> Vec<u8> {
    let response_type = if connect_time { BW_RESULTS_CONNECTTIME } else { BW_RESULTS_CONTINUOUS };
    let mut out = Vec::with_capacity(18);
    response_header(&mut out);
    out.push(0x0E); // headerLength
    out.push(TYPE_ID_RESPONSE); // headerTypeId
    put_u16(sequence, &mut out);
    put_u16(response_type, &mut out);
    put_u32(time_delta_ms, &mut out);
    put_u32(byte_count, &mut out);
    out
}

/// Tracks an in-flight bandwidth measurement. The caller drives it from the
/// parsed requests; on Stop it yields the elapsed milliseconds and byte count to
/// feed [`bandwidth_results`]. Kept transport-agnostic (no clock dependency) so
/// it stays unit-testable — the caller supplies timestamps.
#[derive(Debug, Default, Clone, Copy)]
pub struct BandwidthMeter {
    byte_count: u32,
    active: bool,
}

impl BandwidthMeter {
    /// Begin a measurement: reset the byte counter. The caller records the start
    /// time separately (e.g. `Instant::now()`).
    pub fn start(&mut self) {
        self.byte_count = 0;
        self.active = true;
    }

    /// Accumulate a payload's bytes (saturating).
    pub fn add(&mut self, len: u16) {
        self.byte_count = self.byte_count.saturating_add(len as u32);
    }

    /// Finish: add the Stop's trailing payload and return the total byte count,
    /// or `None` if no Start was seen (a stray Stop — ignore it).
    pub fn stop(&mut self, trailing: u16) -> Option<u32> {
        if !self.active {
            return None;
        }
        self.add(trailing);
        self.active = false;
        Some(self.byte_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a request PDU with a Basic Security Header, as seen on TLS.
    fn req(request_type: u16, sequence: u16, trailing: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        put_u16(SEC_AUTODETECT_REQ, &mut v);
        put_u16(0, &mut v); // flagsHi
        v.push(0x06); // headerLength (server side)
        v.push(TYPE_ID_REQUEST);
        put_u16(sequence, &mut v);
        put_u16(request_type, &mut v);
        v.extend_from_slice(trailing);
        v
    }

    #[test]
    fn parses_rtt_both_modes() {
        assert_eq!(
            parse_request(&req(RTT_REQUEST_CONTINUOUS, 7, &[])),
            Some(AutoDetectRequest::RttMeasure { sequence: 7 })
        );
        assert_eq!(
            parse_request(&req(RTT_REQUEST_CONNECTTIME, 8, &[])),
            Some(AutoDetectRequest::RttMeasure { sequence: 8 })
        );
    }

    #[test]
    fn parses_bandwidth_sequence() {
        assert_eq!(
            parse_request(&req(BW_START_CONTINUOUS, 1, &[])),
            Some(AutoDetectRequest::BandwidthStart { sequence: 1 })
        );
        // payloadLength = 512 at offset 10.
        assert_eq!(
            parse_request(&req(BW_PAYLOAD, 2, &512u16.to_le_bytes())),
            Some(AutoDetectRequest::BandwidthPayload { sequence: 2, payload_len: 512 })
        );
        // Connect-time stop carries a trailing payloadLength; continuous does not.
        assert_eq!(
            parse_request(&req(BW_STOP_CONNECTTIME, 3, &16u16.to_le_bytes())),
            Some(AutoDetectRequest::BandwidthStop { sequence: 3, payload_len: 16, connect_time: true })
        );
        assert_eq!(
            parse_request(&req(BW_STOP_CONTINUOUS, 4, &[])),
            Some(AutoDetectRequest::BandwidthStop { sequence: 4, payload_len: 0, connect_time: false })
        );
    }

    #[test]
    fn parses_netchar_result() {
        assert_eq!(
            parse_request(&req(NETCHAR_RESULT_ALL, 9, &[0u8; 12]))
                .and_then(|r| match r {
                    AutoDetectRequest::NetCharResult {
                        sequence,
                        base_rtt_us,
                        bandwidth,
                        average_rtt_us,
                    } => Some((sequence, base_rtt_us, bandwidth, average_rtt_us)),
                    _ => None,
                }),
            Some((9, Some(0), Some(0), Some(0)))
        );
    }

    #[test]
    fn rejects_non_autodetect() {
        // A Share Control Header for a Data PDU: totalLength then pduType=0x17.
        // flagsHi (bytes 2..4) would be the non-zero pduType → rejected.
        let share = [0x1C, 0x10, 0x17, 0x00, 0xEA, 0x03, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(parse_request(&share), None);
        // Missing the autodetect flag.
        let mut no_flag = req(RTT_REQUEST_CONTINUOUS, 1, &[]);
        no_flag[0] = 0;
        no_flag[1] = 0;
        assert_eq!(parse_request(&no_flag), None);
        // Too short.
        assert_eq!(parse_request(&[0x00, 0x10, 0x00, 0x00]), None);
        // Unknown requestType.
        assert_eq!(parse_request(&req(0x7777, 1, &[])), None);
    }

    #[test]
    fn rtt_response_layout() {
        let r = rtt_response(0x1234);
        // Security header: SEC_AUTODETECT_RSP, flagsHi 0.
        assert_eq!(&r[0..4], &[0x00, 0x20, 0x00, 0x00]);
        assert_eq!(r[4], 0x06); // headerLength
        assert_eq!(r[5], TYPE_ID_RESPONSE);
        assert_eq!(u16::from_le_bytes([r[6], r[7]]), 0x1234); // sequenceNumber
        assert_eq!(u16::from_le_bytes([r[8], r[9]]), RTT_RESPONSE);
        assert_eq!(r.len(), 10);
    }

    #[test]
    fn bandwidth_results_layout() {
        let r = bandwidth_results(0x0005, true, 250, 65536);
        assert_eq!(&r[0..4], &[0x00, 0x20, 0x00, 0x00]); // SEC_AUTODETECT_RSP
        assert_eq!(r[4], 0x0E); // headerLength
        assert_eq!(r[5], TYPE_ID_RESPONSE);
        assert_eq!(u16::from_le_bytes([r[6], r[7]]), 0x0005);
        assert_eq!(u16::from_le_bytes([r[8], r[9]]), BW_RESULTS_CONNECTTIME);
        assert_eq!(u32::from_le_bytes([r[10], r[11], r[12], r[13]]), 250);
        assert_eq!(u32::from_le_bytes([r[14], r[15], r[16], r[17]]), 65536);
        assert_eq!(r.len(), 18);
        // Continuous selects the other responseType.
        let c = bandwidth_results(1, false, 1, 1);
        assert_eq!(u16::from_le_bytes([c[8], c[9]]), BW_RESULTS_CONTINUOUS);
    }

    #[test]
    fn bandwidth_meter_counts_and_guards_stray_stop() {
        let mut m = BandwidthMeter::default();
        assert_eq!(m.stop(10), None); // stray stop before start
        m.start();
        m.add(100);
        m.add(200);
        assert_eq!(m.stop(50), Some(350));
        assert_eq!(m.stop(1), None); // already stopped
    }
}
