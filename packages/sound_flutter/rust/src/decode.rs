//! Pure-Rust audio decoding (via `symphonia`) into interleaved S16LE, which is
//! what the platform playback paths consume. Supports WAV, MP3, OGG/Vorbis and
//! FLAC with no system dependencies.

use std::any::Any;
use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::streaming_source::StreamingSource;

/// Decoded audio ready for playback.
pub struct DecodedAudio {
    pub samples: Vec<i16>,
    pub channels: u32,
    pub rate: u32,
}

/// Most interleaved samples a single decode may produce: 512 MiB of S16, or
/// about 50 minutes of 44.1 kHz stereo. Compressed audio can expand by
/// orders of magnitude, so input size alone does not bound memory use.
pub const MAX_DECODED_SAMPLES: usize = 1 << 28;

/// The error reported when decoding passes [MAX_DECODED_SAMPLES].
pub const DECODED_LIMIT_ERROR: &str = "decoded audio exceeds the 512 MiB limit";

/// Decodes encoded audio [bytes] (format auto-detected) into interleaved S16LE.
///
/// [format_hint] is an optional file extension (e.g. `mp3`) that helps the
/// probe; decoding still works without it.
pub fn decode_bytes(bytes: &[u8], format_hint: Option<&str>) -> Result<DecodedAudio, String> {
    let source = Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    decode_all(StreamDecoder::open(mss, format_hint)?)
}

/// Decodes an audio file at [path] (format auto-detected) into interleaved S16LE.
pub fn decode_file(path: &str) -> Result<DecodedAudio, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let hint = path.rsplit('.').next();
    decode_all(StreamDecoder::open(mss, hint)?)
}

/// Fully decodes whatever [StreamDecoder] yields into one buffer. Used by the
/// bytes/file entry points, which have no reason to play incrementally since
/// their whole input is already available locally.
fn decode_all(decoder: StreamDecoder) -> Result<DecodedAudio, String> {
    decode_all_limited(decoder, MAX_DECODED_SAMPLES)
}

/// [decode_all] with the sample cap as a parameter. The cap is checked before
/// each chunk is appended, so an oversized input fails without first
/// allocating its full decoded size.
fn decode_all_limited(
    mut decoder: StreamDecoder,
    max_samples: usize,
) -> Result<DecodedAudio, String> {
    let mut samples = Vec::new();
    while let Some(chunk) = decoder.next_chunk()? {
        if chunk.len() > max_samples - samples.len() {
            return Err(DECODED_LIMIT_ERROR.to_string());
        }
        samples.extend_from_slice(&chunk);
    }
    if decoder.channels == 0 || decoder.rate == 0 {
        return Err("decoded zero audio frames".to_string());
    }
    Ok(DecodedAudio {
        samples,
        channels: decoder.channels,
        rate: decoder.rate,
    })
}

/// Opens a streaming decode of `url` (with `headers`), for a caller that
/// consumes [StreamDecoder::next_chunk] incrementally so playback can begin
/// before the whole response has arrived. [format_hint] is an optional file
/// extension that helps the probe when the URL's path has no recognizable
/// one. Setting `stop` aborts the download and unblocks a blocked
/// [StreamDecoder::next_chunk].
pub fn open_url_decoder(
    url: String,
    headers: Vec<(String, String)>,
    format_hint: Option<&str>,
    stop: Arc<AtomicBool>,
) -> Result<StreamDecoder, String> {
    let source = StreamingSource::fetch(url, headers, stop);
    let fetch = source.status();
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    // A failed fetch fails the probe too, for a misleading reason (an HTTP
    // 404 reads as "no suitable format reader found"); report the fetch's.
    StreamDecoder::open(mss, format_hint).map_err(|e| fetch.error().unwrap_or(e))
}

/// How many packets in a row may fail to decode before the stream is taken to
/// be undecodable, rather than just damaged in places.
const MAX_CONSECUTIVE_BAD_PACKETS: u32 = 1000;

