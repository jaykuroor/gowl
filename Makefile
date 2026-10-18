# gowl — per-user install (no root). See packaging/install.sh for exactly what each
# target changes; both print every change and can be run again safely.
#
#   make install                 build, install to ~/.local, keys, unit (not enabled)
#   make install AUTOSTART=1     … and start screen sync at every login
#   make install NIRI_KEYS=0     … without touching niri's config
#   make uninstall               remove everything but ~/.config/gowl/config
#   make uninstall PURGE=1       … that too (and the device cache)

PREFIX ?= $(HOME)/.local
AUTOSTART ?= 0
NIRI_KEYS ?= 1
PURGE ?= 0

# Per user only: refused as root *before* anything is built (a root `cargo build`
# would leave root-owned files in target/ and ~/.cargo).
NOT_ROOT = @if [ "$$(id -u)" = 0 ]; then echo "gowl installs per user: run make install / make uninstall as yourself, not with sudo or as root." >&2; exit 1; fi

.PHONY: all build install uninstall test

all: build

build:
	cargo build --release

install:
	$(NOT_ROOT)
	cargo build --release
	PREFIX="$(PREFIX)" AUTOSTART="$(AUTOSTART)" NIRI_KEYS="$(NIRI_KEYS)" sh packaging/install.sh install

uninstall:
	$(NOT_ROOT)
	PREFIX="$(PREFIX)" PURGE="$(PURGE)" sh packaging/install.sh uninstall

test:
	cargo test --release
