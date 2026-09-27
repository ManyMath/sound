//! Minimal waveOut binding via dlopen of winmm.dll.
//!
//! Same header-free pattern as ALSA/AAudio/CoreAudio: declare the symbols we
//! use and load them at runtime. winmm.dll ships with every Windows install.

use std::cell::UnsafeCell;
use std::sync::Arc;

use libloading::Library;

use crate::PcmSink;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_MAPPER: u32 = 0xFFFF_FFFF;
const CALLBACK_NULL: u32 = 0;
const WHDR_DONE: u32 = 0x0000_0001;
const WHDR_PREPARED: u32 = 0x0000_0002;
const MMSYSERR_NOERROR: u32 = 0;
const MAXPNAMELEN: usize = 32;

type HWaveOut = *mut std::ffi::c_void;

#[repr(C, packed(1))]
#[derive(Clone, Copy)]
struct WaveFormatEx {
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    cb_size: u16,
}

#[repr(C)]
struct WaveHdr {
    data: *mut u8,
    buffer_length: u32,
    bytes_recorded: u32,
    user: usize,
    flags: u32,
    loops: u32,
    next: *mut WaveHdr,
    reserved: usize,
}

#[repr(C)]
struct WaveOutCapsW {
    mid: u16,
    pid: u16,
    driver_version: u32,
    pname: [u16; MAXPNAMELEN],
    formats: u32,
    channels: u16,
    reserved1: u16,
    support: u32,
}

type FnWaveOutOpen =
    unsafe extern "system" fn(*mut HWaveOut, u32, *const WaveFormatEx, usize, usize, u32) -> u32;
type FnWaveOutClose = unsafe extern "system" fn(HWaveOut) -> u32;
type FnWaveOutReset = unsafe extern "system" fn(HWaveOut) -> u32;
type FnWaveOutPrepareHeader = unsafe extern "system" fn(HWaveOut, *mut WaveHdr, u32) -> u32;
type FnWaveOutUnprepareHeader = unsafe extern "system" fn(HWaveOut, *mut WaveHdr, u32) -> u32;
type FnWaveOutWrite = unsafe extern "system" fn(HWaveOut, *mut WaveHdr, u32) -> u32;
type FnWaveOutGetNumDevs = unsafe extern "system" fn() -> u32;
type FnWaveOutGetDevCaps = unsafe extern "system" fn(usize, *mut WaveOutCapsW, u32) -> u32;

pub struct WinMM {
    _lib: Library,
    wave_out_open: FnWaveOutOpen,
    wave_out_close: FnWaveOutClose,
    wave_out_reset: FnWaveOutReset,
    wave_out_prepare_header: FnWaveOutPrepareHeader,
    wave_out_unprepare_header: FnWaveOutUnprepareHeader,
    wave_out_write: FnWaveOutWrite,
    wave_out_get_num_devs: FnWaveOutGetNumDevs,
    wave_out_get_dev_caps: FnWaveOutGetDevCaps,
}

unsafe impl Send for WinMM {}
unsafe impl Sync for WinMM {}

