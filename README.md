# pw-duck-smooth

Fork of [pw-duck](https://github.com/geri1701/pw-duck) (upstream v0.2.5, commit `272f306`) with one
feature: the volume return after ducking ends can be faded in instead of jumping to 100%.

This is an unofficial, independent fork. It is not affiliated with, endorsed by, or supported by the
upstream project or its author, and it is not a drop-in replacement: bugs found here are not
upstream bugs. Upstream pw-duck is MIT licensed, so this fork keeps the original copyright notice
and MIT license (see `LICENSE`) and credits the original author in `Cargo.toml`.

## What this fork changes

- `release_fade_ms` setting (`0` = instant, default `600`, clamp `0..4000`): duration of the
  volume ramp back to 100% when voice activity stops and at session teardown.
- `RouteSession::ramp_to_neutral(fade)`: climbs from the ducked volume to 100% in steps of at most
  4% per step, spread across the requested duration (minimum 20ms per step, never longer than asked).
- `--release-fade-ms` flag on `tray`, `route` and `route-once`.
- `pw-duck-smooth tune` (terminal tuner): new "Release fade" row, `←`/`→` in 100ms steps, `0` shows "off".
- `pw-duck-smooth tune-gui` (GTK tuner): new "Release fade" slider, 0–4000ms.
- Tray summary line shows the fade time next to ducking volume, sensitivity and hold.
- `duck_on_microphone` setting (`false` by default): when enabled, the audio is also ducked while
  your own microphone picks up speech, not only for the configured remote voice stream. The local
  microphone capture follows the same sensitivity, hold and release fade, and can be toggled live
  while the tray is running.
- `pw-duck-smooth tune` (terminal tuner): new "Duck on mic" row, toggled with `←`/`→` or `space`.
- `pw-duck-smooth tune-gui` (GTK tuner): new "Duck when I talk" check button.

Everything else is upstream code, so the config file stays at `~/.config/pw-duck/config.toml` and
is shared with an upstream install:

```toml
release_fade_ms = 600
duck_on_microphone = false
```

Both builds also share the tray runtime lock (`$XDG_RUNTIME_DIR/pw-duck/tray.lock`) and the virtual
sink name prefix, so only one of them can route audio at a time. Stop the other tray first.

## Build and install

```bash
cargo build --release --locked --features gui
install -m755 target/release/pw-duck-smooth ~/.cargo/bin/pw-duck-smooth
```

## License

MIT, like upstream. `LICENSE` keeps the original copyright notice:

```text
Copyright (c) 2026 Gerhard Schwanzer
```

---

# Upstream: pw-duck

`pw-duck` lowers music, games, videos, and other playback while people are speaking in a selected voice-call stream.

It is built for PipeWire desktops and controlled from a tray icon.

## Features

- Automatic ducking for non-call audio
- Voice-call stream selection from running playback streams
- Tray toggle for ducking on/off
- Sensitivity, ducking volume, and hold-time tuning

## Install

### Arch Linux / AUR

```bash
paru -S pw-duck
```

Then start `pw-duck` from your launcher or run:

```bash
pw-duck
```

### Nix

From this repository:

```bash
nix run .#
```

Other app outputs:

```bash
nix run .#tray
nix run .#tune
nix run .#tune-gui
```

### From source

```bash
cargo build --release
cargo build --release --features gui
```

The build needs Rust plus PipeWire/pkg-config development libraries. The `gui` feature also needs GTK4.

## Requirements

- PipeWire with a compatible session manager, for example WirePlumber
- PulseAudio compatibility (`pactl` must work)
- `pw-link`
- A StatusNotifierItem/SNI tray host
- GTK4 for the graphical tuner

KDE Plasma supports SNI natively. GNOME needs an AppIndicator/KStatusNotifierItem extension.

## First run

1. Join a voice call so the call playback stream exists.
2. Start `pw-duck`.
3. In the tray menu, choose `Source: choose`.
4. Select the stream that contains the call audio.
5. Enable `Ducking`.
6. Adjust `Tuner: open` if needed.

## Commands

```bash
pw-duck                 # start tray
pw-duck tray            # start tray explicitly
pw-duck sources         # list selectable playback streams
pw-duck select-source 5 # select stream by sink-input index
pw-duck status          # show current audio state
pw-duck tune            # terminal tuner
pw-duck tune-gui        # graphical tuner, if built with gui
pw-duck config-path     # print config path
```

If `sources` shows a stream as `#546`, pass only the number:

```bash
pw-duck select-source 546
```

## Tuning

- **Sensitivity:** how easily call speech is detected; `0%` disables detection.
- **Ducking volume:** volume for non-call audio while speech is active.
- **Hold:** delay before normal volume is restored after speech stops.
- **Release fade:** how long the volume takes to come back after ducking ends. `0 ms` restores 100% immediately, higher values fade in smoothly (`release_fade_ms` in `config.toml`, `--release-fade-ms` on `tray`/`route`/`route-once`).

Settings are saved to:

```text
~/.config/pw-duck/config.toml
```

## How it works

While ducking is enabled, `pw-duck` creates a temporary virtual PipeWire sink and routes non-call playback through it. The selected call stream stays on the normal output path and is used only for detection.

On shutdown, streams are moved back. If that cannot be done safely, the virtual sink is left alive instead of breaking active application audio.

## Troubleshooting

### No tray icon

- On GNOME, enable an AppIndicator/KStatusNotifierItem extension.
- Restart `pw-duck` if your tray host cached an old icon.

### No call stream appears

- Join a call first; many apps create playback streams only while audio is active.
- Run `pw-duck sources` and pick the stream that contains the call audio.

### Ducking does not react

- Check the selected source.
- Increase sensitivity.
- Make sure sensitivity is not `0%`.
- Run `pw-duck status`.

## Development

Use the dev shell:

```bash
direnv exec . cargo fmt --check
direnv exec . cargo check
direnv exec . cargo test
direnv exec . cargo check --features gui
```

Regenerate icons after editing `assets/icons/source/*.png`:

```bash
scripts/generate-icons.sh
```

Build the Nix package:

```bash
nix build .#
```

## License

MIT
