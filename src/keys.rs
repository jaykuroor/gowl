//! `gowl keys [status|add|remove]`: the niri keybinds Mod+M (screen sync on/off)
//! and Mod+Shift+M (music sync on/off), D-070. `make install`, `make uninstall`
//! and `gowl setup` all go through here, so there is one implementation.
//!
//! - The binds live in gowl's own file, `~/.config/gowl/niri-binds.kdl`, written
//!   from `packaging/niri-binds.kdl` with the **absolute** path of this gowl
//!   (niri's PATH is often `/usr/local/bin:/usr/bin`, without `~/.local/bin`; D-061).
//! - niri's config gets one marked line, `include optional=true
//!   "~/.config/gowl/niri-binds.kdl"`, appended only if it isn't there yet, only if
//!   neither key is bound already (config.kdl and every file it includes — niri
//!   itself accepts a duplicate bind in another file, so `niri validate` can't be
//!   the judge), only after `niri validate -c` passed on the edited copy (written
//!   next to config.kdl, so relative includes resolve the same), and after a
//!   timestamped backup in `~/.config/gowl/backups/`. The write is atomic and goes
//!   through a symlinked config.kdl to its target.
//! - Generated files carry a first line saying so; `add` rewrites a generated
//!   binds file (a new gowl path) but leaves one whose first line the user deleted.
//! - gowl never creates niri's config.kdl (niri writes its own default on first
//!   start; a file with only our include would replace all of niri's defaults).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;
use crate::error::Error;

const TEMPLATE: &str = include_str!("../packaging/niri-binds.kdl");
/// First line of a generated binds file.
pub const GENERATED: &str =
    "// gowl keys for niri, written by `gowl keys add` — delete this line to keep your own edits";
/// The comment line `add` puts above the include.
const MARK: &str = "// gowl: Mod+M screen sync, Mod+Shift+M music sync — `gowl keys remove` (or make uninstall) takes this out";
/// What identifies our include line.
const NEEDLE: &str = "gowl/niri-binds.kdl\"";

/// Where everything lives.
#[derive(Clone, Debug)]
pub struct Paths {
    /// niri's config (`$NIRI_CONFIG`, else `$XDG_CONFIG_HOME/niri/config.kdl`).
    pub niri: PathBuf,
    /// `~/.config/gowl/niri-binds.kdl`.
    pub binds: PathBuf,
    /// `~/.config/gowl/backups/`.
    pub backups: PathBuf,
    home: Option<PathBuf>,
}

impl Paths {
    pub fn from_env() -> Result<Paths, Error> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from);
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")))
            .ok_or_else(|| Error::Runtime("HOME and XDG_CONFIG_HOME are unset".into()))?;
        let niri = std::env::var_os("NIRI_CONFIG")
            .filter(|h| !h.is_empty())
            .map_or_else(|| xdg.join("niri/config.kdl"), PathBuf::from);
        let gowl = xdg.join("gowl");
        Ok(Paths {
            niri,
            binds: gowl.join("niri-binds.kdl"),
            backups: gowl.join("backups"),
            home,
        })
    }

    /// The include line's path: `~/…` under HOME (niri expands it), else absolute.
    fn include_path(&self) -> String {
        if let Some(h) = &self.home
            && let Ok(rel) = self.binds.strip_prefix(h)
        {
            return format!("~/{}", rel.display());
        }
        self.binds.display().to_string()
    }

    fn include_line(&self) -> String {
        format!("include optional=true \"{}\"", self.include_path())
    }

    /// `~` and relative include paths, the way niri resolves them.
    fn resolve(&self, from: &Path, p: &str) -> PathBuf {
        if let (Some(rest), Some(h)) = (p.strip_prefix("~/"), &self.home) {
            return h.join(rest);
        }
        let p = Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            from.parent().unwrap_or(Path::new(".")).join(p)
        }
    }
}

