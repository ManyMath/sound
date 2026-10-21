//! Push-driven CoreAudio output sink targeting a NAMED device.
//!
//! Where `coreaudio.rs` plays a decoded clip to the default output, this opens
//! an output on a specific device (looked up by name or UID through the
//! CoreAudio HAL) and accepts a continuous stream of caller-pushed PCM frames.
//! It is the macOS feeder for the virtual-microphone path: the soundboard mix
//! is written here and a loopback device (BlackHole, or an own-brand driver)
//! presents it as a microphone other apps can select.
//!
//! Like the rest of the crate it loads CoreAudio.framework and
//! AudioToolbox.framework via dlopen, so no SDK is linked at build time.
//!
//! The device wants Float32; callers push interleaved S16LE. We convert
//! (`f = s / 32768.0`) in the render callback and up-mix mono -> stereo there
//! too (BlackHole 2ch is stereo; the mixer may be mono).

use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use libloading::Library;

use crate::bounded_send::send_or_stall;

const K_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const K_PCM_FLAG_IS_FLOAT: u32 = 0x1;
const K_PCM_FLAG_IS_PACKED: u32 = 0x8;

// HAL property selectors (four-char codes) and scopes/elements.
const K_DEVICES: u32 = u32::from_be_bytes(*b"dev#");
const K_DEVICE_UID: u32 = u32::from_be_bytes(*b"uid ");
const K_OBJECT_NAME: u32 = u32::from_be_bytes(*b"lnam");
const K_DEVICE_STREAMS: u32 = u32::from_be_bytes(*b"stm#");
const K_SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");
const K_SCOPE_OUTPUT: u32 = u32::from_be_bytes(*b"outp");
const K_ELEMENT_MAIN: u32 = 0;
const K_OBJECT_SYSTEM: u32 = 1; // kAudioObjectSystemObject

// AudioQueue property to bind the queue to a specific output device (value is
// the device UID as a CFStringRef).
const K_QUEUE_CURRENT_DEVICE: u32 = u32::from_be_bytes(*b"aqcd");

type AudioObjectID = u32;
type OSStatus = i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
struct AudioQueueBuffer {
    audio_data_bytes_capacity: u32,
    audio_data: *mut c_void,
    audio_data_byte_size: u32,
    user_data: *mut c_void,
    packet_description_capacity: u32,
    packet_descriptions: *mut c_void,
    packet_description_count: u32,
}

type AudioQueueRef = *mut c_void;
type AudioQueueBufferRef = *mut AudioQueueBuffer;
type CFStringRef = *const c_void;

type FnNewOutput = unsafe extern "C" fn(
    *const AudioStreamBasicDescription,
    Option<unsafe extern "C" fn(*mut c_void, AudioQueueRef, AudioQueueBufferRef)>,
    *mut c_void,
    *mut c_void,
    *mut c_void,
    u32,
    *mut AudioQueueRef,
) -> c_int;
type FnSetProperty = unsafe extern "C" fn(AudioQueueRef, u32, *const c_void, u32) -> c_int;
type FnAlloc = unsafe extern "C" fn(AudioQueueRef, u32, *mut AudioQueueBufferRef) -> c_int;
type FnEnqueue =
    unsafe extern "C" fn(AudioQueueRef, AudioQueueBufferRef, u32, *const c_void) -> c_int;
type FnStart = unsafe extern "C" fn(AudioQueueRef, *mut c_void) -> c_int;
type FnStop = unsafe extern "C" fn(AudioQueueRef, u8) -> c_int;
type FnDispose = unsafe extern "C" fn(AudioQueueRef, u8) -> c_int;

type FnGetSize = unsafe extern "C" fn(
    AudioObjectID,
    *const AudioObjectPropertyAddress,
    u32,
    *const c_void,
    *mut u32,
) -> OSStatus;
type FnGetData = unsafe extern "C" fn(
    AudioObjectID,
    *const AudioObjectPropertyAddress,
    u32,
    *const c_void,
    *mut u32,
    *mut c_void,
) -> OSStatus;

type FnCFStringCreate =
    unsafe extern "C" fn(*const c_void, *const u8, isize, u32, u8) -> CFStringRef;
type FnCFStringLen = unsafe extern "C" fn(CFStringRef) -> isize;
type FnCFStringGetCString = unsafe extern "C" fn(CFStringRef, *mut u8, isize, u32) -> u8;
type FnCFRelease = unsafe extern "C" fn(*const c_void);

