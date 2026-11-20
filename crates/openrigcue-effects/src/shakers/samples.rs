//! Shaker samples: the folder/number scheme, the built-in sounds (the default,
//! [`SampleSource::BuiltIn`]) and, as an option, importing the user's SRS sound files from their
//! installation or installer ([`SampleSource::SrsImport`]).

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use super::native_mono::Format as NativeFormat;
use super::nsis::{NsisArchive, NsisError};
use super::synth::synthesize;

mod vorbis_window;

/// A sub-folder of a `sounds` directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleFolder {
    Suspension,
    Transmission,
    Afterburner,
    CannonFiring,
    MissileFiring,
    Canopy,
    LandingGear,
    Turbulence,
}

impl SampleFolder {
    pub fn dir_name(self) -> &'static str {
        match self {
            SampleFolder::Suspension => "suspension",
            SampleFolder::Transmission => "transmission",
            SampleFolder::Afterburner => "afterburner",
            SampleFolder::CannonFiring => "cannon_firing",
            SampleFolder::MissileFiring => "missile_firing",
            SampleFolder::Canopy => "canopy",
            SampleFolder::LandingGear => "landing_gear",
            SampleFolder::Turbulence => "turbulence",
        }
    }
}

/// `sounds/<folder>/<index>.ogg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SampleKey {
    pub folder: SampleFolder,
    pub index: u8,
}

impl SampleKey {
    pub const fn new(folder: SampleFolder, index: u8) -> SampleKey {
        SampleKey { folder, index }
    }

    /// Every sample the shaker engine can play.
    pub const PLAYED: [SampleKey; 14] = [
        SampleKey::new(SampleFolder::Suspension, 0),
        SampleKey::new(SampleFolder::Suspension, 1),
        SampleKey::new(SampleFolder::Suspension, 2),
        SampleKey::new(SampleFolder::Suspension, 3),
        SampleKey::new(SampleFolder::Suspension, 4),
        SampleKey::new(SampleFolder::Suspension, 5),
        SampleKey::new(SampleFolder::Suspension, 6),
        SampleKey::new(SampleFolder::Transmission, 1),
        SampleKey::new(SampleFolder::Afterburner, 0),
        SampleKey::new(SampleFolder::CannonFiring, 0),
        SampleKey::new(SampleFolder::MissileFiring, 0),
        SampleKey::new(SampleFolder::Canopy, 0),
        SampleKey::new(SampleFolder::LandingGear, 0),
        SampleKey::new(SampleFolder::Turbulence, 0),
    ];

    /// Path below the `sounds` directory (`file_extension` is `.ogg`).
    pub fn relative_path(self) -> String {
        format!("{}/{}.ogg", self.folder.dir_name(), self.index)
    }
}

/// Decoded PCM.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub rate: u32,
    pub channels: u16,
    /// Interleaved, nominally -1..1. Six-channel data is in speaker order
    /// (FL, FR, C, LFE, RL, RR, as [`super::layout::Speaker::ALL`]), the order of WAV files.
    /// Eight-channel data additionally follows WAVE order: FL, FR, C, LFE, BL, BR, SL, SR.
    pub data: Vec<f32>,
}

/// For each speaker of [`super::layout::Speaker::ALL`], its channel in a six-channel Vorbis
/// stream, whose order is FL, C, FR, RL, RR, LFE (Vorbis I specification, section 4.3.9).
const VORBIS_51_CHANNEL_OF_SPEAKER: [usize; 6] = [0, 2, 1, 5, 3, 4];
/// The eight-channel permutation from Vorbis order to WAVE order.
const VORBIS_71_CHANNEL_OF_SPEAKER: [usize; 8] = [0, 2, 1, 7, 5, 6, 3, 4];

/// Reorder the six/eight-channel Vorbis layouts into speaker order.
fn vorbis_to_speaker_order(channels: u16, data: Vec<f32>) -> Vec<f32> {
    match channels {
        6 => data.as_chunks::<6>().0.iter().flat_map(|frame| VORBIS_51_CHANNEL_OF_SPEAKER.map(|c| frame[c])).collect(),
        8 => data.as_chunks::<8>().0.iter().flat_map(|frame| VORBIS_71_CHANNEL_OF_SPEAKER.map(|c| frame[c])).collect(),
        _ => data,
    }
}

impl Sample {
    pub fn frames(&self) -> usize {
        self.data.len() / usize::from(self.channels.max(1))
    }
}

#[derive(Debug, Error)]
pub enum SampleError {
    #[error("cannot read {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("installer: {0}")]
    Installer(#[from] NsisError),
    #[error("Ogg Vorbis decoding failed: {0}")]
    Vorbis(String),
    #[error("WAV decoding failed: {0}")]
    Wav(String),
    #[error("{0} has no audio")]
    Empty(String),
}

/// Where a sample of the bank came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleOrigin {
    Imported,
    Synthesized,
}

/// Which sounds the shakers play.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SampleSource {
    /// OpenRigCue's built-in sounds ([`super::synth`]), the default.
    #[default]
    BuiltIn,
    /// The user's own SRS recordings, read from an SRS install directory, its `sounds`
    /// directory or the SRS installer; a sample that cannot be read keeps its built-in sound.
    SrsImport(PathBuf),
}

/// The samples the mixer plays.
#[derive(Debug, Clone)]
pub struct SampleBank {
    samples: HashMap<SampleKey, (Arc<Sample>, SampleOrigin)>,
    source_formats: HashMap<SampleKey, NativeFormat>,
    /// A stream shorter than its advertised length keeps that length but has a shorter loop region.
    loop_frames: HashMap<SampleKey, usize>,
    /// A Sound loop starting after frame 0 (a WAV smpl loop).
    loop_starts: HashMap<SampleKey, usize>,
}

impl SampleBank {
    /// The built-in samples only.
    pub fn synthesized() -> SampleBank {
        let samples = SampleKey::PLAYED
            .into_iter()
            .filter_map(|k| synthesize(k).map(|s| (k, (Arc::new(s), SampleOrigin::Synthesized))))
            .collect();
        SampleBank { samples, source_formats: HashMap::new(), loop_frames: HashMap::new(), loop_starts: HashMap::new() }
    }

    /// The bank for `source`. An import that cannot open its path at all gives the built-in
    /// bank; the list says what was not imported and why (`None`: the whole source).
    pub fn load(source: &SampleSource) -> (SampleBank, Vec<(Option<SampleKey>, SampleError)>) {
        match source {
            SampleSource::BuiltIn => (SampleBank::synthesized(), Vec::new()),
            SampleSource::SrsImport(path) => match SampleBank::import(path) {
                Ok((bank, failures)) => (bank, failures.into_iter().map(|(k, e)| (Some(k), e)).collect()),
                Err(e) => (SampleBank::synthesized(), vec![(None, e)]),
            },
        }
    }

    /// Import the user's SRS samples from `path`: an SRS install directory (holding `sounds/`), its
    /// `sounds` directory, or the SRS installer executable. Samples that cannot be found or
    /// decoded keep their built-in replacement; the error list says why.
    pub fn import(path: &Path) -> Result<(SampleBank, Vec<(SampleKey, SampleError)>), SampleError> {
        let source = Source::open(path)?;
        let mut bank = SampleBank::synthesized();
        let mut failures = Vec::new();
        for key in SampleKey::PLAYED {
            match source.read(key).and_then(|bytes| {
                let format = source_format(&bytes);
                decode_with_region(&bytes, &key.relative_path())
                    .map(|(sample, loop_frames, loop_start)| (sample, format, loop_frames, loop_start))
            }) {
                Ok((sample, format, loop_frames, loop_start)) => {
                    if let Some(frames) = loop_frames {
                        bank.loop_frames.insert(key, frames);
                    }
                    if loop_start != 0 {
                        bank.loop_starts.insert(key, loop_start);
                    }
                    if let Some(format) = format {
                        bank.source_formats.insert(key, format);
                    }
                    bank.samples.insert(key, (Arc::new(sample), SampleOrigin::Imported));
                }
                Err(e) => failures.push((key, e)),
            }
        }
        Ok((bank, failures))
    }

    pub fn get(&self, key: SampleKey) -> Option<Arc<Sample>> {
        self.samples.get(&key).map(|(s, _)| Arc::clone(s))
    }

    pub(super) fn source_format(&self, key: SampleKey) -> Option<NativeFormat> {
        self.source_formats.get(&key).copied()
    }

    pub(super) fn loop_frames(&self, key: SampleKey) -> Option<usize> {
        self.loop_frames.get(&key).copied()
    }

    pub(super) fn loop_start(&self, key: SampleKey) -> usize {
        self.loop_starts.get(&key).copied().unwrap_or(0)
    }

