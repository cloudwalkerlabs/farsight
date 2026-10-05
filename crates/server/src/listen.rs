//! Guard rails for plaintext mode (docs/design.md §1): it needs an explicit
//! address, and warns unless that address is on a network that encrypts
//! and authenticates by itself.

use std::ffi::CStr;
use std::net::IpAddr;

/// Why `addr` looks like a protected network, if it does.
pub fn protected(addr: IpAddr) -> Option<String> {
    if addr.is_loopback() {
        return Some("loopback".into());
    }
    let tailscale = match addr {
        // 100.64.0.0/10, Tailscale's CGNAT range.
        IpAddr::V4(v4) => v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64,
        // fd7a:115c:a1e0::/48.
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    };
    if tailscale {
        return Some("Tailscale's address range".into());
    }
    let name = interface_of(addr)?;
    (name.starts_with("tailscale") || name.starts_with("wg")).then(|| format!("interface {name}"))
}

/// The name of the interface that has `addr`.
fn interface_of(addr: IpAddr) -> Option<String> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, which is freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cur = list;
    while !cur.is_null() {
        // SAFETY: a node of the list getifaddrs returned.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: ifa_addr points to a sockaddr of the family it names.
        let ip = unsafe {
            match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::from(u32::from_be(sin.sin_addr.s_addr).to_be_bytes())
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::from(sin6.sin6_addr.s6_addr)
                }
                _ => continue,
            }
        };
        if ip == addr {
            // SAFETY: ifa_name is a NUL-terminated string.
            found = Some(unsafe { CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned());
            break;
        }
    }
    // SAFETY: the list from getifaddrs, freed once.
    unsafe { libc::freeifaddrs(list) };
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert!(protected("127.0.0.1".parse().unwrap()).is_some());
        assert!(protected("100.101.102.103".parse().unwrap()).is_some());
        assert!(protected("100.128.0.1".parse().unwrap()).is_none());
        assert!(protected("fd7a:115c:a1e0::1".parse().unwrap()).is_some());
        assert!(protected("192.0.2.1".parse().unwrap()).is_none());
    }
}
