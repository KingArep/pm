//! ALAC playback — decode to 16-bit PCM inside the request, serve it as WAV.
//!
//! Android/Eclipse cannot decode ALAC in an `.m4a`, and the PHP port solved
//! that by transcoding to FLAC on disk with ffmpeg (a detached worker, a
//! conversion lock, a prewarm worker, a 30 s first play). None of that is
//! needed here: a music track is small enough to pull into memory, so this
//! module decodes **one range per request** from a `Cursor` and writes nothing
//! to `storage/`.
//!
//! A bare PCM stream is unplayable — the client needs a container to know the
//! sample rate, channel count and sample size — so the PCM is framed by a
//! 44-byte canonical WAV header (`RIFF`/`fmt `/`data`) and served as
//! `audio/wav`. That is the whole "container": no re-encode happens, and the
//! bytes after the header are the decoder's own output.
//!
//! Because there is exactly one decode per request and no shared state, both
//! traps documented for the PHP implementation cannot occur: nothing is piped
//! into the decoder's stdin, and no second decoder ever touches the same file.
//!
//! Sample width: Symphonia hands ALAC back as `i32` (24-bit in a 32-bit
//! container), so the 16-bit form is the top half of each sample
//! ([`pcm_i32_to_i16`]). 16-bit matches what ffmpeg's FLAC output gave the PHP
//! port and halves the bytes a mobile client has to buffer.

use std::io::Cursor;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

/// Size of the canonical PCM WAV header this module emits.
pub const WAV_HEADER_LEN: u64 = 44;

/// Which audio codec an MP4/M4A announces, from the bytes we already hold.
///
/// Why this exists: symphonia only names the codec AFTER it has read the `moov`
/// box, and its error is a bare `CodecType(n)`. Measured on a CPH2581:
/// `no decoder for codec CodecType(4100)` — `4100` is `0x1004`, which is **AAC**
/// (`symphonia_core::codecs::CODEC_TYPE_AAC`); ALAC is `0x2003`. Symphonia 0.5
/// has **no AAC decoder at all**, so an AAC `.m4a` could never be transcoded —
/// yet [`crate::download`] pulled the WHOLE track (8-25 MB) from Telegram before
/// the probe said so, on the first play of every such file.
///
/// The first `stsd` sample entry names the codec (`alac`, `mp4a` = AAC/MP4 audio,
/// …). Works on a prefix of the file, so the answer arrives after the first 1 MB
/// chunk. `None` means "cannot tell yet" — the caller keeps its old behaviour.
pub fn mp4_audio_fourcc(data: &[u8]) -> Option<&'static str> {
    // `at` is the 's' of the tag, so the box header is behind us. Then:
    //   [at+4,  at+8)  version + flags
    //   [at+8,  at+12) entry_count
    //   [at+12, at+16) first sample entry size
    //   [at+16, at+20) first sample entry fourcc
    let needle = b"stsd";
    let at = data.windows(needle.len()).position(|w| w == needle)?;
    let fourcc = data.get(at + 16..at + 20)?;
    match fourcc {
        b"alac" => Some("alac"),
        b"mp4a" => Some("mp4a"),
        b"ac-3" => Some("ac-3"),
        b"ec-3" => Some("ec-3"),
        b"Opus" | b"opus" => Some("opus"),
        b"fLaC" | b"flac" => Some("flac"),
        _ => None,
    }
}

/// What the WAV header (and the byte↔frame mapping) needs to know about the
/// decoded stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavInfo {
    pub sample_rate: u32,
    pub channels: usize,
    /// Total sample frames in the file (not bytes).
    pub n_frames: u64,
}

impl WavInfo {
    pub fn bytes_per_frame(&self) -> u64 {
        (self.channels.max(1) as u64) * 2
    }

    /// Only used to document the byte→time mapping (and asserted in the tests);
    /// the range math itself works in bytes and frames.
    #[allow(dead_code)]
    pub fn bytes_per_second(&self) -> u64 {
        (self.sample_rate as u64) * self.bytes_per_frame()
    }

    pub fn pcm_bytes(&self) -> u64 {
        self.n_frames.saturating_mul(self.bytes_per_frame())
    }

    /// Total length of the WAV representation (header + PCM).
    pub fn total_bytes(&self) -> u64 {
        WAV_HEADER_LEN + self.pcm_bytes()
    }
}

/* ── probe ───────────────────────────────────────────────────────────────── */

/// Open the file with Symphonia's probe (format only) and describe the default
/// track. `None` means "this is not something we can decode" — the caller must
/// fall through to the ordinary byte-for-byte download.
pub fn probe(bytes: &[u8]) -> Option<WavInfo> {
    probe_why(bytes).ok()
}

