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
    // Any address of the LAN interface counts (secondary/labelled addresses too).
    let on_lan = |a: Ipv4Addr| ifaces.iter().any(|i| i.name == lan.name && i.addr == a);
    match net::route_source(dev.ip, ports.control) {
        Ok(src) if on_lan(src) => println!(
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
    let icmp_before = net::icmp_unreach_sent();
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
    let rejected = net::rejected_since(icmp_before);
    let zone = firewall_zone(&lan.name);
    match &zone {
        Some(z) => println!("firewall:  {} is in zone {z:?}", lan.name),
        None => println!("firewall:  zone unknown (firewall-cmd unavailable)"),
    }
    println!(
        "rp_filter: {}={} all={} (0/2 fine; 1 drops replies only if a VPN hijacks the LAN route)",
        lan.name,
        rp_filter(&lan.name),
        rp_filter("all")
    );
    if rejected > 0 {
        println!(
            "cause:     the light likely answered, but this machine rejected {rejected} repl{} (the kernel sent ICMP",
            if rejected == 1 { "y" } else { "ies" }
        );
        println!(
            "           unreachable while gowl waited): inbound UDP {} is blocked by the firewall. The light",
            ports.reply
        );
        println!(
            "           replies from a different source port than gowl sent to, so conntrack can't match it."
        );
        println!(
            "suggest:   sudo firewall-cmd --zone={} --add-rich-rule='rule family=\"ipv4\" source address=\"{}\" port port=\"{}\" protocol=\"udp\" accept'",
            zone.as_deref().unwrap_or("public"),
            subnet(lan),
            ports.reply
        );
        println!(
            "           (runtime only; repeat with --permanent once it works). gowl never changes the firewall."
        );
    } else {
        println!(
            "cause:     no reply reached this machine: light offline, wrong IP, or LAN Control off in the"
        );
        println!("           Govee Home app (or a firewall that drops silently)");
    }
    println!("note:      sync and on/off/color are send-only and work without any reply");
    Ok(())
}