const K_CF_ENCODING_UTF8: u32 = 0x0800_0100;

/// Holds the dynamically-loaded symbols from AudioToolbox, CoreAudio, and
/// CoreFoundation that the sink needs.
struct Libs {
    _audio_toolbox: Library,
    _core_audio: Library,
    _core_foundation: Library,
    new_output: FnNewOutput,
    set_property: FnSetProperty,
    alloc: FnAlloc,
    enqueue: FnEnqueue,
    start: FnStart,
    stop: FnStop,
    dispose: FnDispose,
    get_size: FnGetSize,
    get_data: FnGetData,
    cfstring_create: FnCFStringCreate,
    cfstring_len: FnCFStringLen,
    cfstring_get_cstring: FnCFStringGetCString,
    cfrelease: FnCFRelease,
}

unsafe impl Send for Libs {}
unsafe impl Sync for Libs {}

fn open_first(candidates: &[&str]) -> Result<Library, String> {
    let mut last = String::new();
    for p in candidates {
        match unsafe { Library::new(p) } {
            Ok(l) => return Ok(l),
            Err(e) => last = format!("{p}: {e}"),
        }
    }
    Err(last)
}

impl Libs {
    fn load() -> Result<Libs, String> {
        let at = open_first(&[
            "/System/Library/Frameworks/AudioToolbox.framework/AudioToolbox",
            "AudioToolbox.framework/AudioToolbox",
            "AudioToolbox",
        ])
        .map_err(|e| format!("failed to load AudioToolbox: {e}"))?;
        let ca = open_first(&[
            "/System/Library/Frameworks/CoreAudio.framework/CoreAudio",
            "CoreAudio.framework/CoreAudio",
            "CoreAudio",
        ])
        .map_err(|e| format!("failed to load CoreAudio: {e}"))?;
        let cf = open_first(&[
            "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation",
            "CoreFoundation.framework/CoreFoundation",
            "CoreFoundation",
        ])
        .map_err(|e| format!("failed to load CoreFoundation: {e}"))?;
        unsafe {
            macro_rules! sym {
                ($lib:expr, $name:literal) => {
                    *$lib.get($name).map_err(|e| {
                        format!("missing symbol {}: {e}", String::from_utf8_lossy($name))
                    })?
                };
            }
            Ok(Libs {
                new_output: sym!(at, b"AudioQueueNewOutput\0"),
                set_property: sym!(at, b"AudioQueueSetProperty\0"),
                alloc: sym!(at, b"AudioQueueAllocateBuffer\0"),
                enqueue: sym!(at, b"AudioQueueEnqueueBuffer\0"),
                start: sym!(at, b"AudioQueueStart\0"),
                stop: sym!(at, b"AudioQueueStop\0"),
                dispose: sym!(at, b"AudioQueueDispose\0"),
                get_size: sym!(ca, b"AudioObjectGetPropertyDataSize\0"),
                get_data: sym!(ca, b"AudioObjectGetPropertyData\0"),
                cfstring_create: sym!(cf, b"CFStringCreateWithBytes\0"),
                cfstring_len: sym!(cf, b"CFStringGetLength\0"),
                cfstring_get_cstring: sym!(cf, b"CFStringGetCString\0"),
                cfrelease: sym!(cf, b"CFRelease\0"),
                _audio_toolbox: at,
                _core_audio: ca,
                _core_foundation: cf,
            })
        }
    }

    /// Reads a CFString device property into a Rust String, or None if absent.
    unsafe fn device_string(&self, dev: AudioObjectID, selector: u32) -> Option<String> {
        let addr = AudioObjectPropertyAddress {
            selector,
            scope: K_SCOPE_GLOBAL,
            element: K_ELEMENT_MAIN,
        };
        let mut size: u32 = 0;
        if (self.get_size)(dev, &addr, 0, std::ptr::null(), &mut size) != 0
            || size as usize != std::mem::size_of::<CFStringRef>()
        {
            return None;
        }
        let mut cfstr: CFStringRef = std::ptr::null();
        if (self.get_data)(
            dev,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut cfstr as *mut CFStringRef as *mut c_void,
        ) != 0
            || cfstr.is_null()
        {
            return None;
        }
        let result = self.cfstring_to_string(cfstr);
        (self.cfrelease)(cfstr);
        result
    }

