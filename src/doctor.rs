//! `gowl doctor [DEV]` — network facts, no fixes applied (docs/NETWORK.md).

use std::net::Ipv4Addr;

use crate::devices::Device;
use crate::error::Error;
use crate::net::{self, Iface, Ports, ScanTarget};

fn rp_filter(name: &str) -> String {
    std::fs::read_to_string(format!("/proc/sys/net/ipv4/conf/{name}/rp_filter"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "?".into())
}

/// Read-only `firewall-cmd --get-zone-of-interface` (spawned once, doctor only).
fn firewall_zone(name: &str) -> Option<String> {
    let out = std::process::Command::new("firewall-cmd")
        .arg(format!("--get-zone-of-interface={name}"))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let z = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !z.is_empty()).then_some(z)
}

fn subnet(i: &Iface) -> String {
    format!(
        "{}/{}",
        Ipv4Addr::from(u32::from(i.addr) & i.mask()),
        i.prefix
    )
}

/// Print the report. `dev` is `None` when no device could be resolved.
pub fn run(dev: Option<&Device>, lan_if: Option<&str>, ports: Ports) -> Result<(), Error> {
    let ifaces = net::interfaces().map_err(|e| Error::Io("getifaddrs", e))?;
    let tunnels = net::tunnel_names(&ifaces);
    println!(
        "tunnels:   {}",
        if tunnels.is_empty() {
            "none detected".to_string()
        } else {
            tunnels.join(", ")
        }
    );

    let Some(dev) = dev else {
        let d = net::discovery_ifaces(&ifaces, lan_if);
        println!("device:    none resolved (pass DEV, set device= or run `gowl scan`)");
        for i in &d {
            println!("candidate: {i} ({})", subnet(i));
        }
        if d.is_empty() {
            println!("candidate: none — no UP non-VPN interface with an IPv4 subnet");
        }
        return Ok(());
    };
    println!(
        "device:    {}{}",
        dev.label(),
        dev.mac
            .as_deref()
            .map_or(String::new(), |m| format!(" {m}"))
    );

    let sel = match net::select_for(&ifaces, dev.ip, lan_if) {
        Ok(s) => s,
        Err(e) => {
            println!("lan:       NONE — {e}");
            return Err(e);
        }
    };
    let lan = &sel.iface;
    println!("lan:       {lan} — {}", sel.why);
    let others: Vec<String> = net::discovery_ifaces(&ifaces, None)
        .into_iter()
        .filter(|i| i.name != lan.name)
        .map(|i| i.to_string())
        .collect();
    if !others.is_empty() {
        println!("others:    {}", others.join(", "));
    }

    // Route hijack check: where would an unbound socket go?
    match net::route_source(dev.ip, ports.control) {
        Ok(src) if src == lan.addr => println!(
            "route:     kernel routes {} via {} (no VPN hijack)",
            dev.ip, lan.name
        ),
        Ok(src) => {
            let via = ifaces
                .iter()
                .find(|i| i.addr == src)
                .map_or_else(|| src.to_string(), |i| i.name.clone());
            println!(
                "route:     your VPN routes the light's IP into {via}; gowl bypasses this by binding to {}",
                lan.name
            );
        }
        Err(e) => println!(
            "route:     no route to {} ({e}); gowl still sends via {}",
            dev.ip, lan.name
        ),
    }

    // Bound devStatus, 500 ms.
    let reply = net::status(lan, ports, dev.ip, ports.control, 500);
    match &reply {
        Ok(Some(r)) => {
            println!(
                "reply:     {} answered in {} ms from {} — LAN path and firewall OK",
                r.query, r.elapsed_ms, r.from
            );
            return Ok(());
        }
        Ok(None) => println!("reply:     no status reply within 500 ms"),
        Err(Error::Io(_, e)) if net::is_blocked(e) => {
            println!("reply:     sending was refused ({e})");
            println!("cause:     outbound blocked — a VPN kill switch drops LAN traffic.");
            println!(
                "           Enable \"allow LAN connections\" / \"local network sharing\" in the VPN app;"
            );
            println!("           gowl will not bypass a kill switch.");
            return Ok(());
        }
        Err(e) => {
            println!("reply:     status query failed: {e}");
            return Ok(());
        }
    }

    // No status reply: does a unicast scan get one? (Replies from the port we sent
    // to pass conntrack even with inbound 4002 closed.)
    let probe = net::scan(
        &[ScanTarget {
            iface: lan.clone(),
            discover: false,
            unicast: vec![dev.ip],
        }],
        ports,
        500,
        false,
    );
    match probe {
        Ok(f) if !f.is_empty() => println!(
            "scan:      unicast scan answered from {} — device is alive; status replies are being dropped",
            f[0].from
        ),
        Ok(_) => println!("scan:      unicast scan: no reply either"),
        Err(e) => println!("scan:      unicast scan failed: {e}"),
    }
    println!(
        "rp_filter: {}={} all={} (0/2 fine; 1 drops replies only if a VPN hijacks the LAN route)",
        lan.name,
        rp_filter(&lan.name),
        rp_filter("all")
    );
    match firewall_zone(&lan.name) {
        Some(z) => println!("firewall:  {} is in zone {z:?}", lan.name),
        None => println!("firewall:  zone unknown (firewall-cmd unavailable)"),
    }
    println!("causes:    (a) light offline, wrong IP, or LAN Control off in the Govee Home app");
    println!(
        "           (b) inbound UDP {} blocked by the firewall (see docs/NETWORK.md for a LAN-scoped rule)",
        ports.reply
    );
    println!("note:      sync and on/off/color are send-only and work without any reply");
    Ok(())
}