/// The binds file for gowl at `exe`.
pub fn binds_text(exe: &str) -> String {
    let body: String = TEMPLATE
        .lines()
        .filter(|l| !l.starts_with("//!"))
        .map(|l| l.replace("spawn \"gowl\"", &format!("spawn \"{exe}\"")) + "\n")
        .collect();
    format!("{GENERATED}\n{body}")
}

/// `Mod+M` / `Mod+Shift+M` if `token` (a bind's key, e.g. `Super+Shift+m`) is one
/// of gowl's keys. Mod, Super and Win count as the same modifier (niri's Mod is
/// Super on a TTY).
pub fn gowl_key(token: &str) -> Option<&'static str> {
    let mut parts: Vec<String> = token.split('+').map(str::to_ascii_lowercase).collect();
    if parts.pop()? != "m" || parts.is_empty() {
        return None;
    }
    let mut mods: Vec<&str> = Vec::new();
    for p in &parts {
        let m = match p.as_str() {
            "mod" | "super" | "win" => "mod",
            "shift" => "shift",
            _ => return None,
        };
        if !mods.contains(&m) {
            mods.push(m);
        }
    }
    mods.sort_unstable();
    match mods.as_slice() {
        ["mod"] => Some("Mod+M"),
        ["mod", "shift"] => Some("Mod+Shift+M"),
        _ => None,
    }
}

/// Lines of KDL text with comments removed (`//…`, `/* … */`, and `/-` node
/// comments on the line they start). Good enough for keys and include paths,
/// which never contain those sequences.
fn code_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let mut s = String::new();
        let mut rest = line;
        loop {
            if in_block {
                match rest.find("*/") {
                    Some(i) => {
                        rest = &rest[i + 2..];
                        in_block = false;
                    }
                    None => break,
                }
            }
            let cut = [rest.find("//"), rest.find("/*")]
                .into_iter()
                .flatten()
                .min();
            match cut {
                Some(i) if rest[i..].starts_with("/*") => {
                    s.push_str(&rest[..i]);
                    rest = &rest[i + 2..];
                    in_block = true;
                }
                Some(i) => {
                    s.push_str(&rest[..i]);
                    break;
                }
                None => {
                    s.push_str(rest);
                    break;
                }
            }
        }
        let t = s.trim_start();
        out.push(if t.starts_with("/-") {
            String::new()
        } else {
            s
        });
    }
    out
}

/// The first quoted string on a line.
fn quoted(line: &str) -> Option<&str> {
    let a = line.find('"')? + 1;
    let b = line[a..].find('"')? + a;
    Some(&line[a..b])
}

fn is_include(code: &str) -> bool {
    let t = code.trim_start();
    t.starts_with("include") && t[7..].starts_with([' ', '\t', '"'])
}

/// Where gowl's keys are bound already: (file, line number, key), over `file` and
/// everything it includes (except gowl's own binds file).
pub fn conflicts(paths: &Paths, file: &Path) -> Vec<(PathBuf, usize, &'static str)> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    scan(paths, file, 0, &mut seen, &mut out);
    out
}

