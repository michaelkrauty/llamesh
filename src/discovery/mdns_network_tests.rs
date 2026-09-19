//! Real wrapper-to-wrapper discovery in a disposable, dummy-only Linux network.
//! Requires `unshare`, `ip`, `timeout`, and enabled user/network namespaces.

use super::*;
use std::{
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    process::Command,
    time::{Duration, Instant},
};

const PARENT_NETNS: &str = "LLAMESH_MDNS_TEST_PARENT_NETNS";
const SERVICE: &str = "_mesh-test._tcp.local.";
const IPV4: &str = "192.0.2.1";
const IPV6: &str = "fd00:168::1";

#[test]
fn wrappers_discover_ipv4_and_remove_shutdown_peer() {
    isolated_discovery(false, false);
}

#[test]
fn wrappers_discover_ipv6_and_remove_dropped_peer() {
    isolated_discovery(true, false);
}

#[test]
fn wildcard_ipv4_advertises_connectable_ipv4() {
    isolated_discovery(false, true);
}

#[test]
fn wildcard_ipv6_advertises_connectable_ipv6() {
    isolated_discovery(true, true);
}

fn isolated_discovery(ipv6: bool, wildcard: bool) {
    let namespace = std::fs::read_link("/proc/self/ns/net").unwrap();
    let Ok(parent_namespace) = std::env::var(PARENT_NETNS) else {
        let test = thread::current().name().unwrap().to_string();
        let output = Command::new("timeout")
            .args([
                "--signal=KILL",
                "45s",
                "unshare",
                "--user",
                "--map-root-user",
                "--net",
            ])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(PARENT_NETNS, namespace)
            .output()
            .expect("install coreutils (timeout) and util-linux (unshare) for mDNS regression");
        assert!(
            output.status.success(),
            "isolated mDNS wrapper regression failed ({}); requires unshare, iproute2 and enabled user/network namespaces:\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };

    // Fail closed before opening sockets. The only interfaces are our dummy
    // network and loopback (needed for the daemon's internal command socket).
    assert_ne!(namespace.to_str().unwrap(), parent_namespace);
    ip(&["link", "set", "lo", "up"]);
    ip(&["link", "add", "mesh-test0", "type", "dummy"]);
    ip(&[
        "-6",
        "address",
        "add",
        &format!("{IPV6}/64"),
        "dev",
        "mesh-test0",
        "nodad",
    ]);
    if !ipv6 || wildcard {
        ip(&["address", "add", &format!("{IPV4}/24"), "dev", "mesh-test0"]);
    }
    ip(&["link", "set", "mesh-test0", "multicast", "on", "up"]);
    ip(&["-6", "route", "add", "ff00::/8", "dev", "mesh-test0"]);
    if !ipv6 || wildcard {
        ip(&["route", "add", "224.0.0.0/4", "dev", "mesh-test0"]);
    }

    // Wildcard listeners run on a dual-stack interface to catch advertisements
    // from the wrong family. Concrete IPv6 also works without any non-loopback
    // IPv4 address; its endpoint needs brackets and cannot use the link-local IP.
    let address: IpAddr = if ipv6 { IPV6 } else { IPV4 }.parse().unwrap();
    let bind_address = if wildcard {
        if ipv6 { "::" } else { "0.0.0.0" }.parse().unwrap()
    } else {
        address
    };
    let listener_a = TcpListener::bind((bind_address, 0)).unwrap();
    let listener_b = TcpListener::bind((bind_address, 0)).unwrap();
    let peers_a = Arc::new(RwLock::new(HashSet::new()));
    let peers_b = Arc::new(RwLock::new(HashSet::new()));
    let a = MdnsDiscovery::new(
        SERVICE,
        "node-a",
        listener_a.local_addr().unwrap(),
        "public-key-a",
        peers_a.clone(),
    )
    .unwrap();
    let b = MdnsDiscovery::new(
        SERVICE,
        "node-b",
        listener_b.local_addr().unwrap(),
        "public-key-b",
        peers_b.clone(),
    )
    .unwrap();

    wait_until(Duration::from_secs(15), || {
        peers_a.read().iter().any(|p| p.node_id == "node-b")
            && peers_b.read().iter().any(|p| p.node_id == "node-a")
    });
    assert_peer(&peers_a, "node-b", "public-key-b", address, &listener_b);
    assert_peer(&peers_b, "node-a", "public-key-a", address, &listener_a);

    if !ipv6 && !wildcard {
        // A TXT identity need not equal its instance label (e.g. after conflict
        // renaming). Update that service's endpoint, then let wrapper shutdown
        // remove it alongside the wrapper's own advertisement.
        for listener in [&listener_a, &listener_b] {
            let endpoint = listener.local_addr().unwrap();
            let service = ServiceInfo::new(
                SERVICE,
                "renamed-instance",
                "renamed-host.local.",
                address,
                endpoint.port(),
                [("node_id", "alias-node"), ("pubkey", "alias-key")].as_slice(),
            )
            .unwrap();
            b.daemon.register(service).unwrap();
            wait_until(Duration::from_secs(5), || {
                peers_a
                    .read()
                    .iter()
                    .any(|p| p.node_id == "alias-node" && p.cluster_addr == endpoint.to_string())
            });
            let peers = peers_a.read();
            assert_eq!(peers.len(), 2, "endpoint updates must replace stale peers");
            let alias = peers.iter().find(|p| p.node_id == "alias-node").unwrap();
            assert_eq!(alias.cluster_addr, endpoint.to_string());
            assert_eq!(alias.public_key.as_deref(), Some("alias-key"));
            TcpStream::connect_timeout(&endpoint, Duration::from_secs(3)).unwrap();
        }

        // The same TXT identity may have multiple fullnames after per-interface
        // conflict renaming. Withdraw the selected alias and retain the original.
        let duplicate = ServiceInfo::new(
            SERVICE,
            "duplicate-instance",
            "duplicate-host.local.",
            address,
            listener_a.local_addr().unwrap().port(),
            [("node_id", "alias-node"), ("pubkey", "alias-key")].as_slice(),
        )
        .unwrap();
        let duplicate_name = duplicate.get_fullname().to_string();
        b.daemon.register(duplicate).unwrap();
        for listener in [&listener_a, &listener_b] {
            let endpoint = listener.local_addr().unwrap().to_string();
            wait_until(Duration::from_secs(5), || {
                peers_a
                    .read()
                    .iter()
                    .any(|p| p.node_id == "alias-node" && p.cluster_addr == endpoint)
            });
            {
                let peers = peers_a.read();
                assert_eq!(peers.len(), 2);
                assert!(
                    peers
                        .iter()
                        .any(|p| p.node_id == "alias-node" && p.cluster_addr == endpoint),
                    "withdrawal must preserve the surviving service's endpoint: {peers:?}"
                );
            }
            if listener.local_addr().unwrap() == listener_a.local_addr().unwrap() {
                b.daemon
                    .unregister(&duplicate_name)
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
            }
        }
    }

    // Exercise both public lifecycle paths. A raw ServiceDaemon shutdown would
    // hide a wrapper that merely drops its handle and leaves its worker alive.
    if ipv6 {
        drop(b);
    } else {
        b.shutdown();
    }
    wait_until(Duration::from_secs(10), || peers_a.read().is_empty());
    assert!(
        peers_a.read().is_empty(),
        "departing wrapper must remove its advertised peer: {:?}",
        peers_a.read()
    );
    drop(a);
}

fn ip(args: &[&str]) {
    let output = Command::new("ip")
        .args(args)
        .output()
        .expect("install iproute2 (ip) to configure the isolated mDNS network");
    assert!(
        output.status.success(),
        "isolated network setup failed: ip {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_until(timeout: Duration, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ready() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
}

fn assert_peer(
    peers: &Arc<RwLock<HashSet<DiscoveredPeer>>>,
    node_id: &str,
    public_key: &str,
    address: IpAddr,
    listener: &TcpListener,
) {
    let peers = peers.read();
    assert_eq!(
        peers.len(),
        1,
        "wrapper must discover exactly its remote peer, excluding self: {peers:?}"
    );
    let peer = peers.iter().next().unwrap();
    assert_eq!(peer.node_id, node_id);
    assert_eq!(peer.public_key.as_deref(), Some(public_key));
    let endpoint: SocketAddr = peer
        .cluster_addr
        .parse()
        .expect("discovered endpoint must be a valid socket address");
    assert_eq!(
        endpoint,
        SocketAddr::new(address, listener.local_addr().unwrap().port())
    );
    TcpStream::connect_timeout(&endpoint, Duration::from_secs(3))
        .expect("discovered endpoint must accept TCP connections");
}