    /// Whether `dev` has any output streams (input-only devices such as a
    /// microphone have none).
    unsafe fn has_output_streams(&self, dev: AudioObjectID) -> bool {
        let addr = AudioObjectPropertyAddress {
            selector: K_DEVICE_STREAMS,
            scope: K_SCOPE_OUTPUT,
            element: K_ELEMENT_MAIN,
        };
        let mut size: u32 = 0;
        (self.get_size)(dev, &addr, 0, std::ptr::null(), &mut size) == 0 && size > 0
    }

    unsafe fn cfstring_to_string(&self, s: CFStringRef) -> Option<String> {
        let len = (self.cfstring_len)(s);
        // Worst case 4 UTF-8 bytes per UTF-16 unit, plus a NUL.
        let cap = (len.max(0) as usize) * 4 + 1;
        let mut buf = vec![0u8; cap];
        if (self.cfstring_get_cstring)(s, buf.as_mut_ptr(), cap as isize, K_CF_ENCODING_UTF8) == 0 {
            return None;
        }
        let nul = buf.iter().position(|&b| b == 0).unwrap_or(0);
        buf.truncate(nul);
        String::from_utf8(buf).ok()
    }

    /// Returns the UID of the first output device whose name or UID matches
    /// `wanted` (case-insensitive). The UID is needed to bind the AudioQueue.
    unsafe fn find_device_uid(&self, wanted: &str) -> Result<String, String> {
        let addr = AudioObjectPropertyAddress {
            selector: K_DEVICES,
            scope: K_SCOPE_GLOBAL,
            element: K_ELEMENT_MAIN,
        };
        let mut size: u32 = 0;
        if (self.get_size)(K_OBJECT_SYSTEM, &addr, 0, std::ptr::null(), &mut size) != 0 {
            return Err("could not query the audio device list".into());
        }
        let count = size as usize / std::mem::size_of::<AudioObjectID>();
        let mut ids = vec![0 as AudioObjectID; count];
        if (self.get_data)(
            K_OBJECT_SYSTEM,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            ids.as_mut_ptr() as *mut c_void,
        ) != 0
        {
            return Err("could not read the audio device list".into());
        }
        let want = wanted.to_lowercase();
        let mut names: Vec<String> = Vec::new();
        for &dev in &ids {
            if !self.has_output_streams(dev) {
                continue;
            }
            let uid = self.device_string(dev, K_DEVICE_UID);
            let name = self.device_string(dev, K_OBJECT_NAME);
            if let Some(n) = &name {
                names.push(n.clone());
            }
            let uid_match = uid.as_deref().map(|u| u.to_lowercase()) == Some(want.clone());
            let name_match = name.as_deref().map(|n| n.to_lowercase()) == Some(want.clone());
            if uid_match || name_match {
                if let Some(u) = uid {
                    return Ok(u);
                }
            }
        }
        Err(format!(
            "no audio device named \"{wanted}\" (found: {}). \
             Install the loopback device, e.g. `brew install blackhole-2ch`.",
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        ))
    }
}

/// Passed as the AudioQueue user_data. Holds the consumer end of the chunk
/// channel and the re-enqueue entry point the callback needs.
struct Channel {
    rx: Mutex<Receiver<Vec<i16>>>,
    enqueue: FnEnqueue,
    // Carry-over interleaved S16 samples from a chunk that did not fit the last
    // buffer, drained before pulling the next chunk.
    pending: Mutex<Vec<i16>>,
    // Source channel count (1 or 2); the device output is always stereo.
    src_channels: u32,
    closed: AtomicBool,
}

unsafe impl Send for Channel {}
unsafe impl Sync for Channel {}

/// A push-driven output on a named device. Drop closes it.
pub struct Sink {
    libs: Libs,
    queue: AudioQueueRef,
    tx: SyncSender<Vec<i16>>,
    channel: Box<Channel>,
    buffers: Vec<AudioQueueBufferRef>,
    started: AtomicBool,
}

unsafe impl Send for Sink {}

const RING_BUFFERS: u32 = 4;
const FRAMES_PER_BUFFER: u32 = 2048;
const OUT_CHANNELS: u32 = 2; // device output is stereo (Float32)

// How long `write` waits for the render callback to drain a slot before giving
// up. A healthy queue frees a slot every ~43 ms (2048 frames at 48 kHz), so a
// second of no progress means the device stopped pulling (unplugged, failed).
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(1);

