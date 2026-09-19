//! Received-TTL regression. Requires Linux user/network namespaces, `unshare`,
//! `ip` (iproute2), and `timeout` (coreutils). Run explicitly with:
//! `cargo test --test integration_test_mdns_ttl -- --nocapture`
//!
//! All packets stay inside a fresh network namespace: older mdns-sd versions
//! panic on these TTLs, so they must never reach other mDNS listeners.
#![cfg(target_os = "linux")]

use mdns_sd::{DaemonStatus, ServiceDaemon, ServiceEvent};
use std::{
    net::{Ipv4Addr, UdpSocket},
    process::Command,
    time::{Duration, Instant},
};

const PARENT_NETNS: &str = "LLAMESH_TTL_TEST_PARENT_NETNS";
const SERVICE: &str = "_ttl-test._tcp.local.";
const ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
const WAIT: Duration = Duration::from_secs(3);

// Also request shutdown when an assertion unwinds. The outer process deadline
// bounds even a wedged worker, and exiting the child destroys its namespace.
struct DaemonGuard(ServiceDaemon);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Ok(done) = self.0.shutdown() {
            let _ = done.recv_timeout(WAIT);
        }
    }
}

#[test]
fn received_large_ttl_keeps_discovery_alive() {
    let namespace = std::fs::read_link("/proc/self/ns/net").unwrap();
    let Ok(parent_namespace) = std::env::var(PARENT_NETNS) else {
        let output = Command::new("timeout")
            .args([
                "--signal=KILL",
                "20s",
                "unshare",
                "--user",
                "--map-root-user",
                "--net",
            ])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "received_large_ttl_keeps_discovery_alive",
                "--nocapture",
            ])
            .env(PARENT_NETNS, namespace)
            .output()
            .expect("install coreutils (timeout) and util-linux (unshare) to run this regression");
        assert!(
            output.status.success(),
            "isolated mDNS regression failed ({}); requires unshare, iproute2 and enabled user/network namespaces:\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };

    // Fail closed before configuring networking or creating any socket.
    assert_ne!(namespace.to_str().unwrap(), parent_namespace);
    for args in [
        vec!["link", "set", "lo", "up"],
        vec!["link", "add", "ttl-test0", "type", "dummy"],
        vec!["address", "add", "192.0.2.1/24", "dev", "ttl-test0"],
        vec!["link", "set", "ttl-test0", "up"],
        vec!["route", "add", "224.0.0.0/4", "dev", "ttl-test0"],
    ] {
        let status = Command::new("ip")
            .args(&args)
            .status()
            .expect("install iproute2 (ip) to configure the isolated test network");
        assert!(
            status.success(),
            "isolated network setup failed: ip {args:?}"
        );
    }

    let daemon = DaemonGuard(ServiceDaemon::new().unwrap());
    let events = daemon.0.browse(SERVICE).unwrap();
    assert!(matches!(
        events.recv_timeout(WAIT).expect("browse must start"),
        ServiceEvent::SearchStarted(_)
    ));
    let sender = UdpSocket::bind((ADDRESS, 0)).unwrap();
    sender.set_multicast_loop_v4(true).unwrap();

    // A control establishes that the isolated interface and packet encoder work.
    // A subsequent ordinary response proves discovery still works after the
    // oversized TTL, rather than merely checking that enqueueing a command works.
    // RFC 2181 limits TTLs to 2^31 - 1; exercise that valid limit as well as
    // the high-bit-set wire value that implementations may clamp.
    for (instance, ttl) in [
        ("before", 120),
        ("wrapped-expiry", 1 << 29), // Old release arithmetic expires immediately.
        ("large", i32::MAX as u32),
        ("high-bit", u32::MAX),
        ("after", 120),
    ] {
        sender
            .send_to(
                &response(instance, ttl),
                (Ipv4Addr::new(224, 0, 0, 251), 5353),
            )
            .unwrap();
        let deadline = Instant::now() + WAIT;
        loop {
            let event = events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!("service {instance} (TTL {ttl}) was not resolved: {error}")
                });
            if let ServiceEvent::ServiceResolved(info) = event {
                if info.get_fullname() == format!("{instance}.{SERVICE}") {
                    assert_eq!(info.get_port(), 8080);
                    assert!(info.get_addresses_v4().contains(&ADDRESS));
                    break;
                }
            }
        }
    }
    assert!(matches!(
        daemon.0.status().unwrap().recv_timeout(WAIT).unwrap(),
        DaemonStatus::Running
    ));
    assert!(matches!(
        daemon.0.shutdown().unwrap().recv_timeout(WAIT).unwrap(),
        DaemonStatus::Shutdown
    ));
}

fn name(value: &str) -> Vec<u8> {
    let mut wire = Vec::new();
    for label in value.trim_end_matches('.').split('.') {
        wire.push(label.len().try_into().unwrap());
        wire.extend_from_slice(label.as_bytes());
    }
    wire.push(0);
    wire
}

fn response(instance: &str, ttl: u32) -> Vec<u8> {
    let fullname = format!("{instance}.{SERVICE}");
    let host = format!("{instance}.local.");
    // Response + authoritative answer; four answers, no questions/additionals.
    let mut wire = vec![0, 0, 0x84, 0, 0, 0, 0, 4, 0, 0, 0, 0];
    let mut srv = vec![0, 0, 0, 0]; // Priority and weight.
    srv.extend_from_slice(&8080u16.to_be_bytes());
    srv.extend(name(&host));
    for (owner, kind, data) in [
        (SERVICE, 12u16, name(&fullname)),             // PTR
        (fullname.as_str(), 33, srv),                  // SRV
        (fullname.as_str(), 16, vec![0]),              // Empty TXT
        (host.as_str(), 1, ADDRESS.octets().to_vec()), // A
    ] {
        wire.extend(name(owner));
        wire.extend(kind.to_be_bytes());
        wire.extend(1u16.to_be_bytes()); // IN class.
        wire.extend(ttl.to_be_bytes());
        wire.extend(u16::try_from(data.len()).unwrap().to_be_bytes());
        wire.extend(data);
    }
    wire
}