/// Decodes one packet at a time from a [MediaSourceStream], so a caller can
/// start consuming audio before the whole source has been read (relevant
/// when the source is still downloading, as with [StreamingSource]).
///
/// `channels`/`rate` are `0` until the first packet decodes, since some
/// formats don't expose them before that; callers reading incrementally
/// should treat the first successful [next_chunk] as also finalizing them.
pub struct StreamDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_buf: Option<SampleBuffer<i16>>,
    /// Set once the stream has ended early (see [Self::next_chunk]), so later
    /// calls keep reporting the end instead of resuming.
    ended: bool,
    /// Set once a panic inside symphonia has been caught; the reader and
    /// decoder state is unknown after that, so later calls fail instead.
    poisoned: bool,
    pub channels: u32,
    pub rate: u32,
}

impl StreamDecoder {
    /// Probes `mss` and builds a decoder for its first decodable track. Like
    /// [Self::next_chunk], turns a panic inside symphonia into an error.
    fn open(mss: MediaSourceStream, format_hint: Option<&str>) -> Result<StreamDecoder, String> {
        catch_unwind(AssertUnwindSafe(|| Self::open_unguarded(mss, format_hint)))
            .unwrap_or_else(|panic| Err(panic_error(panic)))
    }

    fn open_unguarded(
        mss: MediaSourceStream,
        format_hint: Option<&str>,
    ) -> Result<StreamDecoder, String> {
        let mut hint = Hint::new();
        if let Some(ext) = format_hint {
            hint.with_extension(ext);
        }

        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .map_err(|e| format!("unsupported or invalid audio: {e}"))?;

        let format = probed.format;
        let (track_id, decoder) = make_decoder(format.as_ref())?;

        Ok(StreamDecoder {
            format,
            decoder,
            track_id,
            sample_buf: None,
            ended: false,
            poisoned: false,
            channels: 0,
            rate: 0,
        })
    }

    /// Decodes and returns the next chunk of interleaved S16LE samples, or
    /// `None` at a clean end of stream. Skips packets for other tracks and
    /// packets that fail to decode (up to [MAX_CONSECUTIVE_BAD_PACKETS] in a
    /// row), only failing on a genuine I/O or format error.
    ///
    /// Chained streams (consecutive Ogg links) are decoded one after another
    /// as long as they keep the first one's channel count and sample rate,
    /// which callers fix at the first chunk; the stream ends at the first
    /// link that changes either.
    ///
    /// symphonia asserts on some malformed input. Such a panic comes back as
    /// an error rather than unwinding into the caller (across the C ABI that
    /// would abort the host process), and the decoder is unusable after it.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<i16>>, String> {
        if self.poisoned {
            return Err("decoder unusable after an earlier panic".to_string());
        }
        match catch_unwind(AssertUnwindSafe(|| self.next_chunk_unguarded())) {
            Ok(result) => result,
            Err(panic) => {
                self.poisoned = true;
                Err(panic_error(panic))
            }
        }
    }

    fn next_chunk_unguarded(&mut self) -> Result<Option<Vec<i16>>, String> {
        if self.ended {
            return Ok(None);
        }
        let mut bad_packets = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                // symphonia signals a clean end-of-stream as an UnexpectedEof
                // IO error. Any OTHER IO error (a dropped connection, a read
                // timeout, a truncated download) is a real failure and must
                // not masquerade as successful completion.
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None)
                }
                Err(SymphoniaError::IoError(e)) => return Err(format!("read error: {e}")),
                // The reader moved on to a new stream with its own tracks (a
                // chained Ogg link): decode it with a fresh decoder, and a
                // fresh sample buffer sized for that decoder's packets.
                Err(SymphoniaError::ResetRequired) => {
                    (self.track_id, self.decoder) = make_decoder(self.format.as_ref())?;
                    self.sample_buf = None;
                    continue;
                }
                Err(e) => return Err(format!("read error: {e}")),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let channels = spec.channels.count() as u32;
                    if self.rate != 0 && (channels != self.channels || spec.rate != self.rate) {
                        // A chained link in another format; see above.
                        self.ended = true;
                        return Ok(None);
                    }
                    self.channels = channels;
                    self.rate = spec.rate;
                    let buf = self
                        .sample_buf
                        .get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, spec));
                    buf.copy_interleaved_ref(decoded);
                    return Ok(Some(buf.samples().to_vec()));
                }
                // decode() only reads the packet already in memory, so every
                // error here means that one packet is malformed -- including
                // an IoError, which is how symphonia reports a packet shorter
                // than its own contents claim. It never means the stream
                // ended, so skip the packet rather than stopping or failing.
                Err(e) => {
                    bad_packets += 1;
                    if bad_packets >= MAX_CONSECUTIVE_BAD_PACKETS {
                        return Err(format!(
                            "decode error: {e} ({bad_packets} packets in a row failed)"
                        ));
                    }
                }
            }
        }
    }
}

