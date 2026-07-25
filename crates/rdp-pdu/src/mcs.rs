//! MCS (T.125) Connect-Initial and the framed basic-settings PDU.
//!
//! The Connect-Initial is BER, tagged `[APPLICATION 101]`, and carries three
//! sets of `DomainParameters` plus the GCC Conference Create Request as its
//! `userData`. On the wire it rides inside an X.224 Data TPDU under a TPKT
//! header (added by [`basic_settings_pdu`]). The matching Connect-Response
//! (`[APPLICATION 102]`) parser and channel-join PDUs come next.

use crate::gcc::{
    self, ClientClusterData, ClientCoreData, ClientMonitorData, ClientMultitransportData,
    ClientNetworkData, ClientSecurityData,
};
use crate::{x224, PduResult};
use rdp_asn1::der;

/// `[APPLICATION 101] IMPLICIT SEQUENCE` tag for MCS Connect-Initial.
const CONNECT_INITIAL_TAG: [u8; 2] = [0x7f, 0x65];

/// Encode a `DomainParameters` SEQUENCE from its eight integer fields:
/// `[maxChannelIds, maxUserIds, maxTokenIds, numPriorities, minThroughput,
/// maxHeight, maxMCSPDUsize, protocolVersion]`.
fn domain_parameters(values: &[u32; 8]) -> Vec<u8> {
    let mut inner = Vec::new();
    for &value in values {
        inner.extend(der::integer(value));
    }
    der::sequence(&inner)
}

/// Build the MCS Connect-Initial PDU (BER) wrapping `gcc_ccr` as its userData.
pub fn connect_initial(gcc_ccr: &[u8]) -> Vec<u8> {
    let mut content = Vec::new();
    content.extend(der::octet_string(&[0x01])); // callingDomainSelector
    content.extend(der::octet_string(&[0x01])); // calledDomainSelector
    content.extend_from_slice(&[0x01, 0x01, 0xff]); // upwardFlag = TRUE
    content.extend(domain_parameters(&[34, 2, 0, 1, 0, 1, 0xffff, 2])); // target
    content.extend(domain_parameters(&[1, 1, 1, 1, 0, 1, 0x420, 2])); // minimum
    content.extend(domain_parameters(&[
        0xffff, 0xfc17, 0xffff, 1, 0, 1, 0xffff, 2,
    ])); // maximum
    content.extend(der::octet_string(gcc_ccr)); // userData

    let mut out = Vec::with_capacity(content.len() + 8);
    out.extend_from_slice(&CONNECT_INITIAL_TAG);
    der::encode_length(content.len(), &mut out);
    out.extend_from_slice(&content);
    out
}

/// Build the full wire PDU for the MCS basic-settings exchange:
/// `TPKT + X.224 Data + MCS Connect-Initial( GCC CCR( client data blocks ) )`.
pub fn basic_settings_pdu(
    core: &ClientCoreData,
    security: &ClientSecurityData,
    network: &ClientNetworkData,
    cluster: &ClientClusterData,
    monitors: &ClientMonitorData,
    multitransport: &ClientMultitransportData,
) -> PduResult<Vec<u8>> {
    let user_data =
        gcc::encode_client_data(core, security, network, cluster, monitors, multitransport);
    let ccr = gcc::conference_create_request(&user_data);
    let connect = connect_initial(&ccr);

    let mut out = Vec::with_capacity(connect.len() + x224::TPKT_HEADER_LEN + 3);
    x224::write_data_header(connect.len(), &mut out)?;
    out.extend_from_slice(&connect);
    Ok(out)
}

// --- Connect-Response + domain PDUs -----------------------------------------

/// `[APPLICATION 102]` tag for MCS Connect-Response.
const CONNECT_RESPONSE_TAG: [u8; 2] = [0x7f, 0x66];
/// MCS user/channel ids start at 1001.
pub const MCS_BASE_CHANNEL_ID: u16 = 1001;

const TAG_ENUMERATED: u8 = 0x0a;

