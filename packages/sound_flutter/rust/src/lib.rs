//! C ABI for `sound_dart`'s native playback.
//!
//! The Dart `FfiBackend` calls these functions. Each `sound_play_*` call
//! decodes the audio and spawns a playback thread, returning an opaque voice
//! id that can be polled, stopped, and freed. Concurrent voices are mixed by
//! the system audio server (PipeWire/PulseAudio/dmix on Linux).
//!
//! `sound_play_url` streams its source rather than decoding it up front: a
//! background thread pulls bytes from the network (`net`/`streaming_source`)
//! while `decode`'s `StreamDecoder` decodes packets as they arrive and
//! `play_pcm` consumes them from a growing [`LiveBuffer`], so playback can
//! start before the whole file has downloaded.

// The exported functions take the opaque `*mut Player` returned by
// `sound_player_new`; the caller is responsible for its validity. That makes
// the raw-pointer derefs expected here rather than a smell.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod decode;
mod net;
mod streaming_source;

#[cfg(target_os = "linux")]
mod alsa;

#[cfg(target_os = "android")]
mod aaudio;

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod coreaudio;

#[cfg(any(target_os = "macos", target_os = "ios", test))]
mod bounded_send;

// The macOS output sink (feeds a named device, e.g. a virtual-mic loopback).
#[cfg(target_os = "macos")]
mod sink;

#[cfg(target_os = "windows")]
mod winmm;

#[cfg(any(target_os = "windows", test))]
mod winmm_name;

#[cfg(target_os = "macos")]
use sink::Sink as NamedOutputSink;
#[cfg(target_os = "windows")]
use winmm::NamedSink as NamedOutputSink;

/// A platform audio output that accepts interleaved S16LE frames.
pub(crate) trait PcmSink {
    /// Writes one interleaved chunk, blocking until accepted.
    fn write(&self, samples: &[i16]) -> Result<(), String>;
    /// Blocks until buffered audio has finished playing.
    fn drain(&self);
}

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use decode::{DecodedAudio, StreamDecoder};

// Voice state codes, mirrored by the Dart side.
const STATE_PLAYING: u8 = 0;
const STATE_COMPLETED: u8 = 1;
const STATE_STOPPED: u8 = 2;
const STATE_ERROR: u8 = 3;
const STATE_PAUSED: u8 = 4;
/// A [sound_play_url] voice that has been created but hasn't yet connected,
/// probed the format, and decoded a first chunk - all of which happen on its
/// background thread rather than blocking [sound_play_url] itself. Callers
/// polling [sound_voice_state] see this before [STATE_PLAYING].
const STATE_CONNECTING: u8 = 5;

// Sentinel for "no seek pending" in the seek_request slot.
const NO_SEEK: i64 = -1;

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_last_error(msg: impl Into<String>) {
    let c = CString::new(msg.into()).unwrap_or_default();
    LAST_ERROR.with(|e| *e.borrow_mut() = c);
}

/// Runs an entry point's body, turning a panic into `fallback` plus an error
/// message. Unwinding out of an `extern "C"` fn aborts the host process, so
/// the entry points that decode or spawn threads go through this.
fn catch_panic<T>(fallback: T, on_panic: impl FnOnce(String), f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|payload| {
        on_panic(format!("internal error: {}", panic_message(&*payload)));
        fallback
    })
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s
    } else {
        "panic"
    }
}

/// Controls shared between a voice's playback thread and the C ABI.
#[derive(Clone)]
struct VoiceShared {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    // Linear gain stored as f32 bits; read by the playback thread per chunk.
    volume: Arc<AtomicU32>,
    // When set, the playback thread restarts from the beginning at end of audio.
    looping: Arc<AtomicBool>,
    // Pending seek target in frames, or NO_SEEK when none is pending.
    seek_request: Arc<AtomicI64>,
    // Current playback position in frames.
    position: Arc<AtomicU64>,
}