/// Describes a panic caught from symphonia as a decode error.
fn panic_error(panic: Box<dyn Any + Send>) -> String {
    let cause = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown cause");
    format!("decoder panicked on malformed input: {cause}")
}

/// Picks the first track symphonia can decode and builds its decoder.
///
/// Not `default_track()`: that is just the first track, and Ogg orders its
/// tracks by stream serial number, so it can be a stream symphonia does not
/// recognize at all (e.g. an Ogg Skeleton index), with no codec.
fn make_decoder(format: &dyn FormatReader) -> Result<(u32, Box<dyn Decoder>), String> {
    let mut error = "no audio track found".to_string();
    for track in format
        .tracks()
        .iter()
        .filter(|t| t.codec_params.codec != CODEC_TYPE_NULL)
    {
        match symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())
        {
            Ok(decoder) => return Ok((track.id, decoder)),
            Err(e) => error = format!("no decoder for track: {e}"),
        }
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Writes a `frames`-long WAV (one interleaved value per sample) with the
    /// given spec, filling every sample with `value` (int) for easy assertions.
    fn const_wav(
        channels: u16,
        rate: u32,
        bits: u16,
        format: SampleFormat,
        frames: u32,
        value: i32,
    ) -> Vec<u8> {
        let spec = WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: bits,
            sample_format: format,
        };
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = WavWriter::new(&mut buf, spec).unwrap();
            for _ in 0..frames * channels as u32 {
                match format {
                    SampleFormat::Int => w.write_sample(value).unwrap(),
                    SampleFormat::Float => w.write_sample(value as f32 / i16::MAX as f32).unwrap(),
                }
            }
            w.finalize().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn decodes_stereo_and_preserves_frame_count() {
        let wav = const_wav(2, 44_100, 16, SampleFormat::Int, 100, 1000);
        let audio = decode_bytes(&wav, Some("wav")).unwrap();
        assert_eq!(audio.channels, 2);
        assert_eq!(audio.rate, 44_100);
        assert_eq!(audio.samples.len(), 200); // 100 frames * 2 channels
        assert!(audio.samples.iter().all(|&s| s == 1000));
    }

    #[test]
    fn decodes_non_default_sample_rate() {
        let wav = const_wav(1, 48_000, 16, SampleFormat::Int, 64, 0);
        let audio = decode_bytes(&wav, Some("wav")).unwrap();
        assert_eq!(audio.rate, 48_000);
        assert_eq!(audio.channels, 1);
    }

    #[test]
    fn scales_8bit_up_to_16bit() {
        // 8-bit stores ~half-scale; decoding shifts it up into 16-bit range.
        let wav = const_wav(1, 8_000, 8, SampleFormat::Int, 16, 100);
        let audio = decode_bytes(&wav, Some("wav")).unwrap();
        // value 100 (8-bit) << 8 ~ 25600.
        assert!(audio
            .samples
            .iter()
            .all(|&s| (s as i32 - 25_600).abs() < 512));
    }

    #[test]
    fn scales_24bit_down_to_16bit() {
        // 24-bit value shifted down by 8 bits to fit 16-bit.
        let wav = const_wav(1, 44_100, 24, SampleFormat::Int, 16, 0x10_0000);
        let audio = decode_bytes(&wav, Some("wav")).unwrap();
        // 0x100000 >> 8 == 0x1000 == 4096.
        assert!(audio.samples.iter().all(|&s| (s as i32 - 4096).abs() < 4));
    }

    #[test]
    fn decodes_float_wav() {
        let wav = const_wav(1, 44_100, 32, SampleFormat::Float, 32, 16_000);
        let audio = decode_bytes(&wav, Some("wav")).unwrap();
        assert_eq!(audio.channels, 1);
        // ~16000/32767 scaled back to ~16000.
        assert!(audio.samples.iter().all(|&s| (s as i32 - 16_000).abs() < 8));
    }

    #[test]
    fn decode_cap_rejects_oversized_audio() {
        let wav = const_wav(1, 8_000, 16, SampleFormat::Int, 1000, 7);
        let open = || {
            let mss =
                MediaSourceStream::new(Box::new(Cursor::new(wav.clone())), Default::default());
            StreamDecoder::open(mss, Some("wav")).unwrap()
        };
        let err = decode_all_limited(open(), 999).err().unwrap();
        assert_eq!(err, DECODED_LIMIT_ERROR);
        let audio = decode_all_limited(open(), 1000).unwrap();
        assert_eq!(audio.samples.len(), 1000);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_bytes(&[0u8; 64], None).is_err());
    }

    /// Encodes 16-bit samples to FLAC (lossless) in memory.
    fn encode_flac(samples: &[i32], channels: usize, rate: usize) -> Vec<u8> {
        use flacenc::bitsink::ByteSink;
        use flacenc::component::BitRepr;
        use flacenc::error::Verify;
        let config = flacenc::config::Encoder::default()
            .into_verified()
            .expect("flac config");
        let source = flacenc::source::MemSource::from_samples(samples, channels, 16, rate);
        let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
            .expect("flac encode");
        let mut sink = ByteSink::new();
        stream.write(&mut sink).expect("flac write");
        sink.as_slice().to_vec()
    }

    /// Encodes 16-bit samples to FLAC in `block`-frame blocks, returning the
    /// STREAMINFO block's body and each encoded frame separately, for
    /// wrapping in an Ogg container with [ogg_flac].
    fn flac_parts(
        samples: &[i32],
        channels: usize,
        rate: usize,
        block: usize,
    ) -> (Vec<u8>, Vec<Vec<u8>>) {
        use flacenc::bitsink::ByteSink;
        use flacenc::component::BitRepr;
        use flacenc::error::Verify;
        let mut config = flacenc::config::Encoder::default();
        config.block_size = block;
        let config = config.into_verified().expect("flac config");
        let source = flacenc::source::MemSource::from_samples(samples, channels, 16, rate);
        let stream =
            flacenc::encode_with_fixed_block_size(&config, source, block).expect("flac encode");
        let mut sink = ByteSink::new();
        stream.write(&mut sink).expect("flac write");
        // "fLaC", then STREAMINFO's 4-byte block header and 34-byte body.
        let streaminfo = sink.as_slice()[8..8 + 34].to_vec();
        let frames = (0..stream.frame_count())
            .map(|i| {
                let mut sink = ByteSink::new();
                stream
                    .frame(i)
                    .unwrap()
                    .write(&mut sink)
                    .expect("flac write");
                sink.as_slice().to_vec()
            })
            .collect();
        (streaminfo, frames)
    }

    /// One Ogg page carrying exactly one whole `packet`.
    fn ogg_page(header_type: u8, granule: u64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
        let mut lacing = vec![255u8; packet.len() / 255];
        lacing.push((packet.len() % 255) as u8);
        let mut page = b"OggS\0".to_vec();
        page.push(header_type);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&serial.to_le_bytes());
        page.extend_from_slice(&seq.to_le_bytes());
        page.extend_from_slice(&[0; 4]); // CRC, filled in below
        page.push(lacing.len() as u8);
        page.extend_from_slice(&lacing);
        page.extend_from_slice(packet);
        // Ogg's CRC-32: polynomial 0x04C11DB7, unreflected, no final XOR.
        let mut crc = 0u32;
        for &b in &page {
            crc ^= (b as u32) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
            }
        }
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        page
    }

    /// Wraps FLAC `frames` (each `block` samples long) as one Ogg FLAC
    /// logical stream with the given `serial`, one frame per page.
    fn ogg_flac(streaminfo: &[u8], frames: &[Vec<u8>], block: u64, serial: u32) -> Vec<u8> {
        // Ogg FLAC mapping v1.0 identification packet, then the STREAMINFO
        // block (flagged as the last metadata block).
        let mut id = b"\x7fFLAC\x01\x00\x00\x00fLaC\x80\x00\x00\x22".to_vec();
        id.extend_from_slice(streaminfo);
        let mut ogg = ogg_page(0x02, 0, serial, 0, &id);
        for (i, frame) in frames.iter().enumerate() {
            let eos = if i + 1 == frames.len() { 0x04 } else { 0 };
            let granule = block * (i as u64 + 1);
            ogg.extend_from_slice(&ogg_page(eos, granule, serial, i as u32 + 1, frame));
        }
        ogg
    }

    fn sine(len: usize) -> Vec<i32> {
        (0..len)
            .map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i32)
            .collect()
    }

    #[test]
    fn skips_unrecognized_tracks_when_picking_one_to_decode() {
        // An unknown logical stream (here a fake Ogg Skeleton header) with
        // a lower serial than the audio sorts first among the tracks, and
        // used to be picked as the default track, which has no decoder.
        let (streaminfo, frames) = flac_parts(&sine(4096 * 2), 1, 44_100, 4096);
        let mut skeleton = b"fishead\0".to_vec();
        skeleton.resize(64, 0);
        let mut ogg = ogg_page(0x02, 0, 1, 0, &skeleton);
        ogg.extend_from_slice(&ogg_flac(&streaminfo, &frames, 4096, 0x1234));
        let audio = decode_bytes(&ogg, None).unwrap();
        assert_eq!(audio.rate, 44_100);
        assert_eq!(audio.samples.len(), 4096 * 2);
    }

    #[test]
    fn skips_a_malformed_packet_mid_stream() {
        // A packet cut short reports an IoError from decode(): a few bytes
        // in, symphonia's UnexpectedEof "buffer underrun" used to end the
        // stream there silently; further in, "unexpected end of bitstream"
        // used to fail the whole decode.
        let (streaminfo, frames) = flac_parts(&sine(4096 * 6), 1, 44_100, 4096);
        for cut in [3, frames[1].len() / 2] {
            let mut damaged = frames.clone();
            damaged[1].truncate(cut);
            let ogg = ogg_flac(&streaminfo, &damaged, 4096, 0x1234);
            let audio = decode_bytes(&ogg, None).unwrap();
            assert_eq!(audio.samples.len(), 4096 * 5, "cut to {cut} bytes");
        }
    }

    #[test]
    fn gives_up_after_too_many_bad_packets_in_a_row() {
        let (streaminfo, frames) = flac_parts(&sine(4096 * 2), 1, 44_100, 4096);
        let mut bad = frames[1].clone();
        bad.truncate(3);
        let mut damaged = vec![frames[0].clone()];
        damaged.extend(std::iter::repeat_n(
            bad,
            MAX_CONSECUTIVE_BAD_PACKETS as usize,
        ));
        damaged.push(frames[1].clone());
        let ogg = ogg_flac(&streaminfo, &damaged, 4096, 0x1234);
        let err = decode_bytes(&ogg, None).err().expect("should give up");
        assert!(err.contains("in a row"), "{err}");
    }

    fn open_bytes(bytes: Vec<u8>) -> StreamDecoder {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        StreamDecoder::open(mss, None).unwrap()
    }

    #[test]
    fn decodes_chained_ogg_streams() {
        // Two Ogg links back to back. Starting the second made symphonia
        // return ResetRequired, which used to fail the whole decode.
        let (streaminfo, frames) = flac_parts(&sine(4096 * 3), 1, 44_100, 4096);
        let mut ogg = ogg_flac(&streaminfo, &frames[..2], 4096, 0x1234);
        ogg.extend_from_slice(&ogg_flac(&streaminfo, &frames, 4096, 0x5678));
        let audio = decode_bytes(&ogg, None).unwrap();
        assert_eq!(audio.rate, 44_100);
        assert_eq!(audio.samples.len(), 4096 * 5);
    }

    #[test]
    fn ends_a_chained_ogg_stream_at_a_format_change() {
        let (streaminfo, frames) = flac_parts(&sine(4096 * 2), 1, 44_100, 4096);
        let (streaminfo_48k, frames_48k) = flac_parts(&sine(4096 * 2), 1, 48_000, 4096);
        let mut ogg = ogg_flac(&streaminfo, &frames, 4096, 0x1234);
        ogg.extend_from_slice(&ogg_flac(&streaminfo_48k, &frames_48k, 4096, 0x5678));
        let mut decoder = open_bytes(ogg);
        let mut samples = 0;
        while let Some(chunk) = decoder.next_chunk().unwrap() {
            samples += chunk.len();
        }
        assert_eq!((samples, decoder.rate), (4096 * 2, 44_100));
        // The end sticks rather than resuming with the 48 kHz link.
        assert!(decoder.next_chunk().unwrap().is_none());
    }

    #[test]
    fn contains_a_decoder_panic_on_malformed_input() {
        // STREAMINFO claims 16-sample blocks, but the frames hold 4096. The
        // FLAC decoder sizes its buffer from STREAMINFO and symphonia then
        // asserts ("capacity will be exceeded"); through the C ABI that
        // panic used to abort the whole host process.
        let (mut streaminfo, frames) = flac_parts(&sine(4096 * 2), 1, 44_100, 4096);
        streaminfo[0..4].copy_from_slice(&[0, 16, 0, 16]); // min/max block size
        let ogg = ogg_flac(&streaminfo, &frames, 4096, 0x1234);
        assert!(decode_bytes(&ogg, None).is_err());

        let mut decoder = open_bytes(ogg);
        let err = decoder.next_chunk().expect_err("decoding must fail");
        assert!(err.contains("panicked"), "{err}");
        // Its state is unknown after the panic, so it refuses to carry on.
        assert!(decoder.next_chunk().is_err());
    }

    #[test]
    fn round_trips_flac_losslessly() {
        // A 440 Hz mono ramp encoded to FLAC and decoded back must match,
        // proving a real compressed codec path (not just WAV).
        let rate = 44_100usize;
        let input: Vec<i32> = (0..2_205)
            .map(|i| {
                (0.3 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin()
                    * i16::MAX as f32) as i32
            })
            .collect();
        let flac = encode_flac(&input, 1, rate);

        let audio = decode_bytes(&flac, Some("flac")).unwrap();
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.rate, 44_100);
        // Fixed-block encoding pads the final block, so the decoded stream is at
        // least as long as the input; the meaningful prefix must match exactly
        // (FLAC is lossless).
        assert!(audio.samples.len() >= input.len());
        for (got, want) in audio.samples.iter().zip(&input) {
            assert_eq!(*got as i32, *want);
        }
    }

    /// Serves `body` slowly (in small delayed writes) over plain HTTP,
    /// returning the URL and a count of the body bytes sent so far (each
    /// write counted before it starts, so the client can't be ahead of it).
    /// Exercises the full `net` -> `StreamingSource` ->
    /// `StreamDecoder` chain end-to-end against a real (if synthetic)
    /// encoded track, the same way [sound_play_url] does.
    fn serve_slowly(body: Vec<u8>) -> (String, Arc<AtomicUsize>) {
        use std::io::Write;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let sent = Arc::new(AtomicUsize::new(0));
        let sent_count = sent.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut discard = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut discard);
            stream
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
                )
                .unwrap();
            for chunk in body.chunks(512) {
                sent_count.fetch_add(chunk.len(), Ordering::SeqCst);
                // The client may hang up early once it has what it needs.
                if stream
                    .write_all(chunk)
                    .and_then(|_| stream.flush())
                    .is_err()
                {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        (format!("http://{addr}/track"), sent)
    }

    #[test]
    fn decodes_wav_streamed_over_http_incrementally() {
        let wav = const_wav(2, 44_100, 16, SampleFormat::Int, 4_000, 777);
        let (url, _) = serve_slowly(wav);

        let mut decoder = open_url_decoder(
            url,
            Vec::new(),
            Some("wav"),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let mut samples = Vec::new();
        let mut chunk_count = 0;
        while let Some(chunk) = decoder.next_chunk().unwrap() {
            chunk_count += 1;
            samples.extend_from_slice(&chunk);
        }
        assert_eq!(decoder.channels, 2);
        assert_eq!(decoder.rate, 44_100);
        assert_eq!(samples.len(), 8_000); // 4,000 frames * 2 channels
        assert!(samples.iter().all(|&s| s == 777));
        // More than one chunk proves packets were consumed as they decoded,
        // not all at once after the whole body arrived.
        assert!(
            chunk_count > 1,
            "expected multiple decoded chunks, got {chunk_count}"
        );
    }

    #[test]
    fn open_url_decoder_reports_the_fetch_error_over_the_probe_error() {
        // A 404 means the probe never sees any bytes, so it fails too; its
        // reason ("no suitable format reader found") used to hide the HTTP
        // status.
        use std::io::Write;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut discard = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut discard);
            stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found")
                .unwrap();
        });
        let err = open_url_decoder(
            format!("http://{addr}/missing.mp3"),
            Vec::new(),
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .err()
        .expect("a 404 must not open");
        assert!(err.contains("404"), "{err}");
    }

    /// `frames` MPEG-1 Layer III frames of digital silence (mono, 44.1 kHz,
    /// 128 kbps, no CRC): a header, all-zero side info, zero main data.
    fn silent_mp3(frames: usize) -> Vec<u8> {
        let mut frame = vec![0u8; 417]; // 144 * 128000 / 44100, unpadded
        frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0xC0]);
        frame.repeat(frames)
    }

    /// Pseudo-random 16-bit noise, which FLAC barely compresses.
    fn noise(len: usize) -> Vec<i32> {
        let mut x = 1u32;
        (0..len)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((x >> 16) as i16 / 4) as i32
            })
            .collect()
    }

    #[test]
    fn probes_every_format_from_a_streamed_source() {
        // The streaming source is not seekable; each supported container
        // must still probe and decode from it, with no format hint.
        let (streaminfo, frames) = flac_parts(&sine(4096 * 2), 1, 48_000, 4096);
        for (name, body, rate, samples) in [
            (
                "wav",
                const_wav(1, 22_050, 16, SampleFormat::Int, 1_000, 5),
                22_050,
                1_000,
            ),
            (
                "flac",
                encode_flac(&sine(4096 * 2), 1, 44_100),
                44_100,
                8_192,
            ),
            (
                "ogg",
                ogg_flac(&streaminfo, &frames, 4096, 0x1234),
                48_000,
                8_192,
            ),
            ("mp3", silent_mp3(20), 44_100, 20 * 1152),
        ] {
            let (url, _) = serve_slowly(body);
            let mut decoder =
                open_url_decoder(url, Vec::new(), None, Arc::new(AtomicBool::new(false)))
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
            let mut decoded = 0;
            while let Some(chunk) = decoder.next_chunk().unwrap() {
                decoded += chunk.len();
            }
            assert_eq!((decoder.rate, decoded), (rate, samples), "{name}");
        }
    }

    #[test]
    fn starts_decoding_a_streamed_ogg_before_the_download_finishes() {
        // The source used to claim to be seekable, so opening an Ogg stream
        // with a known Content-Length made symphonia seek to near its end
        // (to measure its duration), which blocked until the whole body had
        // downloaded.
        let (streaminfo, frames) = flac_parts(&noise(4096 * 24), 1, 44_100, 4096);
        let ogg = ogg_flac(&streaminfo, &frames, 4096, 0x1234);
        let len = ogg.len();
        assert!(len > 128 * 1024);
        let (url, sent) = serve_slowly(ogg);
        let mut decoder =
            open_url_decoder(url, Vec::new(), None, Arc::new(AtomicBool::new(false))).unwrap();
        assert!(decoder.next_chunk().unwrap().is_some());
        let sent = sent.load(Ordering::SeqCst);
        assert!(
            sent < len / 2,
            "first chunk decoded only after {sent} of {len} bytes were sent"
        );
    }

    #[test]
    fn open_url_decoder_reports_headers_sent() {
        use std::io::Write;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                let n = std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
                if n == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            let wav = if request.contains("Authorization: Bearer secret") {
                const_wav(1, 22_050, 16, SampleFormat::Int, 8, 5)
            } else {
                Vec::new()
            };
            stream
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", wav.len()).as_bytes(),
                )
                .unwrap();
            stream.write_all(&wav).unwrap();
        });
        let url = format!("http://{addr}/track.wav");
        let mut decoder = open_url_decoder(
            url,
            vec![("Authorization".to_string(), "Bearer secret".to_string())],
            Some("wav"),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let mut samples = Vec::new();
        while let Some(chunk) = decoder.next_chunk().unwrap() {
            samples.extend_from_slice(&chunk);
        }
        assert_eq!(decoder.rate, 22_050);
        assert_eq!(samples.len(), 8);
    }
}
