//! Govee LAN protocol (docs/PROTOCOL.md): message templates, razer frames, base64,
//! and a tiny tolerant reply scanner. No JSON library; nothing here allocates.

use crate::color::Rgb;

pub const SCAN_PORT: u16 = 4001;
pub const REPLY_PORT: u16 = 4002;
pub const CONTROL_PORT: u16 = 4003;
pub const MULTICAST: [u8; 4] = [239, 255, 255, 250];

/// Exactly the scan probe legacy sends.
pub const SCAN_MSG: &[u8] = br#"{"msg":{"cmd":"scan","data":{"account_topic":"reserve"}}}"#;

/// Largest segment count a B0 frame can carry (1-byte N).
pub const MAX_SEGMENTS: usize = 255;
/// Raw razer B0 frame length for `n` segments.
pub const fn frame_len(n: usize) -> usize {
    7 + 3 * n
}
pub const MAX_FRAME: usize = frame_len(MAX_SEGMENTS);
/// Base64 output length for `n` input bytes (with padding).
pub const fn b64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

pub const RAZER_PREFIX: &[u8] = br#"{"msg":{"cmd":"razer","data":{"pt":""#;
pub const RAZER_SUFFIX: &[u8] = br#""}}}"#;
/// Big enough for every command template, including a 255-segment razer frame.
pub const MSG_MAX: usize = RAZER_PREFIX.len() + b64_len(MAX_FRAME) + RAZER_SUFFIX.len();

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding. `dst` must hold `b64_len(src.len())` bytes.
/// Returns the number of bytes written.
#[inline]
pub fn b64_encode(src: &[u8], dst: &mut [u8]) -> usize {
    let out = &mut dst[..b64_len(src.len())];
    let (chunks, rem) = src.as_chunks::<3>();
    let mut o = 0;
    for c in chunks {
        let v = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        out[o] = B64[(v >> 18) as usize & 63];
        out[o + 1] = B64[(v >> 12) as usize & 63];
        out[o + 2] = B64[(v >> 6) as usize & 63];
        out[o + 3] = B64[v as usize & 63];
        o += 4;
    }
    match *rem {
        [a] => {
            let v = u32::from(a) << 16;
            out[o] = B64[(v >> 18) as usize & 63];
            out[o + 1] = B64[(v >> 12) as usize & 63];
            out[o + 2] = b'=';
            out[o + 3] = b'=';
            o += 4;
        }
        [a, b] => {
            let v = (u32::from(a) << 16) | (u32::from(b) << 8);
            out[o] = B64[(v >> 18) as usize & 63];
            out[o + 1] = B64[(v >> 12) as usize & 63];
            out[o + 2] = B64[(v >> 6) as usize & 63];
            out[o + 3] = b'=';
            o += 4;
        }
        _ => {}
    }
    o
}

#[inline]
fn xor(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |a, b| a ^ b)
}

/// `BB 00 01 B1 <on> <xor>` — enter/leave segment streaming mode.
pub fn razer_mode_frame(on: bool) -> [u8; 6] {
    let mut f = [0xBB, 0x00, 0x01, 0xB1, u8::from(on), 0];
    f[5] = xor(&f[..5]);
    f
}

/// `BB LEN_HI LEN_LO B0 01 N (R G B)×N XOR` into `out`; returns the frame length.
/// `colors.len()` must be 1..=255 and `out` at least `frame_len(colors.len())`.
#[inline]
pub fn razer_color_frame(colors: &[Rgb], out: &mut [u8]) -> usize {
    let n = colors.len();
    debug_assert!((1..=MAX_SEGMENTS).contains(&n));
    let len = 2 + 3 * n;
    let total = frame_len(n);
    let f = &mut out[..total];
    f[0] = 0xBB;
    f[1] = (len >> 8) as u8;
    f[2] = len as u8;
    f[3] = 0xB0;
    f[4] = 0x01;
    f[5] = n as u8;
    for (dst, c) in f[6..total - 1]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(colors)
    {
        dst.copy_from_slice(c);
    }
    f[total - 1] = xor(&f[..total - 1]);
    total
}