fn scan(
    paths: &Paths,
    file: &Path,
    depth: usize,
    seen: &mut Vec<PathBuf>,
    out: &mut Vec<(PathBuf, usize, &'static str)>,
) {
    let canon = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    if depth > 8 || seen.contains(&canon) {
        return;
    }
    seen.push(canon);
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    for (i, code) in code_lines(&text).iter().enumerate() {
        if is_include(code) {
            if let Some(p) = quoted(code)
                && !p.ends_with("gowl/niri-binds.kdl")
            {
                scan(paths, &paths.resolve(file, p), depth + 1, seen, out);
            }
            continue;
        }
        let token = code
            .trim_start()
            .split(|c: char| c.is_whitespace() || c == '{')
            .next()
            .unwrap_or("");
        if let Some(k) = gowl_key(token) {
            out.push((file.to_path_buf(), i + 1, k));
        }
    }
}

/// Indices of our include lines in niri's config text (and our marker above them).
fn our_lines(text: &str) -> Vec<usize> {
    let lines: Vec<&str> = text.lines().collect();
    let code = code_lines(text);
    let mut idx = Vec::new();
    for (i, c) in code.iter().enumerate() {
        if is_include(c) && c.contains(NEEDLE) {
            if i > 0 && lines[i - 1].trim() == MARK {
                idx.push(i - 1);
            }
            idx.push(i);
        }
    }
    idx
}

/// Is gowl's include in niri's config?
pub fn included(paths: &Paths) -> bool {
    std::fs::read_to_string(&paths.niri).is_ok_and(|t| {
        code_lines(&t)
            .iter()
            .any(|c| is_include(c) && c.contains(NEEDLE))
    })
}

/// Why `niri validate` refused, or that niri isn't installed.
enum Invalid {
    NoNiri,
    Refused(String),
}

/// `niri validate -c FILE`.
fn validate(file: &Path) -> Result<(), Invalid> {
    let out = match Command::new("niri")
        .args(["validate", "-c"])
        .arg(file)
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Invalid::NoNiri),
        Err(e) => return Err(Invalid::Refused(e.to_string())),
    };
    if out.status.success() {
        return Ok(());
    }
    let text = strip_ansi(&String::from_utf8_lossy(&out.stderr));
    let tail: Vec<&str> = text.lines().rev().take(12).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    Err(Invalid::Refused(tail.join("\n")))
}

fn strip_ansi(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' && it.peek() == Some(&'[') {
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            o.push(c);
        }
    }
    o
}

/// `YYYYmmdd-HHMMSS` (UTC) for backup names.
fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// Validate `new` as niri's config (a sibling temp file), back the old one up and
/// write `new` atomically. Returns the backup path.
fn replace_niri_config(paths: &Paths, old: &str, new: &str) -> Result<PathBuf, Error> {
    let dir = paths.niri.parent().unwrap_or(Path::new("."));
    let check = dir.join(format!(".gowl-check-{}.kdl", std::process::id()));
    std::fs::write(&check, new).map_err(|e| Error::Io("write a temp niri config", e))?;
    let r = validate(&check);
    let _ = std::fs::remove_file(&check);
    match r {
        Ok(()) => {}
        Err(Invalid::NoNiri) => {
            return Err(Error::Runtime(
                "niri isn't installed (no `niri` on PATH), so the edit can't be checked; niri's config was left alone".into(),
            ));
        }
        Err(Invalid::Refused(why)) => {
            return Err(Error::Runtime(format!(
                "`niri validate` rejected the edited config, so it was left alone:\n{why}"
            )));
        }
    }
    std::fs::create_dir_all(&paths.backups).map_err(|e| Error::Io("create backups dir", e))?;
    let bak = paths.backups.join(format!("niri-config.kdl.{}", stamp()));
    std::fs::write(&bak, old).map_err(|e| Error::Io("back up niri config", e))?;
    config::write_atomic(&paths.niri, new)?;
    Ok(bak)
}

/// What `add` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Added {
    /// The include was added (backup path).
    Added(PathBuf),
    /// It was there already.
    Present,
}

