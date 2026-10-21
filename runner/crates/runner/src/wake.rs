//! Wake-on-LAN (docs/protocol.md "Keep-awake and Wake-on-LAN"): this
//! machine's network interfaces for `hello.runner.network`, and `wol.send`,
//! which wakes another machine on the same network.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, UdpSocket};
use tmuxedo_protocol::wake::{
    broadcast_of, format_mac, magic_packet, parse_mac, Interface, Network, MAX_INTERFACES, WOL_PORT,
};

/// Interface name prefixes of virtual, tunnel and container links: never a
/// network another machine sleeps on.
const VIRTUAL: &[&str] = &[
    "docker",
    "veth",
    "br-",
    "virbr",
    "tun",
    "tap",
    "utun",
    "tailscale",
    "wg",
    "zt",
    "cni",
    "flannel",
    "cali",
    "vmnet",
    "vboxnet",
    "lxc",
    "lxd",
    "podman",
    "awdl",
    "llw",
    "bridge",
    "gif",
    "stf",
    "anpi",
    "ap",
];

/// One interface as the OS lists it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawIf {
    pub name: String,
    pub up: bool,
    pub loopback: bool,
    pub mac: Option<[u8; 6]>,
    pub ipv4: Option<(Ipv4Addr, u8)>,
}

/// The interfaces worth reporting: up, physical-looking, with a MAC and an IPv4.
pub fn pick(raw: Vec<RawIf>) -> Network {
    let mut interfaces = vec![];
    for r in raw {
        if !r.up || r.loopback || VIRTUAL.iter().any(|v| r.name.starts_with(v)) {
            continue;
        }
        let (Some(mac), Some((ip, prefix))) = (r.mac, r.ipv4) else {
            continue;
        };
        if mac == [0; 6] || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() {
            continue;
        }
        interfaces.push(Interface {
            mac: format_mac(&mac),
            ipv4: ip.to_string(),
            prefix,
        });
        if interfaces.len() == MAX_INTERFACES {
            break;
        }
    }
    Network { interfaces }
}

/// This machine's interfaces now.
pub fn discover() -> Network {
    pick(list())
}

/// getifaddrs, merged by interface name (first IPv4 per interface).
fn list() -> Vec<RawIf> {
    let mut by: BTreeMap<String, RawIf> = BTreeMap::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return vec![];
    }
    let mut cur = head;
    while !cur.is_null() {
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_name.is_null() || ifa.ifa_addr.is_null() {
            continue;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }
            .to_string_lossy()
            .into_owned();
        let flags = ifa.ifa_flags as libc::c_int;
        let e = by.entry(name.clone()).or_insert_with(|| RawIf {
            name,
            ..Default::default()
        });
        e.up = flags & libc::IFF_UP != 0;
        e.loopback = flags & libc::IFF_LOOPBACK != 0;
        let family = libc::c_int::from(unsafe { (*ifa.ifa_addr).sa_family });
        if family == libc::AF_INET {
            if e.ipv4.is_some() || ifa.ifa_netmask.is_null() {
                continue;
            }
            let a = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
            let m = unsafe { &*(ifa.ifa_netmask as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr));
            let prefix = u32::from_be(m.sin_addr.s_addr).count_ones() as u8;
            e.ipv4 = Some((ip, prefix));
        } else if let Some(mac) = unsafe { link_mac(ifa.ifa_addr) } {
            e.mac = Some(mac);
        }
    }
    unsafe { libc::freeifaddrs(head) };
    by.into_values().collect()
}