/// MCS DomainMCSPDU choice values (the high 6 bits of the first byte).
const CHOICE_ATTACH_USER_CONFIRM: u8 = 11;
const CHOICE_CHANNEL_JOIN_CONFIRM: u8 = 15;

/// The parsed MCS Connect-Response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectResponse {
    /// MCS result (0 = rt-successful).
    pub result: u8,
    /// The GCC Conference Create Response user data (server `SC_*` blocks).
    pub user_data: Vec<u8>,
}

/// Strip an optional TPKT + X.224 Data header, returning the inner MCS bytes.
fn strip_framing(pdu: &[u8]) -> &[u8] {
    if pdu.first() == Some(&x224::TPKT_VERSION) && pdu.len() > x224::TPKT_HEADER_LEN {
        // X.224 Data header length = LI byte + 1.
        let li = pdu[x224::TPKT_HEADER_LEN] as usize;
        let start = x224::TPKT_HEADER_LEN + li + 1;
        if start <= pdu.len() {
            return &pdu[start..];
        }
    }
    pdu
}

/// Parse an MCS Connect-Response (`[APPLICATION 102]`), with or without TPKT/X.224
/// framing, extracting the result and the GCC server user data.
pub fn parse_connect_response(pdu: &[u8]) -> PduResult<ConnectResponse> {
    let mut body = strip_framing(pdu);
    if body.len() < 2 || body[0] != CONNECT_RESPONSE_TAG[0] || body[1] != CONNECT_RESPONSE_TAG[1] {
        return Err(crate::PduError::InvalidField {
            field: "mcs_connect_response_tag",
            detail: "expected [APPLICATION 102]".into(),
        });
    }
    body = &body[2..];
    let _len = der::decode_length(&mut body)?;

    let result = der::expect(&mut body, TAG_ENUMERATED)?
        .first()
        .copied()
        .unwrap_or(0xff);
    let _called_connect_id = der::expect(&mut body, der::TAG_INTEGER)?;
    let _domain_parameters = der::expect(&mut body, der::TAG_SEQUENCE)?;
    let user_data = der::expect(&mut body, der::TAG_OCTET_STRING)?;

    Ok(ConnectResponse {
        result,
        user_data: user_data.to_vec(),
    })
}

/// MCS Erect Domain Request (subHeight = subInterval = 0).
pub fn erect_domain_request() -> [u8; 5] {
    [0x04, 0x01, 0x00, 0x01, 0x00]
}

/// MCS Attach User Request.
pub fn attach_user_request() -> [u8; 1] {
    [0x28]
}

/// Parse an MCS Attach User Confirm, returning the assigned user channel id.
/// Accepts framed (TPKT + X.224) or bare MCS bytes.
pub fn parse_attach_user_confirm(pdu: &[u8]) -> PduResult<u16> {
    let mcs = strip_framing(pdu);
    if mcs.len() < 4 || (mcs[0] >> 2) != CHOICE_ATTACH_USER_CONFIRM {
        return Err(invalid("attach_user_confirm", "bad choice or length"));
    }
    if mcs[1] != 0 {
        return Err(invalid("attach_user_confirm", "non-zero result"));
    }
    let initiator = u16::from_be_bytes([mcs[2], mcs[3]]);
    Ok(MCS_BASE_CHANNEL_ID + initiator)
}

/// MCS Channel Join Request for `channel_id`, from user `user_id`.
pub fn channel_join_request(user_id: u16, channel_id: u16) -> [u8; 5] {
    let initiator = (user_id - MCS_BASE_CHANNEL_ID).to_be_bytes();
    let channel = channel_id.to_be_bytes();
    [0x38, initiator[0], initiator[1], channel[0], channel[1]]
}