/// Locks ignoring poisoning: these mutexes guard plain data (no invariants a
/// panic could break), and the render callback must never panic itself, which
/// `unwrap()` on a poisoned lock would.
fn lock_unpoisoned<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Sink {
    pub fn open(device: &str, rate: u32, channels: u32) -> Result<Sink, String> {
        let libs = Libs::load()?;
        if !(1..=2).contains(&channels) {
            return Err(format!(
                "unsupported channel count {channels} (the sink takes mono or stereo)"
            ));
        }
        let src_channels = channels;
        let uid = unsafe { libs.find_device_uid(device)? };

        let asbd = AudioStreamBasicDescription {
            sample_rate: rate as f64,
            format_id: K_LINEAR_PCM,
            format_flags: K_PCM_FLAG_IS_FLOAT | K_PCM_FLAG_IS_PACKED,
            bytes_per_packet: OUT_CHANNELS * 4,
            frames_per_packet: 1,
            bytes_per_frame: OUT_CHANNELS * 4,
            channels_per_frame: OUT_CHANNELS,
            bits_per_channel: 32,
            reserved: 0,
        };
        let (tx, rx) = sync_channel::<Vec<i16>>(RING_BUFFERS as usize * 2);
        let channel = Box::new(Channel {
            rx: Mutex::new(rx),
            enqueue: libs.enqueue,
            pending: Mutex::new(Vec::new()),
            src_channels,
            closed: AtomicBool::new(false),
        });
        let user_data = &*channel as *const Channel as *mut c_void;
        unsafe {
            let mut queue: AudioQueueRef = std::ptr::null_mut();
            let rc = (libs.new_output)(
                &asbd,
                Some(output_cb),
                user_data,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &mut queue,
            );
            if rc != 0 {
                return Err(format!("AudioQueueNewOutput: {rc}"));
            }

            // Bind the queue to the requested device by its UID.
            let cfuid = (libs.cfstring_create)(
                std::ptr::null(),
                uid.as_ptr(),
                uid.len() as isize,
                K_CF_ENCODING_UTF8,
                0,
            );
            if cfuid.is_null() {
                (libs.dispose)(queue, 1);
                return Err("could not build the device UID string".into());
            }
            let rc = (libs.set_property)(
                queue,
                K_QUEUE_CURRENT_DEVICE,
                &cfuid as *const CFStringRef as *const c_void,
                std::mem::size_of::<CFStringRef>() as u32,
            );
            (libs.cfrelease)(cfuid);
            if rc != 0 {
                (libs.dispose)(queue, 1);
                return Err(format!("AudioQueueSetProperty(CurrentDevice): {rc}"));
            }

            let buffer_bytes = FRAMES_PER_BUFFER * OUT_CHANNELS * 4;
            let mut buffers = Vec::with_capacity(RING_BUFFERS as usize);
            for _ in 0..RING_BUFFERS {
                let mut buf: AudioQueueBufferRef = std::ptr::null_mut();
                let rc = (libs.alloc)(queue, buffer_bytes, &mut buf);
                if rc != 0 {
                    (libs.dispose)(queue, 1);
                    return Err(format!("AudioQueueAllocateBuffer: {rc}"));
                }
                std::ptr::write_bytes((*buf).audio_data as *mut u8, 0, buffer_bytes as usize);
                (*buf).audio_data_byte_size = buffer_bytes;
                (libs.enqueue)(queue, buf, 0, std::ptr::null());
                buffers.push(buf);
            }
            Ok(Sink {
                libs,
                queue,
                tx,
                channel,
                buffers,
                started: AtomicBool::new(false),
            })
        }
    }

    fn ensure_started(&self) -> Result<(), String> {
        // compare_exchange so exactly one caller starts the queue even if the
        // C ABI is driven from multiple threads.
        if self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        unsafe {
            let rc = (self.libs.start)(self.queue, std::ptr::null_mut());
            if rc != 0 {
                self.started.store(false, Ordering::Release);
                return Err(format!("AudioQueueStart: {rc}"));
            }
        }
        Ok(())
    }

    /// Queues `samples` (interleaved S16LE at the source channel count) for the
    /// device. Returns the number of source frames queued.
    ///
    /// Blocks briefly when the ring is full (that is the backpressure that
    /// paces a faster-than-real-time producer), but never indefinitely: if the
    /// device stops draining for [WRITE_STALL_TIMEOUT] the write fails instead
    /// of freezing the caller.
    pub fn write(&self, samples: &[i16]) -> Result<usize, String> {
        self.ensure_started()?;
        let stride = FRAMES_PER_BUFFER as usize * self.channel.src_channels as usize;
        for chunk in samples.chunks(stride) {
            send_or_stall(&self.tx, chunk.to_vec(), WRITE_STALL_TIMEOUT)?;
        }
        Ok(samples.len() / self.channel.src_channels as usize)
    }
}

