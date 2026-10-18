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
/// An include path ending in this is gowl's binds file.
const BINDS_SUFFIX: &str = "gowl/niri-binds.kdl";

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

/// A KDL token, as much as finding binds and includes needs (D-076): strings
/// (quoted, raw `r#"…"#` / `#"…"#`) are unquoted, so `"Mod+M"` and `Mod+M` look alike.
#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// A bare word or a string: node names, arguments, property names and values.
    Word(String),
    Open,
    Close,
    /// `;` or a newline: ends a node.
    End,
    /// `/-`: comments out the next node, argument or children block.
    SlashDash,
    Eq,
}

/// Tokens with their line numbers. Comments (`//`, nested `/* */`), type
/// annotations `(…)` and line continuations `\` are dropped.
fn tokenize(text: &str) -> Vec<(Tok, usize)> {
    let b: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut line) = (0, 1);
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        match c {
            '\n' => {
                out.push((Tok::End, line));
                line += 1;
                i += 1;
            }
            ';' => {
                out.push((Tok::End, line));
                i += 1;
            }
            '{' => {
                out.push((Tok::Open, line));
                i += 1;
            }
            '}' => {
                out.push((Tok::Close, line));
                i += 1;
            }
            '=' => {
                out.push((Tok::Eq, line));
                i += 1;
            }
            '/' if next == Some('/') => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                let mut depth = 0;
                while i < b.len() {
                    if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        line += usize::from(b[i] == '\n');
                        i += 1;
                    }
                }
            }
            '/' if next == Some('-') => {
                out.push((Tok::SlashDash, line));
                i += 2;
            }
            '\\' => {
                // Line continuation: up to and including the newline.
                i += 1;
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
                if i < b.len() {
                    line += 1;
                    i += 1;
                }
            }
            '(' => {
                while i < b.len() && b[i] != ')' {
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                let at = line;
                let mut s = String::new();
                i += 1;
                while i < b.len() && b[i] != '"' {
                    if b[i] == '\\' && i + 1 < b.len() {
                        i += 1;
                        s.push(match b[i] {
                            'n' => '\n',
                            't' => '\t',
                            c => c,
                        });
                    } else {
                        line += usize::from(b[i] == '\n');
                        s.push(b[i]);
                    }
                    i += 1;
                }
                i += 1;
                out.push((Tok::Word(s), at));
            }
            '#' | 'r'
                if {
                    // Raw string: r#"…"#, r"…" (KDL v1) or #"…"# (v2).
                    let mut j = i + usize::from(c == 'r');
                    while b.get(j) == Some(&'#') {
                        j += 1;
                    }
                    b.get(j) == Some(&'"') && (c == 'r' || j > i)
                } =>
            {
                let at = line;
                i += usize::from(c == 'r');
                let mut hashes = 0;
                while b[i] == '#' {
                    hashes += 1;
                    i += 1;
                }
                i += 1; // opening quote
                let mut s = String::new();
                while i < b.len() {
                    if b[i] == '"' && (1..=hashes).all(|k| b.get(i + k) == Some(&'#')) {
                        i += 1 + hashes;
                        break;
                    }
                    line += usize::from(b[i] == '\n');
                    s.push(b[i]);
                    i += 1;
                }
                out.push((Tok::Word(s), at));
            }
            c if c.is_whitespace() => i += 1,
            _ => {
                let mut s = String::new();
                while i < b.len()
                    && !b[i].is_whitespace()
                    && !matches!(b[i], '{' | '}' | ';' | '=' | '"' | '(' | ')')
                    && !(b[i] == '/' && matches!(b.get(i + 1), Some('/' | '*')))
                {
                    s.push(b[i]);
                    i += 1;
                }
                out.push((Tok::Word(s), line));
            }
        }
    }
    out
}

/// What a KDL file says that matters here: every node name directly inside a
/// `binds` block (with its line), and every top-level `include` path.
#[derive(Debug, Default, PartialEq)]
struct Kdl {
    binds: Vec<(String, usize)>,
    includes: Vec<String>,
}