/// Write the binds file for `exe` (unless the user took it over) and include it
/// from niri's config. Errors explain how to do it by hand.
pub fn add(paths: &Paths, exe: &str) -> Result<Added, Error> {
    let old = match std::fs::read_to_string(&paths.niri) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Runtime(format!(
                "no niri config at {} (niri writes one on its first start); keys not added",
                paths.niri.display()
            )));
        }
        Err(e) => return Err(Error::Io("read niri config", e)),
    };
    if !included(paths) {
        let found = conflicts(paths, &paths.niri);
        if !found.is_empty() {
            let list: Vec<String> = found
                .iter()
                .map(|(f, n, k)| format!("  {k} is bound in {}:{n}", f.display()))
                .collect();
            return Err(Error::Runtime(format!(
                "a gowl key is taken already, so niri's config was left alone:\n{}\nTo use other keys: write {} yourself (template: {}), then add this line to {}:\n  {}",
                list.join("\n"),
                paths.binds.display(),
                "packaging/niri-binds.kdl",
                paths.niri.display(),
                paths.include_line()
            )));
        }
    }
    write_binds(paths, exe)?;
    if included(paths) {
        return Ok(Added::Present);
    }
    let mut new = old.clone();
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(&format!("\n{MARK}\n{}\n", paths.include_line()));
    let bak = replace_niri_config(paths, &old, &new).inspect_err(|_| {
        // Nothing points at the binds file: don't leave it behind.
        if !included(paths) {
            let _ = std::fs::remove_file(&paths.binds);
        }
    })?;
    Ok(Added::Added(bak))
}

/// The binds file: written if absent or still generated (first line).
fn write_binds(paths: &Paths, exe: &str) -> Result<bool, Error> {
    let want = binds_text(exe);
    match std::fs::read_to_string(&paths.binds) {
        Ok(t) if t == want => return Ok(false),
        Ok(t) if t.lines().next() != Some(GENERATED) => {
            eprintln!(
                "gowl: {} was edited by hand (its first line is gone); left as it is",
                paths.binds.display()
            );
            return Ok(false);
        }
        _ => {}
    }
    config::write_atomic(&paths.binds, &want)?;
    Ok(true)
}

/// What `remove` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Removed {
    /// The include was taken out of niri's config (backup path).
    pub include: Option<PathBuf>,
    pub binds_file: bool,
}

/// Take the include out of niri's config (validated, backed up) and delete the
/// binds file. Idempotent.
pub fn remove(paths: &Paths) -> Result<Removed, Error> {
    let mut r = Removed::default();
    if let Ok(old) = std::fs::read_to_string(&paths.niri) {
        let drop = our_lines(&old);
        if !drop.is_empty() {
            let mut new: String = old
                .split_inclusive('\n')
                .enumerate()
                .filter(|(i, _)| !drop.contains(i))
                .map(|(_, l)| l)
                .collect();
            // The blank line `add` put before the marker.
            if new.ends_with("\n\n") && !old.ends_with("\n\n") {
                new.pop();
            }
            let bak = match replace_niri_config(paths, &old, &new) {
                Err(Error::Runtime(m)) if m.starts_with("niri isn't installed") => {
                    // Nothing reads this config without niri: take the line out unchecked.
                    std::fs::create_dir_all(&paths.backups)
                        .map_err(|e| Error::Io("create backups dir", e))?;
                    let bak = paths.backups.join(format!("niri-config.kdl.{}", stamp()));
                    std::fs::write(&bak, &old).map_err(|e| Error::Io("back up niri config", e))?;
                    config::write_atomic(&paths.niri, &new)?;
                    bak
                }
                Err(Error::Runtime(m)) => {
                    return Err(Error::Runtime(format!(
                        "{m}\nRemove the gowl lines from {} by hand.",
                        paths.niri.display()
                    )));
                }
                other => other?,
            };
            r.include = Some(bak);
        }
    }
    match std::fs::remove_file(&paths.binds) {
        Ok(()) => r.binds_file = true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Io("remove niri binds file", e)),
    }
    Ok(r)
}

/// This gowl's absolute path (what niri should spawn).
pub fn own_exe() -> Result<String, Error> {
    let p = std::env::current_exe().map_err(|e| Error::Io("find gowl's own path", e))?;
    let s = p.display().to_string();
    // A binary replaced while running reads as "… (deleted)".
    Ok(s.strip_suffix(" (deleted)").unwrap_or(&s).to_string())
}