/// Parse an MCS Channel Join Confirm, returning the joined channel id.
/// Accepts framed (TPKT + X.224) or bare MCS bytes.
pub fn parse_channel_join_confirm(pdu: &[u8]) -> PduResult<u16> {
    let mcs = strip_framing(pdu);
    if mcs.len() < 8 || (mcs[0] >> 2) != CHOICE_CHANNEL_JOIN_CONFIRM {
        return Err(invalid("channel_join_confirm", "bad choice or length"));
    }
    if mcs[1] != 0 {
        return Err(invalid("channel_join_confirm", "non-zero result"));
    }
    Ok(u16::from_be_bytes([mcs[6], mcs[7]]))
}

/// Frame an MCS PDU as TPKT + X.224 Data for the wire.
pub fn frame(mcs: &[u8]) -> PduResult<Vec<u8>> {
    let mut out = Vec::with_capacity(mcs.len() + x224::TPKT_HEADER_LEN + 3);
    x224::write_data_header(mcs.len(), &mut out)?;
    out.extend_from_slice(mcs);
    Ok(out)
}

/// MCS Send Data Request carrying `payload` from `user_id` on `channel_id`
/// (e.g. the Client Info PDU on the I/O channel, or share data later).
pub fn send_data_request(user_id: u16, channel_id: u16, payload: &[u8]) -> Vec<u8> {
    let initiator = (user_id - MCS_BASE_CHANNEL_ID).to_be_bytes();
    let channel = channel_id.to_be_bytes();
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.push(0x64); // SendDataRequest (25 << 2)
    out.extend_from_slice(&initiator);
    out.extend_from_slice(&channel);
    out.push(0x70); // dataPriority(high) + segmentation(begin|end)
    if payload.len() > 0x7f {
        out.push(0x80 | (payload.len() >> 8) as u8);
        out.push((payload.len() & 0xff) as u8);
    } else {
        out.push(payload.len() as u8);
    }
    out.extend_from_slice(payload);
    out
}

/// Parse an MCS Send Data Indication (server -> client), returning the channel
/// id and the payload (a security header and/or share PDU). Accepts framed or
/// bare MCS bytes.
pub fn parse_send_data_indication(pdu: &[u8]) -> PduResult<(u16, Vec<u8>)> {
    let mcs = strip_framing(pdu);
    crate::ensure(mcs, 7)?;
    if (mcs[0] >> 2) != 26 {
        return Err(invalid("send_data_indication", "not a SendDataIndication"));
    }
    let channel_id = u16::from_be_bytes([mcs[3], mcs[4]]);
    // byte5 = dataPriority/segmentation; byte6.. = PER length, then payload.
    let mut idx = 6;
    let first = mcs[idx];
    let payload_len = if first & 0x80 != 0 {
        crate::ensure(mcs, idx + 2)?;
        let len = (((first & 0x7f) as usize) << 8) | mcs[idx + 1] as usize;
        idx += 2;
        len
    } else {
        idx += 1;
        first as usize
    };
    crate::ensure(mcs, idx + payload_len)?;
    Ok((channel_id, mcs[idx..idx + payload_len].to_vec()))
}

