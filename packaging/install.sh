#!/bin/sh
# gowl installer — `make install` / `make uninstall` run this (D-072). Per user, no
# root: everything goes under $PREFIX (default ~/.local) and ~/.config. It prints
# one line per thing it looks at:  + added   ~ updated   - removed   = left as is
#   ! needs you (nothing was changed there). Running it twice changes nothing more.
#
#   sh packaging/install.sh install     [PREFIX=…] [AUTOSTART=1] [NIRI_KEYS=0]
#   sh packaging/install.sh uninstall   [PREFIX=…] [PURGE=1]
#
# install:   $PREFIX/bin/gowl; ~/.config/systemd/user/gowl.service (ExecStart = that
#            absolute path; enabled only with AUTOSTART=1 — or later by `gowl setup`);
#            ~/.config/gowl/config from config.example if there is none (never
#            overwritten); niri keys via `gowl keys add` (~/.config/gowl/niri-binds.kdl
#            + one include line in niri's config, validated and backed up; skipped
#            with NIRI_KEYS=0); $PREFIX/share/gowl/ (quickshell files, config.example).
# uninstall: stops a running sync, disables + removes the unit, `gowl keys remove`,
#            removes the binary and $PREFIX/share/gowl; keeps ~/.config/gowl/config
#            (and niri-config backups) unless PURGE=1, which also drops the device
#            cache in ~/.local/state/gowl.
# Never touched: the firewall, the network, VPNs, audio, your shell's config.
set -eu

action=${1:-}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
: "${HOME:?HOME is not set}"
PREFIX=${PREFIX:-$HOME/.local}
BINDIR=$PREFIX/bin
DATADIR=$PREFIX/share/gowl
CONFIG_HOME=${XDG_CONFIG_HOME:-$HOME/.config}
STATE_HOME=${XDG_STATE_HOME:-$HOME/.local/state}
UNIT_DIR=$CONFIG_HOME/systemd/user
UNIT=$UNIT_DIR/gowl.service
WANTS=$UNIT_DIR/graphical-session.target.wants/gowl.service
CONF_DIR=$CONFIG_HOME/gowl
CONF=$CONF_DIR/config
NIRI_CONF=${NIRI_CONFIG:-$CONFIG_HOME/niri/config.kdl}
BUILT=${GOWL_BIN:-$ROOT/target/release/gowl}
GOWL=$BINDIR/gowl

say() { printf '  %s\n' "$*"; }
have() { command -v "$1" >/dev/null 2>&1; }

if [ "$(id -u)" = 0 ] && [ -z "${GOWL_ALLOW_ROOT:-}" ]; then
    echo "gowl installs per user: run make install / make uninstall as yourself, not as root." >&2
    exit 1
fi

# install_file SRC DST MODE: copy through a temp file + rename (a running gowl keeps
# its old inode). Returns 0 if DST changed.
install_file() {
    if [ -f "$2" ] && cmp -s "$1" "$2"; then
        say "= $2"
        return 1
    fi
    mkdir -p "$(dirname "$2")"
    tmp="$2.gowl-new.$$"
    cp "$1" "$tmp"
    chmod "$3" "$tmp"
    if [ -e "$2" ]; then verb="~"; else verb="+"; fi
    mv -f "$tmp" "$2"
    say "$verb $2"
    return 0
}

userctl() {
    # systemctl --user ARGS…, reporting (not failing) when there is no user manager.
    if ! have systemctl; then
        say "! systemctl not found: run 'systemctl --user $*' yourself"
        return 1
    fi
    if ! systemctl --user "$@"; then
        say "! 'systemctl --user $*' failed (no user session manager?); run it again after logging in"
        return 1
    fi
}

