//! The RDP connection sequence (MS-RDPBCGR 2.2.1.1): X.224 negotiation, MCS
//! connect, security exchange, channel joins, Client Info, licensing, and the
//! capability exchange. Every PDU is encoded/decoded from scratch by the codec
//! in [`crate::pdu`]; this module only sequences the steps.
//!
//! wire-main implements Standard RDP Security (the `--insecure` path): the
//! client random is RSA-encrypted with the server's certificate public key and
//! the bulk stream is protected with RC4 + MAC signatures. TLS/NLA is not
//! implemented here, so [`ConnectOptions::insecure`] must be set.

use crate::error::{WireError, WireResult};
use crate::pdu::{caps, gcc, info, license, mcs, security, x224};
use crate::session::{ConnectOptions, SessionSettings};
use crate::transport::WireTransport;

/// Outcome of the connection sequence: negotiated settings plus the live
/// security layer used to seal/open every subsequent PDU.
pub struct Handshake {
    pub settings: SessionSettings,
    pub security: security::SecurityLayer,
}

/// Run the full connection sequence on an already-connected transport.
pub fn run(transport: &mut WireTransport, opts: &ConnectOptions) -> WireResult<Handshake> {
    // 1. X.224 Connection Request / Confirm.
    let selected_protocol = negotiate(transport, opts)?;
    if selected_protocol != 0 {
        return Err(WireError::Unsupported(format!(
            "server selected security protocol {selected_protocol:#x}; \
             wire-main only implements Standard RDP Security (--insecure)"
        )));
    }

    // 2. MCS Connect Initial / Response (carries the GCC server blocks).
    let blocks = mcs_connect(transport, opts)?;

    // 3. Erect Domain + Attach User.
    let user_id = attach_user(transport)?;

    // 4. Security Exchange (Standard RDP Security).
    let mut security = standard_security(transport, &blocks, user_id)?;

    // 5. Channel joins: I/O channel first, then each static virtual channel.
    let (io_channel, static_channels) =
        join_channels(transport, user_id, &blocks, opts.channel_names())?;

    // 6. Client Info PDU (encrypted).
    send_client_info(transport, &mut security, user_id, io_channel, opts)?;

    // 7. Licensing.
    licensing(transport, &mut security, user_id, io_channel)?;

    // 8. Capability exchange: Demand Active → Confirm Active + Synchronize +
    //    Control Cooperate + Control Request.
    let share_id = capability_exchange(transport, &mut security, user_id, io_channel, opts)?;

    let settings = SessionSettings {
        host: opts.host.clone(),
        port: opts.port,
        desktop_width: opts.width,
        desktop_height: opts.height,
        color_depth: opts.color_depth,
        selected_protocol,
        user_channel: user_id,
        io_channel,
        static_channels,
        share_id,
        server_caps: Vec::new(),
        server_core_version: blocks.core.as_ref().map(|c| c.version).unwrap_or(0),
    };

    tracing::debug!(
        io_channel,
        channels = settings.static_channels.len(),
        share_id,
        "connection sequence complete"
    );
    Ok(Handshake { settings, security })
}

// --- Step 1: X.224 ---------------------------------------------------------

fn negotiate(transport: &mut WireTransport, opts: &ConnectOptions) -> WireResult<u32> {
    let cr = x224::ConnectionRequest {
        // Standard RDP Security is protocol 0.
        requested_protocols: 0,
        cookie: (!opts.username.is_empty()).then(|| opts.username.clone()),
    };
    let mut body = Vec::new();
    cr.encode(&mut body)?;
    transport.send(&body)?;

    let cc = transport.recv()?;
    let mut cur: &[u8] = &cc;
    match x224::ConnectionConfirm::decode(&mut cur)? {
        x224::ConnectionConfirm::Response {
            selected_protocol, ..
        } => Ok(selected_protocol),
        x224::ConnectionConfirm::Failure { code } => Err(WireError::NegotiationRejected(code)),
        x224::ConnectionConfirm::NoNegotiation => Ok(0),
    }
}

// --- Step 2: MCS Connect ---------------------------------------------------

