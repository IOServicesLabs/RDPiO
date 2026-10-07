//! Inbound RTP probe: logs what actually arrives on each remote stream.
//!
//! A webrtc-rs interceptor that sits under every inbound RTP stream and logs
//! the SSRC, payload type, sequence number and size of its first packets (and
//! then every power-of-two packet). Diagnostics for traffic webrtc-rs won't
//! classify — e.g. a payload type missing from the answer, which makes it drop
//! the packet before the application ever sees it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use webrtc::interceptor::stream_info::StreamInfo;
use webrtc::interceptor::{
    Attributes, Error, Interceptor, InterceptorBuilder, RTCPReader, RTCPWriter, RTPReader,
    RTPWriter,
};
use webrtc::rtp::packet::Packet;

/// How many packets per stream are logged individually.
const LOG_FIRST: u64 = 12;

/// Builds an [`RtpProbe`] per peer connection.
pub struct RtpProbeBuilder;

impl InterceptorBuilder for RtpProbeBuilder {
    fn build(&self, _id: &str) -> Result<Arc<dyn Interceptor + Send + Sync>, Error> {
        Ok(Arc::new(RtpProbe))
    }
}

/// Passes everything through; wraps inbound RTP streams in a [`ProbeReader`].
pub struct RtpProbe;

#[async_trait]
impl Interceptor for RtpProbe {
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        Arc::new(RtcpProbeReader { inner: reader, packets: AtomicU64::new(0) })
    }

    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        writer
    }

    async fn bind_local_stream(
        &self,
        _info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        writer
    }

    async fn unbind_local_stream(&self, _info: &StreamInfo) {}

    async fn bind_remote_stream(
        &self,
        info: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        tracing::info!(
            ssrc = info.ssrc,
            payload_type = info.payload_type,
            mime = %info.mime_type,
            "inbound RTP stream bound"
        );
        Arc::new(ProbeReader {
            inner: reader,
            ssrc: info.ssrc,
            packets: AtomicU64::new(0),
        })
    }

    async fn unbind_remote_stream(&self, _info: &StreamInfo) {}

    async fn close(&self) -> Result<(), Error> {
        Ok(())
    }
}

/// Logs the remote side's RTCP: every sender report (its own count of what it
/// sent on each SSRC — compare with the "inbound RTP" count to tell media never
/// sent from media lost on the way) and the first few other packets.
struct RtcpProbeReader {
    inner: Arc<dyn RTCPReader + Send + Sync>,
    packets: AtomicU64,
}

#[async_trait]
impl RTCPReader for RtcpProbeReader {
    async fn read(
        &self,
        buf: &mut [u8],
        attributes: &Attributes,
    ) -> Result<(Vec<Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>>, Attributes), Error> {
        let result = self.inner.read(buf, attributes).await;
        if let Ok((pkts, _)) = &result {
            for p in pkts {
                let n = self.packets.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some(sr) = p
                    .as_any()
                    .downcast_ref::<webrtc::rtcp::sender_report::SenderReport>()
                {
                    tracing::info!(
                        ssrc = sr.ssrc,
                        packet_count = sr.packet_count,
                        octet_count = sr.octet_count,
                        rtp_time = sr.rtp_time,
                        "inbound RTCP sender report"
                    );
                } else if n <= 2 * LOG_FIRST || n.is_power_of_two() {
                    tracing::info!(
                        packet_type = ?p.header().packet_type,
                        ssrcs = ?p.destination_ssrc(),
                        n,
                        "inbound RTCP"
                    );
                }
            }
        }
        result
    }
}

/// Logs packets as they are read through it.
struct ProbeReader {
    inner: Arc<dyn RTPReader + Send + Sync>,
    ssrc: u32,
    packets: AtomicU64,
}

#[async_trait]
impl RTPReader for ProbeReader {
    async fn read(
        &self,
        buf: &mut [u8],
        attributes: &Attributes,
    ) -> Result<(Packet, Attributes), Error> {
        let result = self.inner.read(buf, attributes).await;
        if let Ok((pkt, _)) = &result {
            let n = self.packets.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= LOG_FIRST || n.is_power_of_two() {
                tracing::info!(
                    ssrc = self.ssrc,
                    packet_ssrc = pkt.header.ssrc,
                    payload_type = pkt.header.payload_type,
                    seq = pkt.header.sequence_number,
                    marker = pkt.header.marker,
                    padding = pkt.header.padding,
                    len = pkt.payload.len(),
                    n,
                    "inbound RTP"
                );
            }
        }
        result
    }
}