fn walk_kdl(text: &str) -> Kdl {
    let toks = tokenize(text);
    let mut k = Kdl::default();
    let mut stack: Vec<String> = Vec::new();
    let mut i = 0;
    // Skip one node from `i` (its arguments and children), stopping before the
    // token that ends it at this level.
    let skip_node = |i: &mut usize| {
        let mut depth = 0usize;
        while *i < toks.len() {
            match toks[*i].0 {
                Tok::Open => depth += 1,
                Tok::Close if depth == 0 => return,
                Tok::Close => {
                    depth -= 1;
                    if depth == 0 {
                        *i += 1;
                        return;
                    }
                }
                Tok::End if depth == 0 => return,
                _ => {}
            }
            *i += 1;
        }
    };
    while i < toks.len() {
        match &toks[i].0 {
            Tok::End => i += 1,
            Tok::Close => {
                stack.pop();
                i += 1;
            }
            Tok::SlashDash => {
                i += 1;
                while matches!(toks.get(i).map(|t| &t.0), Some(Tok::End)) {
                    i += 1;
                }
                skip_node(&mut i);
            }
            Tok::Open | Tok::Eq => i += 1, // stray: tolerate
            Tok::Word(name) => {
                let (name, line) = (name.clone(), toks[i].1);
                if stack.last().is_some_and(|p| p == "binds") {
                    k.binds.push((name.clone(), line));
                }
                i += 1;
                let mut first_arg: Option<String> = None;
                // Arguments and properties, up to the end of the node or its children.
                while i < toks.len() {
                    match &toks[i].0 {
                        Tok::End | Tok::Close => break,
                        Tok::Open => {
                            stack.push(name.clone());
                            i += 1;
                            break;
                        }
                        Tok::SlashDash => {
                            i += 1;
                            if matches!(toks.get(i).map(|t| &t.0), Some(Tok::Open)) {
                                // A commented-out children block.
                                let mut depth = 0usize;
                                while i < toks.len() {
                                    match toks[i].0 {
                                        Tok::Open => depth += 1,
                                        Tok::Close => {
                                            depth -= 1;
                                            if depth == 0 {
                                                i += 1;
                                                break;
                                            }
                                        }
                                        _ => {}
                                    }
                                    i += 1;
                                }
                            } else {
                                i += 1;
                                if matches!(toks.get(i).map(|t| &t.0), Some(Tok::Eq)) {
                                    i += 2;
                                }
                            }
                        }
                        Tok::Word(w) => {
                            if matches!(toks.get(i + 1).map(|t| &t.0), Some(Tok::Eq)) {
                                i += 3; // a property: key = value
                            } else {
                                if first_arg.is_none() {
                                    first_arg = Some(w.clone());
                                }
                                i += 1;
                            }
                        }
                        Tok::Eq => i += 1,
                    }
                }
                if name == "include"
                    && stack.is_empty()
                    && let Some(p) = first_arg
                {
                    k.includes.push(p);
                }
            }
        }
    }
    k
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
    let k = walk_kdl(&text);
    for (name, line) in &k.binds {
        if let Some(key) = gowl_key(name) {
            out.push((file.to_path_buf(), *line, key));
        }
    }
    for p in &k.includes {
        if !p.ends_with(BINDS_SUFFIX) {
            scan(paths, &paths.resolve(file, p), depth + 1, seen, out);
        }
    }
}

/// Indices of the lines gowl added: its marker comment and the include line right
/// after it. An include of the binds file written by hand is left alone.
fn our_lines(text: &str) -> Vec<usize> {
    let lines: Vec<&str> = text.lines().collect();
    let mut idx = Vec::new();
    for i in 0..lines.len().saturating_sub(1) {
        let next = lines[i + 1].trim();
        if lines[i].trim() == MARK && next.starts_with("include ") && next.contains(NEEDLE) {
            idx.extend([i, i + 1]);
        }
    }
    idx
}

/// Is gowl's binds file included from niri's config (by gowl or by hand)?
pub fn included(paths: &Paths) -> bool {
    std::fs::read_to_string(&paths.niri).is_ok_and(|t| includes_binds(&t))
}