impl VoiceShared {
    fn new(looping: bool) -> VoiceShared {
        VoiceShared {
            stop: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            state: Arc::new(AtomicU8::new(STATE_PLAYING)),
            error: Arc::new(Mutex::new(None)),
            volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            looping: Arc::new(AtomicBool::new(looping)),
            seek_request: Arc::new(AtomicI64::new(NO_SEEK)),
            position: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Whether the playback loop has something to act on besides new audio:
    /// a stop, a pause, or a pending seek.
    fn interrupted(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
            || self.paused.load(Ordering::SeqCst)
            || self.seek_request.load(Ordering::SeqCst) != NO_SEEK
    }
}

/// Interleaved S16LE samples fed by a concurrent decode thread (for
/// [`sound_play_url`]) rather than fully available up front. `play_pcm`
/// blocks on [Condvar] when it catches up to the decode thread, and stops
/// treating "index == len" as end-of-stream until `final_len` is set.
struct LiveBuffer {
    state: Mutex<LiveBufferState>,
    ready: std::sync::Condvar,
}

struct LiveBufferState {
    samples: Vec<i16>,
    /// Set once decoding finishes (successfully) with the true sample count;
    /// `None` while still growing.
    final_len: Option<usize>,
    /// Set if decoding failed after producing `samples`.
    error: Option<String>,
}

impl LiveBuffer {
    fn new() -> Arc<LiveBuffer> {
        Arc::new(LiveBuffer {
            state: Mutex::new(LiveBufferState {
                samples: Vec::new(),
                final_len: None,
                error: None,
            }),
            ready: std::sync::Condvar::new(),
        })
    }

    fn push(&self, chunk: &[i16]) {
        let mut state = self.state.lock().unwrap();
        state.samples.extend_from_slice(chunk);
        self.ready.notify_all();
    }

    fn finish(&self, error: Option<String>) {
        let mut state = self.state.lock().unwrap();
        state.final_len = Some(state.samples.len());
        state.error = error;
        self.ready.notify_all();
    }

    /// Blocks until at least `upto` samples are available, decoding has
    /// finished, decoding has failed, or the voice is interrupted (stopped,
    /// paused, or seeked). The interrupt check is why this polls with a timed
    /// wait: a stalled network feeder may never notify again, and the voice
    /// must still respond to its controls.
    fn wait_until(&self, upto: usize, sh: &VoiceShared) {
        let mut state = self.state.lock().unwrap();
        while state.samples.len() < upto && state.final_len.is_none() && !sh.interrupted() {
            let (s, _) = self
                .ready
                .wait_timeout(state, std::time::Duration::from_millis(50))
                .unwrap();
            state = s;
        }
    }
}

/// Either a complete, already-decoded buffer (bytes/file playback) or one
/// still being filled by a concurrent decode thread (URL playback). Lets
/// [play_pcm] serve both without duplicating the chunk/gain/seek/loop logic.
enum Samples {
    Complete(Arc<Vec<i16>>),
    Live(Arc<LiveBuffer>),
}

impl Samples {
    /// Blocks until at least `upto` samples exist, no more are coming, or the
    /// voice is interrupted, then returns the number currently available and
    /// whether that number is final (no more will ever be produced). Fewer
    /// than `upto` non-final samples means an interrupt cut the wait short.
    fn wait_len(&self, upto: usize, sh: &VoiceShared) -> (usize, bool) {
        match self {
            Samples::Complete(v) => (v.len(), true),
            Samples::Live(live) => {
                live.wait_until(upto, sh);
                let state = live.state.lock().unwrap();
                (state.samples.len(), state.final_len.is_some())
            }
        }
    }

    /// Copies `range` into `out`. Panics if the range isn't yet available;
    /// callers must `wait_len` first.
    fn copy_range(&self, range: std::ops::Range<usize>, out: &mut Vec<i16>) {
        match self {
            Samples::Complete(v) => out.extend_from_slice(&v[range]),
            Samples::Live(live) => {
                let state = live.state.lock().unwrap();
                out.extend_from_slice(&state.samples[range]);
            }
        }
    }

    fn take_error(&self) -> Option<String> {
        match self {
            Samples::Complete(_) => None,
            Samples::Live(live) => live.state.lock().unwrap().error.clone(),
        }
    }
}

struct Voice {
    shared: VoiceShared,
    rate: Arc<AtomicU32>,
    total_frames: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

/// Owns the audio backend and the table of live voices.
pub struct Player {
    voices: Mutex<HashMap<u64, Voice>>,
    next_id: AtomicU64,
    #[cfg(target_os = "linux")]
    alsa: Arc<alsa::Alsa>,
    #[cfg(target_os = "android")]
    aaudio: Arc<aaudio::Aaudio>,
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    coreaudio: Arc<coreaudio::CoreAudio>,
    #[cfg(target_os = "windows")]
    winmm: Arc<winmm::WinMM>,
}

impl Player {
    fn new() -> Result<Player, String> {
        Ok(Player {
            voices: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            #[cfg(target_os = "linux")]
            alsa: Arc::new(alsa::Alsa::load()?),
            #[cfg(target_os = "android")]
            aaudio: Arc::new(aaudio::Aaudio::load()?),
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            coreaudio: Arc::new(coreaudio::CoreAudio::load()?),
            #[cfg(target_os = "windows")]
            winmm: Arc::new(winmm::WinMM::load()?),
        })
    }

    fn spawn(&self, audio: DecodedAudio, looping: bool) -> u64 {
        self.spawn_shared(Arc::new(audio.samples), audio.channels, audio.rate, looping)
    }

    /// Starts a voice over a complete sample buffer that may be shared with
    /// other voices or a [Decoded] handle; nothing is copied.
    fn spawn_shared(&self, samples: Arc<Vec<i16>>, channels: u32, rate: u32, looping: bool) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let shared = VoiceShared::new(looping);
        let rate_atomic = Arc::new(AtomicU32::new(rate));
        let frame_width = channels.max(1) as usize;
        let total_frames = Arc::new(AtomicU64::new((samples.len() / frame_width) as u64));

        let handle = self.start_thread(channels, rate, Samples::Complete(samples), shared.clone());

        self.voices.lock().unwrap().insert(
            id,
            Voice {
                shared,
                rate: rate_atomic,
                total_frames,
                handle: Some(handle),
            },
        );
        id
    }

    /// Returns a voice id immediately and does everything else -
    /// connect, probe, decode the first chunk, then open the platform sink
    /// and play - on a background thread, so this never blocks the caller on
    /// network I/O. Callers poll [sound_voice_state]: it reports
    /// [STATE_CONNECTING] until the first chunk decodes (or [STATE_ERROR] if
    /// that fails, with [sound_voice_error] carrying the reason), then
    /// behaves like any other voice.
    fn spawn_url(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        format_hint: Option<String>,
        looping: bool,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let shared = VoiceShared::new(looping);
        shared.state.store(STATE_CONNECTING, Ordering::SeqCst);
        let rate = Arc::new(AtomicU32::new(0));
        let total_frames = Arc::new(AtomicU64::new(0));

        let handle = self.start_thread_url(
            url,
            headers,
            format_hint,
            rate.clone(),
            total_frames.clone(),
            shared.clone(),
        );

        self.voices.lock().unwrap().insert(
            id,
            Voice {
                shared,
                rate,
                total_frames,
                handle: Some(handle),
            },
        );
        id
    }

    #[cfg(target_os = "linux")]
    fn start_thread(
        &self,
        channels: u32,
        rate: u32,
        samples: Samples,
        shared: VoiceShared,
    ) -> JoinHandle<()> {
        let alsa = self.alsa.clone();
        std::thread::spawn(move || {
            let sink = alsa::PcmPlayback::open(&alsa, channels, rate);
            run_voice(sink, channels, samples, &shared);
        })
    }

    #[cfg(target_os = "android")]
    fn start_thread(
        &self,
        channels: u32,
        rate: u32,
        samples: Samples,
        shared: VoiceShared,
    ) -> JoinHandle<()> {
        let aaudio = self.aaudio.clone();
        std::thread::spawn(move || {
            let sink = aaudio::AaudioPlayback::open(&aaudio, channels, rate);
            run_voice(sink, channels, samples, &shared);
        })
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn start_thread(
        &self,
        channels: u32,
        rate: u32,
        samples: Samples,
        shared: VoiceShared,
    ) -> JoinHandle<()> {
        let ca = self.coreaudio.clone();
        std::thread::spawn(move || {
            let sink = coreaudio::CoreAudioPlayback::open(&ca, channels, rate);
            run_voice(sink, channels, samples, &shared);
        })
    }

    #[cfg(target_os = "windows")]
    fn start_thread(
        &self,
        channels: u32,
        rate: u32,
        samples: Samples,
        shared: VoiceShared,
    ) -> JoinHandle<()> {
        let wmm = self.winmm.clone();
        std::thread::spawn(move || {
            let sink = winmm::WinMMPlayback::open(wmm, channels, rate);
            run_voice(sink, channels, samples, &shared);
        })
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    )))]
    fn start_thread(
        &self,
        _channels: u32,
        _rate: u32,
        _samples: Samples,
        shared: VoiceShared,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            *shared.error.lock().unwrap() =
                Some("native playback is not implemented on this platform yet".into());
            shared.state.store(STATE_ERROR, Ordering::SeqCst);
        })
    }

    #[cfg(target_os = "linux")]
    fn start_thread_url(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        format_hint: Option<String>,
        rate: Arc<AtomicU32>,
        total_frames: Arc<AtomicU64>,
        shared: VoiceShared,
    ) -> JoinHandle<()> {
        let alsa = self.alsa.clone();
        std::thread::spawn(move || {
            run_voice_url(
                url,
                headers,
                format_hint,
                rate,
                total_frames,
                &shared,
                |c, r| alsa::PcmPlayback::open(&alsa, c, r),
            );
        })
    }

    #[cfg(target_os = "android")]
    fn start_thread_url(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        format_hint: Option<String>,
        rate: Arc<AtomicU32>,
        total_frames: Arc<AtomicU64>,
        shared: VoiceShared,
    ) -> JoinHandle<()> {
        let aaudio = self.aaudio.clone();
        std::thread::spawn(move || {
            run_voice_url(
                url,
                headers,
                format_hint,
                rate,
                total_frames,
                &shared,
                |c, r| aaudio::AaudioPlayback::open(&aaudio, c, r),
            );
        })
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn start_thread_url(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        format_hint: Option<String>,
        rate: Arc<AtomicU32>,
        total_frames: Arc<AtomicU64>,
        shared: VoiceShared,
    ) -> JoinHandle<()> {
        let ca = self.coreaudio.clone();
        std::thread::spawn(move || {
            run_voice_url(
                url,
                headers,
                format_hint,
                rate,
                total_frames,
                &shared,
                |c, r| coreaudio::CoreAudioPlayback::open(&ca, c, r),
            );
        })
    }

    #[cfg(target_os = "windows")]
    fn start_thread_url(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        format_hint: Option<String>,
        rate: Arc<AtomicU32>,
        total_frames: Arc<AtomicU64>,
        shared: VoiceShared,
    ) -> JoinHandle<()> {
        let wmm = self.winmm.clone();
        std::thread::spawn(move || {
            run_voice_url(
                url,
                headers,
                format_hint,
                rate,
                total_frames,
                &shared,
                |c, r| winmm::WinMMPlayback::open(wmm, c, r),
            );
        })
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    )))]
    fn start_thread_url(
        &self,
        _url: String,
        _headers: Vec<(String, String)>,
        _format_hint: Option<String>,
        _rate: Arc<AtomicU32>,
        _total_frames: Arc<AtomicU64>,
        shared: VoiceShared,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            *shared.error.lock().unwrap() =
                Some("native playback is not implemented on this platform yet".into());
            shared.state.store(STATE_ERROR, Ordering::SeqCst);
        })
    }
}

