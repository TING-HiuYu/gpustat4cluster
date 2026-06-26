use std::{
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use common::{DiscoveryAnnounce, DiscoveryQuery, PROTOCOL_VERSION};
use serde_json::Value;
use socket2::{Domain, Protocol, Socket, Type};

use crate::logger;

pub const STATIC_NODES_ENV: &str = "GPUSTAT4CLUSTER_STATIC_NODES";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredNode {
    pub hostname: String,
    pub addr: SocketAddr,
    pub ts_ms: i64,
}

pub fn static_nodes_from_env() -> Result<Vec<DiscoveredNode>, String> {
    match std::env::var(STATIC_NODES_ENV) {
        Ok(raw) => parse_static_nodes(&raw),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(e) => Err(format!("read {} failed: {}", STATIC_NODES_ENV, e)),
    }
}

pub fn parse_static_nodes(raw: &str) -> Result<Vec<DiscoveredNode>, String> {
    let mut nodes = Vec::new();
    let mut seen = HashSet::new();
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match resolve_addr(item) {
            Ok(addr) if seen.insert(addr) => {
                nodes.push(DiscoveredNode {
                    hostname: addr.ip().to_string(),
                    addr,
                    ts_ms: now_ms(),
                });
            }
            Ok(_) => {}
            Err(e) => logger::warn(format!("static node '{}' ignored: {}", item, e)),
        }
    }
    Ok(nodes)
}

pub fn merge_discovered_nodes(
    discovered: Vec<DiscoveredNode>,
    static_nodes: Vec<DiscoveredNode>,
) -> Vec<DiscoveredNode> {
    let mut map = HashMap::new();
    for node in discovered.into_iter().chain(static_nodes) {
        map.insert(node.addr.to_string(), node);
    }
    let mut nodes: Vec<_> = map.into_values().collect();
    nodes.sort_by(|a, b| a.addr.to_string().cmp(&b.addr.to_string()));
    nodes
}

pub fn discover_nodes(
    multicast_addr: &str,
    wait: Duration,
    outbound_ips: &[String],
    protocol: &str,
) -> Result<Vec<DiscoveredNode>, String> {
    let target = resolve_addr(multicast_addr)?;
    let outbound = parse_multicast_outbound_ips(outbound_ips)?;
    let interfaces: Vec<Option<Ipv4Addr>> = if outbound.is_empty() {
        vec![None]
    } else {
        outbound.into_iter().map(Some).collect()
    };

    let query = serde_json::to_vec(&DiscoveryQuery {
        version: PROTOCOL_VERSION,
    })
    .map_err(|e| format!("encode discovery query failed: {}", e))?;

    let mut sockets = Vec::new();
    let mut first_send_error = None;
    for interface in interfaces {
        let socket = create_udp_socket(SocketAddr::from(([0, 0, 0, 0], 0)), interface)
            .map_err(|e| format!("udp bind failed: {}", e))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|e| format!("set read timeout failed: {}", e))?;
        match socket.send_to(&query, target) {
            Ok(_) => sockets.push(socket),
            Err(e) => {
                let message =
                    multicast_route_hint(format!("send query to {} failed: {}", target, e), &e);
                if first_send_error.is_none() {
                    first_send_error = Some(message);
                }
            }
        }
    }

    if sockets.is_empty() {
        return Err(first_send_error
            .unwrap_or_else(|| format!("send query to {} failed on every outbound IP", target)));
    }

    let deadline = std::time::Instant::now() + wait;
    let mut buf = [0u8; 2048];
    let mut map: HashMap<String, DiscoveredNode> = HashMap::new();

    while std::time::Instant::now() < deadline {
        for socket in &sockets {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let msg = String::from_utf8_lossy(&buf[..n]);
                    if let Some(node) = parse_announce_for_protocol(&msg, from, protocol) {
                        map.insert(node.addr.to_string(), node);
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(format!("recv announce failed: {}", e)),
            }
        }
    }

    let mut items: Vec<_> = map.into_values().collect();
    items.sort_by(|a, b| a.addr.to_string().cmp(&b.addr.to_string()));
    Ok(items)
}