#[cfg(target_os = "linux")]
unsafe fn link_mac(sa: *const libc::sockaddr) -> Option<[u8; 6]> {
    if libc::c_int::from((*sa).sa_family) != libc::AF_PACKET {
        return None;
    }
    let ll = &*(sa as *const libc::sockaddr_ll);
    if ll.sll_halen != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&ll.sll_addr[..6]);
    Some(mac)
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
unsafe fn link_mac(sa: *const libc::sockaddr) -> Option<[u8; 6]> {
    if libc::c_int::from((*sa).sa_family) != libc::AF_LINK {
        return None;
    }
    let dl = sa as *const libc::sockaddr_dl;
    if (*dl).sdl_alen != 6 {
        return None;
    }
    // The address follows the name inside sdl_data (which can be longer
    // than the struct declares).
    let base = std::ptr::addr_of!((*dl).sdl_data) as *const u8;
    let p = base.add(usize::from((*dl).sdl_nlen));
    let mut mac = [0u8; 6];
    mac.copy_from_slice(std::slice::from_raw_parts(p, 6));
    Some(mac)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios", target_os = "freebsd")))]
unsafe fn link_mac(_sa: *const libc::sockaddr) -> Option<[u8; 6]> {
    None
}

/// A checked `wol.send`: the MAC to wake and the broadcast address to send to.
#[derive(Debug, PartialEq)]
pub struct Send {
    pub mac: [u8; 6],
    pub broadcast: Ipv4Addr,
}

/// Validate `wol.send` params against this machine's own interfaces: the
/// broadcast must be one of theirs (or the limited broadcast), so the method
/// only ever reaches the network this machine is on.
pub fn check(params: &serde_json::Value, own: &Network) -> Result<Send, String> {
    let mac = params["mac"]
        .as_str()
        .and_then(parse_mac)
        .ok_or("mac must be a unicast hardware address like 02:00:00:00:00:01")?;
    let broadcast: Ipv4Addr = params["broadcast"]
        .as_str()
        .ok_or("broadcast must be an IPv4 address")?
        .parse()
        .map_err(|_| "broadcast must be an IPv4 address")?;
    let ours = broadcast == Ipv4Addr::BROADCAST
        || own
            .interfaces
            .iter()
            .any(|i| i.ipv4.parse().ok().and_then(|ip| broadcast_of(ip, i.prefix)) == Some(broadcast));
    if !ours {
        return Err("broadcast is not on a network this machine is on".into());
    }
    if own.interfaces.iter().any(|i| parse_mac(&i.mac) == Some(mac)) {
        return Err("that is this machine, which is awake".into());
    }
    Ok(Send { mac, broadcast })
}

/// Send the wake packet (three times: UDP is lossy) to `broadcast`:9.
pub fn send(s: &Send) -> Result<(), String> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
    sock.set_broadcast(true).map_err(|e| e.to_string())?;
    let p = magic_packet(&s.mac);
    for _ in 0..3 {
        sock.send_to(&p, (s.broadcast, WOL_PORT)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

impl crate::daemon::Daemon {
    /// `wol.send {mac, broadcast}` (gateway only): wake another machine on
    /// this machine's network.
    pub(crate) fn wol_send(&mut self, params: &serde_json::Value, reply: crate::api::ReplyTo) {
        use crate::api::{err, respond, ReplyTo};
        if matches!(reply, ReplyTo::Local(_)) {
            let e = err(
                "not_supported",
                "Waking machines is not available on the machine's local socket.",
            );
            return respond(reply, Err(e), self);
        }
        let r = match check(params, &discover()) {
            Err(m) => Err(err("invalid_params", m)),
            Ok(s) => match send(&s) {
                Ok(()) => {
                    crate::log!("wake: sent a wake packet to {}:{WOL_PORT}", s.broadcast);
                    Ok(serde_json::json!({
                        "type": "wol_sent",
                        "broadcast": s.broadcast.to_string(),
                        "port": WOL_PORT,
                    }))
                }
                Err(m) => Err(err("send_failed", format!("could not send the wake packet: {m}"))),
            },
        };
        respond(reply, r, self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(name: &str, mac: [u8; 6], ip: [u8; 4], prefix: u8) -> RawIf {
        RawIf {
            name: name.into(),
            up: true,
            loopback: false,
            mac: Some(mac),
            ipv4: Some((Ipv4Addr::from(ip), prefix)),
        }
    }

    #[test]
    fn picks_physical_interfaces() {
        let mac = [2, 0, 0, 0, 0, 1];
        let mut down = raw("eth1", mac, [198, 51, 100, 2], 24);
        down.up = false;
        let mut lo = raw("lo", [0; 6], [127, 0, 0, 1], 8);
        lo.loopback = true;
        let n = pick(vec![
            raw("eth0", mac, [192, 0, 2, 20], 24),
            down,
            lo,
            raw("docker0", [2, 0, 0, 0, 0, 2], [172, 17, 0, 1], 16),
            raw("tailscale0", [2, 0, 0, 0, 0, 3], [100, 64, 0, 1], 32),
            raw("en0", [2, 0, 0, 0, 0, 4], [169, 254, 1, 1], 16),
            RawIf {
                name: "wlan0".into(),
                up: true,
                mac: Some(mac),
                ..Default::default()
            },
        ]);
        assert_eq!(
            n.interfaces,
            vec![Interface {
                mac: "02:00:00:00:00:01".into(),
                ipv4: "192.0.2.20".into(),
                prefix: 24
            }]
        );
        let many = (0..20)
            .map(|i| raw(&format!("eth{i}"), mac, [10, 0, i, 1], 24))
            .collect();
        assert_eq!(pick(many).interfaces.len(), MAX_INTERFACES);
    }

    #[test]
    fn checks_params_against_own_networks() {
        let own = pick(vec![raw("eth0", [2, 0, 0, 0, 0, 1], [192, 0, 2, 20], 24)]);
        let ok = check(&json!({"mac": "02:00:00:00:00:09", "broadcast": "192.0.2.255"}), &own).unwrap();
        assert_eq!(ok.broadcast, Ipv4Addr::new(192, 0, 2, 255));
        assert!(check(
            &json!({"mac": "02:00:00:00:00:09", "broadcast": "255.255.255.255"}),
            &own
        )
        .is_ok());
        for bad in [
            json!({"mac": "02:00:00:00:00:09", "broadcast": "198.51.100.255"}),
            json!({"mac": "02:00:00:00:00:09", "broadcast": "192.0.2.20"}),
            json!({"mac": "02:00:00:00:00:09"}),
            json!({"mac": "nope", "broadcast": "192.0.2.255"}),
            json!({"mac": "02:00:00:00:00:01", "broadcast": "192.0.2.255"}),
        ] {
            assert!(check(&bad, &own).is_err(), "{bad}");
        }
    }

    #[test]
    fn discover_does_not_panic() {
        let n = discover();
        assert!(n.interfaces.len() <= MAX_INTERFACES);
    }
}