/// [`probe`] with the reason kept, so a rejection can be logged and diagnosed
/// (`--audio-probe <file>` prints this) instead of looking like a silent
/// fallback.
pub fn probe_why(bytes: &[u8]) -> Result<WavInfo, String> {
    let stream = open_stream(bytes)?;
    let n_frames = stream
        .n_frames
        .filter(|n| *n > 0)
        .ok_or_else(|| "track has no frame count".to_string())?;
    Ok(WavInfo {
        sample_rate: stream.sample_rate,
        channels: stream.channels,
        n_frames,
    })
}

/// An opened track: reader, decoder, and the spec/params the caller needs.
struct Stream {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_rate: u32,
    channels: usize,
    n_frames: Option<u64>,
    /// Track time base, needed to turn a seek's timestamp back into a frame.
    time_base: Option<TimeBase>,
    /// 16-bit LE PCM of frames `[0, prefix_frames)` — the packet decoded to
    /// learn the spec when the container omits the channel layout.
    prefix: Vec<u8>,
    prefix_frames: u64,
}

fn open_stream(bytes: &[u8]) -> Result<Stream, String> {
    let mut format = open(bytes)?;
    let (track_id, params) = default_track_info(&format)
        .ok_or_else(|| "container has no default track".to_string())?;
    // Only claim the file if a decoder for its codec can actually be built.
    // With just the `alac` feature compiled in, an AAC `.m4a` reaches this
    // probe too, and it must NOT be served as an empty WAV — it has to fall
    // through to the normal download path.
    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("no decoder for codec {:?}: {}", params.codec, e))?;

    let mut sample_rate = params.sample_rate.filter(|r| *r > 0);
    let mut channels = params.channels.map(|c| c.count()).filter(|n| *n > 0);
    let mut prefix: Vec<u8> = Vec::new();
    let mut prefix_frames = 0u64;

    // ALAC keeps its channel layout in the magic cookie, NOT in `stsd`, so the
    // container parameters carry no channel count. The decoder read the cookie,
    // so its own first output is the authoritative spec.
    if sample_rate.is_none() || channels.is_none() {
        if let Some((rate, chans, pcm, frames)) =
            decode_prefix_packet(format.as_mut(), decoder.as_mut(), track_id)
        {
            sample_rate = Some(rate);
            channels = Some(chans);
            prefix = pcm;
            prefix_frames = frames;
        }
    }

    Ok(Stream {
        sample_rate: sample_rate.ok_or_else(|| "track has no sample rate".to_string())?,
        channels: channels.ok_or_else(|| "track has no channel layout".to_string())?,
        n_frames: frames_from(&params),
        time_base: params.time_base,
        format,
        decoder,
        track_id,
        prefix,
        prefix_frames,
    })
}

/// Decode the first audio packet, returning its spec and its samples as 16-bit
/// LE PCM. The samples are reused by [`decode_range`] when a range starts
/// inside that packet, so nothing is decoded twice.
fn decode_prefix_packet(
    format: &mut dyn FormatReader,
    decoder: &mut dyn Decoder,
    track_id: u32,
) -> Option<(u32, usize, Vec<u8>, u64)> {
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(_) => return None,
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        let spec = *decoded.spec();
        let mut sb = SampleBuffer::<i32>::new(decoded.capacity() as u64, spec);
        sb.copy_interleaved_ref(decoded);
        let mut pcm = Vec::with_capacity(sb.samples().len() * 2);
        for &raw in sb.samples() {
            pcm.extend_from_slice(&pcm_i32_to_i16(raw).to_le_bytes());
        }
        return Some((spec.rate, spec.channels.count(), pcm, frames as u64));
    }
}

fn open(bytes: &[u8]) -> Result<Box<dyn FormatReader>, String> {
    // `MediaSourceStream` wants a `'static` source, so the slice is copied.
    // Music tracks are a few MB, and this happens once (twice at most) per
    // request — a cheaper borrow would need a custom `MediaSource` for no
    // measurable gain.
    let source = MediaSourceStream::new(
        Box::new(Cursor::new(bytes.to_vec())),
        MediaSourceStreamOptions::default(),
    );
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("container probe failed: {}", e))?;
    Ok(probed.format)
}

fn default_track_info(format: &Box<dyn FormatReader>) -> Option<(u32, symphonia::core::codecs::CodecParameters)> {
    let track = format.default_track()?;
    Some((track.id, track.codec_params.clone()))
}