    pub fn origin(&self, key: SampleKey) -> Option<SampleOrigin> {
        self.samples.get(&key).map(|(_, o)| *o)
    }
}

impl Default for SampleBank {
    /// The built-in sounds.
    fn default() -> SampleBank {
        SampleBank::synthesized()
    }
}

/// Where imported samples are read from.
pub(super) enum Source {
    Sounds(PathBuf),
    /// The installer, opened once.
    Installer(NsisArchive),
}

impl Source {
    /// `path`: the installer, an install directory holding `sounds/`, or the `sounds` directory.
    pub(super) fn open(path: &Path) -> Result<Source, SampleError> {
        if path.is_file() {
            let bytes = std::fs::read(path).map_err(|e| SampleError::Io(path.to_path_buf(), e))?;
            return Ok(Source::Installer(NsisArchive::open(bytes)?));
        }
        let nested = path.join("sounds");
        Ok(Source::Sounds(if nested.is_dir() { nested } else { path.to_path_buf() }))
    }

    fn read(&self, key: SampleKey) -> Result<Vec<u8>, SampleError> {
        self.read_file(&[key.folder.dir_name(), &format!("{}.ogg", key.index)])
    }

    /// The file at `parts` below the `sounds` directory (e.g. `["engine", "Vehicles.bank"]`).
    pub(super) fn read_file(&self, parts: &[&str]) -> Result<Vec<u8>, SampleError> {
        match self {
            Source::Sounds(dir) => {
                let file = parts.iter().fold(dir.clone(), |p, part| p.join(part));
                std::fs::read(&file).map_err(|e| SampleError::Io(file, e))
            }
            Source::Installer(archive) => Ok(archive.read(&format!("sounds/{}", parts.join("/")))?),
        }
    }
}

/// The sample format a file had, kept for the resampler kernels.
/// Ogg decodes to PCM16. Integer PCM8/24/32 use the generic kernel;
/// PCM16 and float32 use their CPU-specific kernels.
fn source_format(bytes: &[u8]) -> Option<NativeFormat> {
    if bytes.starts_with(b"OggS") {
        return Some(NativeFormat::Pcm16);
    }
    // The format the chunk walk settles on (the last "fmt " chunk read).
    let (_, canonical) = fmod_wav(bytes).ok()?.normalized(bytes);
    let reader = hound::WavReader::new(Cursor::new(canonical)).ok()?;
    match (reader.spec().sample_format, reader.spec().bits_per_sample) {
        (hound::SampleFormat::Int, 16) => Some(NativeFormat::Pcm16),
        (hound::SampleFormat::Int, 8 | 24 | 32) => Some(NativeFormat::GenericInteger),
        (hound::SampleFormat::Float, 32) => Some(NativeFormat::Float),
        _ => None,
    }
}

/// Decode an Ogg Vorbis or RIFF WAV file.
pub fn decode(bytes: &[u8], name: &str) -> Result<Sample, SampleError> {
    decode_with_loop(bytes, name).map(|(sample, _)| sample)
}

fn decode_with_loop(bytes: &[u8], name: &str) -> Result<(Sample, Option<usize>), SampleError> {
    decode_with_region(bytes, name).map(|(sample, loop_end, _)| (sample, loop_end))
}

/// The decoded Sound with its loop end (when it is not the Sound's end) and its loop start.
fn decode_with_region(bytes: &[u8], name: &str) -> Result<(Sample, Option<usize>, usize), SampleError> {
    let (sample, loop_end, loop_start) = if bytes.starts_with(b"OggS") {
        let (sample, loop_end) = decode_vorbis(bytes)?;
        (sample, loop_end, 0)
    } else {
        decode_wav(bytes)?
    };
    if sample.data.is_empty() {
        return Err(SampleError::Empty(name.to_string()));
    }
    Ok((sample, loop_end, loop_start))
}

fn decode_vorbis(bytes: &[u8]) -> Result<(Sample, Option<usize>), SampleError> {
    let err = |e: lewton::VorbisError| SampleError::Vorbis(e.to_string());
    // Obtain the packet reader without depending on a second Ogg version.
    // Decode packets directly: the high-level reader mis-trims one-page
    // streams and the first audio page of a chained stream.
    let mut packets = lewton::inside_ogg::OggStreamReader::new_native(Cursor::new(bytes)).map_err(err)?.into_inner();
    packets.seek_bytes(std::io::SeekFrom::Start(0)).map_err(|e| SampleError::Vorbis(e.to_string()))?;
    let ((mut ident, _, mut setup), mut serial) = lewton::inside_ogg::read_headers_native(&mut packets).map_err(err)?;
    vorbis_window::configure(&mut ident);
    let channels = u16::from(ident.audio_channels);
    let rate = ident.audio_sample_rate;
    let mut window = lewton::audio::PreviousWindowRight::new();
    let mut data = Vec::new();
    let mut chunks = Vec::new();
    let mut mixed_channels = false;
    let mut advertised = 0u32;
    let mut link_granule = 0u64;
    let mut link_start = None;
    let mut position = VorbisPosition::default();
    while let Some(packet) = packets.read_packet().map_err(|e| SampleError::Vorbis(e.to_string()))? {
        if packet.stream_serial() != serial {
            if !packet.first_in_stream() {
                continue;
            }
            advertised = advertised.wrapping_add(link_granule.saturating_sub(link_start.unwrap_or(0)) as u32);
            link_granule = 0;
            link_start = None;
            ident = lewton::header::read_header_ident(&packet.data).map_err(|e| SampleError::Vorbis(e.to_string()))?;
            mixed_channels |= u16::from(ident.audio_channels) != channels;
            vorbis_window::configure(&mut ident);
            let comment = packets.read_packet_expected().map_err(|e| SampleError::Vorbis(e.to_string()))?;
            lewton::header::read_header_comment(&comment.data).map_err(|e| SampleError::Vorbis(e.to_string()))?;
            let setup_packet = packets.read_packet_expected().map_err(|e| SampleError::Vorbis(e.to_string()))?;
            setup = lewton::header::read_header_setup_native(
                &setup_packet.data,
                ident.audio_channels,
                (ident.blocksize_0, ident.blocksize_1),
            )
            .map_err(|e| SampleError::Vorbis(e.to_string()))?;
            serial = packet.stream_serial();
            position = VorbisPosition::default();
            window = lewton::audio::PreviousWindowRight::new();
            continue;
        }
        if packet.discontinuity() {
            // The granule estimate is forgotten when packet numbers
            // skip. Keep the overlap samples; they still join the next block.
            position = VorbisPosition { discontinuous: true, ..Default::default() };
        }
        if lewton::audio::native_synthesis_rejects(&setup, &packet.data) {
            // A packet that is not audio, or is a bad packet, is dropped and
            // the next one is read. Its packet number is consumed, so the next block
            // is out of sequence and loses its granule count; the overlap
            // stays. The page's granule still bounds the advertised total, and
            // the initial offset counts no block for the dropped packet.
            position.granule = None;
            position.discontinuous = true;
            if packet.last_in_page() && packet.absgp_page() != u64::MAX {
                link_granule = packet.absgp_page();
                link_start.get_or_insert(link_granule.saturating_sub(position.decoded));
            }
            continue;
        }
        let decoded = lewton::audio::read_audio_packet_generic::<lewton::samples::InterleavedSamples<f32>>(
            &ident,
            &setup,
            &packet.data,
            &mut window,
        )
        .map_err(|e| SampleError::Vorbis(e.to_string()))?;
        // Ogg samples are converted to signed PCM16 with
        // round-to-nearest/even; Lewton's i16 API truncates.
        let start = data.len();
        data.extend(decoded.samples.into_iter().map(vorbis_pcm16));
        position.reconcile(
            &mut data,
            start,
            usize::from(ident.audio_channels),
            packet.last_in_page().then_some(packet.absgp_page()),
            packet.last_in_stream(),
        );
        if packet.last_in_page() && packet.absgp_page() != u64::MAX {
            link_granule = packet.absgp_page();
            link_start.get_or_insert(link_granule.saturating_sub(position.decoded));
        }
        chunks.push((usize::from(ident.audio_channels), start..data.len()));
    }
    advertised = advertised.wrapping_add(link_granule.saturating_sub(link_start.unwrap_or(0)) as u32);
    // The sound's length is the stream's advertised total, and it is loaded with
    // one read: a read shorter
    // than that keeps the advertised length (cleared tail) and moves the
    // default loop end to the frames read; a longer stream is read only up
    // to it. A stream whose decoded audio matches its granules is unchanged.
    if advertised == 0 {
        // Opening sums every link's advertised total into 32 bits and
        // refuses a zero length as a format error.
        return Err(SampleError::Vorbis("stream length (ov_pcm_total) is zero: FMOD_ERR_FORMAT".into()));
    }
    let decoded_frames = data.len() / usize::from(channels.max(1));
    if mixed_channels || advertised as usize != decoded_frames {
        return mixed_vorbis_chain(channels, rate, advertised, &data, &chunks);
    }
    Ok((Sample { rate, channels, data: vorbis_to_speaker_order(channels, data) }, None))
}

