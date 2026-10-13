//! Network settings held for the length of a Hyper stream (both ends).
//!
//! - **Wi-Fi media streaming mode, background scans off.** By default a
//!   laptop's Wi-Fi radio naps between beacons — the access point holds its
//!   packets meanwhile, tens of milliseconds — and every minute or so it
//!   leaves the channel to scan for others, a 100+ ms hole. Both wreck a game
//!   stream; Moonlight and Parsec switch them off the same way. Windows puts
//!   them back when our WLAN handle closes (or the process ends).
//! - **QoS.** The stream's socket is marked as audio/video traffic (qWAVE),
//!   which Wi-Fi maps to its video access category (WMM) and routers that
//!   honour DSCP put ahead of bulk downloads.
//!
//! - **Awake.** The system and display stay on: someone watching without
//!   touching the input would otherwise see the laptop dim and sleep — and a
//!   host whose display sleeps gives Desktop Duplication nothing to capture.
//! - **Thread priorities** for the stream's latency-critical threads: a
//!   game on the host (or a browser on the laptop) mustn't delay the threads
//!   that receive input and video.
//!
//! All best effort: a desktop without Wi-Fi, or a qWAVE refusal, just logs.

use std::net::{SocketAddr, UdpSocket};

use windows::Win32::Foundation::HANDLE;

/// Wi-Fi interfaces held in low-latency mode until dropped.
pub struct WifiLowLatency(HANDLE);

impl WifiLowLatency {
    pub fn enable() -> Option<Self> {
        use windows::Win32::NetworkManagement::WiFi::*;
        unsafe {
            let (mut version, mut handle) = (0u32, HANDLE::default());
            if WlanOpenHandle(2, None, &mut version, &mut handle) != 0 {
                return None; // no WLAN service: no Wi-Fi here
            }
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(handle, None, &mut list) != 0 || list.is_null() {
                WlanCloseHandle(handle, None);
                return None;
            }
            let count = (*list).dwNumberOfItems as usize;
            let interfaces = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), count);
            let mut changed = false;
            for i in interfaces.iter().filter(|i| i.isState == wlan_interface_state_connected) {
                let name = String::from_utf16_lossy(&i.strInterfaceDescription)
                    .trim_end_matches('\0')
                    .to_string();
                let set = |opcode, value: windows::core::BOOL| {
                    WlanSetInterface(
                        handle,
                        &i.InterfaceGuid,
                        opcode,
                        std::mem::size_of::<windows::core::BOOL>() as u32,
                        &value as *const _ as *const _,
                        None,
                    )
                };
                let streaming = set(wlan_intf_opcode_media_streaming_mode, true.into());
                let scans = set(wlan_intf_opcode_background_scan_enabled, false.into());
                tracing::info!(
                    adapter = %name,
                    streaming_mode = streaming == 0,
                    background_scans_off = scans == 0,
                    "Wi-Fi low-latency mode for the stream"
                );
                changed |= streaming == 0 || scans == 0;
            }
            WlanFreeMemory(list as *const _);
            if !changed {
                WlanCloseHandle(handle, None);
                return None;
            }
            Some(Self(handle))
        }
    }
}

impl Drop for WifiLowLatency {
    fn drop(&mut self) {
        // Closing the handle hands both settings back to Windows.
        unsafe {
            windows::Win32::NetworkManagement::WiFi::WlanCloseHandle(self.0, None);
        }
    }
}

/// The system and display kept awake until dropped. Per thread: drop it on
/// the thread that made it (it isn't `Send`).
pub struct KeepAwake(std::marker::PhantomData<*const ()>);

impl KeepAwake {
    pub fn new() -> Self {
        use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED};
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
        }
        Self(std::marker::PhantomData)
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}

/// The calling thread in MMCSS's "Games" class (scheduled like a game's own
/// render thread) until dropped.
pub struct ThreadBoost(HANDLE);

impl ThreadBoost {
    pub fn games() -> Option<Self> {
        use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
        let mut index = 0u32;
        match unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Games"), &mut index) } {
            Ok(h) => Some(Self(h)),
            Err(e) => {
                tracing::debug!(error = %e, "no MMCSS boost for this thread");
                None
            }
        }
    }
}