/// Sample frames in the stream.
///
/// The mp4 demuxer fills `n_frames` from the `mdhd` duration, i.e. in
/// **time-base units**, not necessarily sample frames (`mdhd.timescale` is
/// usually the sample rate, but not guaranteed). Converting through the time
/// base is therefore required for the byte↔frame mapping to line up with the
/// decoded frames.
fn frames_from(params: &symphonia::core::codecs::CodecParameters) -> Option<u64> {
    let raw = params.n_frames?;
    let sample_rate = params.sample_rate? as f64;
    match params.time_base {
        Some(tb) => {
            let secs = (raw as f64) * (tb.numer as f64) / (tb.denom as f64);
            let frames = (secs * sample_rate).round();
            if frames >= 1.0 {
                Some(frames as u64)
            } else {
                None
            }
        }
        None => Some(raw),
    }
}

/// Convert a time-base timestamp into a sample-frame index.
fn frame_of_ts(ts: u64, time_base: Option<TimeBase>, sample_rate: u32) -> u64 {
    match time_base {
        Some(tb) => {
            let secs = (ts as f64) * (tb.numer as f64) / (tb.denom as f64);
            let frames = (secs * sample_rate as f64).round();
            if frames <= 0.0 { 0 } else { frames as u64 }
        }
        None => ts,
    }
}

/* ── WAV header ──────────────────────────────────────────────────────────── */

/// A canonical 44-byte PCM WAV header.
pub fn wav_header(total_pcm_bytes: u64, sample_rate: u32, channels: usize) -> [u8; 44] {
    let ch = channels.clamp(1, u16::MAX as usize) as u16;
    let block_align = ch.saturating_mul(2);
    let byte_rate = sample_rate.saturating_mul(block_align as u32);
    // A RIFF chunk cannot describe more than 4 GB. A lossless music track never
    // comes close, but the fields must stay self-consistent if one ever does.
    let data_len = total_pcm_bytes.min(u32::MAX as u64 - 36) as u32;

    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&ch.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

/* ── samples ─────────────────────────────────────────────────────────────── */

/// Symphonia's internal `i32` sample is full-scale at `i32::MIN..=i32::MAX`, so
/// the 16-bit form is the top half. Clamped so a decoder overshoot cannot wrap
/// a peak into a click.
pub fn pcm_i32_to_i16(v: i32) -> i16 {
    let shifted = v >> 16;
    if shifted > i16::MAX as i32 {
        i16::MAX
    } else if shifted < i16::MIN as i32 {
        i16::MIN
    } else {
        shifted as i16
    }
}

/* ── decode a range ──────────────────────────────────────────────────────── */

/// Decode `want` bytes of 16-bit LE PCM starting at `start_pcm_byte` of the
/// *PCM stream* (i.e. relative to the first sample, not to the WAV header).
///
/// The byte offset is mapped onto sample frames, the reader seeks to that
/// time, and decoding continues forward — so a Range starting mid-track costs
/// one seek instead of decoding from the beginning. Fewer bytes than `want`
/// means end-of-stream (or a decode failure); the caller advertises whatever
/// it actually got.
pub fn decode_range(bytes: &[u8], start_pcm_byte: u64, want: usize) -> Vec<u8> {
    if want == 0 {
        return Vec::new();
    }
    let mut stream = match open_stream(bytes) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let channels = stream.channels;
    let sample_rate = stream.sample_rate;
    let bytes_per_frame = (channels as u64) * 2;
    let target_frame = start_pcm_byte / bytes_per_frame;
    let skip_bytes = (start_pcm_byte % bytes_per_frame) as usize;

    let mut out: Vec<u8> = Vec::with_capacity(want.min(4 << 20));

    // A range that starts inside the packet already decoded for the spec is
    // served from that buffer — it is never decoded twice.
    if target_frame < stream.prefix_frames {
        let from = (target_frame * bytes_per_frame) as usize + skip_bytes;
        emit(&mut out, &stream.prefix, from, want);
    }

    // Seek first — a best-effort optimisation, never a correctness
    // requirement: if the reader refuses, decoding simply continues from the
    // packet after the prefix and the skip below does the same job more slowly.
    let mut cur_frame = stream.prefix_frames;
    if target_frame > stream.prefix_frames && stream.n_frames.is_some() {
        let secs = target_frame as f64 / sample_rate as f64;
        let time = Time::new(secs.trunc() as u64, secs.fract());
        if let Ok(seeked) = stream.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(stream.track_id),
            },
        ) {
            // `Accurate` promises a position at or before the request, but a
            // reader that lands after it would silently drop audio, so never
            // trust anything beyond the target.
            cur_frame =
                frame_of_ts(seeked.actual_ts, stream.time_base, sample_rate).min(target_frame);
        }
    }

    if out.len() < want {
        let rest = decode_forward(
            stream.format.as_mut(),
            stream.decoder.as_mut(),
            stream.track_id,
            cur_frame,
            target_frame,
            skip_bytes,
            want - out.len(),
            channels,
        );
        out.extend_from_slice(&rest);
    }
    out
}

