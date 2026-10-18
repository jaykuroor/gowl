//! `net::status` and `net::scan` end to end on loopback, with the "device" played
//! by sockets on 127.0.0.2 and a rogue on 127.0.0.3: status accepts replies only
//! from the queried IP; scan parses, dedupes and never trusts a payload address
//! outside the interface's subnet. Nothing leaves loopback (unicast probes only).

mod common;

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use gowl::net::{self, Iface, Ports, ScanTarget};

fn lo() -> Iface {
    let ifaces = net::interfaces().unwrap();
    net::select_for(&ifaces, Ipv4Addr::LOCALHOST, None)
        .unwrap()
        .iface
}

fn ports(base: u16) -> Ports {
    Ports {
        scan: base + 1,
        reply: base + 2,
        control: base + 3,
    }
}

const DEV: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
const ROGUE: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 3);

fn status_json(brightness: u32) -> String {
    format!(
        r#"{{"msg":{{"cmd":"devStatus","data":{{"onOff":1,"brightness":{brightness},"color":{{"r":1,"g":2,"b":3}},"colorTemInKelvin":0}}}}}}"#
    )
}

#[test]
fn status_ignores_replies_from_other_addresses() {
    let p = ports(16600);
    let dev = UdpSocket::bind((DEV, p.control)).unwrap();
    dev.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let rogue = UdpSocket::bind((ROGUE, 0)).unwrap();
    let lo = lo();
    let q = std::thread::spawn(move || net::status(&lo, p, DEV, p.control, 1000));
    let mut buf = [0u8; 256];
    let (n, from) = dev.recv_from(&mut buf).unwrap();
    assert!(
        std::str::from_utf8(&buf[..n])
            .unwrap()
            .contains("devStatus")
    );
    let back = SocketAddrV4::new(Ipv4Addr::LOCALHOST, p.reply);
    // The rogue answers first (a forged status), then the real device.
    rogue.send_to(status_json(99).as_bytes(), back).unwrap();
    std::thread::sleep(Duration::from_millis(30));
    dev.send_to(status_json(42).as_bytes(), back).unwrap();
    let r = q.join().unwrap().unwrap().expect("a reply");
    assert_eq!(*r.from.ip(), DEV, "from {from}");
    assert_eq!(
        r.status.brightness,
        Some(42),
        "the rogue's reply was ignored"
    );
    assert_eq!(r.query, "devStatus");

    // Only the rogue answers: no reply at all (after the status fallback).
    let p = ports(16610);
    let dev = UdpSocket::bind((DEV, p.control)).unwrap();
    dev.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let lo = self::lo();
    let q = std::thread::spawn(move || net::status(&lo, p, DEV, p.control, 400));
    dev.recv_from(&mut buf).unwrap();
    rogue
        .send_to(
            status_json(99).as_bytes(),
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, p.reply),
        )
        .unwrap();
    let (n, _) = dev.recv_from(&mut buf).unwrap();
    assert!(
        std::str::from_utf8(&buf[..n])
            .unwrap()
            .contains(r#""cmd":"status""#),
        "falls back to the legacy status query"
    );
    assert!(q.join().unwrap().unwrap().is_none());
}

#[test]
fn scan_end_to_end_dedupes_and_distrusts_payload_ip() {
    let p = ports(16620);
    let dev = UdpSocket::bind((DEV, p.scan)).unwrap();
    dev.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let target = ScanTarget {
        iface: lo(),
        discover: false,
        unicast: vec![DEV],
    };
    let q = std::thread::spawn(move || net::scan(&[target], p, 300, false));
    let mut buf = [0u8; 256];
    let (n, _) = dev.recv_from(&mut buf).unwrap();
    assert!(
        std::str::from_utf8(&buf[..n])
            .unwrap()
            .contains(r#""cmd":"scan""#)
    );
    let back = SocketAddrV4::new(Ipv4Addr::LOCALHOST, p.reply);
    // Payload claims an address off this interface's subnet: the source IP wins.
    let reply =
        br#"{"msg":{"cmd":"scan","data":{"ip":"10.9.9.9","device":"aa:bb:cc","sku":"H6609"}}}"#;
    dev.send_to(reply, back).unwrap();
    dev.send_to(reply, back).unwrap(); // duplicate
    dev.send_to(b"not json", back).unwrap();
    let found = q.join().unwrap().unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].mac, "AA:BB:CC");
    assert_eq!(found[0].ip, DEV);
    assert_eq!(found[0].sku, "H6609");
}

/// Per-device `port =` (legacy `Device_Port`): a one-shot goes to the override port,
/// not to 4003, and the override wins over the loopback `GOWL_PORT_BASE` hook.
#[test]
fn device_port_override_is_used_by_one_shots() {
    let dev = UdpSocket::bind((Ipv4Addr::new(127, 0, 0, 4), 0)).unwrap();
    dev.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let port = dev.local_addr().unwrap().port();
    let tmp = std::env::temp_dir().join(format!("gowl-port-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let cfg = tmp.join("config");
    std::fs::write(
        &cfg,
        format!("[device lamp]\nip = 127.0.0.4\nport = {port}\n"),
    )
    .unwrap();
    let out = common::fake_desktop(
        &mut std::process::Command::new(env!("CARGO_BIN_EXE_gowl")),
        &tmp,
    )
    .args(["color", "ff0000", "lamp"])
    .env("GOWL_CONFIG", &cfg)
    .env("GOWL_PORT_BASE", "17000")
    .env("XDG_STATE_HOME", &tmp)
    .env("XDG_CACHE_HOME", &tmp)
    .env("XDG_RUNTIME_DIR", &tmp)
    .env("HOME", &tmp)
    .env_remove("GOWL_DEVICE")
    .env_remove("GOWL_LOG")
    .output()
    .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut buf = [0u8; 512];
    let n = dev.recv(&mut buf).unwrap();
    let s = std::str::from_utf8(&buf[..n]).unwrap();
    assert!(s.contains(r#""cmd":"colorwc""#), "{s}");
    let _ = std::fs::remove_dir_all(&tmp);
}
