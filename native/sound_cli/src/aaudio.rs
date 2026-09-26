//! Minimal AAudio binding obtained by `dlopen`-ing the runtime `libaaudio.so`.
//!
//! AAudio ships with Android (API 26+) and needs no NDK headers at build time;
//! we declare the few symbols we use ourselves, mirroring the Linux/ALSA
//! approach. This keeps the "no system dependencies" guarantee on Android too.

use std::os::raw::{c_char, c_int, c_void};

use libloading::Library;

use crate::PcmSink;

// Stable AAudio ABI constants.
const AAUDIO_DIRECTION_OUTPUT: c_int = 0;
const AAUDIO_FORMAT_PCM_I16: c_int = 1;
const AAUDIO_OK: c_int = 0;

type Builder = c_void;
type Stream = c_void;

type FnCreateBuilder = unsafe extern "C" fn(*mut *mut Builder) -> c_int;
type FnBuilderSetI32 = unsafe extern "C" fn(*mut Builder, c_int);
type FnBuilderOpen = unsafe extern "C" fn(*mut Builder, *mut *mut Stream) -> c_int;
type FnBuilderDelete = unsafe extern "C" fn(*mut Builder) -> c_int;
type FnStreamOp = unsafe extern "C" fn(*mut Stream) -> c_int;
type FnStreamWrite = unsafe extern "C" fn(*mut Stream, *const c_void, c_int, i64) -> c_int;
type FnStreamFrames = unsafe extern "C" fn(*mut Stream) -> i64;
type FnResultText = unsafe extern "C" fn(c_int) -> *const c_char;

/// Resolved entry points into `libaaudio.so`.
pub struct Aaudio {
    _lib: Library,
    create_builder: FnCreateBuilder,
    builder_set_direction: FnBuilderSetI32,
    builder_set_sample_rate: FnBuilderSetI32,
    builder_set_channel_count: FnBuilderSetI32,
    builder_set_format: FnBuilderSetI32,
    builder_open: FnBuilderOpen,
    builder_delete: FnBuilderDelete,
    stream_start: FnStreamOp,
    stream_write: FnStreamWrite,
    stream_frames_read: FnStreamFrames,
    stream_frames_written: FnStreamFrames,
    stream_stop: FnStreamOp,
    stream_close: FnStreamOp,
    result_text: FnResultText,
}

unsafe impl Send for Aaudio {}
unsafe impl Sync for Aaudio {}

impl Aaudio {
    pub fn load() -> Result<Aaudio, String> {
        unsafe {
            let lib = Library::new("libaaudio.so")
                .map_err(|e| format!("failed to load libaaudio.so: {e}"))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get($name).map_err(|e| {
                        format!("missing symbol {}: {e}", String::from_utf8_lossy($name))
                    })?
                };
            }
            let aaudio = Aaudio {
                create_builder: sym!(b"AAudio_createStreamBuilder\0"),
                builder_set_direction: sym!(b"AAudioStreamBuilder_setDirection\0"),
                builder_set_sample_rate: sym!(b"AAudioStreamBuilder_setSampleRate\0"),
                builder_set_channel_count: sym!(b"AAudioStreamBuilder_setChannelCount\0"),
                builder_set_format: sym!(b"AAudioStreamBuilder_setFormat\0"),
                builder_open: sym!(b"AAudioStreamBuilder_openStream\0"),
                builder_delete: sym!(b"AAudioStreamBuilder_delete\0"),
                stream_start: sym!(b"AAudioStream_requestStart\0"),
                stream_write: sym!(b"AAudioStream_write\0"),
                stream_frames_read: sym!(b"AAudioStream_getFramesRead\0"),
                stream_frames_written: sym!(b"AAudioStream_getFramesWritten\0"),
                stream_stop: sym!(b"AAudioStream_requestStop\0"),
                stream_close: sym!(b"AAudioStream_close\0"),
                result_text: sym!(b"AAudio_convertResultToText\0"),
                _lib: lib,
            };
            Ok(aaudio)
        }
    }

    fn result_text(&self, code: c_int) -> String {
        unsafe {
            let ptr = (self.result_text)(code);
            if ptr.is_null() {
                return format!("AAudio error {code}");
            }
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

/// An open AAudio output stream configured for interleaved S16 frames.
pub struct AaudioPlayback<'a> {
    aaudio: &'a Aaudio,
    stream: *mut Stream,
    channels: i32,
}

impl<'a> AaudioPlayback<'a> {
    pub fn open(aaudio: &'a Aaudio, channels: u32, rate: u32) -> Result<Self, String> {
        unsafe {
            let mut builder: *mut Builder = std::ptr::null_mut();
            let rc = (aaudio.create_builder)(&mut builder);
            if rc != AAUDIO_OK {
                return Err(format!("createStreamBuilder: {}", aaudio.result_text(rc)));
            }
            (aaudio.builder_set_direction)(builder, AAUDIO_DIRECTION_OUTPUT);
            (aaudio.builder_set_sample_rate)(builder, rate as c_int);
            (aaudio.builder_set_channel_count)(builder, channels as c_int);
            (aaudio.builder_set_format)(builder, AAUDIO_FORMAT_PCM_I16);

            let mut stream: *mut Stream = std::ptr::null_mut();
            let rc = (aaudio.builder_open)(builder, &mut stream);
            (aaudio.builder_delete)(builder);
            if rc != AAUDIO_OK {
                return Err(format!("openStream: {}", aaudio.result_text(rc)));
            }
            let rc = (aaudio.stream_start)(stream);
            if rc != AAUDIO_OK {
                (aaudio.stream_close)(stream);
                return Err(format!("requestStart: {}", aaudio.result_text(rc)));
            }
            Ok(AaudioPlayback {
                aaudio,
                stream,
                channels: channels as i32,
            })
        }
    }
}

impl PcmSink for AaudioPlayback<'_> {
    fn write(&self, samples: &[i16]) -> Result<(), String> {
        let total_frames = samples.len() as i32 / self.channels;
        if total_frames == 0 {
            return Ok(());
        }
        let mut offset = 0i32;
        unsafe {
            while offset < total_frames {
                let ptr = samples.as_ptr().add((offset * self.channels) as usize) as *const c_void;
                // 1 s timeout - blocks until the device buffer has room. On
                // timeout it returns however much it wrote, possibly nothing.
                let n = (self.aaudio.stream_write)(
                    self.stream,
                    ptr,
                    total_frames - offset,
                    1_000_000_000,
                );
                if n < 0 {
                    return Err(format!(
                        "AAudioStream_write: {}",
                        self.aaudio.result_text(n)
                    ));
                }
                if n == 0 {
                    return Err("AAudioStream_write: the stream stopped accepting audio".into());
                }
                offset += n;
            }
        }
        Ok(())
    }

    fn drain(&self) {
        // Wait until the device has played out everything we wrote, then
        // stop. Give up early if playback stops advancing (e.g. the device
        // went away) so the voice thread can still finish.
        unsafe {
            let target = (self.aaudio.stream_frames_written)(self.stream);
            let mut last = (self.aaudio.stream_frames_read)(self.stream);
            let mut idle = std::time::Instant::now();
            for _ in 0..2000 {
                let read = (self.aaudio.stream_frames_read)(self.stream);
                if read >= target {
                    break;
                }
                if read != last {
                    last = read;
                    idle = std::time::Instant::now();
                } else if idle.elapsed() >= std::time::Duration::from_secs(1) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            (self.aaudio.stream_stop)(self.stream);
        }
    }
}

impl Drop for AaudioPlayback<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.aaudio.stream_close)(self.stream);
        }
    }
}
