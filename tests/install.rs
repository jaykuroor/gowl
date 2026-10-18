//! Phase 7: `make install` / `make uninstall` (packaging/install.sh), `gowl keys`
//! and `gowl setup`, all inside a throwaway HOME. The real ~/.local, ~/.config,
//! systemd user manager and desktop are never touched: `systemctl` and `niri` are
//! fakes first on PATH (the fake niri hands `validate -c <temp file>` to the real
//! niri when it is installed, which only reads that file and its includes under the
//! temp HOME), notify-send is the recording fake of tests/common, there is no
//! session bus, and the only "light" is a UDP socket on loopback.

mod common;

use std::collections::BTreeMap;
use std::net::UdpSocket;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const NIRI_CONF: &str = "\
// the human's own niri config (hand-written)
input {
    keyboard {
        xkb {
            layout \"us\"
        }
    }
}
include \"~/.config/vault/binds.kdl\"
binds {
    Mod+T { spawn \"foot\"; }
    Mod+Shift+L { spawn \"swaylock\"; }
}
";
const VAULT_BINDS: &str = "binds {\n    Mod+Shift+P { spawn \"true\"; }\n}\n";

struct Home {
    root: PathBuf,
    home: PathBuf,
}

impl Home {
    fn new(name: &str) -> Home {
        let root = std::env::temp_dir().join(format!("gowl-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".config/niri")).unwrap();
        std::fs::create_dir_all(home.join(".config/vault")).unwrap();
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::write(home.join(".config/niri/config.kdl"), NIRI_CONF).unwrap();
        std::fs::write(home.join(".config/vault/binds.kdl"), VAULT_BINDS).unwrap();
        let bin = root.join("fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        script(
            &bin.join("systemctl"),
            "#!/bin/sh\n\
             echo \"$*\" >> \"$FAKE_LOG/systemctl.log\"\n\
             d=\"${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user\"\n\
             case \" $* \" in\n\
               *\" enable \"*) mkdir -p \"$d/graphical-session.target.wants\"; ln -sf \"$d/gowl.service\" \"$d/graphical-session.target.wants/gowl.service\" ;;\n\
               *\" disable \"*) rm -f \"$d/graphical-session.target.wants/gowl.service\" ;;\n\
               *\" is-active \"*) exit 3 ;;\n\
             esac\n\
             exit 0\n",
        );
        script(
            &bin.join("niri"),
            "#!/bin/sh\n\
             echo \"$*\" >> \"$FAKE_LOG/niri.log\"\n\
             if [ -n \"${FAKE_NIRI_FAIL:-}\" ]; then echo 'error: unexpected node' >&2; exit 1; fi\n\
             if [ \"$1\" = validate ] && [ -x /usr/bin/niri ]; then exec /usr/bin/niri \"$@\"; fi\n\
             exit 0\n",
        );
        Home { root, home }
    }

    fn cmd(&self, prog: &str) -> Command {
        let mut c = Command::new(prog);
        common::fake_desktop(&mut c, &self.root);
        let path = format!("{}:/usr/bin:/bin", self.root.join("fakebin").display());
        c.env("PATH", path)
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("FAKE_LOG", &self.root)
            .env("GOWL_BIN", env!("CARGO_BIN_EXE_gowl"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("NIRI_CONFIG")
            .env_remove("PREFIX")
            .env_remove("AUTOSTART")
            .env_remove("PURGE")
            .env_remove("NIRI_KEYS")
            .env_remove("GOWL_CONFIG")
            .env_remove("GOWL_DEVICE")
            .env_remove("GOWL_UNIT")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("GOWL_LOG");
        c
    }

    fn installer(&self, action: &str, env: &[(&str, &str)]) -> (String, i32) {
        let mut c = self.cmd("sh");
        c.arg(common::root().join("packaging/install.sh"))
            .arg(action);
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        (text, out.status.code().unwrap_or(-1))
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.p(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(name)).unwrap_or_default()
    }

    /// Every file and symlink under HOME with its content (symlinks: their target).
    fn tree(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        walk(&self.home, &self.home, &mut m);
        m
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        let rel = p.strip_prefix(base).unwrap().display().to_string();
        let ft = e.file_type().unwrap();
        if ft.is_symlink() {
            out.insert(
                rel,
                format!("-> {}", std::fs::read_link(&p).unwrap().display()),
            );
        } else if ft.is_dir() {
            walk(base, &p, out);
        } else {
            out.insert(
                rel,
                String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned(),
            );
        }
    }
}

fn script(p: &Path, body: &str) {
    std::fs::write(p, body).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn has_real_niri() -> bool {
    Path::new("/usr/bin/niri").exists()
}

#[test]
fn install_uninstall_reinstall_in_a_temp_home() {
    let h = Home::new("cycle");
    let before = h.tree();
    let built = std::fs::read(env!("CARGO_BIN_EXE_gowl")).unwrap();
    let bin = h.home.join(".local/bin/gowl").display().to_string();

    // ---- install
    let (out, code) = h.installer("install", &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(std::fs::read(h.p(".local/bin/gowl")).unwrap(), built);
    let mode = std::fs::metadata(h.p(".local/bin/gowl"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    let unit = h.read(".config/systemd/user/gowl.service");
    assert!(
        unit.contains(&format!("ExecStart=\"{bin}\" screen\n")),
        "{unit}"
    );
    assert!(unit.contains("Environment=GOWL_UNIT=1\n"));
    assert!(unit.contains("RestartPreventExitStatus=2 3 6\n"));
    assert!(
        !h.p(".config/systemd/user/graphical-session.target.wants")
            .exists(),
        "not enabled"
    );
    let ctl = h.log("systemctl.log");
    assert_eq!(ctl.matches("daemon-reload").count(), 1, "{ctl}");
    assert!(!ctl.contains("enable"), "{ctl}");
    assert_eq!(
        h.read(".config/gowl/config"),
        include_str!("../config.example")
    );
    for f in [
        "share/gowl/quickshell/Gowl.qml",
        "share/gowl/quickshell/GowlTile.qml",
        "share/gowl/config.example",
    ] {
        assert!(h.p(&format!(".local/{f}")).exists(), "{f}");
    }
    // niri: the binds file spawns the absolute path; one include line appended.
    let binds = h.read(".config/gowl/niri-binds.kdl");
    assert!(
        binds.contains(&format!("spawn \"{bin}\" \"toggle\" \"screen\"")),
        "{binds}"
    );
    let niri = h.read(".config/niri/config.kdl");
    assert!(
        niri.starts_with(NIRI_CONF),
        "the human's config is kept byte for byte"
    );
    let added = &niri[NIRI_CONF.len()..];
    assert!(
        added.ends_with("\ninclude optional=true \"~/.config/gowl/niri-binds.kdl\"\n"),
        "{added:?}"
    );
    assert_eq!(
        added.lines().filter(|l| !l.is_empty()).count(),
        2,
        "marker + include: {added:?}"
    );
    let backups: Vec<_> = std::fs::read_dir(h.p(".config/gowl/backups"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(
        std::fs::read_to_string(backups[0].path()).unwrap(),
        NIRI_CONF
    );
    assert!(h.log("niri.log").contains("validate -c"), "validated first");
    for want in ["+ ", "gowl.service not enabled", "Next: run 'gowl setup'"] {
        assert!(out.contains(want), "{want:?} in:\n{out}");
    }
    if has_real_niri() {
        // The real niri accepts the result (only temp files are read).
        let st = Command::new("/usr/bin/niri")
            .args(["validate", "-c"])
            .arg(h.p(".config/niri/config.kdl"))
            .env("HOME", &h.home)
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "{}",
            String::from_utf8_lossy(&st.stderr)
        );
    }

    // ---- install again: nothing changes.
    let tree1 = h.tree();
    let (out, code) = h.installer("install", &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(h.tree(), tree1, "second install changed something:\n{out}");
    assert!(!out.contains("\n  + ") && !out.contains("\n  ~ "), "{out}");
    assert!(out.contains("= niri keys already included"), "{out}");
    let ctl = h.log("systemctl.log");
    assert_eq!(
        ctl.matches("daemon-reload").count(),
        1,
        "no reload without a change: {ctl}"
    );

    // ---- AUTOSTART=1 enables the unit.
    let (out, code) = h.installer("install", &[("AUTOSTART", "1")]);
    assert_eq!(code, 0, "{out}");
    assert!(
        h.log("systemctl.log")
            .contains("--user enable gowl.service")
    );
    assert!(
        h.p(".config/systemd/user/graphical-session.target.wants/gowl.service")
            .is_symlink()
    );

    // ---- uninstall: everything but the config (and the niri backups) goes.
    let (out, code) = h.installer("uninstall", &[]);
    assert_eq!(code, 0, "{out}");
    assert!(
        h.log("systemctl.log")
            .contains("--user disable --now gowl.service")
    );
    assert_eq!(
        h.read(".config/niri/config.kdl"),
        NIRI_CONF,
        "niri config restored exactly"
    );
    for gone in [
        ".local/bin/gowl",
        ".local/share/gowl",
        ".config/systemd/user/gowl.service",
        ".config/systemd/user/graphical-session.target.wants/gowl.service",
        ".config/gowl/niri-binds.kdl",
    ] {
        assert!(
            !h.p(gone).exists() && !h.p(gone).is_symlink(),
            "{gone} left behind:\n{out}"
        );
    }
    assert!(h.p(".config/gowl/config").exists(), "config kept");
    assert!(out.contains("kept (PURGE=1 removes it)"), "{out}");
    // Twice: nothing more to do, still fine.
    let tree2 = h.tree();
    let (out, code) = h.installer("uninstall", &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(h.tree(), tree2);
    assert!(!out.contains("\n  - "), "{out}");

    // ---- reinstall works; PURGE=1 leaves HOME exactly as it was before.
    let (out, code) = h.installer("install", &[]);
    assert_eq!(code, 0, "{out}");
    assert!(
        h.read(".config/niri/config.kdl")
            .contains("gowl/niri-binds.kdl")
    );
    std::fs::create_dir_all(h.p(".local/state/gowl")).unwrap();
    std::fs::write(h.p(".local/state/gowl/devices"), "AA 127.0.0.1 H6609\n").unwrap();
    let (out, code) = h.installer("uninstall", &[("PURGE", "1")]);
    assert_eq!(code, 0, "{out}");
    // (Empty directories it created, like ~/.local/bin, may stay; files may not.)
    assert_eq!(h.tree(), before, "left behind after PURGE=1:\n{out}");
}

#[test]
fn keys_are_never_forced() {
    // A gowl key already bound in an included file: nothing is changed.
    let h = Home::new("taken");
    std::fs::write(
        h.p(".config/vault/binds.kdl"),
        "binds {\n    Super+M { spawn \"x\"; }\n}\n",
    )
    .unwrap();
    let (out, code) = h.installer("install", &[]);
    assert_eq!(code, 0, "install goes on without keys: {out}");
    assert!(out.contains("Mod+M is bound in"), "{out}");
    assert!(
        out.contains("include optional=true"),
        "says how to add it by hand: {out}"
    );
    assert_eq!(h.read(".config/niri/config.kdl"), NIRI_CONF);
    assert!(!h.p(".config/gowl/niri-binds.kdl").exists());
    assert!(!h.p(".config/gowl/backups").exists());
    let keys = h
        .cmd(&h.home.join(".local/bin/gowl").display().to_string())
        .arg("keys")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&keys.stdout).contains("taken:       Mod+M"),
        "{keys:?}"
    );

    // niri rejects the edit: left alone, binds file not left behind.
    let h = Home::new("invalid");
    let (out, code) = h.installer("install", &[("FAKE_NIRI_FAIL", "1")]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("rejected the edited config"), "{out}");
    assert_eq!(h.read(".config/niri/config.kdl"), NIRI_CONF);
    assert!(!h.p(".config/gowl/niri-binds.kdl").exists());
    // No temp copy left next to the config either.
    let names: Vec<String> = std::fs::read_dir(h.p(".config/niri"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["config.kdl"]);

    // NIRI_KEYS=0: niri's config untouched; `gowl keys add` / `remove` later.
    let h = Home::new("later");
    let (out, code) = h.installer("install", &[("NIRI_KEYS", "0")]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(h.read(".config/niri/config.kdl"), NIRI_CONF);
    let gowl = h.home.join(".local/bin/gowl").display().to_string();
    let run = |args: &[&str]| -> Output { h.cmd(&gowl).args(args).output().unwrap() };
    assert!(run(&["keys", "add"]).status.success());
    assert!(
        h.read(".config/niri/config.kdl")
            .contains("gowl/niri-binds.kdl")
    );
    // A symlinked config (dotfiles) stays a symlink; the target is edited.
    let real = h.p(".config/niri/real.kdl");
    std::fs::rename(h.p(".config/niri/config.kdl"), &real).unwrap();
    std::os::unix::fs::symlink(&real, h.p(".config/niri/config.kdl")).unwrap();
    let r = run(&["keys", "remove"]);
    assert!(r.status.success(), "{r:?}");
    assert!(h.p(".config/niri/config.kdl").is_symlink());
    assert_eq!(std::fs::read_to_string(&real).unwrap(), NIRI_CONF);
    assert!(!h.p(".config/gowl/niri-binds.kdl").exists());
    // niri missing altogether: remove still takes our line out.
    assert!(run(&["keys", "add"]).status.success());
    std::fs::remove_file(h.root.join("fakebin/niri")).unwrap();
    let r = h
        .cmd(&gowl)
        .env("PATH", h.root.join("fakebin"))
        .args(["keys", "remove"])
        .output()
        .unwrap();
    assert!(r.status.success(), "{r:?}");
    assert_eq!(std::fs::read_to_string(&real).unwrap(), NIRI_CONF);
    script(
        &h.root.join("fakebin/niri"),
        "#!/bin/sh\nif [ -n \"${FAKE_NIRI_FAIL:-}\" ]; then echo 'error: bad' >&2; exit 1; fi\nexit 0\n",
    );

    // An include of the binds file written by hand (no gowl marker) survives
    // `remove`, and so does the file it needs.
    std::fs::remove_file(h.p(".config/niri/config.kdl")).unwrap();
    let by_hand = format!("{NIRI_CONF}include \"~/.config/gowl/niri-binds.kdl\"\n");
    std::fs::write(h.p(".config/niri/config.kdl"), &by_hand).unwrap();
    assert!(run(&["keys", "add"]).status.success());
    assert_eq!(
        h.read(".config/niri/config.kdl"),
        by_hand,
        "nothing appended: already included"
    );
    let r = run(&["keys", "remove"]);
    assert!(r.status.success(), "{r:?}");
    assert_eq!(h.read(".config/niri/config.kdl"), by_hand);
    assert!(
        h.p(".config/gowl/niri-binds.kdl").exists(),
        "still needed by the hand-written include"
    );

    // Already included, but niri rejects the config as it is: said so, not hidden.
    let r = h
        .cmd(&gowl)
        .env("FAKE_NIRI_FAIL", "1")
        .args(["keys", "add"])
        .output()
        .unwrap();
    assert!(!r.status.success());
    assert!(
        String::from_utf8_lossy(&r.stderr).contains("rejects niri's config"),
        "{r:?}"
    );

    // A check file left by a gowl that died mid-check is cleaned up next time.
    std::fs::write(h.p(".config/niri/config.kdl"), NIRI_CONF).unwrap();
    std::fs::remove_file(h.p(".config/gowl/niri-binds.kdl")).unwrap();
    std::fs::write(h.p(".config/niri/.gowl-check-999999999.kdl"), "x").unwrap();
    assert!(run(&["keys", "add"]).status.success());
    assert!(!h.p(".config/niri/.gowl-check-999999999.kdl").exists());
    let names: Vec<String> = std::fs::read_dir(h.p(".config/niri"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "config.kdl + real.kdl only: {names:?}");
    // Several edits within one second: every backup kept, none overwritten.
    for _ in 0..2 {
        assert!(run(&["keys", "remove"]).status.success());
        assert!(run(&["keys", "add"]).status.success());
    }
    let baks = std::fs::read_dir(h.p(".config/gowl/backups"))
        .unwrap()
        .count();
    assert!(baks >= 7, "{baks} backups");
}

/// Run `gowl setup` on a pseudo-terminal, feeding `input` (all lines at once: the
/// terminal hands them out one line per read). Returns (output, exit code).
fn setup_on_pty(h: &Home, base: u16, input: &str) -> (String, i32) {
    setup_steps(h, base, &[("", input)])
}

/// Like `setup_on_pty`, step by step: each `(expect, send)` waits until the output
/// (since the previous step's text) contains `expect`, then types `send`. gowl runs
/// as a session leader with the pty as its controlling terminal, so ^C really is
/// SIGINT from the terminal.
fn setup_steps(h: &Home, base: u16, steps: &[(&str, &str)]) -> (String, i32) {
    const DRIVER: &str = r#"
import os, sys, subprocess, select, time, fcntl, termios
master, slave = os.openpty()
def ctty():
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
p = subprocess.Popen(sys.argv[1:], stdin=slave, stdout=slave, stderr=slave, close_fds=True,
                     start_new_session=True, preexec_fn=ctty)
os.close(slave)
steps = [s.split(b'\x1f', 1) for s in sys.stdin.buffer.read().split(b'\x1e')]
out = b''
seen = 0
end = time.monotonic() + 30
done = False
while True:
    while steps and steps[0][0] in out[seen:]:
        exp, snd = steps.pop(0)
        seen = out.index(exp, seen) + len(exp) if exp else seen
        os.write(master, snd)
    left = end - time.monotonic()
    if left <= 0:
        p.kill()
        out += b'\n[driver: timed out waiting for gowl setup]\n'
        break
    r, _, _ = select.select([master], [], [], left)
    if not r:
        continue
    try:
        b = os.read(master, 65536)
    except OSError:
        break
    if not b:
        break
    out += b
sys.stdout.buffer.write(out)
sys.exit(p.wait())
"#;
    let input: Vec<String> = steps.iter().map(|(e, s)| format!("{e}\u{1f}{s}")).collect();
    let input = input.join("\u{1e}");
    let gowl = h.home.join(".local/bin/gowl");
    let mut c = h.cmd("python3");
    c.args(["-I", "-c", DRIVER])
        .arg(&gowl)
        .arg("setup")
        .env("GOWL_PORT_BASE", base.to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), input.as_bytes()).unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    println!("{text}"); // the transcript (shown with --nocapture or on failure)
    (text, out.status.code().unwrap_or(-1))
}

fn drain(rx: &UdpSocket) -> Vec<Vec<u8>> {
    let mut got = Vec::new();
    let mut buf = [0u8; 2048];
    while let Ok(n) = rx.recv(&mut buf) {
        got.push(buf[..n].to_vec());
    }
    got
}

fn razer(on: bool) -> Vec<u8> {
    let mut m = [0u8; 64];
    let n = gowl::proto::razer_msg(&gowl::proto::razer_mode_frame(on), &mut m);
    m[..n].to_vec()
}

#[test]
fn setup_finds_a_light_by_ip_blinks_it_and_calibrates() {
    let h = Home::new("setup-new");
    let base = 17700;
    let rx = UdpSocket::bind(("127.0.0.1", base + 3)).unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let (out, code) = h.installer("install", &[("NIRI_KEYS", "0")]);
    assert_eq!(code, 0, "{out}");
    // Scans stay on loopback (never the real network): lan_if = lo.
    let conf = h.p(".config/gowl/config");
    let example = h.read(".config/gowl/config");
    std::fs::write(&conf, format!("lan_if = lo\n{example}")).unwrap();

    let input = [
        "",          // LAN Control is on
        "127.0.0.1", // nothing answered the scan: the light's IP, typed
        "y",         // it blinked
        "",          // name: the default ("light")
        "2",         // segments
        "",          // calibrate now: yes
        "4",
        "6",
        "y", // the calibrate walk: left, right, save
        "",  // keepalive walk: no
        "y", // login service: the installed unit → enable
        "",  // niri keys: yes
        "n", // notifications: off
    ]
    .join("\n")
        + "\n";
    let (out, code) = setup_on_pty(&h, base, &input);
    assert_eq!(code, 0, "{out}");
    let text = h.read(".config/gowl/config");
    assert!(
        text.starts_with(&format!("lan_if = lo\n{example}")),
        "every byte kept:\n{text}"
    );
    let c = gowl::config::parse(&text, &mut |n, m| panic!("{n}: {m}"));
    assert_eq!(c.device.as_deref(), Some("light"));
    assert!(!c.notify);
    let d = &c.devices[0];
    assert_eq!(d.alias, "light");
    assert_eq!(d.ip, Some("127.0.0.1".parse().unwrap()));
    assert_eq!(d.segments, Some(2));
    assert_eq!(d.layout.as_deref(), Some(&[4u8, 6][..]));
    assert!(
        h.log("systemctl.log")
            .contains("--user enable gowl.service"),
        "{out}"
    );
    assert!(
        h.read(".config/niri/config.kdl")
            .contains("gowl/niri-binds.kdl")
    );
    for want in [
        "blinking blue",
        "Done. Saved in",
        "light (127.0.0.1) — 2 segments, calibrated layout",
        "at login   on",
        "niri keys  Mod+M / Mod+Shift+M",
    ] {
        assert!(out.contains(want), "{want:?} in:\n{out}");
    }
    // The wire: switched on, razer ON, a blue frame, OFF ×2 (the light is given
    // back after the blink), then calibrate's own session.
    let wire = drain(&rx);
    // First a status query (was it on?) — no answer here, so: switched on, and said so.
    let t = wire
        .iter()
        .position(|m| is_cmd(m, "turn"))
        .expect("turn on");
    assert!(
        t >= 1
            && wire[..t]
                .iter()
                .all(|m| is_cmd(m, "devStatus") || is_cmd(m, "status"))
    );
    assert!(
        String::from_utf8_lossy(&wire[t]).contains("\"value\":1"),
        "on"
    );
    assert!(
        out.contains("gowl switched the light on for this test"),
        "{out}"
    );
    assert_eq!(wire[t + 1], razer(true));
    let off = razer(false);
    let first_off = wire.iter().position(|m| *m == off).expect("razer off");
    assert_eq!(wire[first_off + 1], off, "OFF ×2 after the blink");
    assert!(first_off >= t + 3, "a frame between ON and OFF");
    assert!(
        wire[first_off + 2..].contains(&razer(true)),
        "calibrate streamed"
    );
    assert_eq!(
        wire[wire.len() - 2..],
        [off.clone(), off],
        "calibrate gave it back too"
    );
}

#[test]
fn setup_keeps_an_existing_config_byte_for_byte() {
    let h = Home::new("setup-old");
    let base = 17750;
    let _rx = UdpSocket::bind(("127.0.0.1", base + 3)).unwrap();
    let (out, code) = h.installer("install", &[]);
    assert_eq!(code, 0, "{out}");
    let mine = "device = desk   # mine\nkeepalive_ms = 5600\nlan_if = lo\n\n[device desk]\nip = 127.0.0.1   # fixed in the router\nlayout = 1 7 8 8 8 9 6 3 2 2\n";
    std::fs::write(h.p(".config/gowl/config"), mine).unwrap();
    let input = [
        "",  // LAN Control
        "",  // use the light in the config: yes
        "n", // no blink
        "",  // calibrate again: no
        "",  // keepalive again: no
        "",  // login service: stays off
        "",  // notifications: stay on
    ]
    .join("\n")
        + "\n";
    let (out, code) = setup_on_pty(&h, base, &input);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        h.read(".config/gowl/config"),
        mine,
        "nothing to change, nothing written:\n{out}"
    );
    assert!(
        out.contains("niri keys: Mod+M (screen) and Mod+Shift+M (music) are set up"),
        "{out}"
    );
    assert!(out.contains("Already calibrated: 10 segments"), "{out}");
    assert!(!h.log("systemctl.log").contains("enable"), "{out}");
}

#[test]
fn setup_needs_a_terminal_and_cancels_cleanly() {
    let h = Home::new("setup-tty");
    let gowl = env!("CARGO_BIN_EXE_gowl");
    let out = h.cmd(gowl).arg("setup").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs a terminal"));
    // End of input at the first question: cancelled, nothing written.
    let (out, code) = h.installer("install", &[("NIRI_KEYS", "0")]);
    assert_eq!(code, 0, "{out}");
    let before = h.read(".config/gowl/config");
    let (out, code) = setup_on_pty(&h, 17790, "\u{4}");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("setup cancelled"), "{out}");
    assert_eq!(h.read(".config/gowl/config"), before);
}

fn is_cmd(m: &[u8], cmd: &str) -> bool {
    String::from_utf8_lossy(m).contains(&format!("\"cmd\":\"{cmd}\""))
}

/// A stand-in light on loopback that records every datagram and answers status
/// queries with "off".
struct OffLight {
    got: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    t: Option<std::thread::JoinHandle<()>>,
}

impl OffLight {
    fn start(base: u16) -> OffLight {
        let rx = UdpSocket::bind(("127.0.0.1", base + 3)).unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (g, s) = (got.clone(), stop.clone());
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while !s.load(std::sync::atomic::Ordering::SeqCst) {
                if let Ok(n) = rx.recv(&mut buf) {
                    let m = buf[..n].to_vec();
                    if is_cmd(&m, "devStatus") {
                        let _ = rx.send_to(
                            br#"{"msg":{"cmd":"devStatus","data":{"onOff":0,"brightness":50}}}"#,
                            ("127.0.0.1", base + 2),
                        );
                    }
                    g.lock().unwrap().push(m);
                }
            }
        });
        OffLight {
            got,
            stop,
            t: Some(t),
        }
    }

    fn wire(&mut self) -> Vec<Vec<u8>> {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.t.take() {
            t.join().unwrap();
        }
        self.got.lock().unwrap().clone()
    }
}

#[test]
fn the_blink_gives_the_light_back_as_it_was_even_on_ctrl_c() {
    let h = Home::new("setup-blink");
    let base = 17800;
    let (out, code) = h.installer("install", &[("NIRI_KEYS", "0")]);
    assert_eq!(code, 0, "{out}");
    let mine = "device = desk\nlan_if = lo\n[device desk]\nip = 127.0.0.1\nlayout = 4 6\n";
    std::fs::write(h.p(".config/gowl/config"), mine).unwrap();
    let off = razer(false);

    // The light answers "off": blink, y, then it is switched off again.
    let mut light = OffLight::start(base);
    let (out, code) = setup_steps(
        &h,
        base,
        &[
            ("LAN Control is on", "\n"),
            ("Use it?", "\n"),
            ("Blink it once", "\n"),
            ("Is it?", "y\n"),
            ("Calibrate again?", "\u{4}"), // then end of input: cancelled
        ],
    );
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("was off before the test"), "{out}");
    let wire = light.wire();
    assert!(is_cmd(&wire[0], "devStatus"), "{wire:?}");
    let n = wire.len();
    assert_eq!(wire[n - 3..n - 1], [off.clone(), off.clone()], "OFF ×2");
    assert!(
        is_cmd(&wire[n - 1], "turn")
            && String::from_utf8_lossy(&wire[n - 1]).contains("\"value\":0")
    );
    assert_eq!(h.read(".config/gowl/config"), mine);

    // Ctrl+C while it blinks: the light is given back (OFF ×2, off again), nothing
    // keeps blinking, nothing is left behind.
    let mut light = OffLight::start(base);
    let (out, code) = setup_steps(
        &h,
        base,
        &[
            ("LAN Control is on", "\n"),
            ("Use it?", "\n"),
            ("Blink it once", "\n"),
            ("Is it?", "\u{3}"),
        ],
    );
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("setup cancelled"), "{out}");
    std::thread::sleep(Duration::from_millis(600)); // a blinking gowl would send more
    let wire = light.wire();
    let n = wire.len();
    assert!(wire.contains(&razer(true)), "{wire:?}");
    assert_eq!(wire[n - 3..n - 1], [off.clone(), off], "{wire:?}");
    assert!(is_cmd(&wire[n - 1], "turn"));
    assert!(!h.root.join("run/gowl.pid").exists());
}

#[test]
fn a_failed_copy_never_replaces_the_installed_binary() {
    let h = Home::new("cpfail");
    let (out, code) = h.installer("install", &[]);
    assert_eq!(code, 0, "{out}");
    let built = std::fs::read(env!("CARGO_BIN_EXE_gowl")).unwrap();
    // A different binary to install, so the copy is really attempted.
    let newer = h.root.join("gowl-newer");
    let mut bytes = built.clone();
    bytes.push(0);
    std::fs::write(&newer, &bytes).unwrap();
    std::fs::set_permissions(&newer, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bad = h.root.join("badcp");
    std::fs::create_dir_all(&bad).unwrap();
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        bad.display(),
        h.root.join("fakebin").display()
    );
    for (what, body) in [
        (
            "cp fails half-way",
            "#!/bin/sh\nhead -c 4096 \"$1\" > \"$2\"\nexit 1\n",
        ),
        (
            "cp claims success with a short file",
            "#!/bin/sh\nhead -c 4096 \"$1\" > \"$2\"\nexit 0\n",
        ),
    ] {
        script(&bad.join("cp"), body);
        let (out, code) = h.installer(
            "install",
            &[("PATH", &path), ("GOWL_BIN", &newer.display().to_string())],
        );
        assert_ne!(code, 0, "{what}: {out}");
        assert!(out.contains("cannot install"), "{what}: {out}");
        assert!(!out.contains("~ "), "{what}: reported as updated: {out}");
        assert_eq!(
            std::fs::read(h.p(".local/bin/gowl")).unwrap(),
            built,
            "{what}"
        );
        let leftovers: Vec<_> = std::fs::read_dir(h.p(".local/bin"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("gowl-new"))
            .collect();
        assert!(leftovers.is_empty(), "{what}: temp file left");
        assert!(!h.log("systemctl.log").contains("restart"), "{what}");
    }
}

#[test]
fn root_is_refused_before_anything_is_built() {
    let h = Home::new("root");
    let fake = h.root.join("asroot");
    std::fs::create_dir_all(&fake).unwrap();
    script(&fake.join("id"), "#!/bin/sh\necho 0\n");
    script(
        &fake.join("cargo"),
        "#!/bin/sh\necho \"$*\" >> \"$FAKE_LOG/cargo.log\"\nexit 0\n",
    );
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        fake.display(),
        h.root.join("fakebin").display()
    );
    for target in ["install", "uninstall"] {
        let out = h
            .cmd("make")
            .args(["-s", "-C"])
            .arg(common::root())
            .arg(target)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{target}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("not with sudo or as root"),
            "{out:?}"
        );
    }
    assert_eq!(h.log("cargo.log"), "", "nothing built as root");
    let (out, code) = h.installer("install", &[("PATH", &path)]);
    assert_eq!(code, 1, "{out}");
    assert!(!h.p(".local/bin/gowl").exists());
}

#[test]
fn an_odd_prefix_is_quoted_for_systemd() {
    let h = Home::new("prefix");
    let prefix = h.home.join("my apps%x$y");
    let (out, code) = h.installer("install", &[("PREFIX", &prefix.display().to_string())]);
    assert_eq!(code, 0, "{out}");
    let bin = prefix.join("bin/gowl");
    assert!(bin.exists());
    let unit = h.read(".config/systemd/user/gowl.service");
    let want = format!(
        "ExecStart=\"{}\" screen\n",
        bin.display().to_string().replace('%', "%%")
    );
    assert!(unit.contains(&want), "{want}\n{unit}");
    // The keys spawn the same binary (a space is fine inside a KDL string).
    assert!(
        h.read(".config/gowl/niri-binds.kdl")
            .contains(&format!("spawn \"{}\"", bin.display()))
    );
}