do_install() {
    [ -x "$BUILT" ] || { echo "no built binary at $BUILT — run 'cargo build --release' (make install does)" >&2; exit 1; }
    echo "Installing gowl (PREFIX=$PREFIX)"

    new_bin=0
    if install_file "$BUILT" "$GOWL" 755; then new_bin=1; fi

    tmpd=$(mktemp -d)
    trap 'rm -rf "$tmpd"' EXIT
    sed "s|%h/.local/bin/gowl|$GOWL|" "$ROOT/packaging/gowl.service" > "$tmpd/gowl.service"
    if install_file "$tmpd/gowl.service" "$UNIT" 644; then
        userctl daemon-reload || true
    fi
    if [ "${AUTOSTART:-0}" = 1 ]; then
        if [ -e "$WANTS" ]; then
            say "= gowl.service enabled (screen sync at login)"
        elif userctl enable gowl.service; then
            say "+ gowl.service enabled: screen sync starts at your next login"
        fi
    elif [ -e "$WANTS" ]; then
        say "= gowl.service enabled (screen sync at login)"
    else
        say "= gowl.service not enabled (AUTOSTART=1 or 'gowl setup' turns on screen sync at login)"
    fi
    if [ "$new_bin" = 1 ] && have systemctl && systemctl --user is-active --quiet gowl.service 2>/dev/null; then
        userctl restart gowl.service && say "~ gowl.service restarted on the new binary"
    fi

    if [ -e "$CONF" ]; then
        say "= $CONF (kept)"
    else
        mkdir -p "$CONF_DIR"
        cp "$ROOT/config.example" "$CONF"
        chmod 644 "$CONF"
        say "+ $CONF (every setting commented out; 'gowl setup' fills in your light)"
    fi

    for f in "$ROOT"/contrib/quickshell/*.qml; do
        install_file "$f" "$DATADIR/quickshell/$(basename "$f")" 644 || true
    done
    install_file "$ROOT/config.example" "$DATADIR/config.example" 644 || true

    if [ "${NIRI_KEYS:-1}" = 0 ]; then
        say "= niri keys skipped (NIRI_KEYS=0); 'gowl keys add' adds them later"
    elif [ ! -e "$NIRI_CONF" ]; then
        say "= no niri config at $NIRI_CONF: niri keys skipped ('gowl keys add' once it exists)"
    elif ! "$GOWL" keys add; then
        say "! niri keys not added (see above); niri's config is unchanged"
    fi

    case ":${PATH}:" in
        *":$BINDIR:"*) ;;
        *) say "! $BINDIR is not on your PATH: type '$GOWL', or add it to PATH in your shell's config" ;;
    esac
    cat <<EOF

Next: run 'gowl setup' — it finds your light, calibrates it, and offers screen sync at login.
quickshell: $DATADIR/quickshell/ has a status service and a tile; README "quickshell" says how
to use them (gowl never edits your shell). Undo everything: make uninstall (PURGE=1 also drops
your gowl config).
EOF
}

do_uninstall() {
    echo "Removing gowl (PREFIX=$PREFIX)"
    if [ -x "$GOWL" ]; then
        case $("$GOWL" state 2>/dev/null || true) in
            *'"running":true'*)
                if "$GOWL" stop >/dev/null 2>&1; then say "- stopped the running sync"; fi ;;
        esac
    fi

    if [ -e "$UNIT" ] || [ -L "$WANTS" ]; then
        if have systemctl; then
            systemctl --user disable --now gowl.service >/dev/null 2>&1 || true
        fi
        if [ -L "$WANTS" ] || [ -e "$WANTS" ]; then rm -f "$WANTS"; fi
        rm -f "$UNIT"
        say "- $UNIT (stopped and disabled)"
        if have systemctl; then systemctl --user daemon-reload >/dev/null 2>&1 || true; fi
    else
        say "= no gowl.service"
    fi

    if [ -x "$GOWL" ]; then
        if ! "$GOWL" keys remove; then
            say "! the gowl lines in $NIRI_CONF need removing by hand (see above)"
        fi
    elif [ -e "$NIRI_CONF" ] && grep -q 'gowl/niri-binds.kdl"' "$NIRI_CONF"; then
        say "! $NIRI_CONF still includes gowl's keys: remove the two gowl lines at its end by hand"
    fi
    if [ -e "$CONF_DIR/niri-binds.kdl" ]; then
        rm -f "$CONF_DIR/niri-binds.kdl"
        say "- $CONF_DIR/niri-binds.kdl"
    fi

    if [ -e "$GOWL" ]; then rm -f "$GOWL"; say "- $GOWL"; else say "= no $GOWL"; fi
    case $DATADIR in
        */share/gowl)
            if [ -d "$DATADIR" ]; then rm -rf "$DATADIR"; say "- $DATADIR/"; else say "= no $DATADIR/"; fi ;;
    esac

    if [ "${PURGE:-0}" = 1 ]; then
        if [ -d "$CONF_DIR" ]; then rm -rf "$CONF_DIR"; say "- $CONF_DIR/ (config, niri-config backups)"; fi
        if [ -d "$STATE_HOME/gowl" ]; then rm -rf "$STATE_HOME/gowl"; say "- $STATE_HOME/gowl/ (device cache)"; fi
    else
        if [ -e "$CONF" ]; then say "= $CONF kept (PURGE=1 removes it)"; fi
        if [ -d "$CONF_DIR/backups" ]; then say "= $CONF_DIR/backups/ kept (copies of niri's config from before each edit)"; fi
    fi
}

case $action in
    install) do_install ;;
    uninstall) do_uninstall ;;
    *) echo "usage: sh packaging/install.sh install|uninstall" >&2; exit 2 ;;
esac
