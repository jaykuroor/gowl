# LEGACY_MAP — where each legacy piece goes

Paths relative to `legacy/lumisync/`. Read only the files marked **port**.

| Legacy | Fate | New home / note |
|---|---|---|
| `connection.py` | **port** | `proto.rs` + `net.rs` (scan, sweep, send, razer on/off, colorwc, brightness, turn, status) |
| `utils/colors.py` (`convert_colors`, `kelvin_to_rgb`, `resample_colors_to_count`, `normalize_rgb`) | **port** | `proto.rs`, `color.rs` — `get_color` (CSS names) dropped |
| `led_mapping.py` (`generate_screen_mapping`, `_largest_remainder_counts`, `normalize_rect`) | **port** | `color.rs` — settings/legacy-mapping parsers dropped |
| `sync/processing.py` | **port** | `color.rs` + `capture/sample.rs` (fixed-point) |
| `sync/monitor.py` (loop only) | **port** | `runtime.rs` — capture code replaced by `capture/` |
| `sync/music.py` (loop only) | **port** | `audio/mod.rs` |
| `sync/audio.py` | **port** (subset, see AUDIO.md) | `audio/{fft,bands,render}.rs` |
| `sku_catalog.py` (`BUILTIN` table only) | **port** | `devices.rs` — Govee Desktop cache import dropped (Windows-only) |
| `config/options.py` | **port values** | defaults in `config.rs` |
| `drivers/base.py`, `drivers/govee_lan.py` | read for semantics | no trait hierarchy; Govee LAN is the only transport |
| `devices.py` | read for semantics | `devices.rs` (cache format is new) |
| `groups.py` | maybe | multi-device fan-out covers the use case |
| `sync/system_volume.py` | drop | Windows fader cancel; AutoGain covers Linux |
| `sync/artwork.py` | drop | album-art palette out of scope |
| `drivers/cloud.py`, `drivers/tuya_lan.py`, `drivers/idotmatrix_ble.py`, `drivers/pool.py`, `drivers/registry.py` | drop | out of scope |
| `accounts/*` | drop | cloud accounts out of scope |
| `gui/*` | drop | no GUI |
| `updates.py`, `utils/logging.py`, `utils/file_operations.py` | drop | |
| `lumisync.py` (CLI) | read for UX only | `main.rs` |
| `tests/test_lan_helpers.py`, `test_processing.py`, `test_segment_counts.py`, `test_audio.py`, `test_transition.py`, `test_color.py`, `test_multicolor.py` | read | edge cases worth mirroring as Rust tests |