fn mcs_connect(
    transport: &mut WireTransport,
    opts: &ConnectOptions,
) -> WireResult<gcc::ServerDataBlocks> {
    let core = gcc::ClientCoreData {
        desktop_width: opts.width,
        desktop_height: opts.height,
        color_depth: opts.color_depth,
        client_name: opts.client_name.clone(),
        keyboard_layout: 0x0000_0409,
        early_capability_flags: gcc::RNS_UD_CS_SUPPORT_ERRINFO_PDU,
        server_selected_protocol: 0,
    };
    let security_data = gcc::ClientSecurityData {
        // 40-bit | 56-bit | 128-bit RC4.
        encryption_methods: gcc::ENCRYPTION_METHOD_40BIT
            | gcc::ENCRYPTION_METHOD_56BIT
            | gcc::ENCRYPTION_METHOD_128BIT,
    };
    let network = gcc::ClientNetworkData {
        channels: opts
            .channel_names()
            .into_iter()
            .map(|name| gcc::ClientChannel {
                name,
                options: CHANNEL_OPTION_INITIALIZED | CHANNEL_OPTION_ENCRYPT_RDP,
            })
            .collect(),
    };
    let monitors = gcc::ClientMonitorData {
        monitors: opts.monitors.iter().map(|&m| m.into()).collect(),
    };
    let multitransport = gcc::ClientMultitransportData {
        flags: if opts.udp {
            gcc::TRANSPORTTYPE_UDPFECR | gcc::TRANSPORTTYPE_UDPFECL
        } else {
            0
        },
    };

    let user_data = gcc::encode_client_data(
        &core,
        &security_data,
        &network,
        &gcc::ClientClusterData::default(),
        &monitors,
        &multitransport,
    );
    let ccr = gcc::conference_create_request(&user_data);
    let connect = mcs::connect_initial(&ccr);
    send_mcs(transport, &connect)?;

    let response = transport.recv()?;
    let parsed = mcs::parse_connect_response(&response)?;
    if parsed.result != 0 {
        return Err(WireError::Sequence(format!(
            "MCS Connect-Response result {:#x}",
            parsed.result
        )));
    }
    let blocks = gcc::parse_conference_create_response(&parsed.user_data)?;
    gcc::parse_server_blocks(&blocks)
}

// --- Step 3: Attach User ---------------------------------------------------

fn attach_user(transport: &mut WireTransport) -> WireResult<u16> {
    send_mcs(transport, &mcs::erect_domain_request())?;
    send_mcs(transport, &mcs::attach_user_request())?;
    let confirm = transport.recv()?;
    Ok(mcs::parse_attach_user_confirm(&confirm)?.user_id)
}

// --- Step 4: Security Exchange ---------------------------------------------

fn standard_security(
    transport: &mut WireTransport,
    blocks: &gcc::ServerDataBlocks,
    user_id: u16,
) -> WireResult<security::SecurityLayer> {
    let sec = blocks
        .security
        .as_ref()
        .ok_or_else(|| WireError::Sequence("server sent no SC_SECURITY block".into()))?;
    if sec.encryption_method == 0 {
        return Err(WireError::Unsupported(
            "server selected no encryption; refusing to continue".into(),
        ));
    }
    let cert = gcc::parse_server_certificate(&sec.server_cert)?;

    let mut client_random = [0u8; 32];
    fill_random(&mut client_random);

    // The client random is RSA-encrypted with 8 trailing zero bytes.
    let mut plain = Vec::with_capacity(40);
    plain.extend_from_slice(&client_random);
    plain.extend_from_slice(&[0u8; 8]);
    let encrypted = rdp_crypto::rsa::encrypt_le(&plain, &cert.modulus, &cert.exponent);

    let exchange = security::security_exchange_pdu(&encrypted);
    send_mcs_raw(transport, user_id, mcs::MCS_USER1, &exchange)?;

    let keys = rdp_crypto::keys::derive(&client_random, &sec.server_random, sec.encryption_method);
    Ok(security::SecurityLayer::new(
        &keys.client_encrypt_key,
        &keys.server_decrypt_key,
        &keys.mac_key,
    ))
}

// --- Step 5: Channel joins -------------------------------------------------

fn join_channels(
    transport: &mut WireTransport,
    user_id: u16,
    blocks: &gcc::ServerDataBlocks,
    names: Vec<String>,
) -> WireResult<(u16, Vec<crate::session::StaticChannel>)> {
    // The server's SC_NET assigns ids: [0] = I/O channel, [1..] = the static
    // channels in the order the client declared them.
    let assigned = blocks
        .net
        .as_ref()
        .map(|n| n.channels.clone())
        .unwrap_or_default();

    let io_channel = assigned.first().copied().unwrap_or(mcs::MCS_IO_CHANNEL);
    join_one(transport, user_id, io_channel)?;

    let mut static_channels = Vec::with_capacity(names.len());
    for (i, name) in names.into_iter().enumerate() {
        let id = assigned
            .get(i + 1)
            .copied()
            .unwrap_or(mcs::MCS_FIRST_STATIC_CHANNEL + i as u16);
        join_one(transport, user_id, id)?;
        static_channels.push(crate::session::StaticChannel { name, id });
    }
    Ok((io_channel, static_channels))
}

