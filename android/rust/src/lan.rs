//! The PC's local addresses, kept (by the address the user gave) for a
//! direct path when the tablet is on its network: see `rdp_hyper::lan`.
//! (`debug.rdpio.lanpath=0`: always the address given.)

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Mutex;

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The app's files folder.
pub fn set_dir(dir: PathBuf) {
    *DIR.lock().unwrap() = Some(dir);
}

fn file() -> Option<PathBuf> {
    DIR.lock().unwrap().as_ref().map(|d| d.join("lan.txt"))
}

/// Keep `host`'s local addresses, as its latest offer gave them. (`lan.txt`:
/// a line `host addr/prefix…` per host.)
pub fn remember(host: &str, lan: &[String]) {
    let Some(f) = file() else { return };
    let old = std::fs::read_to_string(&f).unwrap_or_default();
    let mut lines: Vec<String> = old.lines().filter(|l| l.split(' ').next() != Some(host)).map(String::from).collect();
    if !lan.is_empty() {
        lines.push(format!("{host} {}", lan.join(" ")));
    }
    let new = lines.join("\n");
    if new != old {
        let _ = std::fs::write(&f, new);
    }
}

fn remembered(host: &str) -> Vec<String> {
    let Some(f) = file() else { return Vec::new() };
    let text = std::fs::read_to_string(f).unwrap_or_default();
    text.lines().find(|l| l.split(' ').next() == Some(host)).map_or_else(Vec::new, |l| l.split(' ').skip(1).map(String::from).collect())
}

/// This device's IPv4 networks now.
fn own_networks() -> Vec<(Ipv4Addr, u8)> {
    let mut out = Vec::new();
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut a = list;
        while !a.is_null() {
            let i = &*a;
            a = i.ifa_next;
            if i.ifa_addr.is_null() || i.ifa_netmask.is_null() || (*i.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let ip = Ipv4Addr::from(u32::from_be((*(i.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr));
            let mask = u32::from_be((*(i.ifa_netmask as *const libc::sockaddr_in)).sin_addr.s_addr);
            if !ip.is_loopback() {
                out.push((ip, mask.count_ones() as u8));
            }
        }
        libc::freeifaddrs(list);
    }
    out
}

/// One of `host`'s local addresses, on a network this device is on and
/// answering there; `None` to use `via` (the address given).
pub fn direct(host: &str, via: SocketAddr) -> Option<SocketAddr> {
    if !crate::tune::ab("lanpath", true) {
        return None;
    }
    rdp_hyper::lan::direct(&remembered(host), &own_networks(), via)
}