impl WinMM {
    pub fn load() -> Result<WinMM, String> {
        unsafe {
            let lib =
                Library::new("winmm.dll").map_err(|e| format!("failed to load winmm.dll: {e}"))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get($name).map_err(|e| {
                        format!("missing symbol {}: {e}", String::from_utf8_lossy($name))
                    })?
                };
            }
            Ok(WinMM {
                wave_out_open: sym!(b"waveOutOpen\0"),
                wave_out_close: sym!(b"waveOutClose\0"),
                wave_out_reset: sym!(b"waveOutReset\0"),
                wave_out_prepare_header: sym!(b"waveOutPrepareHeader\0"),
                wave_out_unprepare_header: sym!(b"waveOutUnprepareHeader\0"),
                wave_out_write: sym!(b"waveOutWrite\0"),
                wave_out_get_num_devs: sym!(b"waveOutGetNumDevs\0"),
                wave_out_get_dev_caps: sym!(b"waveOutGetDevCapsW\0"),
                _lib: lib,
            })
        }
    }

    /// Finds an output by its WinMM display name, case-insensitively. An
    /// exact match wins. Drivers may truncate names to 31 UTF-16 units, so
    /// failing that, either side being a prefix of the other is accepted as
    /// long as it picks out a single device.
    fn find_output_device(&self, wanted: &str) -> Result<u32, String> {
        let requested = wanted.trim();
        let wanted = requested.to_lowercase();
        if wanted.is_empty() {
            return Err("output device name is empty".to_string());
        }
        let count = unsafe { (self.wave_out_get_num_devs)() };
        let mut devices = Vec::new();
        for id in 0..count {
            let mut caps = WaveOutCapsW {
                mid: 0,
                pid: 0,
                driver_version: 0,
                pname: [0; MAXPNAMELEN],
                formats: 0,
                channels: 0,
                reserved1: 0,
                support: 0,
            };
            let rc = unsafe {
                (self.wave_out_get_dev_caps)(
                    id as usize,
                    &mut caps,
                    std::mem::size_of::<WaveOutCapsW>() as u32,
                )
            };
            if rc != MMSYSERR_NOERROR {
                continue;
            }
            let end = caps
                .pname
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(MAXPNAMELEN);
            let name = String::from_utf16_lossy(&caps.pname[..end]);
            if !name.is_empty() {
                devices.push((id, name));
            }
        }
        if let Some((id, _)) = devices.iter().find(|(_, n)| n.to_lowercase() == wanted) {
            return Ok(*id);
        }
        let prefixed: Vec<&(u32, String)> = devices
            .iter()
            .filter(|(_, n)| {
                let folded = n.to_lowercase();
                folded.starts_with(&wanted) || wanted.starts_with(&folded)
            })
            .collect();
        match prefixed.as_slice() {
            [(id, _)] => Ok(*id),
            [] => Err(format!(
                "output device {requested:?} not found (available: {})",
                join_names(devices.iter())
            )),
            _ => Err(format!(
                "output device {requested:?} is ambiguous (matches: {})",
                join_names(prefixed.iter().copied())
            )),
        }
    }
}