/// Pulls the next interleaved S16 chunk from the channel or the pending
/// carry-over. Returns None when nothing is queued.
fn next_chunk(channel: &Channel) -> Option<Vec<i16>> {
    {
        let mut pending = lock_unpoisoned(&channel.pending);
        if !pending.is_empty() {
            return Some(std::mem::take(&mut *pending));
        }
    }
    let rx = lock_unpoisoned(&channel.rx);
    rx.try_recv().ok()
}

/// Called by AudioQueue on its own thread when a buffer is ready to be filled.
/// Converts the source S16 frames to interleaved stereo Float32, up-mixing mono
/// by duplication, and pads any remainder with silence.
unsafe extern "C" fn output_cb(
    user_data: *mut c_void,
    queue: AudioQueueRef,
    buf: AudioQueueBufferRef,
) {
    let channel = &*(user_data as *const Channel);
    // The sink is being torn down; do not touch or re-enqueue the buffer.
    if channel.closed.load(Ordering::Acquire) {
        return;
    }
    let cap_bytes = (*buf).audio_data_bytes_capacity as usize;
    let cap_frames = cap_bytes / (OUT_CHANNELS as usize * 4);
    let dst = (*buf).audio_data as *mut f32;
    let src_ch = channel.src_channels as usize;

    let mut frames_written = 0usize;
    while frames_written < cap_frames {
        let chunk = match next_chunk(channel) {
            Some(c) => c,
            None => break,
        };
        let chunk_frames = chunk.len() / src_ch;
        let take = chunk_frames.min(cap_frames - frames_written);
        for f in 0..take {
            let l = chunk[f * src_ch];
            let r = if src_ch == 2 {
                chunk[f * src_ch + 1]
            } else {
                l
            };
            let base = (frames_written + f) * OUT_CHANNELS as usize;
            *dst.add(base) = l as f32 / 32768.0;
            *dst.add(base + 1) = r as f32 / 32768.0;
        }
        frames_written += take;
        // Stash any frames that did not fit this buffer for the next callback.
        if take < chunk_frames {
            let rest = chunk[take * src_ch..].to_vec();
            *lock_unpoisoned(&channel.pending) = rest;
            break;
        }
    }

    // Pad the rest with silence on underrun.
    for f in frames_written..cap_frames {
        let base = f * OUT_CHANNELS as usize;
        *dst.add(base) = 0.0;
        *dst.add(base + 1) = 0.0;
    }
    (*buf).audio_data_byte_size = cap_bytes as u32;
    (channel.enqueue)(queue, buf, 0, std::ptr::null());
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.channel.closed.store(true, Ordering::SeqCst);
        unsafe {
            if self.started.load(Ordering::Acquire) {
                (self.libs.stop)(self.queue, 1);
            }
            // AudioQueueDispose(immediate) synchronizes with in-flight
            // callbacks and frees the queue-owned buffers.
            (self.libs.dispose)(self.queue, 1);
        }
        self.buffers.clear();
        // Keep Channel alive until after the queue is gone so the callback
        // cannot deref a freed pointer mid-stop.
        let _ = &self.channel;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_frameworks() {
        Libs::load().expect("AudioToolbox/CoreAudio/CoreFoundation load on macOS");
    }

    #[test]
    fn unknown_device_errors_with_install_hint() {
        // A device this absurdly named cannot exist; the lookup must fail with a
        // clear message pointing at the loopback install. Runs on any Mac (no
        // BlackHole required) since it only enumerates devices.
        let err = match Sink::open("no-such-device-xyzzy-42", 48_000, 2) {
            Ok(_) => panic!("a bogus device name must not open"),
            Err(e) => e,
        };
        assert!(err.contains("no audio device named"), "got: {err}");
        assert!(err.contains("blackhole"), "got: {err}");
    }
}