impl Drop for ThreadBoost {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.0);
        }
    }
}

/// Raise the calling thread (a stream thread that lives as long as the
/// stream) above normal work.
pub fn raise_thread() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

/// A socket's traffic marked as audio/video until dropped.
pub struct QosFlow(HANDLE);

impl QosFlow {
    /// Mark `socket`: connected (`dest` None), or sending to `dest` (an
    /// unconnected socket's flows are per destination; IPv4 only here).
    pub fn tag(socket: &UdpSocket, dest: Option<SocketAddr>) -> Option<Self> {
        use std::os::windows::io::AsRawSocket;
        use windows::Win32::NetworkManagement::QoS::*;
        use windows::Win32::Networking::WinSock::{AF_INET, IN_ADDR, IN_ADDR_0, SOCKADDR, SOCKADDR_IN, SOCKET};
        let dest_v4 = match dest {
            None => None,
            Some(SocketAddr::V4(v4)) => Some(SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: v4.port().to_be(),
                sin_addr: IN_ADDR { S_un: IN_ADDR_0 { S_addr: u32::from_ne_bytes(v4.ip().octets()) } },
                sin_zero: [0; 8],
            }),
            Some(SocketAddr::V6(_)) => return None,
        };
        unsafe {
            let version = QOS_VERSION { MajorVersion: 1, MinorVersion: 0 };
            let mut handle = HANDLE::default();
            if !QOSCreateHandle(&version, &mut handle).as_bool() {
                tracing::debug!("qWAVE unavailable; stream packets go unmarked");
                return None;
            }
            let mut flow = 0u32;
            let ok = QOSAddSocketToFlow(
                handle,
                SOCKET(socket.as_raw_socket() as usize),
                dest_v4.as_ref().map(|a| a as *const SOCKADDR_IN as *const SOCKADDR),
                QOSTrafficTypeAudioVideo,
                Some(QOS_NON_ADAPTIVE_FLOW),
                &mut flow,
            );
            if !ok.as_bool() {
                tracing::debug!(error = %windows::core::Error::from_thread(), "couldn't mark the stream as audio/video traffic");
                let _ = QOSCloseHandle(handle);
                return None;
            }
            tracing::info!("stream packets marked as audio/video traffic (QoS)");
            Some(Self(handle))
        }
    }
}

impl Drop for QosFlow {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::NetworkManagement::QoS::QOSCloseHandle(self.0);
        }
    }
}

/// The system timer at 1 ms while it lives (this process only, on Windows 10
/// 2004 and later: nothing else is kept awake by it). Media Foundation's
/// hardware video decoder waits for a busy surface with a short sleep, which
/// the default 15.6 ms tick turned into a 13-15 ms stall on one frame in ten
/// — a 240 fps stream decoded at ~150 and showed ~30. Measured at 1080p HEVC,
/// 50 Mbps: decode p90 13.4 ms → 0.5 ms.
pub struct FineTimer;

#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period_ms: u32) -> u32;
    fn timeEndPeriod(period_ms: u32) -> u32;
}

impl FineTimer {
    pub fn new() -> Self {
        unsafe {
            timeBeginPeriod(1);
        }
        Self
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        unsafe {
            timeEndPeriod(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a real network path (set HYPER_QOS_TARGET=ip:port, e.g. the
    /// router): both socket kinds get marked, and the guards drop cleanly.
    #[test]
    #[ignore = "needs a network target"]
    fn sockets_toward_a_real_host_are_marked() {
        let Ok(target) = std::env::var("HYPER_QOS_TARGET") else { return };
        let target: SocketAddr = target.parse().unwrap();
        let connected = UdpSocket::bind("0.0.0.0:0").unwrap();
        connected.connect(target).unwrap();
        assert!(QosFlow::tag(&connected, None).is_some(), "connected socket");
        let unconnected = UdpSocket::bind("0.0.0.0:0").unwrap();
        assert!(QosFlow::tag(&unconnected, Some(target)).is_some(), "unconnected socket");
        let wifi = WifiLowLatency::enable();
        println!("wifi low-latency: {}", wifi.is_some());
        let _awake = KeepAwake::new();
        let _boost = ThreadBoost::games();
        raise_thread();
    }
}
