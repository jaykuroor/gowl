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

.PHONY: all build install uninstall test

all: build

build:
	cargo build --release

install: build
	PREFIX="$(PREFIX)" AUTOSTART="$(AUTOSTART)" NIRI_KEYS="$(NIRI_KEYS)" sh packaging/install.sh install

uninstall:
	PREFIX="$(PREFIX)" PURGE="$(PURGE)" sh packaging/install.sh uninstall

test:
	cargo test --release
