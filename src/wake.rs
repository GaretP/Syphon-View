//! Waking a sleeping Syphon at home: a wake-on-LAN packet to its network card, sent straight to its address (the
//! router passes it on to the sleeping card) and as a broadcast on this computer's own networks (for a node on the
//! same network). Away from home the VPN box does the waking.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

/// "aa:bb:cc:dd:ee:ff" → the magic packet: six 0xFF bytes, then the address sixteen times.
pub fn magic_packet(mac: &str) -> Option<Vec<u8>> {
    let b: Vec<u8> = mac.split([':', '-']).map(|p| u8::from_str_radix(p, 16).ok()).collect::<Option<Vec<u8>>>()?;
    if b.len() != 6 {
        return None;
    }
    let mut p = vec![0xFF; 6];
    for _ in 0..16 {
        p.extend_from_slice(&b);
    }
    Some(p)
}

/// The address in "http://192.168.0.2:8096".
pub fn host_of(url: &str) -> Option<Ipv4Addr> {
    url.split("://").nth(1)?.split(['/', ':']).next()?.parse().ok()
}

/// Is this computer on the same network as one of the node's home addresses (so it is at home)?
pub fn at_home(lan: &[String]) -> bool {
    let hosts: Vec<Ipv4Addr> = lan.iter().filter_map(|u| host_of(u)).collect();
    if_addrs::get_if_addrs().unwrap_or_default().into_iter().any(|i| match i.addr {
        if_addrs::IfAddr::V4(a) if !a.ip.is_loopback() => hosts.iter().any(|h| same_net(*h, a.ip, a.netmask)),
        _ => false,
    })
}

pub fn same_net(a: Ipv4Addr, b: Ipv4Addr, mask: Ipv4Addr) -> bool {
    u32::from(a) & u32::from(mask) == u32::from(b) & u32::from(mask)
}

/// Send the wake packet to each of the node's home addresses and to this computer's network broadcasts.
pub fn send(mac: &str, lan: &[String]) -> bool {
    let Some(p) = magic_packet(mac) else { return false };
    let Ok(s) = UdpSocket::bind("0.0.0.0:0") else { return false };
    let _ = s.set_broadcast(true);
    let mut to: Vec<SocketAddr> = lan.iter().filter_map(|u| host_of(u)).map(|h| SocketAddr::from((h, 9))).collect();
    to.push(SocketAddr::from((Ipv4Addr::BROADCAST, 9)));
    for i in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(a) = i.addr {
            if let (false, Some(b)) = (a.ip.is_loopback(), a.broadcast) {
                to.push(SocketAddr::new(IpAddr::V4(b), 9));
            }
        }
    }
    let mut any = false;
    for t in to {
        any |= s.send_to(&p, t).is_ok();
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_packet_and_addresses_are_right() {
        let p = magic_packet("01:02:03:04:05:06").unwrap();
        assert_eq!((p.len(), &p[..6]), (102, &[0xFF; 6][..]));
        assert!(magic_packet("01:02:03").is_none() && magic_packet("zz:02:03:04:05:06").is_none());
        assert_eq!(host_of("http://192.168.0.2:8096"), Some(Ipv4Addr::new(192, 168, 0, 2)));
        assert_eq!(host_of("http://media.lan:8096"), None);
        let m = Ipv4Addr::new(255, 255, 255, 0);
        assert!(same_net(Ipv4Addr::new(192, 168, 0, 2), Ipv4Addr::new(192, 168, 0, 57), m));
        assert!(!same_net(Ipv4Addr::new(192, 168, 0, 2), Ipv4Addr::new(192, 168, 1, 57), m));
    }
}
