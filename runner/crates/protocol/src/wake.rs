//! Keep-awake and Wake-on-LAN (docs/protocol.md "Keep-awake and Wake-on-LAN").
//!
//! A runner holds the OS sleep inhibitor while an agent in one of its panes is
//! working, and reports that in `hello.runner.keepAwake`. It also reports the
//! network interfaces a sleeping machine can be woken through
//! (`hello.runner.network`), so web can wake it via another online machine of
//! the same owner on the same network ([`WOL_SEND_METHOD`]).

use serde::{Deserialize, Serialize};

/// Gateway-only: send a Wake-on-LAN packet for another machine on this
/// machine's network. Refused on the local socket.
pub const WOL_SEND_METHOD: &str = "wol.send";

/// The UDP port the wake packet goes to (the discard port, the common default).
pub const WOL_PORT: u16 = 9;

/// The most interfaces a hello reports.
pub const MAX_INTERFACES: usize = 8;

/// `hello.runner.keepAwake`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeepAwake {
    /// The owner's setting (default on).
    pub enabled: bool,
    /// This OS has a sleep inhibitor the runner can hold.
    pub available: bool,
    /// The inhibitor is held now.
    pub held: bool,
    /// Why it is not held although agents work (`battery`), if so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `hello.runner.network`: the physical interfaces with an IPv4 address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Network {
    pub interfaces: Vec<Interface>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interface {
    /// `aa:bb:cc:dd:ee:ff`, lower case.
    pub mac: String,
    /// Dotted IPv4 address.
    pub ipv4: String,
    /// Netmask length (0–32).
    pub prefix: u8,
}

impl Interface {
    /// The interface's directed broadcast address.
    pub fn broadcast(&self) -> Option<std::net::Ipv4Addr> {
        let ip: std::net::Ipv4Addr = self.ipv4.parse().ok()?;
        broadcast_of(ip, self.prefix)
    }
}

/// The directed broadcast address of `ip/prefix` (None for /31 and /32).
pub fn broadcast_of(ip: std::net::Ipv4Addr, prefix: u8) -> Option<std::net::Ipv4Addr> {
    if prefix > 30 {
        return None;
    }
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Some(std::net::Ipv4Addr::from(u32::from(ip) | !mask))
}

/// Parse `aa:bb:cc:dd:ee:ff` (or `-` separated); rejects all-zero and
/// broadcast/multicast addresses.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        if p.len() != 2 {
            return None;
        }
        mac[i] = u8::from_str_radix(p, 16).ok()?;
    }
    if mac == [0; 6] || mac[0] & 1 == 1 {
        return None;
    }
    Some(mac)
}

/// Format a MAC as lower-case `aa:bb:cc:dd:ee:ff`.
pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// The 102-byte wake packet: six 0xFF then the MAC sixteen times.
pub fn magic_packet(mac: &[u8; 6]) -> [u8; 102] {
    let mut p = [0xFFu8; 102];
    for i in 0..16 {
        p[6 + i * 6..12 + i * 6].copy_from_slice(mac);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn packet_layout() {
        let mac = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
        let p = magic_packet(&mac);
        assert_eq!(&p[..6], &[0xFF; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..12 + i * 6], &mac);
        }
    }

    #[test]
    fn macs() {
        assert_eq!(parse_mac("02:11:22:33:44:55"), Some([2, 0x11, 0x22, 0x33, 0x44, 0x55]));
        assert_eq!(parse_mac("02-AB-22-33-44-55"), Some([2, 0xab, 0x22, 0x33, 0x44, 0x55]));
        assert_eq!(parse_mac("00:00:00:00:00:00"), None);
        assert_eq!(parse_mac("ff:ff:ff:ff:ff:ff"), None);
        assert_eq!(parse_mac("01:00:5e:00:00:01"), None);
        assert_eq!(parse_mac("02:11:22:33:44"), None);
        assert_eq!(parse_mac("02:11:22:33:44:5"), None);
        assert_eq!(parse_mac("zz:11:22:33:44:55"), None);
        assert_eq!(format_mac(&[2, 0xab, 0, 1, 2, 3]), "02:ab:00:01:02:03");
    }

    #[test]
    fn broadcasts() {
        assert_eq!(
            broadcast_of(Ipv4Addr::new(192, 168, 1, 20), 24),
            Some(Ipv4Addr::new(192, 168, 1, 255))
        );
        assert_eq!(
            broadcast_of(Ipv4Addr::new(10, 1, 2, 3), 16),
            Some(Ipv4Addr::new(10, 1, 255, 255))
        );
        assert_eq!(broadcast_of(Ipv4Addr::new(10, 1, 2, 3), 32), None);
        let i = Interface {
            mac: "02:11:22:33:44:55".into(),
            ipv4: "203.0.113.9".into(),
            prefix: 20,
        };
        assert_eq!(i.broadcast(), Some(Ipv4Addr::new(203, 0, 127, 255)));
    }

    #[test]
    fn keep_awake_wire() {
        let k = KeepAwake {
            enabled: true,
            available: true,
            held: false,
            reason: None,
        };
        assert_eq!(
            serde_json::to_value(&k).unwrap(),
            serde_json::json!({"enabled": true, "available": true, "held": false})
        );
    }
}
