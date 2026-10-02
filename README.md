# pw-duck-smooth

Smooth audio ducking for PipeWire: music, games and video drop to a lower volume while a voice
call is active, then **fade back in** instead of jumping back to 100%.

This is an unofficial fork of [pw-duck](https://github.com/geri1701/pw-duck) (upstream v0.2.5).
It is not affiliated with, endorsed by, or supported by the upstream project. Upstream pw-duck is
MIT licensed, so this fork keeps the original copyright notice and license (see `LICENSE`) and
credits the original author in `Cargo.toml`.

## What this fork adds

- **Release fade** (`release_fade_ms`, default `600`, `0` disables): the volume ramps back to 100%
  in steps over the configured time instead of jumping. New voice activity always interrupts a
  running fade, so ducking works mid-restore.
- **Duck on microphone** (`duck_on_microphone`, default `false`): also ducks while your own
  microphone picks up speech, using the same sensitivity, hold time and release fade. It can be
  toggled live from the tray or the tuner.
- **A `config` CLI**: read and write every setting from scripts, with `--json` for machine use.
- **A `doctor` command**: checks PipeWire, `pw-link`, the tray host, the config file and whether
  the configured voice source is actually streaming.
- **One config code path**: the tray, both tuners and the CLI share the same load/save/validation
  logic, so a value can never be clamped differently depending on where you set it.

## How it works

While ducking is enabled, a temporary virtual PipeWire sink (`pw-duck-*`) is created and all
non-call playback is routed through it. That shared sink is what gets ducked — not individual
streams. The selected call stream stays on the normal output path and is only used for detection.

On shutdown, streams are moved back to the real sink first. If that cannot be done safely, the
virtual sink is left in place instead of breaking audio in running applications, and the previous
default sink is restored.

## Install

### From source

```bash
cargo build --release --locked --features gui
install -m755 target/release/pw-duck-smooth ~/.local/bin/pw-duck-smooth
```

The `gui` feature pulls in GTK4 for the graphical tuner; without it everything else still builds and
the tray reports the tuner as unavailable.

Build requirements: Rust (edition 2024), PipeWire and PulseAudio development libraries, `pkg-config`
and `bindgen`/`libclang`. GTK4 is only needed for `--features gui`.

### Nix

```bash
nix run .#            # tray
nix run .#tray        # tray, explicit
nix run .#tune        # terminal tuner
nix run .#tune-gui    # graphical tuner
```

### Arch / AUR

The AUR package builds the full desktop version (`--features gui`).

## Quick start

```bash
pw-duck-smooth doctor                     # is everything in place?
pw-duck-smooth sources                   # which stream carries the call?
pw-duck-smooth select-source 42          # save it as the voice source
pw-duck-smooth                           # start the tray (this is the default)
```

## Commands

| Command | What it does |
| --- | --- |
| `status` | Default sink, playback streams, config, tray state and effective tuning |
| `sources` | Playback streams as selectable voice-source candidates |
| `select-source <STREAM>` | Store one stream as the voice source |
| `config path\|show\|set\|reset` | Read and write the config without a tuner |
| `tune` | Terminal tuner (arrow keys, live apply) |
| `tune-gui` | Graphical tuner (needs `--features gui`) |
| `doctor` | Installation check |
| `tray` | Start the StatusNotifier tray (default when no command is given) |
| `route` / `route-once` | Low-level routing, both require `--yes-really-route` |

Add `--json` to `status`, `sources` and `config path|show` for script-friendly output.

### Tuning from the command line

```bash
pw-duck-smooth config set duck_percent 30
pw-duck-smooth config set release_fade_ms 1200
pw-duck-smooth config reset hold_ms
pw-duck-smooth config reset              # every tunable back to its default
```

One-shot overrides work on `tray` and `route`:

```bash
pw-duck-smooth tray --duck-percent 20 --release-fade-ms 0 --duck-on-microphone
```

## Configuration

`~/.config/pw-duck/config.toml` — the same file an upstream pw-duck install uses:

```toml
duck_percent = 25          # volume of the virtual sink while ducking
vad_threshold = 0.01       # RMS noise floor; speech starts above 2x this
hold_ms = 700              # keep ducking this long after the voice stops
release_fade_ms = 600      # fade back to 100%; 0 = instant
duck_on_microphone = false # also duck while your own microphone is active

[voice_source]             # written by `select-source`
# label = "…"
```

| Setting | Range | Default | Meaning |
| --- | --- | --- | --- |
| `duck_percent` | 0–100 | 25 | Volume while ducking |
| `vad_threshold` | 0.0025–0.2 | 0.01 | Noise floor; `0.2` disables detection |
| `hold_ms` | 0–4000 | 700 | Delay before releasing |
| `release_fade_ms` | 0–4000 | 600 | Fade-in time back to 100% |
| `duck_on_microphone` | bool | false | Duck on local speech |

The tray re-reads the file about four times a second, so changes from the tuners, the CLI or a text
editor apply to a running instance — including switching the voice source, which cleanly tears down
the current session and starts a new one.

This fork shares upstream's tray runtime lock (`$XDG_RUNTIME_DIR/pw-duck/tray.lock`) and virtual
sink prefix, so only one of the two builds can route audio at a time. Stop the other tray first.

## Troubleshooting

**No tray icon.** GNOME needs an AppIndicator/KStatusNotifierItem extension; KDE Plasma supports
SNI natively. `pw-duck-smooth doctor` tells you whether a watcher is reachable.

**Ducking never triggers.** Join the call first: many apps only create a playback stream while audio
flows. Then check `sources` for the right stream, raise the sensitivity in the tuner (0% is off) and
make sure a voice source is selected.

**Ducking triggers on the wrong app.** Pick a more specific stream with `select-source`, or set a
different threshold. `status` marks the configured stream so you can verify the choice.

**Nothing plays while ducking.** Check that the original default sink was restored
(`pactl get-default-sink`) and run `status`; a leftover virtual sink means a previous teardown could
not move streams back safely.

## Development

With the Nix dev shell:

```bash
direnv exec . cargo fmt --check
direnv exec . cargo clippy --all-targets -- -D warnings
direnv exec . cargo test
direnv exec . cargo check --features gui
```

Without Nix, the same commands work as long as the PipeWire, PulseAudio and GTK4 development
packages are installed.

Project layout:

| File | Responsibility |
| --- | --- |
| `src/main.rs` | CLI dispatch, `status`, `config`, `doctor` |
| `src/cli.rs` | Argument definitions and override resolution |
| `src/config.rs` | Config schema, defaults, clamping, typed keys |
| `src/output.rs` | Tables and JSON rendering |
| `src/duck.rs` | Session loop: routing, VAD, release fade |
| `src/routing.rs` | Virtual sink, stream moves, teardown |
| `src/vad.rs` | PipeWire capture and voice activity detection |
| `src/tray.rs` | StatusNotifier UI, single-instance lock, worker |
| `src/tune.rs`, `src/tune_gui.rs` | Terminal and GTK tuners |
| `src/pulse.rs`, `src/pipewire_sink.rs` | `pactl` and `pw-link` wrappers |
| `src/identity.rs` | Stream identity and voice-source matching |
| `src/shell.rs` | Command execution with timeouts |

Regenerate the tray icons after editing `assets/icons/source/*.png`:

```bash
scripts/generate-icons.sh
```

Publishing a release: tag the version, then run `scripts/update-aur-checksum.sh` so `PKGBUILD` and
`.SRCINFO` carry a real checksum instead of `SKIP`.

## License

MIT, like upstream. `LICENSE` keeps the original copyright notice:

```text
Copyright (c) 2026 Gerhard Schwanzer
```