/// `gowl keys [status|add|remove]`.
pub fn cmd_keys(rest: &[String]) -> Result<(), Error> {
    let paths = Paths::from_env()?;
    match rest.first().map(String::as_str) {
        None | Some("status") if rest.len() <= 1 => {
            let inc = included(&paths);
            println!(
                "niri config: {}{}",
                paths.niri.display(),
                if paths.niri.exists() {
                    ""
                } else {
                    " (missing)"
                }
            );
            println!(
                "gowl keys:   {}",
                if inc {
                    "included (Mod+M screen, Mod+Shift+M music)"
                } else {
                    "not included — `gowl keys add`"
                }
            );
            println!(
                "binds file:  {}{}",
                paths.binds.display(),
                if paths.binds.exists() {
                    ""
                } else {
                    " (missing)"
                }
            );
            if !inc {
                for (f, n, k) in conflicts(&paths, &paths.niri) {
                    println!("taken:       {k} at {}:{n}", f.display());
                }
            }
            Ok(())
        }
        Some("add") if rest.len() == 1 => {
            match add(&paths, &own_exe()?)? {
                Added::Present => println!(
                    "  = niri keys already included from {} (binds file: {})",
                    paths.niri.display(),
                    paths.binds.display()
                ),
                Added::Added(bak) => {
                    println!("  + niri keys: {}", paths.binds.display());
                    println!(
                        "  + niri config: include line appended to {} (backup: {})",
                        paths.niri.display(),
                        bak.display()
                    );
                    println!(
                        "    Mod+M = screen sync on/off, Mod+Shift+M = music sync on/off (live now)"
                    );
                }
            }
            Ok(())
        }
        Some("remove") if rest.len() == 1 => {
            let r = remove(&paths)?;
            match &r.include {
                Some(bak) => println!(
                    "  - niri config: gowl include removed from {} (backup: {})",
                    paths.niri.display(),
                    bak.display()
                ),
                None => println!("  = niri config: no gowl include"),
            }
            if r.binds_file {
                println!("  - niri keys: {}", paths.binds.display());
            }
            Ok(())
        }
        _ => Err(Error::Usage("keys: expected status, add or remove".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gowl_keys_are_recognised_in_any_spelling() {
        for k in ["Mod+M", "Super+m", "mod+M", "Win+M"] {
            assert_eq!(gowl_key(k), Some("Mod+M"), "{k}");
        }
        for k in ["Mod+Shift+M", "Shift+Mod+m", "Super+Shift+M"] {
            assert_eq!(gowl_key(k), Some("Mod+Shift+M"), "{k}");
        }
        for k in [
            "M",
            "Mod+N",
            "Mod+Ctrl+M",
            "Alt+M",
            "Shift+M",
            "Mod+MM",
            "binds",
        ] {
            assert_eq!(gowl_key(k), None, "{k}");
        }
    }

    #[test]
    fn comments_hide_binds_and_includes() {
        let c = code_lines(
            "Mod+M { a; } // x\n// Mod+M { b; }\n/-Mod+M { c; }\n/* Mod+M\nMod+Shift+M */ Mod+X\n",
        );
        assert_eq!(c[0].trim(), "Mod+M { a; }");
        assert_eq!(c[1].trim(), "");
        assert_eq!(c[2].trim(), "");
        assert_eq!(c[3].trim(), "");
        assert_eq!(c[4].trim(), "Mod+X");
    }

    #[test]
    fn binds_file_spawns_the_absolute_path() {
        let t = binds_text("/home/u/.local/bin/gowl");
        assert!(t.starts_with(GENERATED));
        assert!(t.contains("spawn \"/home/u/.local/bin/gowl\" \"toggle\" \"screen\""));
        assert!(t.contains("spawn \"/home/u/.local/bin/gowl\" \"toggle\" \"music\""));
        assert!(!t.contains("spawn \"gowl\""));
        assert!(!t.contains("//!"), "template notes stay out");
    }

    #[test]
    fn backup_stamp_is_a_plausible_date() {
        let s = stamp();
        assert_eq!(s.len(), 15, "{s}");
        assert!(s.starts_with("20"), "{s}");
        assert_eq!(&s[8..9], "-");
    }
}
