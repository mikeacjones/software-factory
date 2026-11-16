//! Who may connect, and under what name the page may be asked for.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// A network of IPv4 addresses, written `192.168.101.0/24`. One address
/// alone is a network of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Net {
    base: u32,
    mask: u32,
}

impl Net {
    pub fn parse(text: &str) -> Option<Net> {
        let (address, bits) = match text.split_once('/') {
            Some((address, bits)) => (address, bits.parse::<u32>().ok()?),
            None => (text, 32),
        };
        if bits > 32 {
            return None;
        }
        let address: Ipv4Addr = address.trim().parse().ok()?;
        let mask = if bits == 0 {
            0
        } else {
            u32::MAX << (32 - bits)
        };
        Some(Net {
            base: u32::from(address) & mask,
            mask,
        })
    }

    /// Is the address in the network? An IPv6 address is only when it is
    /// an IPv4 one written the long way.
    pub fn holds(&self, address: IpAddr) -> bool {
        let address = match address {
            IpAddr::V4(address) => address,
            IpAddr::V6(address) => match address.to_ipv4_mapped() {
                Some(address) => address,
                None => return false,
            },
        };
        u32::from(address) & self.mask == self.base
    }
}

/// May a connection from this address be answered?
pub fn may_connect(allowed: &[Net], peer: IpAddr) -> bool {
    allowed.iter().any(|net| net.holds(peer))
}

/// Was the page asked for under a name that is ours? A browser sends the
/// name it was given. An address is always ours: nobody can make another
/// site's name mean an address. A host name is ours only when it was
/// listed, which is what stops a page on some other site, opened in a
/// browser on the allowed network, from pointing its own name at this
/// machine and reading the answers.
pub fn host_is_ours(host: &str, names: &[String]) -> bool {
    let host = host.trim();
    if host.parse::<SocketAddr>().is_ok() || host.parse::<IpAddr>().is_ok() {
        return true;
    }
    if let Some(inner) = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return inner.parse::<IpAddr>().is_ok();
    }
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    };
    names.iter().any(|ours| ours.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn a_network_holds_its_own_addresses_and_no_others() {
        let net = Net::parse("192.168.101.0/24").unwrap();
        assert!(net.holds(ip("192.168.101.1")));
        assert!(net.holds(ip("192.168.101.255")));
        assert!(net.holds(ip("::ffff:192.168.101.7")));
        assert!(!net.holds(ip("192.168.102.1")));
        assert!(!net.holds(ip("127.0.0.1")));
        assert!(!net.holds(ip("::1")));
        assert!(!net.holds(ip("fe80::1")));
        // An address written into the network's own bits is still the network.
        assert_eq!(Net::parse("192.168.101.77/24"), Some(net));
        let one = Net::parse("10.0.0.5").unwrap();
        assert!(one.holds(ip("10.0.0.5")));
        assert!(!one.holds(ip("10.0.0.6")));
        assert!(Net::parse("0.0.0.0/0").unwrap().holds(ip("8.8.8.8")));
    }

    #[test]
    fn a_network_that_is_not_one_is_refused() {
        for text in [
            "",
            "192.168.101.0/33",
            "192.168.101/24",
            "host/24",
            "::1/128",
        ] {
            assert_eq!(Net::parse(text), None, "{text}");
        }
    }

    #[test]
    fn nobody_connects_when_no_network_is_allowed() {
        assert!(!may_connect(&[], ip("127.0.0.1")));
    }

    #[test]
    fn an_address_is_always_ours_and_a_name_only_when_listed() {
        let names = vec!["factory.local".to_string()];
        assert!(host_is_ours("192.168.101.65:8717", &names));
        assert!(host_is_ours("192.168.101.65", &names));
        assert!(host_is_ours("[::1]:8717", &names));
        assert!(host_is_ours("Factory.Local:8717", &names));
        assert!(host_is_ours("factory.local", &names));
        assert!(!host_is_ours("evil.example:8717", &names));
        assert!(!host_is_ours("evil.example", &[]));
        assert!(!host_is_ours("", &names));
    }
}
