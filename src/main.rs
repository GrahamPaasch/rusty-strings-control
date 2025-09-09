use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, Receiver};
use serde::Deserialize;
use std::collections::HashMap;
use std::f32::consts::PI;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// Keystroke injection
use enigo::{Enigo, Key, KeyboardControllable};

// ---------------------------- Config types ----------------------------

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Action {
    // Send a key sequence like "Ctrl+S" or "Space" or "A"
    Keys { sequence: String },
    // Future extension: launch a command
    // Command { program: String, args: Option<Vec<String>> },
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
    // Pitch gate in cents; note must be within this tolerance of the center
    #[serde(default = "default_tolerance_cents")]
    tolerance_cents: f32,
    // Autocorrelation minimum and maximum pitch bounds
    #[serde(default = "default_min_hz")]
    min_hz: f32,
    #[serde(default = "default_max_hz")]
    max_hz: f32,
    // Processing window and hop (in samples). If 0, auto-choose.
    #[serde(default)]
    window_size: usize,
    #[serde(default)]
    hop_size: usize,
    // How many consecutive frames must match the same note before triggering
    #[serde(default = "default_hold_frames")]
    note_hold_frames: usize,
    // Minimum ms between repeated triggers of the same note
    #[serde(default = "default_retrigger_ms")]
    retrigger_ms: u64,
    // Optional energy/correlation threshold (0..1). Higher = stricter.
    #[serde(default = "default_corr_threshold")]
    corr_threshold: f32,
    // Note mapping: e.g., "A4" = { type = "keys", sequence = "Ctrl+S" }
    #[serde(default)]
    note_map: HashMap<String, Action>,
}

fn default_tolerance_cents() -> f32 { 35.0 }
fn default_min_hz() -> f32 { 90.0 }
fn default_max_hz() -> f32 { 2000.0 }
fn default_hold_frames() -> usize { 3 }
fn default_retrigger_ms() -> u64 { 600 }
fn default_corr_threshold() -> f32 { 0.35 }

impl Default for Config {
    fn default() -> Self {
        let mut note_map = HashMap::new();
        // Sample mappings: change freely in config.toml
        note_map.insert(
            "A4".to_string(),
            Action::Keys {
                sequence: "Ctrl+S".to_string(), // Save
            },
        );
        note_map.insert(
            "E4".to_string(),
            Action::Keys {
                sequence: "Space".to_string(), // Space bar
            },
        );
        note_map.insert(
            "D4".to_string(),
            Action::Keys {
                sequence: "Ctrl+Z".to_string(), // Undo
            },
        );
        note_map.insert(
            "G3".to_string(),
            Action::Keys {
                sequence: "Ctrl+Y".to_string(), // Redo
            },
        );

        Self {
            tolerance_cents: default_tolerance_cents(),
            min_hz: default_min_hz(),
            max_hz: default_max_hz(),
            window_size: 0,
            hop_size: 0,
            note_hold_frames: default_hold_frames(),
            retrigger_ms: default_retrigger_ms(),
            corr_threshold: default_corr_threshold(),
            note_map,
        }
    }
}

// ---------------------------- Main entry ----------------------------

