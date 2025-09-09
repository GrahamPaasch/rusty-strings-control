# Rusty Strings Control

Turn your electric violin into a hands-free controller: capture audio, detect pitch, and trigger keyboard shortcuts when you play in-tune notes.

## Features

- Audio input via `cpal` (WASAPI on Windows)
- Real-time pitch detection (autocorrelation with confidence threshold)
- Tolerance in cents to require in-tune playing
- Stability gating (N consecutive frames) and retrigger timeout
- Map notes (e.g., `A4`, `E4`) to keystrokes (e.g., `Ctrl+S`, `Space`)

## Quick Start (Windows 11)

1. Install Rust (stable) and ensure `cargo` is in PATH.
2. Plug in your audio interface / electric violin and set it as the default input device.
3. Edit `config.toml` to set note mappings and tolerance.
4. Build and run:
   ```sh
   cargo run --release
   ```

The console displays detected frequency, cents offset, and nearest note. When a mapped note is held in tune for the configured stability window, the corresponding keystroke is sent to the OS.

## Config

Edit `config.toml`:

- `tolerance_cents`: Note must be within ±this many cents (default 35)
- `min_hz`/`max_hz`: Search range for pitch detection
- `window_size`/`hop_size`: Processing sizes (0 = auto)
- `note_hold_frames`: Frames of stable, in-tune detection before triggering
- `retrigger_ms`: Minimum time between repeated triggers of the same note
- `corr_threshold`: Autocorrelation confidence threshold (0..1)
- `note_map`: Mapping from note name to action

Example mapping:

```toml
[note_map]
A4 = { type = "keys", sequence = "Ctrl+S" } # Save
E4 = { type = "keys", sequence = "Space" }  # Space bar
```

Supported keys: modifiers `Ctrl`, `Shift`, `Alt`, `Win/Meta`; special keys `Space`, `Enter/Return`, `Tab`, `Esc/Escape`, `Up/Down/Left/Right`; single letters/digits like `A`, `1`.

## Notes and Tuning

- Reference is A4 = 440 Hz. Detected pitches are mapped to the nearest semitone; triggering requires being within your configured tolerance.
- Violin range fits well within defaults (≈196–2637 Hz). If you use extended-lower tunings, consider lowering `min_hz`.

## Implementation Details

- Audio: `cpal` input stream mixed to mono and buffered.
- Pitch: time-domain normalized autocorrelation with Hann window and parabolic peak interpolation. This provides robust, low-CPU estimation without external DSP crates.
- Actions: `enigo` to inject keystrokes via the system APIs (uses `SendInput` on Windows).

## Troubleshooting

- No input device: ensure your interface is the default input in Windows Sound Settings.
- Sensitivity: raise `corr_threshold` or `note_hold_frames` to reduce false triggers; lower to make detection more permissive.
- Latency: reduce `window_size` (or allow auto) and/or lower `note_hold_frames`, but very small windows degrade low-note accuracy.

## Extensibility

- Add command-launch actions (e.g., start apps) or MIDI output.
- Swap in a library detector (e.g., YIN/MCLeod from `pitch-detection`) if desired.
- Persist per-note custom tolerances or hysteresis.

## Safety

Keystroke injection affects the active application. Test with a harmless target (e.g., Notepad) and choose mappings that won’t cause data loss.