/// Copy up to `want` bytes of `src` starting at `from`.
fn emit(out: &mut Vec<u8>, src: &[u8], from: usize, want: usize) {
    if from >= src.len() {
        return;
    }
    let end = (from + want).min(src.len());
    out.extend_from_slice(&src[from..end]);
}

#[allow(clippy::too_many_arguments)]
fn decode_forward(
    format: &mut dyn FormatReader,
    decoder: &mut dyn Decoder,
    track_id: u32,
    cur_frame: u64,
    target_frame: u64,
    skip_bytes: usize,
    want: usize,
    channels: usize,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(want.min(4 << 20));
    // Samples still to drop: whole frames between where decoding resumes and
    // the target, plus the requested byte offset inside the first emitted frame
    // (always an even number of bytes, because a frame is `channels * 2`).
    let mut skip_samples: u64 =
        target_frame.saturating_sub(cur_frame) * channels as u64 + (skip_bytes / 2) as u64;

    while out.len() < want {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // A damaged packet is skipped; the rest of the track still plays.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        };
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        // `SampleBuffer` asserts its capacity covers the packet, so it is sized
        // from the buffer's own capacity, and `i32` matches ALAC's output
        // (24-bit in a 32-bit container) without a lossy intermediate step.
        let mut sb = SampleBuffer::<i32>::new(decoded.capacity() as u64, *decoded.spec());
        sb.copy_interleaved_ref(decoded);
        let samples = sb.samples();

        if skip_samples >= samples.len() as u64 {
            skip_samples -= samples.len() as u64;
            continue;
        }
        let start = skip_samples as usize;
        skip_samples = 0;

        for &raw in &samples[start..] {
            let room = want - out.len();
            if room == 0 {
                break;
            }
            let le = pcm_i32_to_i16(raw).to_le_bytes();
            if room >= 2 {
                out.extend_from_slice(&le);
            } else {
                // The client's range can end mid-sample.
                out.push(le[0]);
            }
        }
    }
    out
}

