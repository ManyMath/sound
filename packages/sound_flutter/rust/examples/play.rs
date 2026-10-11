//! Smoke test for the native playback path: generates a 440 Hz tone and plays
//! it through the C ABI, then reports the final voice state. Run with:
//!
//! ```sh
//! cargo run --example play
//! ```

use std::ffi::CStr;
use std::io::Cursor;
use std::thread::sleep;
use std::time::Duration;

use hound::{SampleFormat, WavSpec, WavWriter};
use sound_cli::{
    sound_last_error, sound_play_bytes, sound_player_free, sound_player_new, sound_voice_error,
    sound_voice_state,
};

fn tone(freq: f32, secs: f32) -> Vec<u8> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut buf = Cursor::new(Vec::new());
    {
        let mut w = WavWriter::new(&mut buf, spec).unwrap();
        let n = (44_100.0 * secs) as usize;
        for i in 0..n {
            let t = i as f32 / 44_100.0;
            let v = (0.3 * (2.0 * std::f32::consts::PI * freq * t).sin() * i16::MAX as f32) as i16;
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
    }
    buf.into_inner()
}

fn main() {
    let wav = tone(440.0, 1.0);
    unsafe {
        let player = sound_player_new();
        if player.is_null() {
            let msg = CStr::from_ptr(sound_last_error()).to_string_lossy();
            eprintln!("player init failed: {msg}");
            std::process::exit(1);
        }
        let id = sound_play_bytes(player, wav.as_ptr(), wav.len(), std::ptr::null(), 0);
        if id == 0 {
            let msg = CStr::from_ptr(sound_last_error()).to_string_lossy();
            eprintln!("play failed: {msg}");
            std::process::exit(1);
        }
        println!("playing voice {id} (440 Hz, 1 s)...");
        loop {
            let state = sound_voice_state(player, id);
            if state != 0 {
                let label = match state {
                    1 => "completed",
                    2 => "stopped",
                    3 => "error",
                    _ => "unknown",
                };
                println!("final state: {label}");
                if state == 3 {
                    let msg = CStr::from_ptr(sound_voice_error(player, id)).to_string_lossy();
                    eprintln!("error: {msg}");
                }
                break;
            }
            sleep(Duration::from_millis(50));
        }
        sound_player_free(player);
    }
}