fn includes_binds(text: &str) -> bool {
    walk_kdl(text)
        .includes
        .iter()
        .any(|p| p.ends_with(BINDS_SUFFIX))
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

/// The temp copy `niri validate` checks; removed when dropped, on every path.
struct CheckFile(PathBuf);

impl Drop for CheckFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `.gowl-check-PID.kdl` files in `dir` left by a gowl that died mid-check
/// (its pid is gone): removed.
fn remove_stale_checks(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name
            .strip_prefix(".gowl-check-")
            .and_then(|r| r.strip_suffix(".kdl"))
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `niri validate` on `text` as if it were niri's config: written next to
/// config.kdl so relative includes resolve the same.
fn check_as_niri_config(paths: &Paths, text: &str) -> Result<(), Invalid> {
    let dir = paths.niri.parent().unwrap_or(Path::new("."));
    remove_stale_checks(dir);
    let check = CheckFile(dir.join(format!(".gowl-check-{}.kdl", std::process::id())));
    if let Err(e) = std::fs::write(&check.0, text) {
        return Err(Invalid::Refused(format!(
            "cannot write a temp copy in {}: {e}",
            dir.display()
        )));
    }
    validate(&check.0)
}

fn invalid_error(r: Invalid) -> Error {
    match r {
        Invalid::NoNiri => Error::Runtime(
            "niri isn't installed (no `niri` on PATH), so the edit can't be checked; niri's config was left alone".into(),
        ),
        Invalid::Refused(why) => Error::Runtime(format!(
            "`niri validate` rejected the edited config, so it was left alone:\n{why}"
        )),
    }
}

/// Copy `old` to a new backup file (never overwriting one: a second backup in the
/// same second gets `.2`, `.3`, …), fsynced. Returns its path.
fn backup(paths: &Paths, old: &str) -> Result<PathBuf, Error> {
    use std::io::Write as _;
    std::fs::create_dir_all(&paths.backups).map_err(|e| Error::Io("create backups dir", e))?;
    let base = format!("niri-config.kdl.{}", stamp());
    for n in 1..1000 {
        let p = if n == 1 {
            paths.backups.join(&base)
        } else {
            paths.backups.join(format!("{base}.{n}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(mut f) => {
                f.write_all(old.as_bytes())
                    .and_then(|()| f.sync_all())
                    .map_err(|e| Error::Io("back up niri config", e))?;
                if let Ok(d) = std::fs::File::open(&paths.backups) {
                    let _ = d.sync_all();
                }
                return Ok(p);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::Io("back up niri config", e)),
        }
    }
    Err(Error::Runtime(
        "too many niri config backups this second".into(),
    ))
}

/// Validate `new` as niri's config, back the old one up and write `new`
/// atomically. Returns the backup path.
fn replace_niri_config(paths: &Paths, old: &str, new: &str) -> Result<PathBuf, Error> {
    check_as_niri_config(paths, new).map_err(invalid_error)?;
    let bak = backup(paths, old)?;
    config::write_atomic(&paths.niri, new)?;
    Ok(bak)
}

/// A path that goes inside a KDL string as is: no `"` or `\` (refused rather than
/// escaped, so what niri runs is exactly the path shown).
fn kdl_safe(what: &str, p: &str) -> Result<(), Error> {
    if p.contains(['"', '\\']) || p.chars().any(char::is_control) {
        return Err(Error::Runtime(format!(
            "{what} {p:?} contains a quote, backslash or control character; niri keys not added — bind them by hand"
        )));
    }
    Ok(())
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
    kdl_safe("gowl's path", exe)?;
    kdl_safe("the binds file's path", &paths.include_path())?;
    if !includes_binds(&old) {
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
    if includes_binds(&old) {
        // Already included: (re)write the binds file only if niri still accepts the
        // whole config with it; otherwise put the previous one back.
        let before = std::fs::read_to_string(&paths.binds).ok();
        if write_binds(paths, exe)? {
            // (Without niri nothing reads the file: keep the new one.)
            if let Err(r @ Invalid::Refused(_)) = check_as_niri_config(paths, &old) {
                match &before {
                    Some(b) => config::write_atomic(&paths.binds, b)?,
                    None => {
                        let _ = std::fs::remove_file(&paths.binds);
                    }
                }
                return Err(invalid_error(r));
            }
        } else if let Err(Invalid::Refused(why)) = check_as_niri_config(paths, &old) {
            return Err(Error::Runtime(format!(
                "gowl's keys are included, but `niri validate` rejects niri's config as it is:\n{why}"
            )));
        }
        return Ok(Added::Present);
    }
    write_binds(paths, exe)?;
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

/// Take gowl's marker and include line out of niri's config (validated, backed
/// up) and delete the binds file — unless an include the user wrote by hand still
/// points at it (a missing non-optional include would break niri's config).
/// Idempotent.
pub fn remove(paths: &Paths) -> Result<Removed, Error> {
    let mut r = Removed::default();
    let mut still_included = false;
    if let Ok(old) = std::fs::read_to_string(&paths.niri) {
        let drop = our_lines(&old);
        let new = if drop.is_empty() {
            old.clone()
        } else {
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
            let bak = match check_as_niri_config(paths, &new) {
                // Nothing reads this config without niri: take the lines out unchecked.
                Ok(()) | Err(Invalid::NoNiri) => backup(paths, &old)?,
                Err(why) => {
                    return Err(Error::Runtime(format!(
                        "{}\nRemove the two gowl lines from {} by hand.",
                        invalid_error(why),
                        paths.niri.display()
                    )));
                }
            };
            config::write_atomic(&paths.niri, &new)?;
            r.include = Some(bak);
            new
        };
        still_included = includes_binds(&new);
    }
    if still_included {
        eprintln!(
            "gowl: {} still includes {} (a line gowl didn't write), so that file stays",
            paths.niri.display(),
            paths.binds.display()
        );
        return Ok(r);
    }
    match std::fs::remove_file(&paths.binds) {
        Ok(()) => r.binds_file = true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Io("remove niri binds file", e)),
    }
    Ok(r)
}

/// The gowl niri should spawn: this binary when it is installed (in a `bin`
/// directory); run from a build tree, the installed `~/.local/bin/gowl` if there is
/// one (so keys don't point into target/), else this binary with a warning.
pub fn keys_exe() -> Result<String, Error> {
    let exe = own_exe()?;
    let in_bin = Path::new(&exe)
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "bin");
    if in_bin {
        return Ok(exe);
    }
    if let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let installed = PathBuf::from(h).join(".local/bin/gowl");
        if installed.is_file() {
            eprintln!(
                "gowl: running from {exe}; the keys will run the installed {}",
                installed.display()
            );
            return Ok(installed.display().to_string());
        }
    }
    eprintln!(
        "gowl: warning: the keys will run {exe}, in a build directory (no installed gowl found; `make install` fixes that)"
    );
    Ok(exe)
}

/// This gowl's absolute path.
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
            match add(&paths, &keys_exe()?)? {
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

    fn keys_in(text: &str) -> Vec<(&'static str, usize)> {
        walk_kdl(text)
            .binds
            .iter()
            .filter_map(|(n, l)| gowl_key(n).map(|k| (k, *l)))
            .collect()
    }

    #[test]
    fn the_kdl_walk_finds_binds_however_they_are_written() {
        // On the same line as `binds {`, quoted, odd spellings and orders.
        assert_eq!(keys_in("binds { Mod+M { spawn \"a\"; } }"), [("Mod+M", 1)]);
        assert_eq!(
            keys_in("binds {\n    \"Shift+Super+m\" repeat=false { spawn \"a\"; }\n}\n"),
            [("Mod+Shift+M", 2)]
        );
        assert_eq!(
            keys_in("binds { Mod+T { spawn \"x\"; }; Win+M { spawn \"y\"; } }"),
            [("Mod+M", 1)]
        );
        assert_eq!(
            keys_in("binds {\n  r#\"Mod+M\"# { spawn \"z\"; }\n}"),
            [("Mod+M", 2)]
        );
        // Only directly inside `binds`: a `spawn "Mod+M"` argument or another
        // section's child doesn't count.
        assert!(keys_in("binds { Mod+T { spawn \"Mod+M\"; } }\nother { Mod+M; }").is_empty());
        // Comments and slashdash hide them.
        assert!(
            keys_in(
                "binds {\n // Mod+M { a; }\n /* Mod+M\n Mod+Shift+M */\n /-Mod+M { c; }\n /- Mod+Shift+M {\n d;\n }\n}"
            )
            .is_empty()
        );
        // Line continuation and a property before the block.
        assert_eq!(
            keys_in("binds {\n  Mod+Shift+M \\\n    cooldown-ms=100 { spawn \"a\"; }\n}"),
            [("Mod+Shift+M", 2)]
        );
    }

    #[test]
    fn includes_are_found_with_props_and_raw_strings() {
        let k = walk_kdl(
            "include \"a.kdl\"\ninclude optional=true \"~/b.kdl\"\n/-include \"c.kdl\"\n// include \"d.kdl\"\ninclude r\"e.kdl\"\nx { include \"nested.kdl\"; }\n",
        );
        assert_eq!(k.includes, ["a.kdl", "~/b.kdl", "e.kdl"]);
    }

    #[test]
    fn remove_takes_out_only_what_add_wrote() {
        let ours = format!("{MARK}\ninclude optional=true \"~/.config/gowl/niri-binds.kdl\"\n");
        let by_hand = "include \"~/.config/gowl/niri-binds.kdl\"\n";
        let text = format!("a {{}}\n{by_hand}\n{ours}");
        let idx = our_lines(&text);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(idx.len(), 2);
        assert_eq!(lines[idx[0]], MARK);
        assert!(lines[idx[1]].starts_with("include optional=true"));
        // A hand-written include alone (no marker above it): nothing to remove.
        assert!(our_lines(&format!("a {{}}\n{by_hand}")).is_empty());
        assert!(includes_binds(by_hand));
    }

    #[test]
    fn paths_with_quotes_or_backslashes_are_refused() {
        assert!(kdl_safe("p", "/home/u/.local/bin/gowl").is_ok());
        assert!(kdl_safe("p", "/home/u/my apps/gowl").is_ok());
        assert!(kdl_safe("p", "/home/u/a\"b/gowl").is_err());
        assert!(kdl_safe("p", "/home/u/a\\b/gowl").is_err());
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