/* ── tests ───────────────────────────────────────────────────────────────── */

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(h: &[u8; 44], at: usize) -> u16 {
        u16::from_le_bytes([h[at], h[at + 1]])
    }

    fn u32_at(h: &[u8; 44], at: usize) -> u32 {
        u32::from_le_bytes([h[at], h[at + 1], h[at + 2], h[at + 3]])
    }

    /// A minimal `stsd` box. The first sample entry's fourcc is what separates
    /// ALAC (0x2003, decodable here) from AAC (0x1004, which symphonia 0.5 has
    /// no decoder for — the `CodecType(4100)` in the device log).
    fn stsd_with(fourcc: &[u8; 4]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&[0, 0, 0, 0x30]); // box size
        v.extend_from_slice(b"stsd"); // <- the tag `mp4_audio_fourcc` finds
        v.extend_from_slice(&[0, 0, 0, 0]); // version + flags
        v.extend_from_slice(&[0, 0, 0, 1]); // entry count
        v.extend_from_slice(&[0, 0, 0, 0x24]); // first sample entry size
        v.extend_from_slice(fourcc); // sample entry format
        v.extend_from_slice(&[0; 8]);
        v
    }

    #[test]
    fn mp4_codec_is_read_from_the_first_sample_entry() {
        assert_eq!(mp4_audio_fourcc(&stsd_with(b"alac")), Some("alac"));
        assert_eq!(mp4_audio_fourcc(&stsd_with(b"mp4a")), Some("mp4a"));
        assert_eq!(mp4_audio_fourcc(&stsd_with(b"ac-3")), Some("ac-3"));
        assert_eq!(mp4_audio_fourcc(&stsd_with(b"ec-3")), Some("ec-3"));
        assert_eq!(mp4_audio_fourcc(&stsd_with(b"Opus")), Some("opus"));
    }

    /// Nothing to decide from yet (`moov` at the end of the file, or a bare
    /// `ftyp`) must answer "unknown" — never a wrong codec — so the caller keeps
    /// its old pull-the-whole-file behaviour instead of skipping a real ALAC.
    #[test]
    fn mp4_codec_is_unknown_without_a_sample_entry() {
        assert_eq!(mp4_audio_fourcc(b""), None);
        assert_eq!(
            mp4_audio_fourcc(b"\x00\x00\x00\x18ftypM4A \x00\x00\x00\x00moov"),
            None
        );
        let mut short = stsd_with(b"alac");
        short.truncate(18); // tag + version/flags + entry count, fourcc not yet read
        assert_eq!(mp4_audio_fourcc(&short), None);
    }

    #[test]
    fn wav_header_fields_stereo_44k() {
        let h = wav_header(1000, 44100, 2);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u32_at(&h, 4), 36 + 1000);
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[12..16], b"fmt ");
        assert_eq!(u32_at(&h, 16), 16, "fmt chunk size");
        assert_eq!(u16_at(&h, 20), 1, "PCM format tag");
        assert_eq!(u16_at(&h, 22), 2, "channels");
        assert_eq!(u32_at(&h, 24), 44100, "sample rate");
        assert_eq!(u32_at(&h, 28), 44100 * 2 * 2, "byte rate");
        assert_eq!(u16_at(&h, 32), 4, "block align");
        assert_eq!(u16_at(&h, 34), 16, "bits per sample");
        assert_eq!(&h[36..40], b"data");
        assert_eq!(u32_at(&h, 40), 1000, "data length");
    }

    #[test]
    fn wav_header_fields_mono_8k() {
        let h = wav_header(0, 8000, 1);
        assert_eq!(u32_at(&h, 4), 36);
        assert_eq!(u16_at(&h, 22), 1);
        assert_eq!(u32_at(&h, 24), 8000);
        assert_eq!(u32_at(&h, 28), 8000 * 1 * 2);
        assert_eq!(u16_at(&h, 32), 2);
        assert_eq!(u32_at(&h, 40), 0);
    }

    #[test]
    fn wav_header_clamps_to_a_valid_riff_size() {
        let h = wav_header(u64::MAX, 48000, 2);
        assert_eq!(u32_at(&h, 40), u32::MAX - 36);
        assert_eq!(u32_at(&h, 4), u32::MAX);
    }

    #[test]
    fn sample_conversion_matches_the_top_half() {
        let cases: [(i32, i16); 8] = [
            (0, 0),
            (1 << 16, 1),
            (-(1 << 16), -1),
            (i32::MAX, i16::MAX),
            (i32::MIN, i16::MIN),
            (0x4000_0000, 16384),
            (-0x4000_0000, -16384),
            (0x7FFF_0000, 32767),
        ];
        for (raw, want) in cases {
            assert_eq!(pcm_i32_to_i16(raw), want, "raw={raw}");
        }
    }

    #[test]
    fn wav_info_byte_math() {
        let info = WavInfo {
            sample_rate: 44100,
            channels: 2,
            n_frames: 44100,
        };
        assert_eq!(info.bytes_per_frame(), 4);
        assert_eq!(info.bytes_per_second(), 176_400);
        assert_eq!(info.pcm_bytes(), 176_400);
        assert_eq!(info.total_bytes(), 44 + 176_400);
    }

    #[test]
    fn probe_rejects_non_audio_bytes() {
        assert!(probe(b"definitely not an mp4").is_none());
        assert!(probe(&[]).is_none());
        // The rejection is specific enough to log (this is what a "rejected,
        // falling back to the raw download" report carries).
        let why = probe_why(b"definitely not an mp4").unwrap_err();
        assert!(why.contains("container probe failed"), "{why}");
    }

    #[test]
    fn decode_range_of_garbage_is_empty() {
        assert!(decode_range(b"definitely not an mp4", 0, 4096).is_empty());
        // A zero-length request never decodes anything, even with good input.
        assert!(decode_range(&[], 0, 0).is_empty());
    }

    #[test]
    fn frame_of_ts_uses_the_time_base() {
        let tb = TimeBase::new(1, 44100);
        assert_eq!(frame_of_ts(44100, Some(tb), 44100), 44100);
        // mdhd timescale 1000 at 48 kHz: one second of timestamps is 48k frames.
        let ms = TimeBase::new(1, 1000);
        assert_eq!(frame_of_ts(1000, Some(ms), 48_000), 48_000);
        assert_eq!(frame_of_ts(1234, None, 44100), 1234);
    }

    #[test]
    fn prefix_slice_emit_clips_to_the_source() {
        let mut out = Vec::new();
        emit(&mut out, &[1, 2, 3, 4, 5, 6], 2, 16);
        assert_eq!(out, vec![3, 4, 5, 6]);
        let mut out = Vec::new();
        emit(&mut out, &[1, 2, 3, 4, 5, 6], 4, 2);
        assert_eq!(out, vec![5, 6]);
        let mut out = Vec::new();
        emit(&mut out, &[1, 2], 8, 4);
        assert!(out.is_empty());
    }
}