/// Connects, probes, and decodes the first chunk of `url` entirely on the
/// caller's (background) thread, then opens the platform sink via `open_sink`
/// once the format is known and runs the normal chunked playback loop. Used
/// by every platform's `start_thread_url` so the connect/probe/decode steps
/// aren't duplicated five times; only sink construction differs per OS.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
))]
fn run_voice_url<S: PcmSink>(
    url: String,
    headers: Vec<(String, String)>,
    format_hint: Option<String>,
    rate: Arc<AtomicU32>,
    total_frames: Arc<AtomicU64>,
    sh: &VoiceShared,
    open_sink: impl FnOnce(u32, u32) -> Result<S, String>,
) {
    // The voice's stop flag doubles as the download-abort signal, so freeing
    // or stopping the voice cancels the fetch and unblocks the probe/decode.
    // The resulting error is just the stop, not a failure.
    let fail = |msg: String| {
        if sh.stop.load(Ordering::SeqCst) {
            sh.state.store(STATE_STOPPED, Ordering::SeqCst);
        } else {
            *sh.error.lock().unwrap() = Some(msg);
            sh.state.store(STATE_ERROR, Ordering::SeqCst);
        }
    };
    let mut decoder =
        match decode::open_url_decoder(url, headers, format_hint.as_deref(), sh.stop.clone()) {
            Ok(d) => d,
            Err(e) => return fail(e),
        };
    let first_chunk = match decoder.next_chunk() {
        Ok(Some(chunk)) => chunk,
        Ok(None) => return fail("stream ended before any audio decoded".to_string()),
        Err(e) => return fail(e),
    };
    if decoder.channels == 0 || decoder.rate == 0 {
        return fail("decoded zero audio frames".to_string());
    }

    let channels = decoder.channels;
    let decoder_rate = decoder.rate;
    rate.store(decoder_rate, Ordering::SeqCst);
    let live = LiveBuffer::new();
    live.push(&first_chunk);
    total_frames.store(
        (first_chunk.len() / channels.max(1) as usize) as u64,
        Ordering::SeqCst,
    );
    spawn_decode_feeder(
        decoder,
        live.clone(),
        channels,
        first_chunk.len(),
        total_frames,
        sh.stop.clone(),
    );
    // However playback ends (including a sink error), stop the feeder and the
    // download behind it rather than letting them run until the voice is
    // freed.
    let _stop_feeder = SetOnDrop(&sh.stop);

    let sink = open_sink(channels, decoder_rate);
    run_voice(sink, channels, Samples::Live(live), sh);
}

/// Sets the flag when dropped.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Decodes the rest of `decoder`'s stream on a background thread, pushing
/// each chunk into `live` and growing `total_frames` as frames become known.
/// `already_decoded` counts samples decoded before the call; the total is
/// capped at [decode::MAX_DECODED_SAMPLES].
/// Runs independently of the playback thread, which only ever reads `live`.
/// Exits when the stream ends, decoding fails, or `stop` is set (so a
/// stopped/freed voice does not keep decoding -- and, via the decoder's
/// streaming source, downloading -- an unbounded stream forever).
fn spawn_decode_feeder(
    mut decoder: StreamDecoder,
    live: Arc<LiveBuffer>,
    channels: u32,
    already_decoded: usize,
    total_frames: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let channels = channels.max(1) as u64;
        let mut decoded = already_decoded;
        let fed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loop {
            if stop.load(Ordering::SeqCst) {
                return None;
            }
            match decoder.next_chunk() {
                Ok(Some(chunk)) => {
                    // Samples already pushed still play before this is reported.
                    if chunk.len() > decode::MAX_DECODED_SAMPLES.saturating_sub(decoded) {
                        return Some(decode::DECODED_LIMIT_ERROR.to_string());
                    }
                    decoded += chunk.len();
                    live.push(&chunk);
                    total_frames.fetch_add(chunk.len() as u64 / channels, Ordering::SeqCst);
                }
                Ok(None) => return None,
                Err(e) => return Some(e),
            }
        }));
        // Always finish, even after a panic, or playback would wait on the
        // buffer until the voice is stopped.
        live.finish(
            fed.unwrap_or_else(|p| Some(format!("internal error: {}", panic_message(&*p)))),
        );
    });
}

/// Opens the result of a platform sink and, on success, runs the playback loop.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
))]
fn run_voice<S: PcmSink>(
    sink: Result<S, String>,
    channels: u32,
    samples: Samples,
    sh: &VoiceShared,
) {
    let sink = match sink {
        Ok(s) => s,
        Err(e) => {
            *sh.error.lock().unwrap() = Some(e);
            sh.state.store(STATE_ERROR, Ordering::SeqCst);
            return;
        }
    };
    // A panic would otherwise leave the voice reporting its last state
    // (e.g. PLAYING) forever.
    let played = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        play_pcm(&sink, channels, samples, sh)
    }));
    if let Err(p) = played {
        let msg = format!("internal error: {}", panic_message(&*p));
        *sh.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
        sh.state.store(STATE_ERROR, Ordering::SeqCst);
    }
}