/// The first link's format is kept through a chain. Each read consumes whole
/// current-link frames, but reports only whole first-link frames to its caller.
/// Loading stops at the advertised capacity.
fn mixed_vorbis_chain(
    channels: u16,
    rate: u32,
    frames: u32,
    decoded: &[f32],
    chunks: &[(usize, std::ops::Range<usize>)],
) -> Result<(Sample, Option<usize>), SampleError> {
    let width = usize::from(channels);
    let length =
        (frames as usize).checked_mul(width).ok_or_else(|| SampleError::Vorbis("sample length overflow".into()))?;
    let capacity = length.checked_add(width).ok_or_else(|| SampleError::Vorbis("sample length overflow".into()))?;
    let mut data = Vec::new();
    data.try_reserve_exact(capacity).map_err(|e| SampleError::Vorbis(e.to_string()))?;
    // The allocation clears the advertised PCM area and the padding frame.
    data.resize(capacity, 0.0);
    let mut written = 0;
    for (current, range) in chunks {
        let chunk = &decoded[range.clone()];
        let mut offset = 0;
        while offset < chunk.len() && written < length {
            let count = ((length - written) / current).min((chunk.len() - offset) / current) * current;
            if count == 0 {
                // The read is invalid here: an invalid-parameter error,
                // and loading the sound fails.
                return Err(SampleError::Vorbis("chained stream frame exceeds remaining sample capacity".into()));
            }
            data[written..written + count].copy_from_slice(&chunk[offset..offset + count]);
            let order: &[usize] = match width {
                6 => &VORBIS_51_CHANNEL_OF_SPEAKER,
                8 => &VORBIS_71_CHANNEL_OF_SPEAKER,
                _ => &[],
            };
            // The channel permutation covers ceil(returned_bytes / first_frame_bytes)
            // frames, including the last partial frame in its cleared buffer.
            if !order.is_empty() {
                for start in (written..written + count).step_by(width) {
                    let mut frame = [0.0; 8];
                    frame[..width].copy_from_slice(&data[start..start + width]);
                    for (index, &source) in order.iter().enumerate() {
                        data[start + index] = frame[source];
                    }
                }
            }
            offset += count;
            let returned = count / width * width;
            written += returned;
            if returned == 0 {
                // The codec has consumed this chunk; its caller retries at the
                // same output address for the next available decoded packet.
                break;
            }
        }
    }
    if written == 0 {
        return Err(SampleError::Vorbis("stream preload read no complete output frame".into()));
    }
    data.truncate(length);
    // A short successful preload changes Sound's default loop end, while its
    // public length and one-shot silent tail retain the advertised frame count.
    let loop_frames = (written < length).then_some(written / width);
    Ok((Sample { rate, channels, data }, loop_frames))
}

#[derive(Default)]
struct VorbisPosition {
    decoded: u64,
    granule: Option<u64>,
    /// The first known post-hole position establishes a new baseline. There is
    /// no count before that packet from which to infer padding.
    discontinuous: bool,
}

impl VorbisPosition {
    fn reconcile(&mut self, data: &mut Vec<f32>, start: usize, channels: usize, granule: Option<u64>, eos: bool) {
        let frames = (data.len() - start) / channels;
        self.decoded = self.decoded.saturating_add(frames as u64);
        let had_granule = self.granule.is_some();
        if let Some(position) = &mut self.granule {
            *position = position.saturating_add(frames as u64);
        }
        let Some(granule) = granule.filter(|value| *value != u64::MAX) else { return };
        let expected = self.granule.unwrap_or(self.decoded);
        // The first known position is reconciled at the front
        // of a non-EOS block, and the end padding at the back of an EOS block.
        // Only the current block is still buffered; prior returned audio stays.
        if expected > granule && (!had_granule || eos) && !self.discontinuous {
            let trim = (expected - granule).min(frames as u64) as usize * channels;
            if eos {
                data.truncate(data.len() - trim);
            } else {
                data.drain(start..start + trim);
            }
        }
        self.granule = Some(granule);
        self.discontinuous = false;
    }
}

fn vorbis_pcm16(sample: f32) -> f32 {
    let scaled = sample * 32768.0;
    // CVTSD2SI produces its indefinite i32 on NaN or overflow, before
    // the codec clamps that integer to signed16. Normal audio is finite.
    let integer =
        if !(-2_147_483_648.0..2_147_483_648.0).contains(&scaled) { i32::MIN } else { scaled.round_ties_even() as i32 };
    integer.clamp(-32768, 32767) as f32 / 32768.0
}

/// The chunks taken from a RIFF file:
/// "RIFF"/"WAVE" first; chunk headers from offset 12, each chunk advancing
/// 8 + size plus one pad byte after an odd size, until the offset relative to "WAVE" reaches the
/// RIFF size or a header read runs out of file (which ends the walk normally). Every "fmt "
/// chunk replaces the format; the first "data" chunk fixes the data offset and its length, which
/// is cut to the bytes the file holds when the 32-bit sum of length and offset exceeds the file
/// size.
struct FmodWav<'a> {
    fmt: &'a [u8],
    data_offset: usize,
    data_length: u32,
    /// The loop start and end from the last "smpl" chunk whose 60-byte read is complete
    /// and whose cSampleLoops is not zero: dwStart and dwEnd of its first loop.
    sample_loop: Option<(u32, u32)>,
}

fn fmod_wav(bytes: &[u8]) -> Result<FmodWav<'_>, SampleError> {
    let bad = |what: &str| SampleError::Wav(what.to_owned());
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|v| u32::from_le_bytes(v.try_into().expect("four bytes")));
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err(bad("no RIFF/WAVE header"));
    }
    let riff_size = u32_at(4).expect("header checked");
    let file_size = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    let (mut fmt, mut data, mut sample_loop) = (None, None, None);
    let mut relative: u32 = 4;
    loop {
        let at = relative as usize + 8;
        let (Some(id), Some(size)) = (bytes.get(at..at + 4), u32_at(at + 4)) else { break };
        let body = at + 8;
        match id {
            // The new format buffer replaces the old one before it is read; a short read
            // (FILE_EOF) keeps what arrived and ends the walk.
            b"fmt " => {
                let end = body.saturating_add(size as usize);
                fmt = Some(&bytes[body.min(bytes.len())..end.min(bytes.len())]);
                if end > bytes.len() {
                    break;
                }
            }
            b"data" if data.is_none() => {
                let offset = body as u32;
                let length =
                    if size.wrapping_add(offset) > file_size { file_size.saturating_sub(offset) } else { size };
                data = Some((body, length));
            }
            // Sixty bytes are read whatever the chunk size says; a short read keeps the old loop.
            b"smpl" if bytes.len() >= body + 60 && u32_at(body + 28) != Some(0) => {
                sample_loop = Some((u32_at(body + 44).expect("in file"), u32_at(body + 48).expect("in file")));
            }
            _ => {}
        }
        relative = relative.wrapping_add(8).wrapping_add(size);
        if size & 1 == 1 {
            relative = relative.wrapping_add(1);
        }
        if relative >= riff_size || relative == 0 {
            break;
        }
    }
    let fmt = fmt.ok_or_else(|| bad("missing fmt chunk"))?;
    let (data_offset, data_length) = data.ok_or_else(|| bad("missing data chunk"))?;
    Ok(FmodWav { fmt, data_offset, data_length, sample_loop })
}

impl FmodWav<'_> {
    fn field(&self, at: usize) -> usize {
        self.fmt.get(at..at + 2).map_or(0, |v| usize::from(u16::from_le_bytes([v[0], v[1]])))
    }

    /// The advertised length in whole frames
    /// (`bytes * 8 / wBitsPerSample / nChannels`, 64-bit floor divisions), and
    /// a canonical file holding the whole frames present.
    fn normalized(&self, bytes: &[u8]) -> (usize, Vec<u8>) {
        let (channels, bits) = (self.field(2).max(1), self.field(14).max(1));
        let advertised = (u64::from(self.data_length) * 8 / bits as u64 / channels as u64) as usize;
        let frame = channels * bits.div_ceil(8);
        let present = bytes.len().saturating_sub(self.data_offset).min(self.data_length as usize) / frame * frame;
        let mut out = Vec::with_capacity(28 + self.fmt.len() + present);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((20 + self.fmt.len() + present) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&(self.fmt.len() as u32).to_le_bytes());
        out.extend_from_slice(self.fmt);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(present as u32).to_le_bytes());
        out.extend_from_slice(&bytes[self.data_offset..self.data_offset + present]);
        (advertised, out)
    }
}

