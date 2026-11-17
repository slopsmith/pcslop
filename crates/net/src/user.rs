//! Settings for user-mode networking, in the syntax of QEMU's `-netdev user`.

use std::net::Ipv4Addr;

/// The guest's network, laid out as QEMU lays it out.
pub const NETWORK: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 0);
pub const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
pub const DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
/// The first address DHCP hands out, and so the guest's.
pub const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);

/// Forwards connections to a host port into the guest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HostFwd {
    pub udp: bool,
    pub host_addr: Ipv4Addr,
    pub host_port: u16,
    pub guest_addr: Ipv4Addr,
    pub guest_port: u16,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct UserNet {
    pub hostfwd: Vec<HostFwd>,
}

impl UserNet {
    /// Parses `user[,hostfwd=[tcp|udp]:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT]...`.
    /// Unlike QEMU, an empty host address means 127.0.0.1 rather than every
    /// interface, so a forwarded guest port stays private to this Mac.
    pub fn parse(spec: &str) -> Result<UserNet, String> {
        let mut parts = spec.split(',');
        if parts.next() != Some("user") {
            return Err(format!("unknown network {spec:?}; only `user` is supported"));
        }
        let mut net = UserNet::default();
        for opt in parts {
            match opt.split_once('=') {
                Some(("hostfwd", rule)) => net.hostfwd.push(parse_hostfwd(rule)?),
                _ => return Err(format!("unknown network option {opt:?}")),
            }
        }
        Ok(net)
    }
}

fn parse_hostfwd(rule: &str) -> Result<HostFwd, String> {
    let bad = || format!("hostfwd {rule:?} is not [tcp|udp]:[HOSTADDR]:HOSTPORT-[GUESTADDR]:GUESTPORT");
    let (proto, rest) = rule.split_once(':').ok_or_else(bad)?;
    let udp = match proto {
        "" | "tcp" => false,
        "udp" => true,
        _ => return Err(bad()),
    };
    let (host, guest) = rest.split_once('-').ok_or_else(bad)?;
    let endpoint = |s: &str, default: Ipv4Addr| -> Result<(Ipv4Addr, u16), String> {
        let (addr, port) = s.rsplit_once(':').ok_or_else(bad)?;
        let addr = if addr.is_empty() { default } else { addr.parse().map_err(|_| bad())? };
        Ok((addr, port.parse().map_err(|_| bad())?))
    };
    let (host_addr, host_port) = endpoint(host, Ipv4Addr::LOCALHOST)?;
    let (guest_addr, guest_port) = endpoint(guest, GUEST)?;
    Ok(HostFwd { udp, host_addr, host_port, guest_addr, guest_port })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_user_has_no_forwards() {
        assert_eq!(UserNet::parse("user"), Ok(UserNet::default()));
    }

    #[test]
    fn hostfwd_defaults_to_tcp_on_localhost_and_the_dhcp_address() {
        let net = UserNet::parse("user,hostfwd=tcp::2222-:22,hostfwd=::8080-:80").unwrap();
        let fwd = |host_port, guest_port| HostFwd { udp: false, host_addr: Ipv4Addr::LOCALHOST, host_port, guest_addr: GUEST, guest_port };
        assert_eq!(net.hostfwd, [fwd(2222, 22), fwd(8080, 80)]);
    }

    #[test]
    fn hostfwd_takes_explicit_addresses_and_udp() {
        let net = UserNet::parse("user,hostfwd=udp:0.0.0.0:5353-10.0.2.16:53").unwrap();
        let want = HostFwd {
            udp: true,
            host_addr: Ipv4Addr::UNSPECIFIED,
            host_port: 5353,
            guest_addr: Ipv4Addr::new(10, 0, 2, 16),
            guest_port: 53,
        };
        assert_eq!(net.hostfwd, [want]);
    }

    #[test]
    fn rejects_malformed_specs() {
        for spec in ["tap", "user,smb=/tmp", "user,hostfwd=tcp:2222:22", "user,hostfwd=sctp::1-:2", "user,hostfwd=tcp::99999-:22"] {
            assert_eq!((spec, UserNet::parse(spec).is_err()), (spec, true));
        }
    }
}