pub fn listen_for_announces(
    multicast_addr: &str,
    outbound_ips: &[String],
) -> Result<UdpSocket, String> {
    let multicast = resolve_addr(multicast_addr)?;
    let socket = create_udp_socket(SocketAddr::from(([0, 0, 0, 0], multicast.port())), None)
        .map_err(|e| format!("announce listener bind failed: {}", e))?;

    if let IpAddr::V4(group) = multicast.ip() {
        let outbound = parse_multicast_outbound_ips(outbound_ips)?;
        let interfaces: Vec<Ipv4Addr> = if outbound.is_empty() {
            vec![Ipv4Addr::UNSPECIFIED]
        } else {
            outbound
        };

        let mut joined_any = false;
        for interface in interfaces {
            match socket.join_multicast_v4(&group, &interface) {
                Ok(_) => {
                    joined_any = true;
                    logger::info(format!(
                        "multicast_announce_listen_join addr={} outbound_ip={}",
                        multicast, interface
                    ));
                }
                Err(e) => logger::warn(multicast_route_hint(
                    format!("join announce multicast group failed: {}", e),
                    &e,
                )),
            }
        }
        if !joined_any {
            return Err("announce listener failed to join every multicast interface".to_string());
        }
    }

    logger::info(format!("multicast_announce_listening addr={}", multicast));
    Ok(socket)
}

pub fn recv_announce_for_protocol(
    socket: &UdpSocket,
    protocol: &str,
) -> Result<Option<DiscoveredNode>, String> {
    let mut buf = [0u8; 2048];
    match socket.recv_from(&mut buf) {
        Ok((n, src)) => {
            let msg = String::from_utf8_lossy(&buf[..n]);
            Ok(parse_announce_for_protocol(&msg, src, protocol))
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            Ok(None)
        }
        Err(e) => Err(format!("recv announce failed: {}", e)),
    }
}

fn parse_multicast_outbound_ips(raw: &[String]) -> Result<Vec<Ipv4Addr>, String> {
    raw.iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.parse::<Ipv4Addr>()
                .map_err(|e| format!("invalid multicast_outbound_ip '{}': {}", item, e))
        })
        .collect()
}

fn create_udp_socket(
    bind_addr: SocketAddr,
    multicast_interface: Option<Ipv4Addr>,
) -> std::io::Result<UdpSocket> {
    let domain = if bind_addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&bind_addr.into())?;
    if let Some(interface) = multicast_interface {
        socket.set_multicast_if_v4(&interface)?;
    }
    Ok(socket.into())
}

fn multicast_route_hint(message: String, error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(101) | Some(19) => format!(
            "{}; multicast route/interface is unavailable, configure [connecting].multicast_outbound_ip with one or more local IPv4 addresses",
            message
        ),
        _ => message,
    }
}

#[cfg(test)]
pub fn parse_announce(msg: &str, src: SocketAddr) -> Option<DiscoveredNode> {
    parse_announce_for_protocol(msg, src, "udp")
}

pub fn parse_announce_for_protocol(
    msg: &str,
    src: SocketAddr,
    protocol: &str,
) -> Option<DiscoveredNode> {
    let ann: DiscoveryAnnounce = match serde_json::from_str(msg) {
        Ok(ann) => ann,
        Err(_) => parse_legacy_announce(msg)?,
    };
    if ann.version != PROTOCOL_VERSION {
        return None;
    }

    let port = match protocol.trim().to_ascii_lowercase().as_str() {
        "tcp" => ann.tcp_port.unwrap_or(ann.port),
        "udp" => ann.udp_port.unwrap_or(ann.port),
        _ => ann.udp_port.unwrap_or(ann.port),
    };

    Some(DiscoveredNode {
        hostname: ann.hostname,
        addr: SocketAddr::new(src.ip(), port),
        ts_ms: now_ms(),
    })
}

