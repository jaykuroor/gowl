//! `zwlr_screencopy_manager_v1` backend (CAPTURE_NIRI.md "frame cycle"):
//! `capture_output(overlay_cursor = 0)` → `buffer`… `buffer_done` → reuse or
//! reallocate the SHM buffer → `copy_with_damage` → `flags`, `damage`…, `ready` |
//! `failed` → `destroy`. One frame in flight; the runtime decides when to ask.
//! Also tracks outputs (name, mode, scale, transform) and hot-plug.

use std::os::fd::RawFd;

use super::shm::ShmBuffer;
use super::wire::{Arg, Conn, DISPLAY, Event};
use super::{Capture, DamageRect, Format, Frame, MAX_DAMAGE, Status, format_name};
use crate::error::Error;

// Opcodes (wayland.xml, wlr-screencopy-unstable-v1.xml).
const DISPLAY_GET_REGISTRY: u16 = 1;
const REGISTRY_BIND: u16 = 0;
const EV_REGISTRY_GLOBAL: u16 = 0;
const EV_REGISTRY_GLOBAL_REMOVE: u16 = 1;
const EV_SHM_FORMAT: u16 = 0;
const OUTPUT_RELEASE: u16 = 0;
const EV_OUTPUT_GEOMETRY: u16 = 0;
const EV_OUTPUT_MODE: u16 = 1;
const EV_OUTPUT_DONE: u16 = 2;
const EV_OUTPUT_SCALE: u16 = 3;
const EV_OUTPUT_NAME: u16 = 4;
const EV_OUTPUT_DESCRIPTION: u16 = 5;
const MGR_CAPTURE_OUTPUT: u16 = 0;
const FRAME_COPY: u16 = 0;
const FRAME_DESTROY: u16 = 1;
const FRAME_COPY_WITH_DAMAGE: u16 = 2;
const EV_FRAME_BUFFER: u16 = 0;
const EV_FRAME_FLAGS: u16 = 1;
const EV_FRAME_READY: u16 = 2;
const EV_FRAME_FAILED: u16 = 3;
const EV_FRAME_DAMAGE: u16 = 4;
const EV_FRAME_LINUX_DMABUF: u16 = 5;
const EV_FRAME_BUFFER_DONE: u16 = 6;
const FLAG_Y_INVERT: u32 = 1;
const MODE_CURRENT: u32 = 1;

const SCREENCOPY: &[u8] = b"zwlr_screencopy_manager_v1";
const IMAGECOPY: &[u8] = b"ext_image_copy_capture_manager_v1";

/// One `wl_output`.
#[derive(Clone, Debug, Default)]
pub struct Output {
    /// Registry name.
    pub global: u32,
    /// Object id.
    pub id: u32,
    pub version: u32,
    pub name: String,
    pub description: String,
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
    pub scale: i32,
    pub transform: u32,
    /// Got the first `done` (all properties known).
    pub ready: bool,
}

/// Registry snapshot plus the bound core globals.
pub struct Globals {
    pub conn: Conn,
    registry: u32,
    /// Bound `wl_shm` id.
    pub shm: Option<u32>,
    pub shm_formats: Vec<u32>,
    /// `(registry name, version)` of the screencopy manager.
    pub screencopy: Option<(u32, u32)>,
    pub imagecopy: bool,
    pub outputs: Vec<Output>,
}

/// What a registry/output event changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    None,
    /// `done` on this output id.
    OutputDone(u32),
    /// Output with this object id was removed (already dropped from the list).
    OutputRemoved(u32),
}

impl Globals {
    /// Connect, list globals, bind `wl_shm` and every `wl_output`, and wait until
    /// their properties have arrived (two roundtrips; setup only).
    pub fn connect() -> Result<Globals, Error> {
        Globals::with_conn(Conn::connect()?)
    }

    pub fn with_conn(mut conn: Conn) -> Result<Globals, Error> {
        let registry = conn.new_id();
        conn.request(DISPLAY, DISPLAY_GET_REGISTRY, &[Arg::Obj(registry)])?;
        let mut g = Globals {
            conn,
            registry,
            shm: None,
            shm_formats: Vec::new(),
            screencopy: None,
            imagecopy: false,
            outputs: Vec::new(),
        };
        g.roundtrip()?; // globals
        g.roundtrip()?; // output properties, shm formats
        Ok(g)
    }

    /// Setup only: block until the compositor has answered everything sent so far.
    fn roundtrip(&mut self) -> Result<(), Error> {
        let cb = self.conn.sync()?;
        loop {
            while let Some(ev) = self.conn.next_event()? {
                if ev.obj == cb {
                    return Ok(());
                }
                self.handle(ev)?;
            }
            if !self.conn.fill()? {
                self.conn.wait_readable()?;
            }
        }
    }