fn join_names<'a>(devices: impl Iterator<Item = &'a (u32, String)>) -> String {
    devices
        .map(|(_, name)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

const NUM_BUFFERS: usize = 4;
const FRAMES_PER_BUFFER: usize = 2048;

struct BufferRing {
    hdrs: Vec<WaveHdr>,
    datas: Vec<Vec<u8>>,
    next: usize,
}

pub struct WinMMPlayback {
    wmm: Arc<WinMM>,
    handle: HWaveOut,
    ring: UnsafeCell<BufferRing>,
    channels: u32,
}

impl WinMMPlayback {
    pub fn open(wmm: Arc<WinMM>, channels: u32, rate: u32) -> Result<Self, String> {
        Self::open_device(wmm, WAVE_MAPPER, channels, rate)
    }

    fn open_device(wmm: Arc<WinMM>, device: u32, channels: u32, rate: u32) -> Result<Self, String> {
        let channels = channels.max(1);
        let block_align = channels
            .checked_mul(2)
            .and_then(|b| u16::try_from(b).ok())
            .ok_or_else(|| format!("unsupported channel count {channels}"))?;
        let avg_bytes_per_sec = rate
            .checked_mul(block_align as u32)
            .ok_or_else(|| format!("unsupported sample rate {rate}"))?;
        let wfx = WaveFormatEx {
            format_tag: WAVE_FORMAT_PCM,
            channels: channels as u16,
            samples_per_sec: rate,
            avg_bytes_per_sec,
            block_align,
            bits_per_sample: 16,
            cb_size: 0,
        };
        let mut handle: HWaveOut = std::ptr::null_mut();
        unsafe {
            let rc = (wmm.wave_out_open)(&mut handle, device, &wfx, 0, 0, CALLBACK_NULL);
            if rc != MMSYSERR_NOERROR {
                return Err(format!("waveOutOpen failed: error {rc}"));
            }
        }

        let buf_bytes = FRAMES_PER_BUFFER * channels as usize * 2;
        let mut hdrs = Vec::with_capacity(NUM_BUFFERS);
        let mut datas = Vec::with_capacity(NUM_BUFFERS);
        for _ in 0..NUM_BUFFERS {
            let data = vec![0u8; buf_bytes];
            hdrs.push(WaveHdr {
                data: data.as_ptr() as *mut u8,
                buffer_length: buf_bytes as u32,
                bytes_recorded: 0,
                user: 0,
                flags: 0,
                loops: 0,
                next: std::ptr::null_mut(),
                reserved: 0,
            });
            datas.push(data);
        }

        Ok(WinMMPlayback {
            wmm,
            handle,
            ring: UnsafeCell::new(BufferRing {
                hdrs,
                datas,
                next: 0,
            }),
            channels,
        })
    }

    /// Reads a header's flags straight from memory. The driver sets
    /// `WHDR_DONE` from its own thread, so this must not go through a
    /// `&WaveHdr`, which lets the compiler assume the value never changes and
    /// hoist the load out of a polling loop.
    fn hdr_flags(hdr: *const WaveHdr) -> u32 {
        unsafe { std::ptr::addr_of!((*hdr).flags).read_volatile() }
    }

    fn wait_for_hdr(hdr: *const WaveHdr) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let flags = Self::hdr_flags(hdr);
            if flags & WHDR_PREPARED == 0 || flags & WHDR_DONE != 0 {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("waveOut buffer stalled for more than one second".to_string());
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

impl PcmSink for WinMMPlayback {
    fn write(&self, samples: &[i16]) -> Result<(), String> {
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(samples.as_ptr() as *const u8, samples.len() * 2) };
        let buf_bytes = FRAMES_PER_BUFFER * self.channels as usize * 2;
        // Safety: PcmSink::write is only called from a single playback thread.
        let ring = unsafe { &mut *self.ring.get() };
        for chunk in bytes.chunks(buf_bytes) {
            let idx = ring.next % NUM_BUFFERS;
            // Only touch headers through raw pointers: the device may still be
            // writing to this one until wait_for_hdr sees WHDR_DONE.
            let hdr = unsafe { ring.hdrs.as_mut_ptr().add(idx) };
            Self::wait_for_hdr(hdr)?;

            if Self::hdr_flags(hdr) & WHDR_PREPARED != 0 {
                unsafe {
                    (self.wmm.wave_out_unprepare_header)(
                        self.handle,
                        hdr,
                        std::mem::size_of::<WaveHdr>() as u32,
                    );
                }
            }

            let len = chunk.len().min(ring.datas[idx].len());
            ring.datas[idx][..len].copy_from_slice(&chunk[..len]);
            unsafe {
                (*hdr).data = ring.datas[idx].as_ptr() as *mut u8;
                (*hdr).buffer_length = len as u32;
                (*hdr).flags = 0;
            }

            unsafe {
                let rc = (self.wmm.wave_out_prepare_header)(
                    self.handle,
                    hdr,
                    std::mem::size_of::<WaveHdr>() as u32,
                );
                if rc != MMSYSERR_NOERROR {
                    return Err(format!("waveOutPrepareHeader: error {rc}"));
                }
                let rc = (self.wmm.wave_out_write)(
                    self.handle,
                    hdr,
                    std::mem::size_of::<WaveHdr>() as u32,
                );
                if rc != MMSYSERR_NOERROR {
                    return Err(format!("waveOutWrite: error {rc}"));
                }
            }
            ring.next += 1;
        }
        Ok(())
    }

    fn drain(&self) {
        let ring = unsafe { &*self.ring.get() };
        for idx in 0..ring.hdrs.len() {
            let _ = Self::wait_for_hdr(unsafe { ring.hdrs.as_ptr().add(idx) });
        }
    }
}

impl Drop for WinMMPlayback {
    fn drop(&mut self) {
        let ring = self.ring.get_mut();
        unsafe {
            (self.wmm.wave_out_reset)(self.handle);
            for hdr in &mut ring.hdrs {
                if hdr.flags & WHDR_PREPARED != 0 {
                    (self.wmm.wave_out_unprepare_header)(
                        self.handle,
                        hdr,
                        std::mem::size_of::<WaveHdr>() as u32,
                    );
                }
            }
            (self.wmm.wave_out_close)(self.handle);
        }
    }
}

/// Caller-pushed output to a specifically named Windows playback device.
/// A virtual cable such as VB-CABLE exposes this endpoint as "CABLE Input"
/// and presents its paired "CABLE Output" endpoint to recording applications.
pub struct NamedSink {
    playback: WinMMPlayback,
    channels: u32,
}

impl NamedSink {
    pub fn open(device: &str, rate: u32, channels: u32) -> Result<Self, String> {
        let wmm = Arc::new(WinMM::load()?);
        let id = wmm.find_output_device(device)?;
        let playback = WinMMPlayback::open_device(wmm, id, channels, rate)?;
        Ok(Self {
            playback,
            channels: channels.max(1),
        })
    }

    pub fn write(&self, samples: &[i16]) -> Result<usize, String> {
        self.playback.write(samples)?;
        Ok(samples.len() / self.channels as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_winmm() {
        WinMM::load().expect("winmm.dll loads on this host");
    }
}