fn invalid(field: &'static str, detail: &str) -> crate::PduError {
    crate::PduError::InvalidField {
        field,
        detail: detail.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_initial_is_application_101_wrapping_userdata() {
        let ccr = gcc::conference_create_request(&[0xAB; 8]);
        let ci = connect_initial(&ccr);
        assert_eq!(&ci[0..2], &CONNECT_INITIAL_TAG);

        // Skip the 2-byte tag, read the BER length, then walk the fields.
        let mut body = &ci[2..];
        let len = der::decode_length(&mut body).unwrap();
        assert_eq!(len, body.len(), "BER length matches the content");

        assert_eq!(
            der::expect(&mut body, der::TAG_OCTET_STRING).unwrap(),
            &[0x01]
        );
        assert_eq!(
            der::expect(&mut body, der::TAG_OCTET_STRING).unwrap(),
            &[0x01]
        );
        assert_eq!(der::read_element(&mut body).unwrap(), (0x01u8, &[0xff][..])); // upwardFlag
        for _ in 0..3 {
            der::expect(&mut body, der::TAG_SEQUENCE).unwrap(); // DomainParameters
        }
        let user = der::expect(&mut body, der::TAG_OCTET_STRING).unwrap();
        assert_eq!(user, ccr.as_slice(), "userData round-trips to the CCR");
        assert!(body.is_empty());
    }

    #[test]
    fn basic_settings_pdu_is_framed() {
        let pdu = basic_settings_pdu(
            &ClientCoreData::default(),
            &ClientSecurityData::default(),
            &ClientNetworkData::with_drdynvc(),
            &ClientClusterData::default(),
            &ClientMonitorData::default(),
            &ClientMultitransportData::default(),
        )
        .unwrap();
        assert_eq!(pdu[0], 0x03); // TPKT version
        assert_eq!(x224::read_tpkt_len(&pdu).unwrap(), pdu.len());
        assert_eq!(&pdu[4..7], &[0x02, 0xf0, 0x80]); // X.224 Data header
        assert_eq!(&pdu[7..9], &CONNECT_INITIAL_TAG); // MCS [APPLICATION 101]
    }

    fn sample_connect_response() -> Vec<u8> {
        // [APPLICATION 102] { ENUM 0, INTEGER 0, SEQUENCE {}, OCTET STRING "hello" }.
        let content = [
            0x0a, 0x01, 0x00, // result = rt-successful
            0x02, 0x01, 0x00, // calledConnectId = 0
            0x30, 0x00, // domainParameters {}
            0x04, 0x05, b'h', b'e', b'l', b'l', b'o', // userData
        ];
        let mut pdu = vec![0x7f, 0x66, content.len() as u8];
        pdu.extend_from_slice(&content);
        pdu
    }

    #[test]
    fn parse_connect_response_unframed_and_framed() {
        let raw = sample_connect_response();
        let parsed = parse_connect_response(&raw).unwrap();
        assert_eq!(parsed.result, 0);
        assert_eq!(parsed.user_data, b"hello");

        let framed = frame(&raw).unwrap();
        let parsed2 = parse_connect_response(&framed).unwrap();
        assert_eq!(parsed2, parsed);
    }

    #[test]
    fn domain_request_pdus_are_constant() {
        assert_eq!(erect_domain_request(), [0x04, 0x01, 0x00, 0x01, 0x00]);
        assert_eq!(attach_user_request(), [0x28]);
    }

    #[test]
    fn attach_user_confirm_yields_user_channel() {
        let id = parse_attach_user_confirm(&[0x2e, 0x00, 0x00, 0x03]).unwrap();
        assert_eq!(id, 1004);
    }

    #[test]
    fn channel_join_request_and_confirm_roundtrip() {
        assert_eq!(
            channel_join_request(1004, 1003),
            [0x38, 0x00, 0x03, 0x03, 0xeb]
        );
        let channel =
            parse_channel_join_confirm(&[0x3e, 0x00, 0x00, 0x03, 0x03, 0xeb, 0x03, 0xeb]).unwrap();
        assert_eq!(channel, 1003);
    }

    #[test]
    fn send_data_request_frames_payload() {
        let pdu = send_data_request(1004, 1003, &[0xAA, 0xBB]);
        assert_eq!(pdu, [0x64, 0x00, 0x03, 0x03, 0xeb, 0x70, 0x02, 0xAA, 0xBB]);
    }

    #[test]
    fn send_data_indication_unwraps_payload() {
        // SendDataIndication on channel 1003 carrying [AA, BB].
        let sdi = [0x68, 0x00, 0x03, 0x03, 0xeb, 0x70, 0x02, 0xAA, 0xBB];
        let (channel, payload) = parse_send_data_indication(&sdi).unwrap();
        assert_eq!(channel, 1003);
        assert_eq!(payload, vec![0xAA, 0xBB]);

        // Same, but TPKT + X.224 framed.
        let framed = frame(&sdi).unwrap();
        let (channel2, payload2) = parse_send_data_indication(&framed).unwrap();
        assert_eq!((channel2, payload2), (channel, payload));
    }
}