/// Platform-independent playback loop: chunks the samples, applies gain, honors
/// stop/pause/seek/loop, tracks position, and drains at the end.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
))]
fn play_pcm<S: PcmSink>(pcm: &S, channels: u32, samples: Samples, sh: &VoiceShared) {
    let channels = channels.max(1) as usize;
    // ~2048 frames per write keeps stop latency under ~50 ms at 44.1 kHz.
    let chunk = channels * 2048;
    let mut scaled: Vec<i16> = Vec::new();
    let mut slice: Vec<i16> = Vec::new();
    let mut i = 0usize;
    loop {
        if sh.stop.load(Ordering::SeqCst) {
            sh.state.store(STATE_STOPPED, Ordering::SeqCst);
            return;
        }

        // Honor a pending seek (frame index -> sample index). A target past
        // what a still-streaming `Samples::Live` has decoded is waited for
        // below, like any other not-yet-decoded audio.
        let seek = sh.seek_request.swap(NO_SEEK, Ordering::SeqCst);
        if seek >= 0 {
            i = (seek as usize).saturating_mul(channels);
            sh.position.store(seek as u64, Ordering::SeqCst);
        }

        // While paused, emit nothing (the device underruns to silence and
        // recovers on the next write) and poll for resume/stop/seek.
        if sh.paused.load(Ordering::SeqCst) {
            sh.state.store(STATE_PAUSED, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }

        // Blocks (for `Samples::Live`) until either one more sample exists,
        // decoding has finished with nothing more to give, or the voice is
        // stopped, paused, or seeked (handled at the top of the loop).
        let (available, is_final) = samples.wait_len(i.saturating_add(1), sh);
        if sh.stop.load(Ordering::SeqCst) {
            sh.state.store(STATE_STOPPED, Ordering::SeqCst);
            return;
        }

        if i >= available {
            if !is_final {
                continue;
            }
            // A seek may have landed past the end.
            i = available;
            sh.position.store((i / channels) as u64, Ordering::SeqCst);
            // A download/decode failure only ends the voice once the audio
            // decoded before it has played out.
            if let Some(e) = samples.take_error() {
                *sh.error.lock().unwrap() = Some(e);
                sh.state.store(STATE_ERROR, Ordering::SeqCst);
                return;
            }
            // Looping nothing would spin without ever writing or sleeping.
            if sh.looping.load(Ordering::SeqCst) && available > 0 {
                i = 0;
                sh.position.store(0, Ordering::SeqCst);
                continue;
            }
            break;
        }

        sh.state.store(STATE_PLAYING, Ordering::SeqCst);
        let end = (i + chunk).min(available);
        slice.clear();
        samples.copy_range(i..end, &mut slice);
        // Apply linear gain unless it is effectively unity.
        let gain = f32::from_bits(sh.volume.load(Ordering::Relaxed));
        let to_write: &[i16] = if (gain - 1.0).abs() < 1e-4 {
            &slice
        } else {
            scaled.clear();
            scaled.extend(
                slice
                    .iter()
                    .map(|&s| (s as f32 * gain).clamp(i16::MIN as f32, i16::MAX as f32) as i16),
            );
            &scaled
        };
        if let Err(e) = pcm.write(to_write) {
            *sh.error.lock().unwrap() = Some(e);
            sh.state.store(STATE_ERROR, Ordering::SeqCst);
            return;
        }
        i = end;
        sh.position.store((i / channels) as u64, Ordering::SeqCst);
    }
    pcm.drain();
    let final_state = if sh.stop.load(Ordering::SeqCst) {
        STATE_STOPPED
    } else {
        STATE_COMPLETED
    };
    sh.state.store(final_state, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// C ABI
// ---------------------------------------------------------------------------

/// Creates a player. Returns null on failure; see [sound_last_error].
#[no_mangle]
pub extern "C" fn sound_player_new() -> *mut Player {
    match Player::new() {
        Ok(p) => Box::into_raw(Box::new(p)),
        Err(e) => {
            set_last_error(e);
            std::ptr::null_mut()
        }
    }
}

/// Frees a player created by [sound_player_new], stopping all voices.
#[no_mangle]
pub extern "C" fn sound_player_free(player: *mut Player) {
    if player.is_null() {
        return;
    }
    let player = unsafe { Box::from_raw(player) };
    let mut voices = player.voices.lock().unwrap();
    for (_, mut voice) in voices.drain() {
        voice.shared.stop.store(true, Ordering::SeqCst);
        if let Some(h) = voice.handle.take() {
            let _ = h.join();
        }
    }
}

fn with_player<'a>(player: *mut Player) -> Option<&'a Player> {
    if player.is_null() {
        set_last_error("null player");
        None
    } else {
        Some(unsafe { &*player })
    }
}

/// Decodes encoded audio bytes (WAV/MP3/OGG/FLAC, auto-detected) and starts
/// playback. Returns a voice id, or 0 on error.
///
/// # Safety
/// `data` must point to `len` readable bytes. `format` is an optional
/// NUL-terminated extension hint (e.g. "mp3"); pass null to auto-detect.
/// `looping` is nonzero to repeat from the start at end of audio.
#[no_mangle]
pub unsafe extern "C" fn sound_play_bytes(
    player: *mut Player,
    data: *const u8,
    len: usize,
    format: *const c_char,
    looping: c_int,
) -> u64 {
    catch_panic(0, set_last_error, || {
        let Some(player) = with_player(player) else {
            return 0;
        };
        if data.is_null() {
            set_last_error("null data");
            return 0;
        }
        let bytes = std::slice::from_raw_parts(data, len);
        let hint = if format.is_null() {
            None
        } else {
            CStr::from_ptr(format).to_str().ok()
        };
        match decode::decode_bytes(bytes, hint) {
            Ok(audio) => player.spawn(audio, looping != 0),
            Err(e) => {
                set_last_error(e);
                0
            }
        }
    })
}

/// Decodes an audio file (format auto-detected) and starts playback. Returns a
/// voice id, or 0 on error.
///
/// # Safety
/// `path` must be a valid NUL-terminated C string. `looping` is nonzero to
/// repeat from the start at end of audio.
#[no_mangle]
pub unsafe extern "C" fn sound_play_file(
    player: *mut Player,
    path: *const c_char,
    looping: c_int,
) -> u64 {
    catch_panic(0, set_last_error, || {
        let Some(player) = with_player(player) else {
            return 0;
        };
        if path.is_null() {
            set_last_error("null path");
            return 0;
        }
        let path = match CStr::from_ptr(path).to_str() {
            Ok(p) => p,
            Err(_) => {
                set_last_error("path is not valid UTF-8");
                return 0;
            }
        };
        match decode::decode_file(path) {
            Ok(audio) => player.spawn(audio, looping != 0),
            Err(e) => {
                set_last_error(e);
                0
            }
        }
    })
}

/// Streams and plays the audio at `url` (`http://`/`https://`), sending
/// `header_count` header name/value pairs (e.g. an `Authorization: Bearer
/// ...` token) with the request. Playback starts once enough of the
/// response has downloaded to know the stream's format, without waiting for
/// the whole file — see the `net`/`streaming_source`/`decode` modules.
///
/// Always returns a nonzero voice id without blocking on any network I/O:
/// connecting, probing the format, and decoding the first chunk all happen
/// on the voice's own background thread after this returns, exactly like
/// the rest of playback. Poll [sound_voice_state]: it reports
/// [STATE_CONNECTING] until the first chunk decodes, then behaves like any
/// other voice; a connect/format failure surfaces as [STATE_ERROR] with the
/// reason in [sound_voice_error], the same as a failure partway through an
/// already-playing stream.
///
/// Memory note: both the compressed download and the decoded PCM are held in
/// memory for the voice's lifetime (that is what makes seek/loop instant), so
/// this suits tracks, not open-ended live streams. Both are capped: a
/// download past 256 MiB fails, and so does audio that decodes to more than
/// 512 MiB of PCM (about 50 minutes of 44.1 kHz stereo), after any audio
/// already decoded has played.
///
/// # Safety
/// `url` must be a valid NUL-terminated UTF-8 C string. `header_names` and
/// `header_values` must each point to `header_count` valid NUL-terminated C
/// strings (parallel arrays: `header_names[i]`/`header_values[i]` is one
/// header). `format` is an optional NUL-terminated extension hint (e.g.
/// "mp3"); pass null to let the probe detect the container from the bytes.
/// `looping` is nonzero to repeat from the start at end of audio.
#[no_mangle]
pub unsafe extern "C" fn sound_play_url(
    player: *mut Player,
    url: *const c_char,
    header_names: *const *const c_char,
    header_values: *const *const c_char,
    header_count: usize,
    format: *const c_char,
    looping: c_int,
) -> u64 {
    catch_panic(0, set_last_error, || {
        let Some(player) = with_player(player) else {
            return 0;
        };
        if url.is_null() {
            set_last_error("null url");
            return 0;
        }
        let url = match CStr::from_ptr(url).to_str() {
            Ok(u) => u.to_string(),
            Err(_) => {
                set_last_error("url is not valid UTF-8");
                return 0;
            }
        };
        let headers = match read_headers(header_names, header_values, header_count) {
            Ok(h) => h,
            Err(e) => {
                set_last_error(e);
                return 0;
            }
        };
        let format_hint = if format.is_null() {
            None
        } else {
            match CStr::from_ptr(format).to_str() {
                Ok(f) => Some(f.to_string()),
                Err(_) => {
                    set_last_error("format hint is not valid UTF-8");
                    return 0;
                }
            }
        };
        player.spawn_url(url, headers, format_hint, looping != 0)
    })
}

/// Reads `count` parallel NUL-terminated C string pairs from `names`/`values`
/// into owned `(String, String)`s.
///
/// # Safety
/// See [sound_play_url]'s safety section for the pointer contract.
unsafe fn read_headers(
    names: *const *const c_char,
    values: *const *const c_char,
    count: usize,
) -> Result<Vec<(String, String)>, String> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if names.is_null() || values.is_null() {
        return Err("null header arrays with nonzero header_count".to_string());
    }
    let mut headers = Vec::with_capacity(count);
    for i in 0..count {
        let name = CStr::from_ptr(*names.add(i))
            .to_str()
            .map_err(|_| format!("header name {i} is not valid UTF-8"))?;
        let value = CStr::from_ptr(*values.add(i))
            .to_str()
            .map_err(|_| format!("header value {i} is not valid UTF-8"))?;
        headers.push((name.to_string(), value.to_string()));
    }
    Ok(headers)
}

/// Returns the voice state (0 playing, 1 completed, 2 stopped, 3 error,
/// 4 paused, 5 connecting), or -1 if the id is unknown.
#[no_mangle]
pub extern "C" fn sound_voice_state(player: *mut Player, id: u64) -> c_int {
    let Some(player) = with_player(player) else {
        return -1;
    };
    let voices = player.voices.lock().unwrap();
    match voices.get(&id) {
        Some(v) => v.shared.state.load(Ordering::SeqCst) as c_int,
        None => -1,
    }
}

/// Signals a voice to stop. Returns 0 on success, -1 if the id is unknown.
#[no_mangle]
pub extern "C" fn sound_stop(player: *mut Player, id: u64) -> c_int {
    let Some(player) = with_player(player) else {
        return -1;
    };
    let voices = player.voices.lock().unwrap();
    match voices.get(&id) {
        Some(v) => {
            v.shared.stop.store(true, Ordering::SeqCst);
            0
        }
        None => -1,
    }
}

/// Pauses a voice. Returns 0 on success, -1 if the id is unknown.
#[no_mangle]
pub extern "C" fn sound_pause(player: *mut Player, id: u64) -> c_int {
    let Some(player) = with_player(player) else {
        return -1;
    };
    let voices = player.voices.lock().unwrap();
    match voices.get(&id) {
        Some(v) => {
            v.shared.paused.store(true, Ordering::SeqCst);
            0
        }
        None => -1,
    }
}

/// Resumes a paused voice. Returns 0 on success, -1 if the id is unknown.
#[no_mangle]
pub extern "C" fn sound_resume(player: *mut Player, id: u64) -> c_int {
    let Some(player) = with_player(player) else {
        return -1;
    };
    let voices = player.voices.lock().unwrap();
    match voices.get(&id) {
        Some(v) => {
            v.shared.paused.store(false, Ordering::SeqCst);
            0
        }
        None => -1,
    }
}

/// Seeks a voice to `frame`. Returns 0 on success, -1 if the id is unknown.
#[no_mangle]
pub extern "C" fn sound_seek(player: *mut Player, id: u64, frame: u64) -> c_int {
    let Some(player) = with_player(player) else {
        return -1;
    };
    let voices = player.voices.lock().unwrap();
    match voices.get(&id) {
        Some(v) => {
            v.shared
                .seek_request
                .store(frame.min(i64::MAX as u64) as i64, Ordering::SeqCst);
            0
        }
        None => -1,
    }
}

/// Returns a voice's current position in frames, or -1 if the id is unknown.
#[no_mangle]
pub extern "C" fn sound_position(player: *mut Player, id: u64) -> i64 {
    let Some(player) = with_player(player) else {
        return -1;
    };
    let voices = player.voices.lock().unwrap();
    match voices.get(&id) {
        Some(v) => v.shared.position.load(Ordering::SeqCst) as i64,
        None => -1,
    }
}

/// Returns a voice's total length in frames, or -1 if the id is unknown.
///
/// For a voice started with [sound_play_url], this is the number of frames
/// *decoded so far*, not the true total, until the stream finishes
/// downloading — callers polling this during URL playback should expect it
/// to grow, the same way a browser's progress bar grows for a streamed
/// media element before its duration is fully known.
#[no_mangle]
pub extern "C" fn sound_duration_frames(player: *mut Player, id: u64) -> i64 {
    let Some(player) = with_player(player) else {
        return -1;
    };
    let voices = player.voices.lock().unwrap();
    match voices.get(&id) {
        Some(v) => v.total_frames.load(Ordering::SeqCst) as i64,
        None => -1,
    }
}

/// Returns a voice's sample rate in Hz, or -1 if the id is unknown.
#[no_mangle]
pub extern "C" fn sound_sample_rate(player: *mut Player, id: u64) -> c_int {
    let Some(player) = with_player(player) else {
        return -1;
    };
    let voices = player.voices.lock().unwrap();
    match voices.get(&id) {
        Some(v) => v.rate.load(Ordering::SeqCst) as c_int,
        None => -1,
    }
}

/// Sets a voice's linear volume (1.0 = original). Values above 1.0 amplify and
/// are clamped per-sample. Returns 0 on success, -1 if the id is unknown.
#[no_mangle]
pub extern "C" fn sound_set_volume(player: *mut Player, id: u64, volume: f32) -> c_int {
    let Some(player) = with_player(player) else {
        return -1;
    };
    let voices = player.voices.lock().unwrap();
    match voices.get(&id) {
        Some(v) => {
            v.shared
                .volume
                .store(volume.max(0.0).to_bits(), Ordering::Relaxed);
            0
        }
        None => -1,
    }
}

/// Turns looping on (nonzero) or off for a voice. Turning it off lets the
/// current pass finish and then completes. Returns 0 on success, -1 if unknown.
#[no_mangle]
pub extern "C" fn sound_set_loop(player: *mut Player, id: u64, looping: c_int) -> c_int {
    let Some(player) = with_player(player) else {
        return -1;
    };
    let voices = player.voices.lock().unwrap();
    match voices.get(&id) {
        Some(v) => {
            v.shared.looping.store(looping != 0, Ordering::SeqCst);
            0
        }
        None => -1,
    }
}

/// Joins a voice's thread and removes it. Returns 0 on success, -1 if unknown.
#[no_mangle]
pub extern "C" fn sound_voice_free(player: *mut Player, id: u64) -> c_int {
    let Some(player) = with_player(player) else {
        return -1;
    };
    let voice = player.voices.lock().unwrap().remove(&id);
    match voice {
        Some(mut v) => {
            v.shared.stop.store(true, Ordering::SeqCst);
            if let Some(h) = v.handle.take() {
                let _ = h.join();
            }
            0
        }
        None => -1,
    }
}

/// Copies a voice's error message into the thread-local error buffer and
/// returns it, or returns an empty string if there is none (or the id is
/// unknown).
#[no_mangle]
pub extern "C" fn sound_voice_error(player: *mut Player, id: u64) -> *const c_char {
    if let Some(player) = with_player(player) {
        let voices = player.voices.lock().unwrap();
        let msg = voices
            .get(&id)
            .and_then(|v| v.shared.error.lock().unwrap().clone());
        set_last_error(msg.unwrap_or_default());
    }
    sound_last_error()
}

/// Returns the most recent error message on the calling thread.
#[no_mangle]
pub extern "C" fn sound_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

// ---------------------------------------------------------------------------
// Decode-to-PCM (for the sound_dart PCM tap)
// ---------------------------------------------------------------------------

/// A decoded interleaved S16LE buffer handed to the Dart PCM tap.
///
/// This is decoupled from playback: it never opens an audio device, so the
/// frames can be pulled (for example to mix a clip into a capture stream)
/// without any speaker output. It reuses the same `decode` path the playback
/// functions use.
pub struct Decoded {
    samples: Arc<Vec<i16>>,
    channels: u32,
    rate: u32,
}

/// Decodes encoded audio bytes (WAV/MP3/OGG/FLAC, auto-detected) into an opaque
/// [Decoded] buffer. Returns null on error; see [sound_last_error].
///
/// # Safety
/// `data` must point to `len` readable bytes. `format` is an optional
/// NUL-terminated extension hint (e.g. "mp3"); pass null to auto-detect. Free
/// the result with [sound_decoded_free].
#[no_mangle]
pub unsafe extern "C" fn sound_decode_bytes(
    data: *const u8,
    len: usize,
    format: *const c_char,
) -> *mut Decoded {
    catch_panic(std::ptr::null_mut(), set_last_error, || {
        if data.is_null() {
            set_last_error("null data");
            return std::ptr::null_mut();
        }
        let bytes = std::slice::from_raw_parts(data, len);
        let hint = if format.is_null() {
            None
        } else {
            CStr::from_ptr(format).to_str().ok()
        };
        match decode::decode_bytes(bytes, hint) {
            Ok(a) => Box::into_raw(Box::new(Decoded {
                samples: Arc::new(a.samples),
                channels: a.channels,
                rate: a.rate,
            })),
            Err(e) => {
                set_last_error(e);
                std::ptr::null_mut()
            }
        }
    })
}

/// Decodes an audio file (format auto-detected) into an opaque [Decoded]
/// buffer. Returns null on error; see [sound_last_error].
///
/// # Safety
/// `path` must be a valid NUL-terminated C string. Free the result with
/// [sound_decoded_free].
#[no_mangle]
pub unsafe extern "C" fn sound_decode_file(path: *const c_char) -> *mut Decoded {
    catch_panic(std::ptr::null_mut(), set_last_error, || {
        if path.is_null() {
            set_last_error("null path");
            return std::ptr::null_mut();
        }
        let path = match CStr::from_ptr(path).to_str() {
            Ok(p) => p,
            Err(_) => {
                set_last_error("path is not valid UTF-8");
                return std::ptr::null_mut();
            }
        };
        match decode::decode_file(path) {
            Ok(a) => Box::into_raw(Box::new(Decoded {
                samples: Arc::new(a.samples),
                channels: a.channels,
                rate: a.rate,
            })),
            Err(e) => {
                set_last_error(e);
                std::ptr::null_mut()
            }
        }
    })
}

/// Returns the number of interleaved samples in a [Decoded] buffer, or -1 if
/// the handle is null.
#[no_mangle]
pub extern "C" fn sound_decoded_len(d: *const Decoded) -> i64 {
    if d.is_null() {
        return -1;
    }
    unsafe { &*d }.samples.len() as i64
}

/// Returns a [Decoded] buffer's channel count, or -1 if the handle is null.
#[no_mangle]
pub extern "C" fn sound_decoded_channels(d: *const Decoded) -> c_int {
    if d.is_null() {
        return -1;
    }
    unsafe { &*d }.channels as c_int
}

/// Returns a [Decoded] buffer's sample rate in Hz, or -1 if the handle is null.
#[no_mangle]
pub extern "C" fn sound_decoded_rate(d: *const Decoded) -> c_int {
    if d.is_null() {
        return -1;
    }
    unsafe { &*d }.rate as c_int
}

/// Copies up to `cap` interleaved samples, starting at interleaved sample index
/// `start`, from `d` into `out`. Returns the number of samples copied, 0 past
/// the end, or -1 if a handle is null.
///
/// # Safety
/// `out` must point to space for at least `cap` `i16` values.
#[no_mangle]
pub unsafe extern "C" fn sound_decoded_copy(
    d: *const Decoded,
    start: usize,
    out: *mut i16,
    cap: usize,
) -> isize {
    if d.is_null() || out.is_null() {
        return -1;
    }
    let d = &*d;
    if start >= d.samples.len() {
        return 0;
    }
    let n = cap.min(d.samples.len() - start);
    std::ptr::copy_nonoverlapping(d.samples.as_ptr().add(start), out, n);
    n as isize
}

/// Starts playback of an already-decoded [Decoded] buffer, skipping the
/// decode that [sound_play_bytes] and [sound_play_file] do on every call.
/// Returns a voice id, or 0 on error.
///
/// The voice shares the buffer's samples rather than copying them, so
/// `decoded` may be freed with [sound_decoded_free] while the voice plays.
///
/// # Safety
/// `decoded` must be null or a live handle from [sound_decode_bytes] or
/// [sound_decode_file]. `looping` is nonzero to repeat from the start at end
/// of audio.
#[no_mangle]
pub unsafe extern "C" fn sound_play_decoded(
    player: *mut Player,
    decoded: *const Decoded,
    looping: c_int,
) -> u64 {
    catch_panic(0, set_last_error, || {
        let Some(player) = with_player(player) else {
            return 0;
        };
        if decoded.is_null() {
            set_last_error("null decoded buffer");
            return 0;
        }
        let d = &*decoded;
        player.spawn_shared(Arc::clone(&d.samples), d.channels, d.rate, looping != 0)
    })
}

/// Frees a [Decoded] buffer returned by [sound_decode_bytes] or
/// [sound_decode_file].
#[no_mangle]
pub extern "C" fn sound_decoded_free(d: *mut Decoded) {
    if !d.is_null() {
        drop(unsafe { Box::from_raw(d) });
    }
}

// ---------------------------------------------------------------------------
// Named output sink (virtual-microphone feeder on macOS and Windows)
// ---------------------------------------------------------------------------
//
// Opens a NAMED output device and accepts caller-pushed interleaved S16LE
// frames. A loopback device (BlackHole on macOS or VB-CABLE on Windows)
// presents that stream as a microphone other apps can select.
//
// This has its own error slot (`sound_sink_last_error`) so a sink failure does
// not race the playback path's `sound_last_error`.

#[cfg(any(target_os = "macos", target_os = "windows"))]
thread_local! {
    static SINK_LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn set_sink_error(msg: impl Into<String>) {
    let c = CString::new(msg.into()).unwrap_or_default();
    SINK_LAST_ERROR.with(|e| *e.borrow_mut() = c);
}

/// Opens an output sink on the device named (or UID'd) by `device`, accepting
/// `channels`-channel interleaved S16LE frames at `rate` Hz. Returns an opaque
/// sink, or null on error; see [sound_sink_last_error].
///
/// # Safety
/// `device` must be a valid NUL-terminated C string.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[no_mangle]
pub unsafe extern "C" fn sound_sink_open(
    device: *const c_char,
    rate: u32,
    channels: u32,
) -> *mut NamedOutputSink {
    catch_panic(std::ptr::null_mut(), set_sink_error, || {
        if device.is_null() {
            set_sink_error("null device name");
            return std::ptr::null_mut();
        }
        let name = match CStr::from_ptr(device).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_sink_error("device name is not valid UTF-8");
                return std::ptr::null_mut();
            }
        };
        match NamedOutputSink::open(name, rate, channels) {
            Ok(s) => Box::into_raw(Box::new(s)),
            Err(e) => {
                set_sink_error(e);
                std::ptr::null_mut()
            }
        }
    })
}

/// Queues `len` interleaved S16LE samples for the device. Returns the number of
/// source frames queued, or -1 on error; see [sound_sink_last_error].
///
/// # Safety
/// `sink` must be a sink from [sound_sink_open]; `samples` must point to `len`
/// readable `i16` values.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[no_mangle]
pub unsafe extern "C" fn sound_sink_write(
    sink: *mut NamedOutputSink,
    samples: *const i16,
    len: usize,
) -> i32 {
    catch_panic(-1, set_sink_error, || {
        if sink.is_null() || samples.is_null() {
            set_sink_error("null sink or samples");
            return -1;
        }
        let sink = &*sink;
        let slice = std::slice::from_raw_parts(samples, len);
        match sink.write(slice) {
            Ok(frames) => i32::try_from(frames).unwrap_or(i32::MAX),
            Err(e) => {
                set_sink_error(e);
                -1
            }
        }
    })
}

/// Closes and frees a sink from [sound_sink_open].
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[no_mangle]
pub extern "C" fn sound_sink_close(sink: *mut NamedOutputSink) {
    if !sink.is_null() {
        drop(unsafe { Box::from_raw(sink) });
    }
}

/// Returns the last sink error on this thread as a NUL-terminated C string. The
/// pointer is valid until the next sink call on the same thread.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[no_mangle]
pub extern "C" fn sound_sink_last_error() -> *const c_char {
    SINK_LAST_ERROR.with(|e| e.borrow().as_ptr())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{WavSpec, WavWriter};
    use std::io::Cursor;

    fn sine_wav() -> Vec<u8> {
        let spec = WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = WavWriter::new(&mut buf, spec).unwrap();
            for i in 0..4_410 {
                let t = i as f32 / 44_100.0;
                let v =
                    (0.2 * (2.0 * std::f32::consts::PI * 440.0 * t).sin() * i16::MAX as f32) as i16;
                w.write_sample(v).unwrap();
            }
            w.finalize().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn decodes_wav_bytes() {
        let bytes = sine_wav();
        let audio = decode::decode_bytes(&bytes, Some("wav")).unwrap();
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.rate, 44_100);
        assert_eq!(audio.samples.len(), 4_410);
    }

    #[test]
    fn decode_ffi_round_trips_wav() {
        let bytes = sine_wav();
        let fmt = CString::new("wav").unwrap();
        let d = unsafe { sound_decode_bytes(bytes.as_ptr(), bytes.len(), fmt.as_ptr()) };
        assert!(!d.is_null());
        assert_eq!(sound_decoded_channels(d), 1);
        assert_eq!(sound_decoded_rate(d), 44_100);
        let len = sound_decoded_len(d);
        assert_eq!(len, 4_410);
        // Copy the whole buffer out, as the Dart tap does.
        let mut buf = vec![0i16; len as usize];
        let n = unsafe { sound_decoded_copy(d, 0, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(n, len as isize);
        // A copy starting past the end yields nothing.
        assert_eq!(
            unsafe { sound_decoded_copy(d, len as usize, buf.as_mut_ptr(), buf.len()) },
            0
        );
        sound_decoded_free(d);
    }

    #[test]
    fn catch_panic_returns_the_fallback() {
        let mut seen = String::new();
        let v = catch_panic(7, |m| seen = m, || -> i32 { panic!("boom") });
        assert_eq!(v, 7);
        assert_eq!(seen, "internal error: boom");
        assert_eq!(catch_panic(7, |_| unreachable!(), || 1), 1);
    }

    #[test]
    fn voice_error_is_empty_for_an_unknown_id() {
        let player = sound_player_new();
        if player.is_null() {
            return; // no audio library on this host
        }
        set_last_error("an earlier failure");
        let msg = sound_voice_error(player, 42);
        assert_eq!(unsafe { CStr::from_ptr(msg) }.to_bytes(), b"");
        sound_player_free(player);
    }

    #[test]
    fn decode_ffi_rejects_garbage() {
        let garbage = [0u8; 64];
        let d = unsafe { sound_decode_bytes(garbage.as_ptr(), garbage.len(), std::ptr::null()) };
        assert!(d.is_null());
    }

    #[test]
    fn play_decoded_rejects_null_handles() {
        assert_eq!(
            unsafe { sound_play_decoded(std::ptr::null_mut(), std::ptr::null(), 0) },
            0
        );
        let player = sound_player_new();
        if player.is_null() {
            return; // no audio library on this host
        }
        assert_eq!(
            unsafe { sound_play_decoded(player, std::ptr::null(), 0) },
            0
        );
        let msg = unsafe { CStr::from_ptr(sound_last_error()) };
        assert_eq!(msg.to_bytes(), b"null decoded buffer");
        sound_player_free(player);
    }

    #[test]
    fn play_decoded_starts_a_voice_and_outlives_its_buffer() {
        let player = sound_player_new();
        if player.is_null() {
            return; // no audio library on this host
        }
        let bytes = sine_wav();
        let d = unsafe { sound_decode_bytes(bytes.as_ptr(), bytes.len(), std::ptr::null()) };
        assert!(!d.is_null());
        let id = unsafe { sound_play_decoded(player, d, 0) };
        assert_ne!(id, 0);
        sound_decoded_free(d);
        // The voice still reports the buffer's duration (no device needed).
        assert_eq!(sound_duration_frames(player, id), 4_410);
        sound_player_free(player);
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    ))]
    mod play_pcm {
        use super::super::*;
        use hound::{WavSpec, WavWriter};
        use std::io::Cursor;

        /// Records everything written instead of playing it.
        #[derive(Default)]
        struct RecordingSink(Mutex<Vec<i16>>);

        impl PcmSink for RecordingSink {
            fn write(&self, samples: &[i16]) -> Result<(), String> {
                self.0.lock().unwrap().extend_from_slice(samples);
                Ok(())
            }
            fn drain(&self) {}
        }

        #[test]
        fn plays_out_buffered_audio_before_a_late_error() {
            let live = LiveBuffer::new();
            live.push(&[7; 10_000]);
            live.finish(Some("body truncated".to_string()));
            let sh = VoiceShared::new(false);
            let sink = RecordingSink::default();

            play_pcm(&sink, 1, Samples::Live(live), &sh);

            assert_eq!(sink.0.lock().unwrap().len(), 10_000);
            assert_eq!(sh.state.load(Ordering::SeqCst), STATE_ERROR);
            assert_eq!(sh.error.lock().unwrap().as_deref(), Some("body truncated"));
        }

        #[test]
        fn a_decoded_buffer_is_shared_with_its_voice_not_copied() {
            let bytes = super::sine_wav();
            let d = unsafe { sound_decode_bytes(bytes.as_ptr(), bytes.len(), std::ptr::null()) };
            assert!(!d.is_null());
            let samples = unsafe { &*d }.samples.clone();
            assert_eq!(Arc::strong_count(&samples), 2);

            let sh = VoiceShared::new(false);
            let sink = RecordingSink::default();
            play_pcm(&sink, 1, Samples::Complete(Arc::clone(&samples)), &sh);

            assert_eq!(sh.state.load(Ordering::SeqCst), STATE_COMPLETED);
            assert_eq!(*sink.0.lock().unwrap(), *samples);
            // Only the Decoded and our clone remain: the voice dropped its
            // handle on the same allocation instead of owning a copy.
            assert_eq!(Arc::strong_count(&samples), 2);
            sound_decoded_free(d);
            assert_eq!(Arc::strong_count(&samples), 1);
        }

        #[test]
        fn freeing_a_decoded_buffer_mid_playback_is_safe() {
            let bytes = super::sine_wav();
            let d = unsafe { sound_decode_bytes(bytes.as_ptr(), bytes.len(), std::ptr::null()) };
            let samples = Arc::clone(&unsafe { &*d }.samples);
            let sh = VoiceShared::new(true);
            let sink = Arc::new(RecordingSink::default());
            let thread = {
                let (sh, sink) = (sh.clone(), sink.clone());
                std::thread::spawn(move || play_pcm(&*sink, 1, Samples::Complete(samples), &sh))
            };
            assert!(eventually(|| !sink.0.lock().unwrap().is_empty()));

            sound_decoded_free(d);

            // The looping voice keeps reading past one full pass.
            assert!(eventually(|| sink.0.lock().unwrap().len() > 4_410));
            sh.stop.store(true, Ordering::SeqCst);
            thread.join().unwrap();
            assert_eq!(sh.state.load(Ordering::SeqCst), STATE_STOPPED);
        }

        /// Polls `cond` for up to two seconds.
        fn eventually(cond: impl Fn() -> bool) -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if cond() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        }

        #[test]
        fn pause_and_seek_work_while_a_stream_is_stalled() {
            let live = LiveBuffer::new();
            live.push(&[1; 4096]);
            let sh = VoiceShared::new(false);
            let sink = Arc::new(RecordingSink::default());
            let thread = {
                let (live, sh, sink) = (live.clone(), sh.clone(), sink.clone());
                std::thread::spawn(move || play_pcm(&*sink, 1, Samples::Live(live), &sh))
            };
            // Everything decoded so far has played; the feeder has stalled.
            assert!(eventually(|| sink.0.lock().unwrap().len() == 4096));

            sh.paused.store(true, Ordering::SeqCst);
            assert!(eventually(
                || sh.state.load(Ordering::SeqCst) == STATE_PAUSED
            ));

            // Seeking back into the buffered audio replays it on resume.
            sh.seek_request.store(0, Ordering::SeqCst);
            sh.paused.store(false, Ordering::SeqCst);
            assert!(eventually(|| sink.0.lock().unwrap().len() == 8192));

            sh.stop.store(true, Ordering::SeqCst);
            thread.join().unwrap();
            assert_eq!(sh.state.load(Ordering::SeqCst), STATE_STOPPED);
        }

        #[test]
        fn seeking_past_the_end_completes() {
            for target in [5_000, i64::MAX] {
                let sh = VoiceShared::new(false);
                sh.seek_request.store(target, Ordering::SeqCst);
                let sink = RecordingSink::default();

                play_pcm(&sink, 2, Samples::Complete(Arc::new(vec![0; 2_000])), &sh);

                assert!(sink.0.lock().unwrap().is_empty());
                assert_eq!(sh.state.load(Ordering::SeqCst), STATE_COMPLETED);
                assert_eq!(sh.position.load(Ordering::SeqCst), 1_000);
            }
        }

        /// Serves `body` as one HTTP response over a local socket, `pace`
        /// bytes every 20 ms, or holds the connection open without replying
        /// when `body` is `None`. Returns the URL.
        fn serve(body: Option<Vec<u8>>, pace: usize) -> String {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/a.wav", listener.local_addr().unwrap());
            std::thread::spawn(move || {
                let (mut conn, _) = listener.accept().unwrap();
                let mut req = [0u8; 1024];
                let _ = conn.read(&mut req);
                let Some(body) = body else {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    return;
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(head.as_bytes());
                for part in body.chunks(pace) {
                    if conn.write_all(part).is_err() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            });
            url
        }

        #[test]
        fn stopping_while_connecting_reports_stopped() {
            let url = serve(None, 0);
            let sh = VoiceShared::new(false);
            sh.state.store(STATE_CONNECTING, Ordering::SeqCst);
            let thread = {
                let sh = sh.clone();
                std::thread::spawn(move || {
                    run_voice_url(
                        url,
                        Vec::new(),
                        None,
                        Arc::new(AtomicU32::new(0)),
                        Arc::new(AtomicU64::new(0)),
                        &sh,
                        |_, _| Ok(RecordingSink::default()),
                    )
                })
            };
            std::thread::sleep(std::time::Duration::from_millis(100));
            sh.stop.store(true, Ordering::SeqCst);
            thread.join().unwrap();
            assert_eq!(sh.state.load(Ordering::SeqCst), STATE_STOPPED);
            assert!(sh.error.lock().unwrap().is_none());
        }

        /// A mono 16-bit 44.1 kHz WAV of `seconds` of silence.
        fn silent_wav(seconds: u32) -> Vec<u8> {
            let spec = WavSpec {
                channels: 1,
                sample_rate: 44_100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut buf = Cursor::new(Vec::new());
            let mut w = WavWriter::new(&mut buf, spec).unwrap();
            for _ in 0..44_100 * seconds {
                w.write_sample(0i16).unwrap();
            }
            w.finalize().unwrap();
            buf.into_inner()
        }

        #[test]
        fn a_sink_error_stops_the_download() {
            // ~2 s of audio served over ~1 s, so it is still downloading when
            // the sink fails.
            let url = serve(Some(silent_wav(2)), 4096);
            let sh = VoiceShared::new(false);
            let total = Arc::new(AtomicU64::new(0));
            run_voice_url(
                url,
                Vec::new(),
                None,
                Arc::new(AtomicU32::new(0)),
                total.clone(),
                &sh,
                |_, _| Err::<RecordingSink, _>("no device".to_string()),
            );
            assert_eq!(sh.state.load(Ordering::SeqCst), STATE_ERROR);

            std::thread::sleep(std::time::Duration::from_millis(100));
            let settled = total.load(Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert_eq!(total.load(Ordering::SeqCst), settled);
            assert!(settled < 88_200, "decoded the whole stream: {settled}");
        }

        #[test]
        fn a_panicking_sink_fails_the_voice() {
            struct PanickingSink;
            impl PcmSink for PanickingSink {
                fn write(&self, _: &[i16]) -> Result<(), String> {
                    panic!("device exploded")
                }
                fn drain(&self) {}
            }
            let sh = VoiceShared::new(false);
            run_voice(
                Ok(PanickingSink),
                1,
                Samples::Complete(Arc::new(vec![0; 100])),
                &sh,
            );
            assert_eq!(sh.state.load(Ordering::SeqCst), STATE_ERROR);
            assert_eq!(
                sh.error.lock().unwrap().as_deref(),
                Some("internal error: device exploded")
            );
        }

        #[test]
        fn looping_empty_audio_completes() {
            let sh = VoiceShared::new(true);
            let thread = {
                let sh = sh.clone();
                std::thread::spawn(move || {
                    play_pcm(
                        &RecordingSink::default(),
                        1,
                        Samples::Complete(Arc::new(vec![])),
                        &sh,
                    )
                })
            };
            let done = eventually(|| sh.state.load(Ordering::SeqCst) == STATE_COMPLETED);
            sh.stop.store(true, Ordering::SeqCst);
            thread.join().unwrap();
            assert!(done);
        }
    }
}