fn main() -> Result<()> {
    let cfg = load_config().unwrap_or_else(|e| {
        eprintln!("Warning: using default config: {e:#}");
        Config::default()
    });

    println!("Starting Rusty Strings Control");
    println!("Tolerance: ±{:.1} cents, range: {:.0}-{:.0} Hz", cfg.tolerance_cents, cfg.min_hz, cfg.max_hz);

    // Set up audio capture
    let (rx, sample_rate, channels, _stream) = build_input_stream()?; // keep _stream alive
    println!("Input sample rate: {} Hz, channels: {}", sample_rate, channels);

    // Choose window and hop
    let window_size = if cfg.window_size > 0 { cfg.window_size } else { 
        // 46 ms @ 48k ~ 2208, round to 2048/4096 depending on sample rate
        // Use power of two near sample_rate/20
        nearest_power_of_two((sample_rate as f32 / 20.0) as usize).max(1024).min(8192)
    };
    let hop_size = if cfg.hop_size > 0 { cfg.hop_size } else { window_size / 4 };
    println!("Window: {} samples, Hop: {} samples", window_size, hop_size);

    // State for triggering
    let mut enigo = Enigo::new();
    let mut last_note: Option<String> = None;
    let mut stable_count: usize = 0;
    let mut last_trigger_time = Instant::now() - Duration::from_millis(cfg.retrigger_ms);

    // Rolling buffer
    let mut buffer: Vec<f32> = Vec::with_capacity(window_size);
    let mut hop_accum = 0usize;

    loop {
        // Fill buffer via hop size increments
        while hop_accum < hop_size {
            let s = rx.recv().context("audio stream ended")?;
            hop_accum += 1;
            buffer.push(s);
            if buffer.len() > window_size {
                let overflow = buffer.len() - window_size;
                buffer.drain(0..overflow);
            }
        }
        hop_accum = 0;

        if buffer.len() < window_size {
            continue;
        }

        let freq = detect_pitch_autocorr(&buffer, sample_rate as f32, cfg.min_hz, cfg.max_hz, cfg.corr_threshold);
        let now = Instant::now();

        if let Some(f0) = freq {
            // Convert to nearest musical note and cents offset
            let (note_name, cents_off) = freq_to_note(f0);
            let cents = cents_off.abs();
            let in_tune = cents <= cfg.tolerance_cents;

            print!("\r{:6.1} Hz  {:>3.0} cents  {:>3}  ", f0, cents_off, note_name);
            std::io::Write::flush(&mut std::io::stdout()).ok();

            if in_tune {
                if Some(note_name.clone()) == last_note {
                    stable_count += 1;
                } else {
                    last_note = Some(note_name.clone());
                    stable_count = 1;
                }

                if stable_count >= cfg.note_hold_frames
                    && now.duration_since(last_trigger_time) >= Duration::from_millis(cfg.retrigger_ms)
                {
                    if let Some(action) = cfg.note_map.get(&note_name) {
                        println!("\nTrigger: {note_name} => {:?}", action_name(action));
                        if let Err(e) = execute_action(&mut enigo, action) {
                            eprintln!("Action failed: {e:#}");
                        } else {
                            last_trigger_time = now;
                        }
                    }
                }
            } else {
                // Detected note but not within tolerance; reset stability
                stable_count = 0;
            }
        } else {
            // No confident pitch detected; reset stability
            print!("\r(no pitch)                                 ");
            std::io::Write::flush(&mut std::io::stdout()).ok();
            stable_count = 0;
            last_note = None;
        }
    }
}

// ---------------------------- Audio setup ----------------------------