fn join_one(transport: &mut WireTransport, user_id: u16, channel_id: u16) -> WireResult<()> {
    let req = mcs::channel_join_request(user_id, channel_id);
    send_mcs(transport, &req)?;
    let confirm = transport.recv()?;
    let got = mcs::parse_channel_join_confirm(&confirm)?.channel_id;
    if got != channel_id {
        return Err(WireError::Sequence(format!(
            "server confirmed channel {got} instead of {channel_id}"
        )));
    }
    Ok(())
}

// --- Step 6: Client Info ---------------------------------------------------

fn send_client_info(
    transport: &mut WireTransport,
    security: &mut security::SecurityLayer,
    user_id: u16,
    io_channel: u16,
    opts: &ConnectOptions,
) -> WireResult<()> {
    let mut packet = Vec::new();
    info::encode(
        &info::ClientInfo {
            domain: opts.domain.clone(),
            username: opts.username.clone(),
            password: opts.password.clone(),
        },
        &mut packet,
    );
    let mut payload = Vec::new();
    security::write_basic_security_header(
        security::SEC_INFO_PKT | security::SEC_ENCRYPT,
        &mut payload,
    );
    payload.extend(security.seal(&packet));
    send_mcs_raw(transport, user_id, io_channel, &payload)
}

// --- Step 7: Licensing -----------------------------------------------------

fn licensing(
    transport: &mut WireTransport,
    security: &mut security::SecurityLayer,
    user_id: u16,
    io_channel: u16,
) -> WireResult<()> {
    loop {
        let (flags, plain) = recv_io_pdu(transport, security, io_channel)?;
        if flags & security::SEC_AUTODETECT_REQ != 0 {
            // Auto-detect probes may arrive interleaved; ignore and continue.
            continue;
        }
        if plain.len() < 4 {
            return Err(WireError::Sequence("short licensing PDU".into()));
        }
        let preamble = license::parse_preamble(&plain)?;
        match preamble.msg_type {
            license::ERROR_ALERT => {
                let err = license::parse_license_error(&plain[4..])?;
                if err.error_code == license::STATUS_VALID_CLIENT {
                    // No CAL needed — reply with a client license error PDU.
                    let reply = license::license_error_pdu(
                        license::STATUS_VALID_CLIENT,
                        license::ST_NO_TRANSITION,
                    );
                    let mut payload = Vec::new();
                    security::write_basic_security_header(security::SEC_ENCRYPT, &mut payload);
                    payload.extend(security.seal(&reply));
                    send_mcs_raw(transport, user_id, io_channel, &payload)?;
                    return Ok(());
                }
                return Err(WireError::Unsupported(format!(
                    "server license error {:#x} (state {:#x})",
                    err.error_code, err.state_transition
                )));
            }
            license::NEW_LICENSE | license::UPGRADE_LICENSE => return Ok(()),
            license::LICENSE_REQUEST | license::PLATFORM_CHALLENGE => {
                return Err(WireError::Unsupported(
                    "server demands full CAL issuance, which this client does not perform".into(),
                ))
            }
            other => {
                return Err(WireError::Sequence(format!(
                    "unexpected licensing message type {other:#04x}"
                )))
            }
        }
    }
}

// --- Step 8: Capability exchange -------------------------------------------

fn capability_exchange(
    transport: &mut WireTransport,
    security: &mut security::SecurityLayer,
    user_id: u16,
    io_channel: u16,
    opts: &ConnectOptions,
) -> WireResult<u32> {
    let (_flags, plain) = loop {
        let (flags, plain) = recv_io_pdu(transport, security, io_channel)?;
        if flags & security::SEC_AUTODETECT_REQ == 0 {
            break (flags, plain);
        }
    };
    let demand = caps::parse_demand_active(&plain)?;

    let pdu_source = user_id;
    let our_caps = caps::all_caps(opts.width, opts.height);
    let confirm = caps::confirm_active_pdu(demand.share_id, pdu_source, &our_caps);
    send_encrypted(transport, security, user_id, io_channel, &confirm)?;

    let sync = caps::synchronize_pdu(demand.share_id, pdu_source, user_id);
    send_encrypted(transport, security, user_id, io_channel, &sync)?;

    let cooperate = caps::control_pdu(demand.share_id, pdu_source, caps::CONTROL_COOPERATE, 1);
    send_encrypted(transport, security, user_id, io_channel, &cooperate)?;

    let request = caps::control_pdu(demand.share_id, pdu_source, caps::CONTROL_REQUEST, 1);
    send_encrypted(transport, security, user_id, io_channel, &request)?;

    // The server replies to the Control Request with a Control Confirm; wait
    // for it so the session starts in a clean state (nothing left in the
    // socket buffer for the application's first recv).
    wait_for_control_confirm(transport, security, io_channel)?;

    Ok(demand.share_id)
}

