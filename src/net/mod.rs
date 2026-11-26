//! Network interface detection and helper utilities.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkInterfaceInfo {
    pub name: String,
    pub ip: String,
    pub is_ipv6: bool,
}

#[cfg(unix)]
pub fn get_local_interfaces() -> Vec<NetworkInterfaceInfo> {
    use std::ffi::CStr;
    use std::net::{Ipv4Addr, Ipv6Addr};

    let mut result = Vec::new();
    let mut ifaddr_ptr: *mut libc::ifaddrs = std::ptr::null_mut();

    let ret = unsafe { libc::getifaddrs(&mut ifaddr_ptr) };
    if ret != 0 || ifaddr_ptr.is_null() {
        return result;
    }

    let mut curr = ifaddr_ptr;
    while !curr.is_null() {
        let ifa = unsafe { &*curr };
        if !ifa.ifa_addr.is_null() && !ifa.ifa_name.is_null() {
            let flags = ifa.ifa_flags as i32;
            let is_loopback = (flags & libc::IFF_LOOPBACK) != 0;
            let is_up = (flags & libc::IFF_UP) != 0;

            if is_up && !is_loopback {
                let name = unsafe { CStr::from_ptr(ifa.ifa_name) }
                    .to_string_lossy()
                    .into_owned();
                let family = unsafe { (*ifa.ifa_addr).sa_family as i32 };

                if family == libc::AF_INET {
                    let addr_in = unsafe { *(ifa.ifa_addr as *const libc::sockaddr_in) };
                    let ip = Ipv4Addr::from(u32::from_be(addr_in.sin_addr.s_addr));
                    if !ip.is_loopback() && !ip.is_unspecified() {
                        result.push(NetworkInterfaceInfo {
                            name,
                            ip: ip.to_string(),
                            is_ipv6: false,
                        });
                    }
                } else if family == libc::AF_INET6 {
                    let addr_in6 = unsafe { *(ifa.ifa_addr as *const libc::sockaddr_in6) };
                    let ip = Ipv6Addr::from(addr_in6.sin6_addr.s6_addr);
                    if !ip.is_loopback()
                        && !ip.is_unspecified()
                        && (ip.segments()[0] & 0xffc0) != 0xfe80
                    {
                        result.push(NetworkInterfaceInfo {
                            name,
                            ip: ip.to_string(),
                            is_ipv6: true,
                        });
                    }
                }
            }
        }
        curr = ifa.ifa_next;
    }

    unsafe {
        libc::freeifaddrs(ifaddr_ptr);
    }

    result
}

#[cfg(not(unix))]
pub fn get_local_interfaces() -> Vec<NetworkInterfaceInfo> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_local_interfaces_no_loopback() {
        let ifaces = get_local_interfaces();
        for iface in ifaces {
            assert_ne!(iface.ip, "127.0.0.1");
            assert_ne!(iface.ip, "::1");
            assert!(!iface.ip.starts_with("fe80:"));
        }
    }
}
