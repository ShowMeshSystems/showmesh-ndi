# showmesh-ndi

A headless Linux receiver that plays [NDI®](https://ndi.video) video sources fullscreen on physical HDMI and DisplayPort outputs. It is built for projection mapping: the mapping happens upstream (for example in Resolume Arena), and this receiver reproduces the resulting picture without changing its geometry.

It runs as a systemd service with no desktop, drives each display directly through DRM/KMS, plays one independent NDI source per output, reconnects on its own when a source or display goes away, and exposes a small JSON REST API for status and control. It is part of the [ShowMesh](https://github.com/ShowMeshSystems/showmesh) ecosystem but runs on its own: it keeps playing its saved configuration with or without ShowMesh.

This is an experimental first release.

## What has been verified

Verified on a Debian 13 x86_64 virtual machine (VMware `vmwgfx` display driver, GStreamer 1.26), with a local NDI test sender:

- Output enumeration and mode setting through KMS, as a non-root systemd service.
- NDI discovery (with Avahi running) and reception by name and by address, 1080p60, displayed through `kmssink`.
- Source loss: the output goes black after the configured timeout and plays again within about three seconds of the sender returning.
- REST API changes saved to the configuration file.

Not yet verified: real GPUs (Intel N100, Raspberry Pi 5), a real HDMI display or projector, Resolume Arena as the sender, display hotplug, more than one physical output at once, and frame pacing on real hardware. The VM measured 55 to 58 fps from a 60 fps test sender with no dropped frames, which says little about a real GPU.

## Requirements

- Debian 12 or 13, Ubuntu 22.04 or later, or Raspberry Pi OS (64-bit). x86_64 or ARM64.
- GStreamer 1.22 or later with the base and bad plugin sets (`kmssink` is in bad).
- `avahi-daemon`. NDI on Linux finds sources through Avahi; without it, sources must be set by address.
- The NDI 6 runtime, `libndi.so.6`, installed separately (see below).
- No desktop session on the display card for `kms` mode. A running desktop holds the display; see Troubleshooting.

## Install

```sh
sudo apt install gstreamer1.0-plugins-base gstreamer1.0-plugins-bad avahi-daemon \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev build-essential pkg-config curl
curl -sSf https://sh.rustup.rs | sh        # Rust 1.92 or later
git clone https://github.com/ShowMeshSystems/showmesh-ndi && cd showmesh-ndi
cargo build --release
sudo packaging/install.sh
```

### NDI runtime

This project never ships the NDI runtime. Download the NDI SDK for Linux from [ndi.video](https://ndi.video/for-developers/ndi-sdk/), accept its license, and copy the library for your architecture:

```sh
# x86_64: lib/x86_64-linux-gnu    Raspberry Pi 5: lib/aarch64-rpi4-linux-gnueabi
sudo cp "NDI SDK for Linux/lib/x86_64-linux-gnu/libndi.so.6"* /usr/local/lib/
sudo ldconfig
```

The service looks in `/usr/local/lib` (set by `NDI_RUNTIME_DIR_V6` in the unit) and then the system library path.

### Configure and start

Edit `/etc/showmesh-ndi/config.toml` (see [packaging/config.example.toml](packaging/config.example.toml)), then:

```sh
sudo systemctl restart showmesh-ndi
journalctl -u showmesh-ndi -f
```

The service starts on boot. Every connected output shows black until it has a source.

## Quick test from a shell

Stop the service first so it releases the display.

```sh
showmesh-ndi outputs                                   # physical outputs and modes
showmesh-ndi sources                                   # NDI sources on the network
showmesh-ndi play --output HDMI-A-1 --pattern smpte    # check the display and its geometry
showmesh-ndi play --output HDMI-A-1 --ndi-name "ARENA-MAC (Arena - Projector 1)" --mode 1920x1080@60
```

`examples/ndi_test_sender.rs` sends a moving test pattern as an NDI source, for testing without Resolume: `cargo run --release --example ndi_test_sender -- "Bench Sender" 1920x1080@60`.

## Web control page

Open `http://<receiver>:8790/` in a browser on the show network. Each output shows its state, frame rate, and dropped frames, refreshed every 2 seconds. Pick a discovered NDI source, an address, or a test pattern, and a display mode, then press **Take**. **Clear** removes the source and the output goes black. **Start**, **Stop** and **Restart** act on that output only. Nothing changes on the output until you press a button.

## REST API

The API listens on port 8790. The full description is [api/openapi.yaml](api/openapi.yaml), also served at `/api/v1/openapi.yaml`.

| Method and path | Purpose |
| --- | --- |
| `GET /api/v1/device`, `PATCH /api/v1/device` | Name, version, display mode, NDI runtime state |
| `GET /api/v1/health` | `ok` or `degraded`, with state, fps and frame counts per output |
| `GET /api/v1/sources` | NDI sources visible now |
| `GET /api/v1/outputs`, `GET /api/v1/outputs/{name}` | Outputs, supported modes, assignment, receiver state |
| `PATCH /api/v1/outputs/{name}` | Change `source`, `mode` or `enabled` |
| `PUT`, `DELETE /api/v1/outputs/{name}/source` | Assign or remove a source |
| `POST /api/v1/outputs/{name}/start`, `stop`, `restart` | Control one receiver |
| `GET /api/v1/config` | The saved configuration |

```sh
curl -X PUT localhost:8790/api/v1/outputs/HDMI-A-1/source \
  -H 'content-type: application/json' \
  -d '{"type":"ndi","name":"ARENA-MAC (Arena - Projector 1)"}'
curl localhost:8790/api/v1/health
```

Changes are saved immediately. Start and stop are saved too, so a stopped output stays stopped after a reboot. The API has no authentication and is meant for a dedicated show network.

## How it works

One process opens the display card once and holds it as DRM master. For each output it sets the display mode and shows a black frame, then runs an independent GStreamer pipeline:

```
ndisrc ! ndisrcdemux ! queue (2 frames, drops oldest) ! videoconvert ! kmssink
```

- `ndisrc` asks the NDI library for BGRX frames, which display planes scan out directly, so `videoconvert` passes frames through without work.
- Every `kmssink` shares the one DRM file descriptor, which is how several outputs work without a desktop.
- If the source and display sizes match, the frame is shown pixel for pixel. If they differ, the display hardware scales it with the aspect ratio kept.
- Frames are shown as they arrive and the display's refresh paces them. There is no added buffering beyond the two-frame queue.
- Each output runs in its own thread with its own pipeline. A failure on one output restarts only that output, with a delay that grows to 5 seconds.
- Audio from the NDI source is discarded.

The NDI elements come from the [`gst-plugin-ndi`](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs) crate, compiled into the binary. That plugin loads `libndi.so.6` at run time, so the binary does not link against it.

## Troubleshooting

- **"Another program, usually a desktop session, controls the display."** Stop the display manager (`sudo systemctl disable --now gdm`, `lightdm`, or `sddm`), or set `display.sink = "wayland"` to play one output in a fullscreen window on the desktop.
- **No sources listed.** Check `systemctl status avahi-daemon`, and that UDP 5353 (mDNS) and TCP 5960 and up are not blocked between sender and receiver. Across subnets, set the source by `address` (`host:port`; the port is usually 5961).
- **Half the expected frame rate.** Some drivers need `display.kms_wait_vsync = true` and others are slowed down by it. The default (`false`) suits current Intel, AMD and Raspberry Pi drivers.
- **"The NDI runtime is not installed."** See [NDI runtime](#ndi-runtime).

## ShowMesh integration

The receiver does not depend on the ShowMesh coordinator. [docs/SHOWMESH-INTEGRATION.md](docs/SHOWMESH-INTEGRATION.md) describes how ShowMesh could manage receivers through this API.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

The tests run real GStreamer pipelines on a fake display and need the GStreamer base plugins, but no NDI runtime, GPU or display. CI runs them on x86_64 and ARM64.

## License

Apache-2.0. The statically linked `gst-plugin-ndi` crate is MPL-2.0. NDI® is a registered trademark of Vizrt NDI AB; the NDI runtime is licensed separately by Vizrt and is never distributed with this project.
