use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

mod adapter;
mod cache;
mod config;
mod connection;
mod discovery;
mod filter;
mod local_api;
mod logger;
mod tcp_client;
mod transport;
mod udp_client;

fn main() {
    if let Err(e) = run() {
        logger::fatal(e);
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config_path = config::get_config_path();
    let cfg = config::load_config(&config_path)?;
    let discover_wait = Duration::from_secs(cfg.connecting.discover_wait_secs);
    let multicast_nodes = match discovery::discover_nodes(
        &cfg.connecting.multicast_addr,
        discover_wait,
        &cfg.connecting.multicast_outbound_ip,
        &cfg.connecting.protocol,
    ) {
        Ok(v) => v,
        Err(e) => {
            logger::warn(format!("discovery failed: {}", e));
            Vec::new()
        }
    };
    let static_nodes = match discovery::static_nodes_from_env() {
        Ok(nodes) => nodes,
        Err(e) => {
            logger::warn(format!("static nodes ignored: {}", e));
            Vec::new()
        }
    };

    logger::info(format!(
        "config_path={} protocol={} discovery_multicast_addr={} static_node_count={} uds_path={}",
        config_path.display(),
        cfg.connecting.protocol,
        cfg.connecting.multicast_addr,
        static_nodes.len(),
        local_api::uds_path_from_config_or_env(cfg.services.uds_path.as_deref())
    ));

    if multicast_nodes.is_empty() && static_nodes.is_empty() {
        logger::warn("no discovery result and no static nodes; starting with empty cache");
    } else if multicast_nodes.is_empty() {
        logger::warn(format!(
            "multicast discovery returned no nodes; using {} static node(s)",
            static_nodes.len()
        ));
    }

    let discovered = discovery::merge_discovered_nodes(multicast_nodes, static_nodes);

    let cache_map = cache::build_cache(discovered.clone());
    let shared = Arc::new(Mutex::new(cache_map));
    let transport_protocol = cfg.connecting.protocol.trim().to_ascii_lowercase();
    let state = local_api::LocalApiState::new(
        shared,
        cfg.services.cache_ttl_ms,
        transport_protocol,
        cfg.connecting.udp_mtu,
        cfg.connecting.multicast_addr.clone(),
        discover_wait,
        Duration::from_secs(cfg.connecting.heartbeat_interval),
        Duration::from_secs(cfg.connecting.connection_idle_timeout),
        cfg.connecting.max_connections,
        cfg.connecting.multicast_outbound_ip.clone(),
    );
    if !discovered.is_empty() {
        let connect_state = state.clone();
        let connect_nodes = discovered.clone();
        if cfg.connecting.protocol.eq_ignore_ascii_case("tcp") {
            std::thread::spawn(move || connect_state.establish_tcp_connections(&connect_nodes));
        } else {
            std::thread::spawn(move || connect_state.establish_udp_connections(&connect_nodes));
        }
    }
    spawn_announce_listener(
        state.clone(),
        cfg.connecting.multicast_addr.clone(),
        cfg.connecting.multicast_outbound_ip.clone(),
        cfg.connecting.protocol.clone(),
    );
    local_api::serve(state, cfg.services.uds_path.as_deref())
}

fn spawn_announce_listener(
    state: local_api::LocalApiState,
    multicast_addr: String,
    multicast_outbound_ip: Vec<String>,
    protocol: String,
) {
    std::thread::spawn(move || {
        let socket = match discovery::listen_for_announces(&multicast_addr, &multicast_outbound_ip)
        {
            Ok(socket) => socket,
            Err(e) => {
                logger::warn(format!("announce listener disabled: {}", e));
                return;
            }
        };

        loop {
            match discovery::recv_announce_for_protocol(&socket, &protocol) {
                Ok(Some(node)) => {
                    logger::info(format!(
                        "multicast_announce_received hostname={} addr={}",
                        node.hostname, node.addr
                    ));
                    state.add_discovered_nodes(std::slice::from_ref(&node));
                    if protocol.eq_ignore_ascii_case("tcp") {
                        state.establish_tcp_connections(std::slice::from_ref(&node));
                    }
                    if protocol.eq_ignore_ascii_case("udp") {
                        state.establish_udp_connections(std::slice::from_ref(&node));
                    }
                }
                Ok(None) => {}
                Err(e) => logger::warn(format!("announce listener error: {}", e)),
            }
        }
    });
}
