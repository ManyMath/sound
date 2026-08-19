//! Pure-Rust audio decoding (via `symphonia`) into interleaved S16LE, which is
//! what the platform playback paths consume. Supports WAV, MP3, OGG/Vorbis and
//! FLAC with no system dependencies.

use std::io::Cursor;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
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
fn decode_all(mut decoder: StreamDecoder) -> Result<DecodedAudio, String> {
    let mut samples = Vec::new();
    while let Some(chunk) = decoder.next_chunk()? {
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
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    StreamDecoder::open(mss, format_hint)
}

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
    pub channels: u32,
    pub rate: u32,
}

impl StreamDecoder {
    fn open(mss: MediaSourceStream, format_hint: Option<&str>) -> Result<StreamDecoder, String> {
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
        let track_id = format
            .default_track()
            .ok_or_else(|| "no audio track found".to_string())?
            .id;
        let decoder = {
            let track = format.default_track().unwrap();
            symphonia::default::get_codecs()
                .make(&track.codec_params, &DecoderOptions::default())
                .map_err(|e| format!("no decoder for track: {e}"))?
        };

        Ok(StreamDecoder {
            format,
            decoder,
            track_id,
            sample_buf: None,
            channels: 0,
            rate: 0,
        })
    }

    /// Decodes and returns the next chunk of interleaved S16LE samples, or
    /// `None` at a clean end of stream. Skips packets for other tracks and
    /// tolerates individual packet decode errors (matching the previous
    /// buffered decoder's behavior), only failing on a genuine I/O or format
    /// error.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<i16>>, String> {
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
                Err(e) => return Err(format!("read error: {e}")),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    self.channels = spec.channels.count() as u32;
                    self.rate = spec.rate;
                    let buf = self
                        .sample_buf
                        .get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, spec));
                    buf.copy_interleaved_ref(decoded);
                    return Ok(Some(buf.samples().to_vec()));
                }
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None)
                }
                Err(SymphoniaError::IoError(e)) => return Err(format!("decode error: {e}")),
                Err(e) => return Err(format!("decode error: {e}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::io::Cursor;

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
    /// returning the URL. Exercises the full `net` -> `StreamingSource` ->
    /// `StreamDecoder` chain end-to-end against a real (if synthetic)
    /// encoded track, the same way [sound_play_url] does.
    fn serve_slowly(body: Vec<u8>) -> String {
        use std::io::Write;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
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
                stream.write_all(chunk).unwrap();
                stream.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        format!("http://{addr}/track.wav")
    }

    #[test]
    fn decodes_wav_streamed_over_http_incrementally() {
        let wav = const_wav(2, 44_100, 16, SampleFormat::Int, 4_000, 777);
        let url = serve_slowly(wav);

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
