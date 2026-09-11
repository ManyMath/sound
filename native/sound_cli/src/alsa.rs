//! Minimal ALSA binding obtained by `dlopen`-ing the runtime `libasound.so.2`.
//!
//! We avoid linking against ALSA at build time (which would need
//! the `libasound2-dev` package / pkg-config). Instead we declare the handful
//! of symbols we use ourselves and resolve them from the shared library that
//! ships with a default Ubuntu install. On this dev host ALSA routes to
//! PipeWire, which also mixes concurrent streams for us.

use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};

use libloading::Library;

// Stable ALSA ABI constants.
pub const SND_PCM_STREAM_PLAYBACK: c_int = 0;
pub const SND_PCM_FORMAT_S16_LE: c_int = 2;
pub const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;

type SndPcm = c_void;

type FnPcmOpen = unsafe extern "C" fn(*mut *mut SndPcm, *const c_char, c_int, c_int) -> c_int;
type FnPcmSetParams =
    unsafe extern "C" fn(*mut SndPcm, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int;
type FnPcmWritei = unsafe extern "C" fn(*mut SndPcm, *const c_void, c_ulong) -> c_long;
type FnPcmRecover = unsafe extern "C" fn(*mut SndPcm, c_int, c_int) -> c_int;
type FnPcmInt = unsafe extern "C" fn(*mut SndPcm) -> c_int;
type FnStrError = unsafe extern "C" fn(c_int) -> *const c_char;

/// Resolved entry points into `libasound.so.2`.
pub struct Alsa {
    // Keep the library loaded for the lifetime of the resolved pointers.
    _lib: Library,
    pcm_open: FnPcmOpen,
    pcm_set_params: FnPcmSetParams,
    pcm_writei: FnPcmWritei,
    pcm_recover: FnPcmRecover,
    pcm_drain: FnPcmInt,
    pcm_close: FnPcmInt,
    strerror: FnStrError,
}

// The resolved function pointers are plain code addresses; sharing them across
// threads is sound. PCM handles are used by a single playback thread.
unsafe impl Send for Alsa {}
unsafe impl Sync for Alsa {}

impl Alsa {
    /// Loads `libasound.so.2` and resolves the symbols we use.
    pub fn load() -> Result<Alsa, String> {
        unsafe {
            let lib = Library::new("libasound.so.2")
                .map_err(|e| format!("failed to load libasound.so.2: {e}"))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get($name).map_err(|e| {
                        format!("missing symbol {}: {e}", String::from_utf8_lossy($name))
                    })?
                };
            }
            let alsa = Alsa {
                pcm_open: sym!(b"snd_pcm_open\0"),
                pcm_set_params: sym!(b"snd_pcm_set_params\0"),
                pcm_writei: sym!(b"snd_pcm_writei\0"),
                pcm_recover: sym!(b"snd_pcm_recover\0"),
                pcm_drain: sym!(b"snd_pcm_drain\0"),
                pcm_close: sym!(b"snd_pcm_close\0"),
                strerror: sym!(b"snd_strerror\0"),
                _lib: lib,
            };
            Ok(alsa)
        }
    }

    /// Translates an ALSA error code into a human-readable message.
    pub fn strerror(&self, err: c_int) -> String {
        unsafe {
            let ptr = (self.strerror)(err);
            if ptr.is_null() {
                return format!("ALSA error {err}");
            }
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

/// An open PCM playback device configured for interleaved S16LE samples.
pub struct PcmPlayback<'a> {
    alsa: &'a Alsa,
    pcm: *mut SndPcm,
    channels: c_uint,
}

impl<'a> PcmPlayback<'a> {
    /// Opens the `default` device and configures it for [channels]/[rate].
    pub fn open(alsa: &'a Alsa, channels: u32, rate: u32) -> Result<Self, String> {
        unsafe {
            let mut pcm: *mut SndPcm = std::ptr::null_mut();
            let name = b"default\0";
            let rc = (alsa.pcm_open)(
                &mut pcm,
                name.as_ptr() as *const c_char,
                SND_PCM_STREAM_PLAYBACK,
                0, // blocking mode
            );
            if rc < 0 {
                return Err(format!("snd_pcm_open: {}", alsa.strerror(rc)));
            }
            let rc = (alsa.pcm_set_params)(
                pcm,
                SND_PCM_FORMAT_S16_LE,
                SND_PCM_ACCESS_RW_INTERLEAVED,
                channels as c_uint,
                rate as c_uint,
                1,       // allow soft resampling
                100_000, // ~100 ms latency
            );
            if rc < 0 {
                (alsa.pcm_close)(pcm);
                return Err(format!("snd_pcm_set_params: {}", alsa.strerror(rc)));
            }
            Ok(PcmPlayback {
                alsa,
                pcm,
                channels: channels as c_uint,
            })
        }
    }

    /// Writes one interleaved chunk of S16LE samples, recovering from
    /// underruns. Returns the number of frames written, or an error string.
    pub fn write(&self, samples: &[i16]) -> Result<usize, String> {
        let frames = (samples.len() / self.channels as usize) as c_ulong;
        if frames == 0 {
            return Ok(0);
        }
        unsafe {
            let mut written =
                (self.alsa.pcm_writei)(self.pcm, samples.as_ptr() as *const c_void, frames);
            if written < 0 {
                // Try to recover from underrun/suspend, then retry once.
                let rc = (self.alsa.pcm_recover)(self.pcm, written as c_int, 1);
                if rc < 0 {
                    return Err(format!("snd_pcm_writei: {}", self.alsa.strerror(rc)));
                }
                written =
                    (self.alsa.pcm_writei)(self.pcm, samples.as_ptr() as *const c_void, frames);
                if written < 0 {
                    return Err(format!(
                        "snd_pcm_writei (retry): {}",
                        self.alsa.strerror(written as c_int)
                    ));
                }
            }
            Ok(written as usize)
        }
    }

    /// Blocks until all buffered audio has been played.
    pub fn drain(&self) {
        unsafe {
            (self.alsa.pcm_drain)(self.pcm);
        }
    }
}

impl crate::PcmSink for PcmPlayback<'_> {
    fn write(&self, samples: &[i16]) -> Result<(), String> {
        PcmPlayback::write(self, samples).map(|_| ())
    }

    fn drain(&self) {
        PcmPlayback::drain(self)
    }
}

impl Drop for PcmPlayback<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.alsa.pcm_close)(self.pcm);
        }
    }
}