fn parse_legacy_announce(msg: &str) -> Option<DiscoveryAnnounce> {
    let raw: Value = serde_json::from_str(msg).ok()?;
    let hostname = raw.get("hostname")?.as_str()?.to_string();
    let ip = raw.get("ip")?.as_str()?.to_string();
    let port = raw.get("port")?.as_u64()?.try_into().ok()?;

    Some(DiscoveryAnnounce {
        version: raw
            .get("version")
            .and_then(Value::as_u64)
            .and_then(|v| v.try_into().ok())
            .unwrap_or(PROTOCOL_VERSION),
        hostname,
        ip,
        port,
        tcp_port: raw
            .get("tcp_port")
            .and_then(Value::as_u64)
            .and_then(|v| v.try_into().ok()),
        udp_port: raw
            .get("udp_port")
            .and_then(Value::as_u64)
            .and_then(|v| v.try_into().ok()),
        kcp_port: None,
        ttl: raw
            .get("ttl")
            .and_then(Value::as_u64)
            .and_then(|v| v.try_into().ok()),
        load: raw
            .get("load")
            .and_then(Value::as_u64)
            .and_then(|v| v.try_into().ok()),
        degraded: raw.get("degraded").and_then(Value::as_bool),
    })
}

fn resolve_addr(raw: &str) -> Result<SocketAddr, String> {
    raw.to_socket_addrs()
        .map_err(|e| format!("resolve address '{}' failed: {}", raw, e))?
        .next()
        .ok_or_else(|| format!("no resolved address for '{}'", raw))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_announce_without_version_is_accepted() {
        let src: SocketAddr = "192.0.2.99:40000".parse().unwrap();
        let node = parse_announce(
            r#"{"hostname":"node-a","ip":"10.1.2.3","port":30001,"ts":1}"#,
            src,
        )
        .expect("legacy announce should parse");

        assert_eq!(node.hostname, "node-a");
        assert_eq!(node.addr.to_string(), "192.0.2.99:30001");
    }

    #[test]
    fn announce_version_mismatch_is_rejected() {
        let src: SocketAddr = "192.0.2.99:40000".parse().unwrap();
        let node = parse_announce(
            r#"{"version":2,"hostname":"node-a","ip":"10.1.2.3","port":30001}"#,
            src,
        );
        assert!(node.is_none());
    }

    #[test]
    fn static_node_parser_accepts_comma_list() {
        let nodes = parse_static_nodes("127.0.0.1:30000, 127.0.0.1:30001").unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].addr.to_string(), "127.0.0.1:30000");
        assert_eq!(nodes[1].addr.to_string(), "127.0.0.1:30001");
    }

    #[test]
    fn static_node_parser_skips_invalid_entries() {
        let nodes = parse_static_nodes("127.0.0.1:30000, 127.0.0.1, bad addr").unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].addr.to_string(), "127.0.0.1:30000");
    }

    #[test]
    fn static_node_parser_trims_and_deduplicates() {
        let nodes =
            parse_static_nodes(" 127.0.0.1:30000 ,127.0.0.1:30000,127.0.0.1:30001 ").unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].addr.to_string(), "127.0.0.1:30000");
        assert_eq!(nodes[1].addr.to_string(), "127.0.0.1:30001");
    }

    #[test]
    fn static_node_parser_supports_multiple_server_ports() {
        let nodes = parse_static_nodes("127.0.0.1:39400,127.0.0.1:39401").unwrap();
        let addrs: Vec<_> = nodes.iter().map(|node| node.addr.to_string()).collect();
        assert_eq!(addrs, vec!["127.0.0.1:39400", "127.0.0.1:39401"]);
    }

    #[test]
    fn merge_discovered_nodes_deduplicates_by_addr() {
        let discovered = parse_static_nodes("127.0.0.1:30000").unwrap();
        let static_nodes = parse_static_nodes("127.0.0.1:30000,127.0.0.1:30001").unwrap();
        let merged = merge_discovered_nodes(discovered, static_nodes);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn no_discovery_and_no_static_nodes_is_valid_empty_set() {
        let merged = merge_discovered_nodes(Vec::new(), parse_static_nodes("").unwrap());
        assert!(merged.is_empty());
    }
}