/// Drain I/O-channel PDUs until the server's Control Confirm arrives.
/// Tolerates unrelated PDUs (server Synchronize, Font List, Auto-Detect).
fn wait_for_control_confirm(
    transport: &mut WireTransport,
    security: &mut security::SecurityLayer,
    io_channel: u16,
) -> WireResult<()> {
    loop {
        let (_flags, plain) = recv_io_pdu(transport, security, io_channel)?;
        let mut cur = &plain[..];
        let Ok((pdu_type, _source)) = caps::read_share_control_header(&mut cur) else {
            continue;
        };
        if pdu_type != caps::PDUTYPE_DATA {
            continue;
        }
        let Ok((_share_id, pdu_type2)) = caps::read_share_data_header(&mut cur) else {
            continue;
        };
        if pdu_type2 != caps::PDUTYPE2_CONTROL || cur.len() < 2 {
            continue;
        }
        let action = u16::from_le_bytes([cur[0], cur[1]]);
        if action == caps::CONTROL_CONFIRM {
            tracing::debug!("received Control Confirm; session active");
            return Ok(());
        }
    }
}

// --- Shared helpers --------------------------------------------------------

/// `CHANNEL_OPTION_INITIALIZED | CHANNEL_OPTION_ENCRYPT_RDP`.
pub(crate) const CHANNEL_OPTION_INITIALIZED: u32 = 0x8000_0000;
/// `CHANNEL_OPTION_ENCRYPT_RDP`.
pub(crate) const CHANNEL_OPTION_ENCRYPT_RDP: u32 = 0x4000_0000;

/// Send a bare MCS PDU framed as TPKT + X.224 Data.
pub(crate) fn send_mcs(transport: &mut WireTransport, mcs_pdu: &[u8]) -> WireResult<()> {
    let mut body = Vec::with_capacity(x224::DATA_HEADER.len() + mcs_pdu.len());
    x224::write_data_header(mcs_pdu.len(), &mut body)?;
    body.extend_from_slice(mcs_pdu);
    transport.send(&body)
}

/// Send a payload as an MCS Send Data Request without encryption (used for the
/// security exchange and Client Info, which carry their own headers).
pub(crate) fn send_mcs_raw(
    transport: &mut WireTransport,
    user_id: u16,
    channel_id: u16,
    payload: &[u8],
) -> WireResult<()> {
    let req = mcs::send_data_request(user_id, channel_id, payload);
    send_mcs(transport, &req)
}

/// Send a slow-path PDU on the I/O channel, sealed with the session keys.
pub(crate) fn send_encrypted(
    transport: &mut WireTransport,
    security: &mut security::SecurityLayer,
    user_id: u16,
    io_channel: u16,
    pdu: &[u8],
) -> WireResult<()> {
    let mut payload = Vec::new();
    security::write_basic_security_header(security::SEC_ENCRYPT, &mut payload);
    payload.extend(security.seal(pdu));
    send_mcs_raw(transport, user_id, io_channel, &payload)
}

/// Receive one I/O-channel PDU: strips MCS framing and the basic security
/// header, decrypts, and returns `(flags, plaintext)`.
pub(crate) fn recv_io_pdu(
    transport: &mut WireTransport,
    security: &mut security::SecurityLayer,
    io_channel: u16,
) -> WireResult<(u16, Vec<u8>)> {
    loop {
        let frame = transport.recv()?;
        let mcs_pdu = mcs::parse_send_data_indication(&frame)?;
        if mcs_pdu.channel_id != io_channel {
            // Not for the I/O channel (e.g. a channel the server opened).
            continue;
        }
        let mut payload = &mcs_pdu.data[..];
        let flags = security::read_basic_security_header(&mut payload)?;
        let plain = if flags & security::SEC_ENCRYPT != 0 {
            security.open(payload)?
        } else {
            payload.to_vec()
        };
        return Ok((flags, plain));
    }
}

/// Deterministic PRNG (xorshift64*) seeded from the system clock and a
/// per-process counter — good enough for the client random.
fn fill_random(buf: &mut [u8]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    let mut state = nanos
        ^ COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
    for chunk in buf.chunks_mut(8) {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let value = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
        let bytes = value.to_le_bytes();
        let n = chunk.len().min(8);
        chunk[..n].copy_from_slice(&bytes[..n]);
    }
}