    fn bind(&mut self, name: u32, iface: &[u8], version: u32) -> Result<u32, Error> {
        let id = self.conn.new_id();
        self.conn.request(
            self.registry,
            REGISTRY_BIND,
            &[
                Arg::Uint(name),
                Arg::Str(iface),
                Arg::Uint(version),
                Arg::Obj(id),
            ],
        )?;
        Ok(id)
    }

    /// Handle a registry / shm / output event. Unknown objects are ignored.
    fn handle(&mut self, ev: Event) -> Result<Change, Error> {
        if ev.obj == self.registry {
            const SHM: u8 = 1;
            const OUTPUT: u8 = 2;
            const OTHER: u8 = 0;
            let mut a = self.conn.args(&ev);
            match ev.op {
                EV_REGISTRY_GLOBAL => {
                    let name = a.uint();
                    let kind = match a.str() {
                        b"wl_shm" => SHM,
                        b"wl_output" => OUTPUT,
                        SCREENCOPY => {
                            let v = a.uint();
                            self.screencopy = Some((name, v));
                            OTHER
                        }
                        IMAGECOPY => {
                            self.imagecopy = true;
                            OTHER
                        }
                        _ => OTHER,
                    };
                    let version = a.uint();
                    match kind {
                        SHM if self.shm.is_none() => {
                            self.shm = Some(self.bind(name, b"wl_shm", 1)?);
                        }
                        OUTPUT => {
                            let v = version.min(4);
                            let id = self.bind(name, b"wl_output", v)?;
                            self.outputs.push(Output {
                                global: name,
                                id,
                                version: v,
                                scale: 1,
                                ..Output::default()
                            });
                        }
                        _ => {}
                    }
                }
                EV_REGISTRY_GLOBAL_REMOVE => {
                    let name = a.uint();
                    if let Some(i) = self.outputs.iter().position(|o| o.global == name) {
                        let o = self.outputs.remove(i);
                        if o.version >= 3 {
                            self.conn.request(o.id, OUTPUT_RELEASE, &[])?;
                        }
                        return Ok(Change::OutputRemoved(o.id));
                    }
                }
                _ => {}
            }
            return Ok(Change::None);
        }
        if Some(ev.obj) == self.shm {
            if ev.op == EV_SHM_FORMAT {
                let f = self.conn.args(&ev).uint();
                if !self.shm_formats.contains(&f) {
                    self.shm_formats.push(f);
                }
            }
            return Ok(Change::None);
        }
        let Some(i) = self.outputs.iter().position(|o| o.id == ev.obj) else {
            return Ok(Change::None);
        };
        let mut a = self.conn.args(&ev);
        let o = &mut self.outputs[i];
        match ev.op {
            EV_OUTPUT_GEOMETRY => {
                let _ = (a.int(), a.int(), a.int(), a.int(), a.int()); // x y pw ph subpixel
                let _ = (a.str(), a.str()); // make, model
                o.transform = (a.int() as u32) & 7;
            }
            EV_OUTPUT_MODE => {
                let flags = a.uint();
                if flags & MODE_CURRENT != 0 {
                    o.width = a.int();
                    o.height = a.int();
                    o.refresh_mhz = a.int();
                }
            }
            EV_OUTPUT_SCALE => o.scale = a.int(),
            EV_OUTPUT_NAME => o.name = String::from_utf8_lossy(a.str()).into_owned(),
            EV_OUTPUT_DESCRIPTION => o.description = String::from_utf8_lossy(a.str()).into_owned(),
            EV_OUTPUT_DONE => {
                o.ready = true;
                return Ok(Change::OutputDone(o.id));
            }
            _ => {}
        }
        Ok(Change::None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    /// `capture_output` sent, collecting buffer offers.
    Negotiating,
    /// `copy_with_damage` sent, waiting for `ready`/`failed`.
    Copying,
}

#[derive(Clone, Copy, Debug, Default)]
struct Offer {
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
}

const MAX_OFFERS: usize = 16;

/// The screencopy backend.
pub struct Screencopy {
    g: Globals,
    mgr: u32,
    mgr_version: u32,
    /// Wanted output name (`--output`); `None` = first output announced.
    want: Option<String>,
    /// Captured `wl_output` id.
    sel: Option<u32>,
    phase: Phase,
    frame: u32,
    offers: [Offer; MAX_OFFERS],
    noffers: usize,
    buf: Option<ShmBuffer>,
    /// The buffer was (re)allocated: the next frame counts as fully damaged.
    fresh: bool,
    y_invert: bool,
    damage: [DamageRect; MAX_DAMAGE],
    ndamage: usize,
    damage_all: bool,
    /// Properties of the last ready frame.
    have_frame: bool,
    frame_transform: u32,
    frame_full: bool,
}

impl Screencopy {
    pub fn new(mut g: Globals, want: Option<&str>) -> Result<Screencopy, Error> {
        let (name, version) = g.screencopy.ok_or_else(|| {
            Error::Capture("the compositor does not offer zwlr_screencopy_manager_v1".into())
        })?;
        if g.shm.is_none() {
            return Err(Error::Capture(
                "the compositor does not offer wl_shm".into(),
            ));
        }
        let v = version.min(3);
        let mgr = g.bind(name, SCREENCOPY, v)?;
        let sel = match want {
            Some(w) => match g.outputs.iter().find(|o| o.name == w) {
                Some(o) => o.id,
                None => {
                    let names: Vec<&str> = g.outputs.iter().map(|o| o.name.as_str()).collect();
                    return Err(Error::Capture(format!(
                        "no output named {w:?}; available: {} (see `gowl outputs`)",
                        if names.is_empty() {
                            "none".into()
                        } else {
                            names.join(", ")
                        }
                    )));
                }
            },
            None => {
                g.outputs
                    .first()
                    .ok_or_else(|| Error::Capture("the compositor has no outputs".into()))?
                    .id
            }
        };
        let s = Screencopy {
            g,
            mgr,
            mgr_version: v,
            want: want.map(str::to_string),
            sel: Some(sel),
            phase: Phase::Idle,
            frame: 0,
            offers: [Offer::default(); MAX_OFFERS],
            noffers: 0,
            buf: None,
            fresh: true,
            y_invert: false,
            damage: [DamageRect::default(); MAX_DAMAGE],
            ndamage: 0,
            damage_all: false,
            have_frame: false,
            frame_transform: 0,
            frame_full: true,
        };
        if let Some(o) = s.selected() {
            debug!(
                "capturing {} {}x{} transform {} via screencopy v{v}",
                o.name,
                o.width,
                o.height,
                super::transform_name(o.transform)
            );
        }
        Ok(s)
    }

    fn selected(&self) -> Option<&Output> {
        let id = self.sel?;
        self.g.outputs.iter().find(|o| o.id == id)
    }

    fn destroy_frame(&mut self) -> Result<(), Error> {
        if self.phase != Phase::Idle {
            self.g.conn.request(self.frame, FRAME_DESTROY, &[])?;
            self.phase = Phase::Idle;
        }
        Ok(())
    }

    /// Pick the best offered SHM format, (re)allocate if needed, start the copy.
    fn start_copy(&mut self) -> Result<(), Error> {
        let best = self.offers[..self.noffers]
            .iter()
            .filter_map(|o| Format::rank(o.format).map(|r| (r, *o)))
            .min_by_key(|(r, _)| *r)
            .map(|(_, o)| o);
        let Some(o) = best else {
            let list: Vec<String> = self.offers[..self.noffers]
                .iter()
                .map(|o| format_name(o.format))
                .collect();
            return Err(Error::Capture(format!(
                "no supported SHM pixel format offered (got: {}); gowl reads XRGB/XBGR 8888 and 2101010",
                if list.is_empty() {
                    "none".into()
                } else {
                    list.join(" ")
                }
            )));
        };
        let reuse = self
            .buf
            .as_ref()
            .is_some_and(|b| b.matches(o.format, o.width, o.height, o.stride));
        if !reuse {
            if let Some(old) = self.buf.take() {
                old.destroy(&mut self.g.conn)?;
            }
            let shm = self.g.shm.unwrap_or(0);
            self.buf = Some(ShmBuffer::new(
                &mut self.g.conn,
                shm,
                o.format,
                o.width,
                o.height,
                o.stride,
            )?);
            self.fresh = true;
            debug!(
                "shm buffer {}x{} stride {} format {} ({} offers)",
                o.width,
                o.height,
                o.stride,
                format_name(o.format),
                self.noffers
            );
        }
        let buffer = self.buf.as_ref().map_or(0, |b| b.buffer);
        let op = if self.mgr_version >= 2 {
            FRAME_COPY_WITH_DAMAGE
        } else {
            FRAME_COPY
        };
        self.g.conn.request(self.frame, op, &[Arg::Obj(buffer)])?;
        self.phase = Phase::Copying;
        Ok(())
    }

    fn handle_frame(&mut self, ev: Event, st: &mut Status) -> Result<(), Error> {
        let mut a = self.g.conn.args(&ev);
        match ev.op {
            EV_FRAME_BUFFER => {
                let o = Offer {
                    format: a.uint(),
                    width: a.uint(),
                    height: a.uint(),
                    stride: a.uint(),
                };
                if self.noffers < MAX_OFFERS {
                    self.offers[self.noffers] = o;
                    self.noffers += 1;
                }
                // v1/v2 send no buffer_done: one buffer event is the whole offer.
                if self.mgr_version < 3 && self.phase == Phase::Negotiating {
                    self.start_copy()?;
                }
            }
            EV_FRAME_LINUX_DMABUF => {}
            EV_FRAME_BUFFER_DONE => {
                if self.phase == Phase::Negotiating {
                    self.start_copy()?;
                }
            }
            EV_FRAME_FLAGS => self.y_invert = a.uint() & FLAG_Y_INVERT != 0,
            EV_FRAME_DAMAGE => {
                let r = DamageRect {
                    x: a.uint(),
                    y: a.uint(),
                    w: a.uint(),
                    h: a.uint(),
                };
                if self.ndamage < MAX_DAMAGE {
                    self.damage[self.ndamage] = r;
                    self.ndamage += 1;
                } else {
                    self.damage_all = true;
                }
            }
            EV_FRAME_READY => {
                // No damage events (copy without damage, v1) = everything changed.
                self.frame_full = self.fresh || self.damage_all || self.ndamage == 0;
                self.fresh = false;
                self.frame_transform = self.selected().map_or(0, |o| o.transform);
                self.have_frame = true;
                self.destroy_frame()?;
                st.ready = true;
            }
            EV_FRAME_FAILED => {
                self.destroy_frame()?;
                self.have_frame = false;
                st.failed = true;
            }
            _ => {}
        }
        Ok(())
    }
}

impl Capture for Screencopy {
    fn fd(&self) -> RawFd {
        self.g.conn.fd()
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.g.conn.flush()
    }

    fn dispatch(&mut self) -> Result<Status, Error> {
        let mut st = Status::default();
        loop {
            while let Some(ev) = self.g.conn.next_event()? {
                if self.phase != Phase::Idle && ev.obj == self.frame {
                    self.handle_frame(ev, &mut st)?;
                    continue;
                }
                match self.g.handle(ev)? {
                    Change::None => {}
                    Change::OutputRemoved(id) => {
                        if self.sel == Some(id) {
                            // The frame (if any) can no longer complete.
                            self.destroy_frame()?;
                            self.have_frame = false;
                            self.sel = None;
                            st.lost = true;
                        }
                    }
                    Change::OutputDone(id) => {
                        if self.sel.is_none() {
                            let o = self.g.outputs.iter().find(|o| o.id == id);
                            let wanted = match (&self.want, o) {
                                (Some(w), Some(o)) => &o.name == w,
                                (None, Some(_)) => true,
                                _ => false,
                            };
                            if wanted {
                                self.sel = Some(id);
                                self.fresh = true;
                                st.found = true;
                            }
                        } else if self.sel == Some(id) {
                            // Mode/transform may have changed: next frame is full.
                            self.fresh = true;
                        }
                    }
                }
            }
            if !self.g.conn.fill()? {
                break;
            }
        }
        Ok(st)
    }

    fn request_next(&mut self) -> Result<bool, Error> {
        let Some(out) = self.sel else {
            return Ok(false);
        };
        if self.phase != Phase::Idle {
            return Ok(false);
        }
        self.frame = self.g.conn.new_id();
        self.g.conn.request(
            self.mgr,
            MGR_CAPTURE_OUTPUT,
            &[Arg::Obj(self.frame), Arg::Int(0), Arg::Obj(out)],
        )?;
        self.phase = Phase::Negotiating;
        self.noffers = 0;
        self.ndamage = 0;
        self.damage_all = false;
        self.y_invert = false;
        self.have_frame = false;
        Ok(true)
    }

    fn cancel(&mut self) -> Result<(), Error> {
        self.destroy_frame()
    }

    fn frame(&self) -> Option<Frame<'_>> {
        if !self.have_frame {
            return None;
        }
        let b = self.buf.as_ref()?;
        Some(Frame {
            data: b.data(),
            width: b.width,
            height: b.height,
            stride: b.stride,
            format: Format::from_wl(b.format)?,
            transform: self.frame_transform,
            y_invert: self.y_invert,
            damage: &self.damage[..self.ndamage],
            full_damage: self.frame_full,
        })
    }

    fn output_name(&self) -> Option<&str> {
        self.selected().map(|o| o.name.as_str())
    }
}