/// Fixed-capacity byte writer for command templates (stack buffer, no allocation).
/// Writes past the end are dropped; buffers are sized so that never happens.
pub struct Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Writer<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Writer { buf, len: 0 }
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        let end = (self.len + b.len()).min(self.buf.len());
        let n = end - self.len;
        self.buf[self.len..end].copy_from_slice(&b[..n]);
        self.len = end;
        self
    }

    /// Decimal integer, itoa-style.
    pub fn uint(&mut self, mut v: u32) -> &mut Self {
        let mut tmp = [0u8; 10];
        let mut i = tmp.len();
        loop {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.bytes(&tmp[i..])
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// `{"msg":{"cmd":"turn","data":{"value":0|1}}}`
pub fn turn(on: bool, out: &mut [u8]) -> usize {
    let mut w = Writer::new(out);
    w.bytes(br#"{"msg":{"cmd":"turn","data":{"value":"#)
        .uint(u32::from(on))
        .bytes(b"}}}");
    w.len()
}

/// `{"msg":{"cmd":"brightness","data":{"value":N}}}`, N clamped to 0..=100.
pub fn brightness(value: u32, out: &mut [u8]) -> usize {
    let mut w = Writer::new(out);
    w.bytes(br#"{"msg":{"cmd":"brightness","data":{"value":"#)
        .uint(value.min(100))
        .bytes(b"}}}");
    w.len()
}

/// `{"msg":{"cmd":"colorwc","data":{"color":{"r":R,"g":G,"b":B},"colorTemInKelvin":K}}}`
pub fn colorwc(c: Rgb, kelvin: u32, out: &mut [u8]) -> usize {
    let mut w = Writer::new(out);
    w.bytes(br#"{"msg":{"cmd":"colorwc","data":{"color":{"r":"#)
        .uint(u32::from(c[0]))
        .bytes(br#","g":"#)
        .uint(u32::from(c[1]))
        .bytes(br#","b":"#)
        .uint(u32::from(c[2]))
        .bytes(br#"},"colorTemInKelvin":"#)
        .uint(kelvin)
        .bytes(b"}}}");
    w.len()
}

/// Status query: `devStatus` (default) or the older `status`.
pub fn status_query(dev_status: bool, out: &mut [u8]) -> usize {
    let mut w = Writer::new(out);
    w.bytes(if dev_status {
        br#"{"msg":{"cmd":"devStatus","data":{}}}"#
    } else {
        br#"{"msg":{"cmd":"status","data":{}}}"#
    });
    w.len()
}

/// `{"msg":{"cmd":"razer","data":{"pt":"<base64 of raw>"}}}`
pub fn razer_msg(raw: &[u8], out: &mut [u8]) -> usize {
    let p = RAZER_PREFIX.len();
    out[..p].copy_from_slice(RAZER_PREFIX);
    let n = b64_encode(raw, &mut out[p..]);
    out[p + n..p + n + RAZER_SUFFIX.len()].copy_from_slice(RAZER_SUFFIX);
    p + n + RAZER_SUFFIX.len()
}

// ------------------------------------------------------------------ reply scanner

#[inline]
fn skip_ws(b: &[u8]) -> &[u8] {
    let n = b
        .iter()
        .take_while(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
        .count();
    &b[n..]
}

/// Slice starting at the value of the first `"key"` followed by `:` (whitespace
/// tolerant). Not a JSON parser: it does not track nesting.
pub fn find_value<'a>(buf: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let k = key.as_bytes();
    let mut i = 0;
    while i + k.len() + 2 <= buf.len() {
        if buf[i] == b'"' && buf[i + 1..].starts_with(k) && buf.get(i + 1 + k.len()) == Some(&b'"')
        {
            let rest = skip_ws(&buf[i + 2 + k.len()..]);
            if let Some(rest) = rest.strip_prefix(b":") {
                return Some(skip_ws(rest));
            }
        }
        i += 1;
    }
    None
}

/// String value of `"key"` (no unescaping — Govee values never contain escapes).
pub fn find_str<'a>(buf: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let v = find_value(buf, key)?.strip_prefix(b"\"")?;
    let end = v.iter().position(|&c| c == b'"')?;
    Some(&v[..end])
}

/// Integer value of `"key"`; also accepts a quoted integer.
pub fn find_int(buf: &[u8], key: &str) -> Option<i64> {
    let v = find_value(buf, key)?;
    let v = v.strip_prefix(b"\"").unwrap_or(v);
    let (neg, v) = match v.strip_prefix(b"-") {
        Some(rest) => (true, rest),
        None => (false, v),
    };
    let digits = v.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 18 {
        return None;
    }
    let n = v[..digits]
        .iter()
        .fold(0i64, |a, &c| a * 10 + i64::from(c - b'0'));
    Some(if neg { -n } else { n })
}

/// The `{...}` object value of `"key"` (brace-matched, string-aware).
pub fn find_object<'a>(buf: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let v = find_value(buf, key)?;
    if v.first() != Some(&b'{') {
        return None;
    }
    let (mut depth, mut in_str) = (0u32, false);
    for (i, &c) in v.iter().enumerate() {
        match c {
            b'"' => in_str = !in_str,
            b'{' if !in_str => depth += 1,
            b'}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(&v[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Parsed device status. Missing fields are `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub on: Option<bool>,
    pub brightness: Option<i64>,
    pub color: Option<Rgb>,
    pub kelvin: Option<i64>,
}

/// Parse a `devStatus`/`status` reply; `None` if it is some other message.
pub fn parse_status(buf: &[u8]) -> Option<Status> {
    let cmd = find_str(buf, "cmd")?;
    if cmd != b"devStatus" && cmd != b"status" {
        return None;
    }
    let data = find_object(buf, "data").unwrap_or(buf);
    let color = find_object(data, "color").and_then(|c| {
        let ch = |k| find_int(c, k).map(|v| v.clamp(0, 255) as u8);
        Some([ch("r")?, ch("g")?, ch("b")?])
    });
    Some(Status {
        on: find_int(data, "onOff").map(|v| v != 0),
        brightness: find_int(data, "brightness"),
        color,
        kelvin: find_int(data, "colorTemInKelvin"),
    })
}

/// Fields of a scan reply, borrowed from the datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanReply<'a> {
    pub ip: Option<&'a [u8]>,
    pub device: &'a [u8],
    pub sku: &'a [u8],
}

/// Parse a scan reply; `None` if it isn't one (or has no `device`).
pub fn parse_scan(buf: &[u8]) -> Option<ScanReply<'_>> {
    if find_str(buf, "cmd")? != b"scan" {
        return None;
    }
    let data = find_object(buf, "data")?;
    Some(ScanReply {
        ip: find_str(data, "ip"),
        device: find_str(data, "device").filter(|d| !d.is_empty())?,
        sku: find_str(data, "sku").unwrap_or(b""),
    })
}

// ------------------------------------------------------------------ white temperature

/// Tanner Helland fit, exact port of legacy `kelvin_to_rgb` (setup path, float OK).
pub fn kelvin_to_rgb(kelvin: u32) -> Rgb {
    let temp = f64::from(kelvin.clamp(1000, 40000)) / 100.0;
    let red = if temp <= 66.0 {
        255.0
    } else {
        329.698727446 * (temp - 60.0).powf(-0.1332047592)
    };
    let green = if temp <= 66.0 {
        99.4708025861 * temp.ln() - 161.1195681661
    } else {
        288.1221695283 * (temp - 60.0).powf(-0.0755148492)
    };
    let blue = if temp >= 66.0 {
        255.0
    } else if temp <= 19.0 {
        0.0
    } else {
        138.5177312231 * (temp - 10.0).ln() - 305.0447927307
    };
    // Python's round() is round-half-to-even.
    let ch = |v: f64| v.round_ties_even().clamp(0.0, 255.0) as u8;
    [ch(red), ch(green), ch(blue)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_rfc4648() {
        let mut out = [0u8; 16];
        for (src, want) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ] {
            let n = b64_encode(src, &mut out);
            assert_eq!(&out[..n], want.as_bytes());
        }
    }

    #[test]
    fn scanner_is_whitespace_tolerant() {
        let r = br#"{ "msg" : { "cmd" : "devStatus", "data" : { "onOff" : 1 , "brightness":42,
            "color" : { "r" : 255, "g":0 ,"b": 7 }, "colorTemInKelvin": 0 } } }"#;
        let s = parse_status(r).unwrap();
        assert_eq!(s.on, Some(true));
        assert_eq!(s.brightness, Some(42));
        assert_eq!(s.color, Some([255, 0, 7]));
        assert_eq!(s.kelvin, Some(0));
        assert_eq!(find_int(b"{\"x\":\"-12\"}", "x"), Some(-12));
        assert!(parse_status(br#"{"msg":{"cmd":"scan","data":{}}}"#).is_none());
    }

    #[test]
    fn color_key_does_not_match_color_temp() {
        let r = br#"{"msg":{"cmd":"status","data":{"colorTemInKelvin":7200,"color":{"r":1,"g":2,"b":3}}}}"#;
        let s = parse_status(r).unwrap();
        assert_eq!(s.color, Some([1, 2, 3]));
        assert_eq!(s.kelvin, Some(7200));
        assert_eq!(s.on, None);
    }

    #[test]
    fn scan_reply() {
        let r = br#"{"msg":{"cmd":"scan","data":{"ip":"192.168.1.23","device":"1F:80:C5:32:32:36:72:4E","sku":"H619C","bleVersionHard":"3.01.01"}}}"#;
        let s = parse_scan(r).unwrap();
        assert_eq!(s.ip, Some(&b"192.168.1.23"[..]));
        assert_eq!(s.device, b"1F:80:C5:32:32:36:72:4E");
        assert_eq!(s.sku, b"H619C");
        assert!(parse_scan(br#"{"msg":{"cmd":"devStatus","data":{}}}"#).is_none());
    }

    #[test]
    fn razer_datagram_is_92_bytes_for_10_segments() {
        let mut raw = [0u8; MAX_FRAME];
        let n = razer_color_frame(&[[1, 2, 3]; 10], &mut raw);
        let mut msg = [0u8; MSG_MAX];
        assert_eq!(razer_msg(&raw[..n], &mut msg), 92);
    }

    #[test]
    fn brightness_clamps() {
        let mut b = [0u8; 64];
        let n = brightness(250, &mut b);
        assert_eq!(
            &b[..n],
            br#"{"msg":{"cmd":"brightness","data":{"value":100}}}"#
        );
    }
}
