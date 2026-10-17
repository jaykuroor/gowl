# PROTOCOL — Govee LAN (UDP), byte-exact

Source of truth: `legacy/lumisync/connection.py`, `lumisync/utils/colors.py`,
`lumisync/sync/processing.py`. Golden bytes: `tests/vectors/protocol.json`.

Prerequisite on the device side: **"LAN Control" enabled** for that device in the Govee
Home phone app. Without it the device ignores everything below.

## Ports
| Purpose | Direction | Address |
|---|---|---|
| Scan request | host → `239.255.255.250:4001` (also `255.255.255.255:4001`) | multicast / broadcast |
| Scan + status replies | device → host **UDP 4002** | unicast to the host |
| Control commands | host → `DEVICE_IP:4003` | unicast |

Legacy keeps a manual override: a device record may carry `Device_Port`/`devicePort`
or a manual `port`; otherwise control goes to 4003.

Only `scan` and `status` need replies, so **only those commands bind 4002**
(`SO_REUSEADDR` + `SO_REUSEPORT`, timeout-bounded, then close). Sync modes and one-shot
control send from an ephemeral port and never read the network.

**Every gowl socket is bound to the LAN interface (`SO_BINDTODEVICE`) so VPNs can't
reroute it. See `docs/NETWORK.md`, which also covers the firewall (`public` zone), the
conntrack test, and the scoped rule to suggest. Never apply firewall changes.**

## Messages (JSON, UTF-8, one datagram each)
Wire JSON may be compact (no spaces). Legacy sends `json.dumps` output with spaces;
devices accept both. Build them with a fixed-template writer into a stack buffer.

```
scan      {"msg":{"cmd":"scan","data":{"account_topic":"reserve"}}}
power     {"msg":{"cmd":"turn","data":{"value":1}}}                       0|1
bright    {"msg":{"cmd":"brightness","data":{"value":42}}}                clamp 0..100
color     {"msg":{"cmd":"colorwc","data":{"color":{"r":255,"g":128,"b":0},"colorTemInKelvin":0}}}
temp      {"msg":{"cmd":"colorwc","data":{"color":{"r":R,"g":G,"b":B},"colorTemInKelvin":K}}}
          (R,G,B = kelvin_to_rgb(K), Tanner Helland fit — vectors in protocol.json)
status    {"msg":{"cmd":"devStatus","data":{}}}     fall back to "status" if no reply
razer     {"msg":{"cmd":"razer","data":{"pt":"<base64>"}}}
```

### Replies
Scan reply (fields beyond these may appear; ignore unknown keys):
```
{"msg":{"cmd":"scan","data":{"ip":"192.168.1.23","device":"1F:80:C5:32:32:36:72:4E","sku":"H619C",
  "bleVersionHard":"…","bleVersionSoft":"…","wifiVersionHard":"…","wifiVersionSoft":"…"}}}
```
`device` is the MAC/ID (8 bytes, colon-separated) — the stable identity. IPs change.

Status reply:
```
{"msg":{"cmd":"devStatus","data":{"onOff":1,"brightness":100,"color":{"r":255,"g":0,"b":0},"colorTemInKelvin":7200}}}
```
Parse with a tiny scanner (find `"key":` → read string or int). No JSON library.
Ignore datagrams whose source IP isn't the queried device (broadcast sockets see noise).

## Razer (segment streaming) binary frames
Carried base64-encoded in the `pt` field. Every frame ends with a 1-byte XOR of all
previous bytes.

### Mode switch
```
BB 00 01 B1 <on> <xor>
on : BB 00 01 B1 01 0A   → pt "uwABsQEK"
off: BB 00 01 B1 00 0B   → pt "uwABsQAL"
```
Must send ON before the first color frame of a stream, OFF at the end.

### Color frame
```
BB  LEN_HI LEN_LO  B0  01  N  (R G B) × N  XOR
LEN = 2 + 3N  (big-endian; counts the 01 flag byte, the N byte and the RGB bytes)
N   = 1..255 segments
```
Example, N=10 (from vectors):
`bb0020b0010a0011ff256cf24ac7e56f22d8947dcbb9d8bede33b1038ea428e9974d448ad5`
→ pt `uwAgsAEKABH/JWzySsflbyLYlH3Ludi+3jOxA46kKOmXTUSK1Q==`

Size: 7 + 3N bytes raw; base64 = 4·⌈(7+3N)/3⌉; total datagram for N=10 is ~90 bytes.
Precompute the JSON prefix/suffix once; per frame only rewrite RGB bytes, XOR, base64,
and the base64 region of the datagram buffer. (Note: the base64 output length depends
only on N, so the datagram length is fixed per device.)

The `01` byte after `B0` is what legacy always sends (believed to be a gradient/stretch
flag). Keep it `01`.

## Stream semantics (from legacy `processing.frame_needs_send`)
- Send a frame when any channel of any segment moved by **more than** `threshold` (3)
  since the last *sent* frame.
- Otherwise resend the last frame every `keepalive` (legacy 0.5 s), because the device
  drops out of razer mode if starved. The real timeout is unknown — Phase 4 measures it
  on the human's device and sets the longest safe keepalive (fewer packets).
- Device-side pacing: don't exceed ~40 frames/s per device (legacy `max_update_hz`).

## Segment count
Devices do not report it. Precedence: config `segments` → built-in SKU table → 10.
Built-in table (from legacy `sku_catalog.py`): H619A/B/C/D/E/Z = 10, H6199 = 10,
H6672 = 14. Unknown SKU → 10 and print a hint to run `gowl calibrate`.

## Discovery details
1. Per LAN interface (NETWORK.md selection): socket bound to the interface, bind 4002
   (reuse flags), send scan to `239.255.255.250:4001`, `255.255.255.255:4001` and the
   subnet-directed broadcast `:4001` (`SO_BROADCAST`, `IP_MULTICAST_IF`, TTL 1).
2. Collect replies until `--timeout` (default 1000 ms) using `poll`, dedupe by `device`.
3. `--sweep`: also unicast the scan to every host of each local IPv4 /24 (from
   `getifaddrs`, not the legacy 8.8.8.8 connect trick) on ports 4001 and 4003, paced at
   ≤ 1 datagram/ms. Only on explicit request — it's 500+ packets.
4. Write the cache atomically (write temp, `rename`).
