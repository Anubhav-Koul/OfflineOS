//! Which microphone does this machine actually record from, and does it deliver
//! anything?
//!
//! Run with:
//!   cargo test -p ic_voice --test mic_probe -- --ignored --nocapture
//!
//! Speak while it runs. It prints every input device, then records through the
//! same path the wake-word wizard uses and reports the peak level. A peak near
//! zero means the chosen endpoint is live but silent — the classic
//! Bluetooth-headset (HFP) failure, where the OS happily hands you a device that
//! never produces samples above the noise floor.

use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait};
use ic_voice::{Capture, CpalCapture, format::SAMPLE_RATE, sample_peak};

#[test]
#[ignore = "needs a real microphone and a human to talk; run with --ignored"]
fn what_does_the_microphone_actually_hear() {
    let host = cpal::default_host();

    println!("\n--- input devices ---");
    if let Some(device) = host.default_input_device() {
        println!(
            "DEFAULT: {}",
            device
                .description()
                .map(|d| d.to_string())
                .unwrap_or_else(|_| "<unnamed>".into())
        );
    } else {
        println!("DEFAULT: (none)");
    }
    match host.input_devices() {
        Ok(devices) => {
            for device in devices {
                let name = device
                    .description()
                    .map(|d| d.to_string())
                    .unwrap_or_else(|_| "<unnamed>".into());
                let config = device
                    .default_input_config()
                    .map(|c| {
                        format!(
                            "{:?} {} ch @ {} Hz",
                            c.sample_format(),
                            c.channels(),
                            c.sample_rate()
                        )
                    })
                    .unwrap_or_else(|error| format!("<no config: {error}>"));
                println!("  - {name}  [{config}]");
            }
        }
        Err(error) => println!("  could not enumerate: {error}"),
    }

    println!("\n--- recording 3s through the wizard's path — SAY SOMETHING ---");
    let capture = match CpalCapture::start(4.0) {
        Ok(capture) => capture,
        Err(error) => panic!("capture failed to start: {error}"),
    };
    let ring = capture.ring();
    std::thread::sleep(Duration::from_secs(3));
    drop(capture);

    let samples = ring.latest(SAMPLE_RATE as usize * 3);
    let peak = sample_peak(&samples);
    println!("captured {} samples, peak = {peak:.4}", samples.len());
    println!(
        "verdict: {}",
        if samples.is_empty() {
            "NOTHING captured — the endpoint delivered no audio at all"
        } else if peak < 0.02 {
            "SILENT — the device is live but hears nothing (wrong mic, or muted)"
        } else {
            "OK — the microphone works"
        }
    );
}

/// The fix: naming the real microphone must bypass the deaf default.
///
/// Run with a device name:
///   IC_MIC="AI Noise-Canceling Microphone (ASUS Utility) via Line" \
///     cargo test -p ic_voice --test mic_probe chosen -- --ignored --nocapture
#[test]
#[ignore = "needs a real microphone and a human to talk; run with --ignored"]
fn a_chosen_microphone_is_used_instead_of_the_default() {
    let Ok(name) = std::env::var("IC_MIC") else {
        println!("set IC_MIC to a device name from the list above");
        return;
    };
    println!("\n--- recording 3s from {name:?} — SAY SOMETHING ---");
    let capture = CpalCapture::start_on(Some(&name), 4.0).expect("the chosen device opens");
    let ring = capture.ring();
    std::thread::sleep(Duration::from_secs(3));
    drop(capture);

    let samples = ring.latest(SAMPLE_RATE as usize * 3);
    let peak = sample_peak(&samples);
    println!("captured {} samples, peak = {peak:.4}", samples.len());
    assert!(!samples.is_empty(), "the chosen device delivered nothing");
}

/// Record from EVERY input device in turn and report which one actually hears a
/// voice. The only way to tell a dead endpoint from a quiet room is for a human to
/// speak, so this asks for exactly that — and then measures, per device, instead of
/// guessing from the default.
#[test]
#[ignore = "needs a human to talk; run with --ignored"]
fn records_every_device_and_reports_which_hears_you() {
    let names = ic_voice::input_devices();
    println!(
        "
=== TALK CONTINUOUSLY — each device gets 5 seconds ===
"
    );

    let mut results: Vec<(String, f32)> = Vec::new();
    for name in &names {
        println!(">>> now recording: {name}  — KEEP TALKING");
        let peak = match CpalCapture::start_on(Some(name), 6.0) {
            Ok(capture) => {
                let ring = capture.ring();
                std::thread::sleep(Duration::from_secs(5));
                drop(capture);
                sample_peak(&ring.latest(SAMPLE_RATE as usize * 5))
            }
            Err(error) => {
                println!("    could not open: {error}");
                continue;
            }
        };
        println!("    peak = {peak:.4}");
        results.push((name.clone(), peak));
    }

    println!(
        "
=== VERDICT ==="
    );
    for (name, peak) in &results {
        let verdict = if *peak >= 0.02 {
            "HEARS YOU"
        } else {
            "deaf / silent"
        };
        println!("  {peak:.4}  {verdict:14}  {name}");
    }
    let best = results.iter().max_by(|a, b| a.1.total_cmp(&b.1));
    match best {
        Some((name, peak)) if *peak >= 0.02 => println!(
            "
USE THIS ONE: {name}"
        ),
        _ => println!(
            "
NO DEVICE HEARD ANYTHING — the mic is muted in Windows, or blocked by privacy settings"
        ),
    }
}