/// WAV import: the data length (see [`fmod_wav`]) in whole frames is the sound's
/// length; the one load read takes the frames the file holds, keeps a
/// cleared tail and moves the default loop end after a short read, and fails as a bad file
/// when it reads nothing.
fn decode_wav(bytes: &[u8]) -> Result<(Sample, Option<usize>, usize), SampleError> {
    let err = |e: hound::Error| SampleError::Wav(e.to_string());
    let wav = fmod_wav(bytes)?;
    let sample_loop = wav.sample_loop;
    let (advertised, canonical) = wav.normalized(bytes);
    let reader = hound::WavReader::new(Cursor::new(canonical)).map_err(err)?;
    let spec = reader.spec();
    let mut data = match spec.sample_format {
        hound::SampleFormat::Float => reader.into_samples::<f32>().collect::<Result<Vec<_>, _>>().map_err(err)?,
        hound::SampleFormat::Int => {
            let scale = 2f32.powi(i32::from(spec.bits_per_sample) - 1);
            reader
                .into_samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<Result<Vec<_>, _>>()
                .map_err(err)?
        }
    };
    let channels = usize::from(spec.channels.max(1));
    let read = data.len() / channels;
    if read == 0 && advertised > 0 {
        return Err(SampleError::Wav("preload read no frame: FMOD_ERR_FILE_BAD".into()));
    }
    let (start, mut end) = sound_loop_points(sample_loop, advertised);
    if advertised > read {
        let length = advertised
            .checked_mul(channels)
            .ok_or_else(|| SampleError::Wav("sample length overflow: FMOD_ERR_MEMORY".into()))?;
        // The cleared tail. A zeroed allocation leaves untouched pages unmapped, so an
        // advertised length far beyond the file costs only the frames actually present.
        let mut probe: Vec<f32> = Vec::new();
        probe.try_reserve_exact(length).map_err(|_| SampleError::Wav("FMOD_ERR_MEMORY".into()))?;
        drop(probe);
        let mut full = vec![0.0f32; length];
        full[..data.len()].copy_from_slice(&data);
        data = full;
        // The loop end moves only while the loop is still the whole sound.
        if start == 0 && end == advertised {
            end = read;
        }
    }
    let loop_end = (start != 0 || end != advertised).then_some(end);
    Ok((Sample { rate: spec.sample_rate, channels: spec.channels, data }, loop_end, start))
}