fn build_input_stream() -> Result<(Receiver<f32>, u32, u16, cpal::Stream)> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("No default input device"))?;
    let config = device
        .default_input_config()
        .context("Failed to get default input config")?;

    let sample_rate = config.sample_rate().0;
    let channels = config.channels();

    let (tx, rx) = bounded::<f32>(sample_rate as usize); // ~1 second buffer

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config.into(), channels, tx.clone())?,
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config.into(), channels, tx.clone())?,
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &config.into(), channels, tx.clone())?,
        // Cover any new formats conservatively
        other => return Err(anyhow!("Unsupported sample format: {:?}", other)),
    };

    stream.play().context("Failed to start input stream")?;

    Ok((rx, sample_rate, channels, stream))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: u16,
    tx: crossbeam_channel::Sender<f32>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample,
{
    let err_fn = |err| eprintln!("Stream error: {err}");
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _| {
            // Mixdown to mono and send
            for frame in data.chunks(channels as usize) {
                let mut acc = 0.0f32;
                for &s in frame {
                    acc += s.to_f32();
                }
                let mono = acc / channels as f32;
                let _ = tx.try_send(mono);
            }
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

// ---------------------------- Pitch detection ----------------------------

fn detect_pitch_autocorr(
    input: &[f32],
    sample_rate: f32,
    min_hz: f32,
    max_hz: f32,
    corr_threshold: f32,
) -> Option<f32> {
    if input.is_empty() { return None; }

    // Remove DC and apply Hann window
    let mean = input.iter().copied().sum::<f32>() / input.len() as f32;
    let mut x: Vec<f32> = input.iter().map(|&s| s - mean).collect();
    let n = x.len();
    for i in 0..n {
        let w = 0.5 - 0.5 * (2.0 * PI * i as f32 / (n as f32 - 1.0)).cos();
        x[i] *= w as f32;
    }

    // Compute normalized autocorrelation for lags in [min_lag, max_lag]
    let min_lag = (sample_rate / max_hz).round() as usize;
    let max_lag = (sample_rate / min_hz).round() as usize;
    if max_lag + 1 >= n { return None; }

    let mut best_lag = 0usize;
    let mut best_r = 0.0f32;

    // Precompute energy for normalization
    let energy0 = x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() as f32;
    if energy0 <= 1e-9 { return None; }

    for lag in min_lag..=max_lag.min(n - 1) {
        let mut num = 0.0f64;
        let mut den = 0.0f64;
        for i in 0..(n - lag) {
            let a = x[i] as f64;
            let b = x[i + lag] as f64;
            num += a * b;
            den += a * a + b * b;
        }
        if den <= 1e-12 { continue; }
        let r = (2.0 * num / den) as f32; // between -1..1
        if r > best_r {
            best_r = r;
            best_lag = lag;
        }
    }

    if best_r < corr_threshold || best_lag == 0 { return None; }

    // Parabolic interpolation around best_lag for sub-sample peak
    let r_at = |lag: usize| -> f32 {
        let mut num = 0.0f64;
        let mut den = 0.0f64;
        for i in 0..(n - lag) {
            let a = x[i] as f64;
            let b = x[i + lag] as f64;
            num += a * b;
            den += a * a + b * b;
        }
        if den <= 1e-12 { 0.0 } else { (2.0 * num / den) as f32 }
    };

    let r0 = r_at(best_lag);
    let r1 = if best_lag > 1 { r_at(best_lag - 1) } else { r0 };
    let r2 = if best_lag + 1 < n { r_at(best_lag + 1) } else { r0 };

    let denom = (2.0 * r0) - r1 - r2;
    let delta = if denom.abs() > 1e-6 {
        0.5 * (r1 - r2) / denom
    } else { 0.0 };
    let est_lag = (best_lag as f32) + delta.clamp(-1.0, 1.0);

    let f0 = sample_rate / est_lag;
    if f0.is_finite() && f0 >= min_hz && f0 <= max_hz { Some(f0) } else { None }
}

// ---------------------------- Note conversion ----------------------------

fn freq_to_note(freq: f32) -> (String, f32) {
    // Reference A4 = 440 Hz
    let midi = 69.0 + 12.0 * (freq / 440.0).log2();
    let nearest = midi.round();
    let cents = (midi - nearest) * 100.0;
    let name = midi_to_name(nearest as i32);
    (name, cents)
}

fn midi_to_name(midi: i32) -> String {
    static NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let pitch_class = ((midi % 12) + 12) % 12;
    let octave = midi / 12 - 1;
    format!("{}{}", NAMES[pitch_class as usize], octave)
}

fn nearest_power_of_two(x: usize) -> usize {
    let mut p = 1usize;
    while p < x { p <<= 1; }
    p
}

// ---------------------------- Actions ----------------------------

fn action_name(a: &Action) -> String {
    match a {
        Action::Keys { sequence } => format!("keys:{}", sequence),
        // Action::Command { program, args } => format!("cmd:{} {}", program, args.as_ref().map(|v| v.join(" ")).unwrap_or_default()),
    }
}

fn execute_action(enigo: &mut Enigo, action: &Action) -> Result<()> {
    match action {
        Action::Keys { sequence } => send_keys(enigo, sequence),
        // Action::Command { .. } => todo!("Not implemented"),
    }
}

fn send_keys(enigo: &mut Enigo, sequence: &str) -> Result<()> {
    // Parse tokens like "Ctrl+Shift+S" or "Enter" or "Space" or "A"
    let tokens: Vec<String> = sequence
        .split('+')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if tokens.is_empty() { return Err(anyhow!("Empty key sequence")); }

    let mut modifiers: Vec<Key> = Vec::new();
    let mut main_key: Option<Key> = None;

    for t in &tokens {
        match t.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.push(Key::Control),
            "shift" => modifiers.push(Key::Shift),
            "alt" => modifiers.push(Key::Alt),
            "win" | "meta" => modifiers.push(Key::Meta),
            "space" => main_key = Some(Key::Space),
            "enter" | "return" => main_key = Some(Key::Return),
            "tab" => main_key = Some(Key::Tab),
            "esc" | "escape" => main_key = Some(Key::Escape),
            "up" | "uparrow" => main_key = Some(Key::UpArrow),
            "down" | "downarrow" => main_key = Some(Key::DownArrow),
            "left" | "leftarrow" => main_key = Some(Key::LeftArrow),
            "right" | "rightarrow" => main_key = Some(Key::RightArrow),
            other => {
                // Try single character
                let mut chars = other.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    main_key = Some(Key::Layout(c));
                } else {
                    return Err(anyhow!("Unknown key token: {other}"));
                }
            }
        }
    }

    let key = main_key.ok_or_else(|| anyhow!("No main key in sequence"))?;
    // Press modifiers
    for m in &modifiers { enigo.key_down(*m); }
    // Click main key
    enigo.key_click(key);
    // Release modifiers
    for m in modifiers.into_iter().rev() { enigo.key_up(m); }
    Ok(())
}

// ---------------------------- Config loading ----------------------------

fn load_config() -> Result<Config> {
    let path = std::env::current_dir()?.join("config.toml");
    if !path.exists() {
        return Err(anyhow!("config.toml not found; using defaults"));
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("Reading {}", path.display()))?;
    let mut cfg: Config = toml::from_str(&text).with_context(|| format!("Parsing {}", path.display()))?;
    // Merge defaults for any missing fields
    let def = Config::default();
    if cfg.window_size == 0 { cfg.window_size = def.window_size; }
    if cfg.hop_size == 0 { cfg.hop_size = def.hop_size; }
    if cfg.note_map.is_empty() { cfg.note_map = def.note_map; }
    Ok(cfg)
}