/// The sound's loop region `[start, end)` in frames. A smpl loop is used
/// only when its end lies after its start; the loop then
/// takes a start inside the sound or 0, clamps the end to the
/// last frame (an end of 0 also meaning the last frame); a start not before the
/// end is refused, keeping the whole sound.
fn sound_loop_points(sample_loop: Option<(u32, u32)>, length: usize) -> (usize, usize) {
    let whole = (0, length);
    let Some((start, end)) = sample_loop.filter(|(start, end)| end > start) else { return whole };
    let (start, end) = (start as usize, end as usize);
    let last = length.saturating_sub(1);
    let start = if start < length { start } else { 0 };
    let end = match if end < length { end } else { last } {
        0 => last,
        end => end,
    };
    if start >= end {
        return whole;
    }
    (start, end + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_bytes(channels: u16, samples: &[i16]) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = Cursor::new(Vec::new());
        let mut w = hound::WavWriter::new(&mut cursor, spec).unwrap();
        for &s in samples {
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
        cursor.into_inner()
    }

    #[test]
    fn imported_vorbis_keeps_the_in_range_prefix_of_final_residue_vectors() {
        for (bytes, expected) in [
            (include_bytes!("testdata/residue_vector_type1.ogg").as_slice(), 0x33f4293953d0b60f),
            (include_bytes!("testdata/residue_vector_type2.ogg").as_slice(), 0x239ac145eec73abe),
        ] {
            let sample = decode(bytes, "residue vector boundary").unwrap();
            assert_eq!((sample.rate, sample.channels, sample.frames()), (48000, 2, 64));
            let hash = sample
                .data
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
            assert_eq!(hash, expected);
        }
    }

    #[test]
    fn imported_vorbis_preserves_all_eight_residue_passes() {
        for (bytes, expected) in [
            (include_bytes!("testdata/residue_pass7_type0.ogg").as_slice(), 0xe9c95afa379ce7a3),
            (include_bytes!("testdata/residue_pass7_type1.ogg").as_slice(), 0xe9c95afa379ce7a3),
            (include_bytes!("testdata/residue_pass7_type2.ogg").as_slice(), 0x14f9c7bbb3a24319),
        ] {
            let sample = decode(bytes, "eighth residue pass").unwrap();
            assert_eq!((sample.rate, sample.channels, sample.frames()), (48000, 2, 64));
            let hash = sample
                .data
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
            assert_eq!(hash, expected);
        }
    }

    /// The long trim exceeds the one buffered block, so 64 frames decode while the stream
    /// advertises 48 (first granule minus overlap, clamped at zero); loading reads only the
    /// advertised 48. The installed libvorbis gives the same frames and
    /// hash for both files.
    #[test]
    fn imported_vorbis_reconciles_first_granule_against_current_packet() {
        for (bytes, frames, expected) in [
            (include_bytes!("testdata/granule_leading_trim.ogg").as_slice(), 48, 0xae73d2397c583bef),
            (include_bytes!("testdata/granule_long_trim.ogg").as_slice(), 48, 0x889d03200c462656),
        ] {
            let sample = decode(bytes, "leading granule trim").unwrap();
            assert_eq!((sample.rate, sample.channels, sample.frames()), (48000, 2, frames));
            let hash = sample
                .data
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
            assert_eq!(hash, expected);
        }
    }

    #[test]
    fn imported_vorbis_restarts_granule_positions_for_each_logical_stream() {
        fn next_stream(bytes: &[u8], serial: u8) -> Vec<u8> {
            let mut bytes = bytes.to_vec();
            let mut offset = 0;
            while offset < bytes.len() {
                let segments = usize::from(bytes[offset + 26]);
                let body: usize = bytes[offset + 27..offset + 27 + segments].iter().map(|v| usize::from(*v)).sum();
                let end = offset + 27 + segments + body;
                let page = &mut bytes[offset..end];
                page[14] ^= serial;
                page[22..26].fill(0);
                let mut crc = 0u32;
                for byte in page.iter() {
                    crc ^= u32::from(*byte) << 24;
                    for _ in 0..8 {
                        crc = (crc << 1) ^ if crc & 0x8000_0000 != 0 { 0x04c1_1db7 } else { 0 };
                    }
                }
                page[22..26].copy_from_slice(&crc.to_le_bytes());
                offset = end;
            }
            bytes
        }
        let hash = |data: &[f32]| {
            data.iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
        };
        // Each link reconciles its own granules; loading then reads the summed
        // advertised total once, so only the chain's end is cut. Hashes as produced by the
        // installed libvorbis on the same chains.
        for (source, chain_hash) in [
            (include_bytes!("testdata/granule_leading_trim.ogg").as_slice(), 0x35f2003130656d2d),
            (include_bytes!("testdata/granule_long_trim.ogg").as_slice(), 0x6a262a7c0c89a1a3),
        ] {
            let single = decode(source, "trimmed stream").unwrap();
            let second = next_stream(source, 0x40);
            let (chain, loop_frames) =
                decode_with_loop(&[source, second.as_slice()].concat(), "trimmed chain").unwrap();
            assert_eq!(chain.frames(), 2 * single.frames());
            assert_eq!(loop_frames, None);
            assert_eq!(chain.data[..single.data.len()], single.data[..]);
            assert_eq!(hash(&chain.data), chain_hash);
        }
        let leading = decode(include_bytes!("testdata/granule_leading_trim.ogg"), "trimmed stream").unwrap();
        let second = next_stream(include_bytes!("testdata/granule_leading_trim.ogg"), 0x40);
        let chain =
            decode(&[include_bytes!("testdata/granule_leading_trim.ogg").as_slice(), &second].concat(), "chain")
                .unwrap();
        assert_eq!(chain.data, [leading.data.as_slice(), leading.data.as_slice()].concat());
    }

    #[test]
    fn granule_reconciliation_keeps_prior_pcm_and_handles_unknown_positions() {
        let mut data = vec![10.0, 20.0];
        let mut position = VorbisPosition::default();
        let start = data.len();
        data.extend([1.0, 2.0, 3.0, 4.0]);
        position.reconcile(&mut data, start, 1, Some(u64::MAX), false);
        let start = data.len();
        data.extend([5.0, 6.0, 7.0, 8.0]);
        // The first known granule is six frames behind. Only these four
        // newly decoded frames are buffered and eligible for removal.
        position.reconcile(&mut data, start, 1, Some(2), false);
        assert_eq!(data, [10.0, 20.0, 1.0, 2.0, 3.0, 4.0]);
        let start = data.len();
        data.extend([9.0, 10.0, 11.0, 12.0]);
        position.reconcile(&mut data, start, 1, Some(5), true);
        assert_eq!(data, [10.0, 20.0, 1.0, 2.0, 3.0, 4.0, 9.0, 10.0, 11.0]);
    }

    #[test]
    fn imported_vorbis_large_multichannel_residue_matches_native_pcm() {
        let bytes = include_bytes!("testdata/residue_8ch_8192.ogg");
        let sample = decode(bytes, "large multichannel residue").unwrap();
        assert_eq!((sample.rate, sample.channels, sample.frames()), (48000, 8, 8192));
        let hash = sample
            .data
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
        // Expected values from an independent C MDCT/window/PCM16 implementation, after 7.1 reordering.
        assert_eq!(hash, 0x2ff0ea45bc73c360);
    }

    #[test]
    fn imported_vorbis_coupling_follows_named_magnitude_and_angle_channels() {
        let forward = include_bytes!("testdata/coupling_forward.ogg");
        let reverse = include_bytes!("testdata/coupling_reverse.ogg");
        let hash = |data: &[f32]| {
            data.iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
        };
        for (bytes, want) in [(forward.as_slice(), 0x6c6291ffd520fa2a), (reverse.as_slice(), 0x3f0504cfd605992e)] {
            let sample = decode(bytes, "coupling").unwrap();
            assert_eq!((sample.rate, sample.channels, sample.frames()), (44100, 2, 64));
            assert_eq!(hash(&sample.data), want);
        }
        let chain = decode(&[forward.as_slice(), reverse.as_slice()].concat(), "coupling chain").unwrap();
        assert_eq!(chain.frames(), 128);
        assert_eq!(hash(&chain.data[..128]), 0x6c6291ffd520fa2a);
        assert_eq!(hash(&chain.data[128..]), 0x3f0504cfd605992e);
    }

    /// The packets decode as before (96 frames, hash `0xb5239c328fea8a7d`), but the
    /// sound's advertised total is 128 frames: the one load read is short, so
    /// the cleared tail is kept and the default loop end moves to 96.
    #[test]
    fn imported_vorbis_drops_partial_packets_across_missing_or_corrupt_pages() {
        let hash = |data: &[f32]| {
            data.iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
        };
        for bytes in [
            include_bytes!("testdata/partial_page_start.ogg").as_slice(),
            include_bytes!("testdata/partial_page_middle.ogg").as_slice(),
            include_bytes!("testdata/partial_page_end.ogg").as_slice(),
            include_bytes!("testdata/partial_page_crc.ogg").as_slice(),
        ] {
            let (sample, loop_frames) = decode_with_loop(bytes, "interrupted audio packet").unwrap();
            assert_eq!((sample.channels, sample.rate, sample.frames()), (2, 48000, 128));
            assert_eq!(loop_frames, Some(96));
            assert_eq!(hash(&sample.data[..2 * 96]), 0xb5239c328fea8a7d);
            assert!(sample.data[2 * 96..].iter().all(|v| *v == 0.0));
        }
    }

    /// Rebuild an Ogg stream page for page, letting `edit` change any packet that starts and
    /// ends in one page (page number, packet number in the page, bytes). Granules, flags and
    /// serials stay; lacing and checksums are recomputed.
    fn remux(bytes: &[u8], mut edit: impl FnMut(usize, usize, &mut Vec<u8>)) -> Vec<u8> {
        let mut out = Vec::new();
        let mut offset = 0;
        let mut page_index = 0;
        while offset < bytes.len() {
            let segments = usize::from(bytes[offset + 26]);
            let lacing = &bytes[offset + 27..offset + 27 + segments];
            let mut body = offset + 27 + segments;
            let end = body + lacing.iter().map(|v| usize::from(*v)).sum::<usize>();
            assert!(bytes[offset + 5] & 1 == 0 && lacing.last() != Some(&255), "fixture packets stay within a page");
            let mut packets = Vec::new();
            let mut current = Vec::new();
            for &len in lacing {
                current.extend_from_slice(&bytes[body..body + usize::from(len)]);
                body += usize::from(len);
                if len < 255 {
                    packets.push(std::mem::take(&mut current));
                }
            }
            for (k, packet) in packets.iter_mut().enumerate() {
                edit(page_index, k, packet);
            }
            let mut page = bytes[offset..offset + 26].to_vec();
            let mut new_lacing = Vec::new();
            for packet in &packets {
                new_lacing.extend(std::iter::repeat_n(255u8, packet.len() / 255));
                new_lacing.push((packet.len() % 255) as u8);
            }
            page.push(new_lacing.len() as u8);
            page.extend(new_lacing);
            for packet in &packets {
                page.extend_from_slice(packet);
            }
            page[22..26].fill(0);
            let mut crc = 0u32;
            for byte in &page {
                crc ^= u32::from(*byte) << 24;
                for _ in 0..8 {
                    crc = (crc << 1) ^ if crc & 0x8000_0000 != 0 { 0x04c1_1db7 } else { 0 };
                }
            }
            page[22..26].copy_from_slice(&crc.to_le_bytes());
            out.extend(page);
            offset = end;
            page_index += 1;
        }
        out
    }

    /// A packet refused at the first checks (a set or
    /// missing packet-type bit: not audio) is dropped and decoding goes on; the next block is out
    /// of sequence. The sound keeps its advertised length with a cleared tail and a default loop
    /// end at the frames read. A packet whose header is readable but whose floors run out decodes as
    /// unused floors. Expected values: the installed libvorbis
    /// on the same bytes.
    #[test]
    fn imported_vorbis_skips_packets_that_synthesis_refuses() {
        let source = include_bytes!("testdata/residue_pass0_type0.ogg");
        let hash = |data: &[f32]| {
            data.iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
        };
        // (page, edit, default loop end, PCM hash)
        type Case = (usize, fn(&mut Vec<u8>), Option<usize>, u64);
        let cases: [Case; 5] = [
            (4, |p| p[0] |= 1, Some(32), 0x2522bab4a2416857),
            (4, |p| p.clear(), Some(32), 0x2522bab4a2416857),
            (4, |p| p.truncate(1), None, 0xfb068de57246a666),
            (3, |p| p[0] |= 1, Some(32), 0x375f66467bc5db54),
            (5, |p| p[0] |= 1, Some(32), 0x96c1ea76e57ebe12),
        ];
        for (page, edit, loop_end, expected) in cases {
            let bytes = remux(source, |p, _, packet| {
                if p == page {
                    edit(packet)
                }
            });
            let (sample, loop_frames) = decode_with_loop(&bytes, "refused packet").unwrap();
            assert_eq!((sample.channels, sample.rate, sample.frames()), (2, 48000, 64), "page {page}");
            assert_eq!(loop_frames, loop_end, "page {page}");
            assert_eq!(hash(&sample.data), expected, "page {page}");
        }
        assert_eq!(remux(source, |_, _, _| {}), source.as_slice(), "the re-muxer alone changes nothing");
    }

    /// libogg hands out complete pages only, so a file cut inside its last page, or followed by
    /// bytes with no further page, ends where its last complete page ends
    /// (end of file there), and the advertised total comes from the last complete granule. Cutting
    /// away every granule page leaves the total at zero, which opening refuses as a
    /// format error. Expected values: the installed libvorbis
    /// on the same bytes (32 frames, hash `0x08755dc8d6a47612`; 64 frames, the unchanged hash).
    #[test]
    fn imported_vorbis_ends_at_the_last_complete_page() {
        let source = include_bytes!("testdata/residue_pass0_type0.ogg");
        let hash = |data: &[f32]| {
            data.iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
        };
        for cut in [5, 17, 40] {
            let (sample, loop_frames) = decode_with_loop(&source[..source.len() - cut], "truncated").unwrap();
            assert_eq!((sample.channels, sample.frames(), loop_frames), (2, 32, None), "cut {cut}");
            assert_eq!(hash(&sample.data), 0x08755dc8d6a47612, "cut {cut}");
        }
        for tail in [&[1u8, 2, 3][..], b"Og", &[0; 300]] {
            let sample = decode(&[source.as_slice(), tail].concat(), "trailing bytes").unwrap();
            assert_eq!(sample.frames(), 64);
            assert_eq!(hash(&sample.data), 0xe9c95afa379ce7a3);
        }
        let layout = include_bytes!("testdata/layout_51.ogg");
        let error = decode(&layout[..layout.len() - 3000], "no complete granule page").unwrap_err();
        assert!(error.to_string().contains("FMOD_ERR_FORMAT"), "{error}");
    }

    /// libogg's sync layer skips any amount of bytes to the next capture, and vorbisfile reads
    /// pages without a boundary, so 200 KiB of junk between two pages (beyond the upstream
    /// reader's 150 KiB search limit) or after the last page changes nothing. The installed
    /// libvorbis gives the same 64 frames and hash.
    #[test]
    fn imported_vorbis_resynchronizes_across_any_amount_of_junk() {
        let source = include_bytes!("testdata/residue_pass0_type0.ogg");
        let page4 = source.windows(4).enumerate().filter(|(_, w)| *w == b"OggS").nth(4).unwrap().0;
        let junk = vec![b'x'; 200 * 1024];
        let hash = |data: &[f32]| {
            data.iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
        };
        for bytes in [[&source[..page4], &junk, &source[page4..]].concat(), [source.as_slice(), &junk].concat()] {
            let (sample, loop_frames) = decode_with_loop(&bytes, "junk").unwrap();
            assert_eq!((sample.frames(), loop_frames), (64, None));
            assert_eq!(hash(&sample.data), 0xe9c95afa379ce7a3);
        }
    }

    /// After the hole the packets keep decoding past the advertised end (96 frames);
    /// loading reads only the advertised total (64 and 16 frames). The installed libvorbis
    /// gives the same frames and hashes.
    #[test]
    fn imported_vorbis_page_gap_reestablishes_granules_without_trimming_valid_pcm() {
        for (bytes, frames, expected) in [
            (include_bytes!("testdata/granule_page_gap.ogg").as_slice(), 64, 0x14f9c7bbb3a24319),
            (include_bytes!("testdata/granule_page_gap_low.ogg").as_slice(), 16, 0x339070c9bfca247b),
        ] {
            let (sample, loop_frames) = decode_with_loop(bytes, "missing page before EOS").unwrap();
            assert_eq!((sample.channels, sample.rate, sample.frames()), (2, 48000, frames));
            assert_eq!(loop_frames, None);
            let hash = sample
                .data
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
            assert_eq!(hash, expected);
        }
        let mut position = VorbisPosition { discontinuous: true, ..Default::default() };
        let mut data = vec![1.; 8];
        position.reconcile(&mut data, 0, 2, None, false);
        assert!(position.discontinuous, "unknown position does not end discontinuity");
        data.extend([2.; 8]);
        position.reconcile(&mut data, 8, 2, Some(1), false);
        assert_eq!(data.len(), 16, "first known post-hole position establishes baseline");
        assert!(!position.discontinuous);
        data.extend([3.; 8]);
        position.reconcile(&mut data, 16, 2, Some(3), true);
        assert_eq!(data.len(), 20, "later EOS still trims real padding");
    }

    /// A RIFF file from raw chunks; `riff` overrides the RIFF size field.
    fn riff(chunks: &[(&[u8; 4], u32, &[u8])], riff: Option<u32>) -> Vec<u8> {
        let mut body = b"WAVE".to_vec();
        for (id, size, bytes) in chunks {
            body.extend_from_slice(*id);
            body.extend_from_slice(&size.to_le_bytes());
            body.extend_from_slice(bytes);
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&riff.unwrap_or(body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    /// fmt body for integer PCM.
    fn pcm_fmt(channels: u16, bits: u16) -> Vec<u8> {
        let align = channels * bits.div_ceil(8);
        let mut fmt = Vec::new();
        for v in [1u16, channels] {
            fmt.extend_from_slice(&v.to_le_bytes());
        }
        fmt.extend_from_slice(&44_100u32.to_le_bytes());
        fmt.extend_from_slice(&(44_100 * u32::from(align)).to_le_bytes());
        for v in [align, bits] {
            fmt.extend_from_slice(&v.to_le_bytes());
        }
        fmt
    }

    fn pcm16(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// WAV reading: the first data chunk's length is cut to the bytes
    /// the file holds, so a truncated file plays its whole frames (hound alone stops with an
    /// unexpected end); chunks are
    /// word aligned after an odd size; the walk ends at the RIFF size; a later data chunk is
    /// ignored; a partial final frame is not part of the length.
    #[test]
    fn imported_wav_follows_the_fmod_codec_chunk_walk() {
        let fmt = pcm_fmt(2, 16);
        let samples = pcm16(&[100, -100, 200, -200, 300, -300]);
        let scale = |v: i16| f32::from(v) / 32768.0;
        let expected = |values: &[i16]| values.iter().map(|v| scale(*v)).collect::<Vec<_>>();
        // Declared 400 bytes, 12 present: three frames.
        let truncated = riff(&[(b"fmt ", 16, &fmt), (b"data", 400, &samples)], None);
        let (sample, loop_frames) = decode_with_loop(&truncated, "truncated").unwrap();
        assert_eq!((sample.channels, sample.rate, sample.frames(), loop_frames), (2, 44_100, 3, None));
        assert_eq!(sample.data, expected(&[100, -100, 200, -200, 300, -300]));
        // One byte short of the last frame, and a declared length that is not frame aligned.
        let (sample, _) = decode_with_loop(&truncated[..truncated.len() - 1], "cut frame").unwrap();
        assert_eq!(sample.frames(), 2);
        let unaligned = riff(&[(b"fmt ", 16, &fmt), (b"data", 10, &samples)], None);
        assert_eq!(decode_with_loop(&unaligned, "unaligned").unwrap().0.frames(), 2);
        // An odd-sized chunk before the data is followed by its pad byte.
        let padded = riff(&[(b"fmt ", 16, &fmt), (b"junk", 3, b"abc\0"), (b"data", 12, &samples)], None);
        assert_eq!(decode_with_loop(&padded, "padded").unwrap().0.data, expected(&[100, -100, 200, -200, 300, -300]));
        // The first data chunk wins.
        let second = pcm16(&[7, 7]);
        let two = riff(&[(b"fmt ", 16, &fmt), (b"data", 4, &samples[..4]), (b"data", 4, &second)], None);
        assert_eq!(decode_with_loop(&two, "two data").unwrap().0.data, expected(&[100, -100]));
        // A RIFF size that ends after the format leaves the data chunk outside the walk.
        let outside = riff(&[(b"fmt ", 16, &fmt), (b"data", 12, &samples)], Some(4 + 8 + 16));
        assert!(matches!(decode_with_loop(&outside, "outside"), Err(SampleError::Wav(m)) if m == "missing data chunk"));
        // A format chunk cut by the end of the file still ends the walk without data.
        let cut_fmt = riff(&[(b"fmt ", 16, &fmt[..8])], None);
        assert!(matches!(decode_with_loop(&cut_fmt, "cut fmt"), Err(SampleError::Wav(m)) if m == "missing data chunk"));
    }

    /// A data length whose 32-bit sum with its offset wraps (here 0xFFFFFFFF, as streaming
    /// writers emit) is not cut: the sound keeps its advertised length, loading reads the
    /// frames present, clears the tail and moves the default loop end to them. Should the host
    /// refuse the allocation, the import fails as an out-of-memory error.
    #[test]
    fn imported_wav_keeps_a_wrapped_data_length() {
        let mut fmt = pcm_fmt(1, 32);
        fmt[0] = 3; // IEEE float
        let samples: Vec<u8> = [0.5f32, -0.25].iter().flat_map(|v| v.to_le_bytes()).collect();
        let wrapped = riff(&[(b"fmt ", 16, &fmt), (b"data", u32::MAX, &samples)], None);
        match decode_with_loop(&wrapped, "wrapped") {
            Ok((sample, loop_frames)) => {
                assert_eq!(sample.frames(), (u64::from(u32::MAX) * 8 / 32) as usize);
                assert_eq!(loop_frames, Some(2));
                assert_eq!(&sample.data[..2], &[0.5, -0.25]);
                assert_eq!(sample.data[sample.data.len() - 1], 0.0);
            }
            Err(error) => assert_eq!(error.to_string(), "WAV decoding failed: FMOD_ERR_MEMORY"),
        }
    }

    /// smpl loop points: dwStart/dwEnd of a complete 60-byte read with cSampleLoops != 0
    /// are used only when the end lies after the start; the sound takes a start inside
    /// it or 0, clamps the end to its last frame and keeps the whole sound when start >= end. The
    /// loop end is exclusive here (inclusive dwEnd + 1).
    #[test]
    fn imported_wav_smpl_loop_points_follow_sound_set_loop_points() {
        assert_eq!(sound_loop_points(None, 1000), (0, 1000));
        assert_eq!(sound_loop_points(Some((100, 199)), 1000), (100, 200));
        assert_eq!(sound_loop_points(Some((100, 5000)), 1000), (100, 1000));
        assert_eq!(sound_loop_points(Some((2000, 5000)), 1000), (0, 1000));
        assert_eq!(sound_loop_points(Some((7, 7)), 1000), (0, 1000), "not promoted");
        assert_eq!(sound_loop_points(Some((9, 20)), 10), (0, 10), "start >= clamped end is refused");
        let fmt = pcm_fmt(1, 16);
        let samples = pcm16(&(0..20).collect::<Vec<i16>>());
        let smpl = |loops: u32, start: u32, end: u32| {
            let mut chunk = vec![0u8; 60];
            chunk[28..32].copy_from_slice(&loops.to_le_bytes());
            chunk[44..48].copy_from_slice(&start.to_le_bytes());
            chunk[48..52].copy_from_slice(&end.to_le_bytes());
            chunk
        };
        let file = |chunk: &[u8], size: u32| {
            riff(&[(b"fmt ", 16, &fmt), (b"data", 40, &samples), (b"smpl", size, chunk)], None)
        };
        let (sample, loop_end, loop_start) = decode_with_region(&file(&smpl(1, 4, 11), 60), "smpl").unwrap();
        assert_eq!((sample.frames(), loop_end, loop_start), (20, Some(12), 4));
        // No loop entry, or a read that cannot complete its sixty bytes, keeps the whole Sound.
        assert_eq!(decode_with_region(&file(&smpl(0, 4, 11), 60), "none").unwrap().1, None);
        assert_eq!(decode_with_region(&file(&smpl(1, 4, 11)[..36], 36), "short").unwrap().1, None);
    }

    #[test]
    fn native_source_format_selects_each_supported_wav_kernel() {
        assert_eq!(source_format(include_bytes!("testdata/small_64.ogg")), Some(NativeFormat::Pcm16));
        for (kind, bits, expected) in [
            (hound::SampleFormat::Int, 8, Some(NativeFormat::GenericInteger)),
            (hound::SampleFormat::Int, 16, Some(NativeFormat::Pcm16)),
            (hound::SampleFormat::Int, 24, Some(NativeFormat::GenericInteger)),
            (hound::SampleFormat::Int, 32, Some(NativeFormat::GenericInteger)),
            (hound::SampleFormat::Float, 32, Some(NativeFormat::Float)),
        ] {
            let mut bytes = Cursor::new(Vec::new());
            let spec = hound::WavSpec { channels: 1, sample_rate: 44100, bits_per_sample: bits, sample_format: kind };
            let mut writer = hound::WavWriter::new(&mut bytes, spec).unwrap();
            if kind == hound::SampleFormat::Float {
                writer.write_sample(0.5f32).unwrap();
            } else {
                writer.write_sample(0i32).unwrap();
            }
            writer.finalize().unwrap();
            assert_eq!(source_format(bytes.get_ref()), expected, "{kind:?} {bits}");
        }
        assert_eq!(source_format(b"bad input"), None);
        assert!(SampleBank::synthesized().source_formats.is_empty());
    }

    #[test]
    fn decodes_wav() {
        let s = decode(&wav_bytes(2, &[16384, -16384, 0, 0]), "t.wav").unwrap();
        assert_eq!((s.rate, s.channels, s.frames()), (44_100, 2, 2));
        assert_eq!(s.data[0], 0.5);
        assert_eq!(s.data[1], -0.5);
    }

    #[test]
    fn vorbis_51_reaches_speaker_order() {
        // Fixture made with ffmpeg/libvorbis: 0.2 s of 100 Hz with peak 0.02 on FL, 0.04 FR,
        // 0.08 C, 0.16 LFE, 0.32 RL, 0.64 RR, stored in Vorbis order (FL, C, FR, RL, RR, LFE).
        // Each level doubles the previous one, so a swapped channel lands far outside its band;
        // the LFE decodes about 2.6 dB above its input (ffmpeg's decoder reads the same).
        let s = decode(include_bytes!("testdata/layout_51.ogg"), "layout_51.ogg").unwrap();
        assert_eq!((s.rate, s.channels), (44_100, 6));
        let expected = [0.02, 0.04, 0.08, 0.16, 0.32, 0.64];
        for (speaker, want) in expected.into_iter().enumerate() {
            let peak = f64::from(s.data.as_chunks::<6>().0.iter().map(|f| f[speaker].abs()).fold(0.0f32, f32::max));
            assert!(peak > want * 0.85 && peak < want * 1.45, "speaker {speaker}: peak {peak}, want {want}");
        }
    }

    #[test]
    fn vorbis_channel_permutation() {
        let frame = [10.0, 11.0, 12.0, 13.0, 14.0, 15.0]; // FL C FR RL RR LFE
        assert_eq!(vorbis_to_speaker_order(6, frame.to_vec()), [10.0, 12.0, 11.0, 15.0, 13.0, 14.0]);
        assert_eq!(vorbis_to_speaker_order(2, vec![1.0, 2.0]), [1.0, 2.0]);
        assert_eq!(
            vorbis_to_speaker_order(7, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
            [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]
        );
        assert_eq!(
            vorbis_to_speaker_order(8, vec![10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0]),
            [10.0, 12.0, 11.0, 17.0, 15.0, 16.0, 13.0, 14.0]
        );
    }

    #[test]
    fn vorbis_71_reaches_native_speaker_order() {
        let sample = decode(include_bytes!("testdata/layout_71.ogg"), "layout_71.ogg").unwrap();
        assert_eq!((sample.rate, sample.channels, sample.frames()), (44_100, 8, 8820));
        // Fixture encoded with ffmpeg/libvorbis, eight 100 Hz channels in
        // WAVE order at doubled amplitudes from 0.005 to 0.64. Vorbis order
        // FL FC FR SL SR BL BR LFE is reordered to FL FR FC LFE BL BR SL SR.
        for (channel, expected) in [0.005, 0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64].into_iter().enumerate() {
            let peak = sample.data.as_chunks::<8>().0.iter().map(|frame| frame[channel].abs()).fold(0.0, f32::max);
            assert!(
                peak > expected * 0.85 && peak < expected * 1.45,
                "channel {channel}: peak {peak}, expected {expected}"
            );
        }
    }

    #[test]
    fn native_vorbis_pcm_rounds_ties_to_even_before_clipping() {
        for (scaled, expected) in [
            (0.5, 0),
            (1.5, 2),
            (2.5, 2),
            (-0.5, 0),
            (-1.5, -2),
            (-2.5, -2),
            (10.75, 11),
            (-10.75, -11),
            (32766.5, 32766),
            (32767.5, 32767),
            (-32767.5, -32768),
            (65536.0, 32767),
            (-65536.0, -32768),
        ] {
            assert_eq!(vorbis_pcm16(scaled / 32768.0), expected as f32 / 32768.0, "scaled={scaled}");
        }
    }

    #[test]
    fn native_vorbis_conversion_keeps_the_signed_invalid_result() {
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX, -f32::MAX] {
            assert_eq!(vorbis_pcm16(sample), -1.0);
        }
    }

    #[test]
    fn imported_vorbis_uses_native_pcm_rounding_and_keeps_packet_extent() {
        let bytes = include_bytes!("testdata/layout_51.ogg");
        let sample = decode(bytes, "layout_51.ogg").unwrap();
        let mut reference = lewton::inside_ogg::OggStreamReader::new(Cursor::new(bytes)).unwrap();
        vorbis_window::configure(&mut reference.ident_hdr);
        let mut expected = Vec::new();
        let mut changed_from_truncation = 0;
        while let Some(packet) =
            reference.read_dec_packet_generic::<lewton::samples::InterleavedSamples<f32>>().unwrap()
        {
            for frame in packet.samples.as_chunks::<6>().0 {
                for channel in [0, 2, 1, 5, 3, 4] {
                    let scaled = frame[channel] * 32768.0;
                    let rounded = scaled.round_ties_even().clamp(-32768.0, 32767.0);
                    let truncated = scaled.clamp(-32768.0, 32767.0) as i16;
                    changed_from_truncation += usize::from(rounded != f32::from(truncated));
                    expected.push(rounded / 32768.0);
                }
            }
        }
        assert!(changed_from_truncation > 1000, "fixture must distinguish native rounding from truncation");
        expected.truncate(8820 * 6);
        assert_eq!(sample.data.len(), expected.len());
        assert!(sample.data.iter().zip(&expected).all(|(a, b)| a == b));
        assert_eq!((sample.rate, sample.channels, sample.frames()), (44_100, 6, 8820));
    }

    #[test]
    fn imported_vorbis_trims_each_same_format_chained_stream() {
        let bytes = include_bytes!("testdata/layout_51.ogg");
        // Make an independent logical stream without altering any audio
        // packets: Ogg identifies a chain by its serial and page checksums.
        let mut second = bytes.to_vec();
        let mut offset = 0;
        while offset < second.len() {
            assert_eq!(&second[offset..offset + 4], b"OggS");
            let segments = usize::from(second[offset + 26]);
            let payload: usize = second[offset + 27..offset + 27 + segments].iter().map(|v| usize::from(*v)).sum();
            let end = offset + 27 + segments + payload;
            let page = &mut second[offset..end];
            page[14] ^= 0x80;
            page[22..26].fill(0);
            let mut crc = 0_u32;
            for byte in page.iter() {
                crc ^= u32::from(*byte) << 24;
                for _ in 0..8 {
                    crc = (crc << 1) ^ if crc & 0x8000_0000 != 0 { 0x04c1_1db7 } else { 0 };
                }
            }
            page[22..26].copy_from_slice(&crc.to_le_bytes());
            offset = end;
        }
        let first = decode(bytes, "single").unwrap();
        let chain = decode(&[bytes.as_slice(), &second].concat(), "chained").unwrap();
        assert_eq!(chain.frames(), first.frames() * 2);
        assert_eq!(&chain.data[..first.data.len()], first.data);
        assert_eq!(&chain.data[first.data.len()..], first.data);
    }

    fn chain_link(channels: u8, block_exponent: u8, trim: u8, serial: u32, rate: u32) -> Vec<u8> {
        let index = usize::from(channels - 1) * 4 + usize::from(block_exponent - 6) * 2 + usize::from(trim);
        let mut records = include_bytes!("testdata/native_chain_links.bin").as_slice();
        for position in 0..=index {
            let length = u32::from_le_bytes(records[..4].try_into().unwrap()) as usize;
            if position != index {
                records = &records[4 + length..];
                continue;
            }
            let mut bytes = records[4..4 + length].to_vec();
            let mut offset = 0;
            while offset < bytes.len() {
                let segments = usize::from(bytes[offset + 26]);
                let body: usize = bytes[offset + 27..offset + 27 + segments].iter().map(|v| usize::from(*v)).sum();
                let end = offset + 27 + segments + body;
                let page = &mut bytes[offset..end];
                if offset == 0 {
                    let header = 27 + segments;
                    page[header + 12..header + 16].copy_from_slice(&rate.to_le_bytes());
                }
                page[14..18].copy_from_slice(&serial.to_le_bytes());
                page[22..26].fill(0);
                let mut crc = 0u32;
                for &byte in page.iter() {
                    crc ^= u32::from(byte) << 24;
                    for _ in 0..8 {
                        crc = (crc << 1) ^ if crc & 0x8000_0000 != 0 { 0x04c1_1db7 } else { 0 };
                    }
                }
                page[22..26].copy_from_slice(&crc.to_le_bytes());
                offset = end;
            }
            return bytes;
        }
        unreachable!()
    }

    #[test]
    fn imported_vorbis_chains_match_native_preload_family() {
        for line in include_str!("testdata/native_chain_reference.txt").lines() {
            let fields: Vec<_> = line.split(',').collect();
            let name: Vec<_> = fields[0].split('-').collect();
            let first: u8 = name[1].parse().unwrap();
            let mut bytes = chain_link(first, 6, 1, 1001, 44100);
            if name[0] == "triple" {
                bytes.extend(chain_link(9 - first, 7, 1, 1002, 48000));
                bytes.extend(chain_link(first, 6, 0, 1003, 22050));
            } else {
                let next: u8 = name[2].parse().unwrap();
                let trim: u8 = name[3].parse().unwrap();
                let rate = if name[4] == "0" { 44100 } else { 48000 };
                bytes = chain_link(first, 6, trim, 1001, 44100);
                bytes.extend(chain_link(next, 7, trim, 1002, rate));
            }
            let decoded = decode_with_loop(&bytes, fields[0]);
            if fields[3] != "0" {
                assert!(decoded.is_err(), "{} must report the native capacity error", fields[0]);
                continue;
            }
            let (sample, loop_frames) = decoded.unwrap();
            let frames: usize = fields[1].parse().unwrap();
            let used: usize = fields[2].parse().unwrap();
            assert_eq!((sample.channels, sample.rate, sample.frames()), (u16::from(first), 44100, frames));
            assert_eq!(loop_frames, (used < frames).then_some(used), "{}", fields[0]);
            let hash = sample
                .data
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
            assert_eq!(hash, u64::from_str_radix(fields[4], 16).unwrap(), "{}", fields[0]);
        }
    }

    #[test]
    fn mixed_vorbis_partial_frames_are_consumed_before_the_next_packet() {
        let chunks = [(2, 0..2), (2, 2..8)];
        let (sample, loop_frames) =
            mixed_vorbis_chain(6, 44100, 2, &[1., 2., 3., 4., 5., 6., 7., 8.], &chunks).unwrap();
        assert_eq!(&sample.data[..6], &[3., 5., 4., 8., 6., 7.]);
        assert_eq!(loop_frames, Some(1));
        assert!(mixed_vorbis_chain(1, 44100, 1, &[1., 2.], &[(2, 0..2)]).is_err());
        assert!(mixed_vorbis_chain(8, 44100, 1, &[1., 2.], &[(2, 0..2)]).is_err());
    }

    #[test]
    fn rejects_garbage_and_empty() {
        assert!(decode(b"nope", "x").is_err());
        assert!(matches!(decode(&wav_bytes(1, &[]), "x"), Err(SampleError::Empty(_))));
    }

    #[test]
    fn import_from_directory_falls_back_per_file() {
        let dir = std::env::temp_dir().join(format!("openrigcue-shakers-import-{}", std::process::id()));
        let susp = dir.join("sounds").join("suspension");
        std::fs::create_dir_all(&susp).unwrap();
        // A WAV file under the .ogg name: the decoder goes by content.
        std::fs::write(susp.join("3.ogg"), wav_bytes(6, &[1000; 60])).unwrap();
        let (bank, failures) = SampleBank::import(&dir).unwrap();
        let key = SampleKey::new(SampleFolder::Suspension, 3);
        assert_eq!(bank.origin(key), Some(SampleOrigin::Imported));
        assert_eq!(bank.get(key).unwrap().channels, 6);
        assert_eq!(bank.origin(SampleKey::new(SampleFolder::Suspension, 4)), Some(SampleOrigin::Synthesized));
        assert_eq!(failures.len(), SampleKey::PLAYED.len() - 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn built_in_sounds_are_the_default() {
        assert_eq!(SampleSource::default(), SampleSource::BuiltIn);
        let (bank, failures) = SampleBank::load(&SampleSource::default());
        assert!(failures.is_empty());
        for key in SampleKey::PLAYED {
            assert_eq!(bank.origin(key), Some(SampleOrigin::Synthesized), "{key:?}");
        }
        let bank = SampleBank::default();
        assert_eq!(bank.origin(SampleKey::new(SampleFolder::Turbulence, 0)), Some(SampleOrigin::Synthesized));
    }

    #[test]
    fn unreadable_import_source_keeps_built_in_sounds() {
        let missing = std::env::temp_dir().join(format!("openrigcue-shakers-missing-{}.exe", std::process::id()));
        std::fs::write(&missing, b"not an installer").unwrap();
        let (bank, failures) = SampleBank::load(&SampleSource::SrsImport(missing.clone()));
        std::fs::remove_file(missing).unwrap();
        assert!(matches!(failures.as_slice(), [(None, SampleError::Installer(_))]));
        assert_eq!(bank.origin(SampleKey::new(SampleFolder::Suspension, 6)), Some(SampleOrigin::Synthesized));
    }

    /// Imports the user's own SRS files when `OPENRIGCUE_SRS_SOUNDS` points at an install dir or installer.
    #[test]
    #[ignore = "needs a local SRS installation or installer"]
    fn real_import() {
        let path = std::env::var("OPENRIGCUE_SRS_SOUNDS").expect("OPENRIGCUE_SRS_SOUNDS");
        let (bank, failures) = SampleBank::import(Path::new(&path)).unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        let s = bank.get(SampleKey::new(SampleFolder::Suspension, 6)).unwrap();
        assert_eq!((s.channels, s.rate), (6, 44_100));
        // suspension/6's LFE channel is about 0.55 dB hotter than its other five (the LFE offset differs per
        // file, between -2.8 and +2.1 dB): after reordering it must
        // be the sub's.
        let rms = |k: usize| {
            let sum: f64 = s.data.as_chunks::<6>().0.iter().map(|f| f64::from(f[k]).powi(2)).sum();
            (sum / s.frames() as f64).sqrt()
        };
        for k in [0, 1, 2, 4, 5] {
            assert!(rms(3) > rms(k) * 1.04, "channel {k}");
        }
        assert_eq!(bank.get(SampleKey::new(SampleFolder::Turbulence, 0)).unwrap().rate, 48_000);
    }
}
