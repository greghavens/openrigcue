//! Software 5.1 mixer: plays the rig's actions
//! (sample voices routed through per-group speaker levels and optional low-pass, plus the
//! engine voice), producing interleaved frames for an output device.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::channel_map::{OutputMatrix, SPEAKER_ORDER};
use super::engine_sound::{EngineError, EngineVoice};

fn native_sound_error(class: &'static str, message: &str) -> EngineError {
    EngineError::SoundOperation { error: DspError { class, message: message.into() } }
}
fn invalid_sound() -> EngineError {
    native_sound_error("pyfmodex.exceptions.FmodError", "INVALID HANDLE")
}
use super::layout::{Group, PerGroup, PerSpeaker, Speaker, GROUP_COUNT};
use super::levels::GroupLevels;
use super::native_mono::Mono;
use super::rig::{Action, DspUpdate, FilterSettings, LowPass, Plays, Voice};
use super::samples::{Sample, SampleBank, SampleKey, SampleOrigin};
use super::tuning::{DspError, NativeDspSettings};

#[path = "multiband.rs"]
mod multiband;
use multiband::Multiband;

/// Speaker buses, in [`Speaker::ALL`] order (the WAVE order FL, FR, C, LFE, RL, RR).
/// [`Mixer::render_layout`] places them on a device's own channel order.
pub const CHANNELS: usize = 6;

/// Identifies one [`GroupBuild`].
type BuildId = u64;

/// Where a voice's signal goes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Route {
    /// Through `group` of the build that was current when the voice started.
    Group { build: BuildId, group: Group },
    /// Fixed speaker levels, bypassing the groups (the speaker test).
    Direct(PerSpeaker),
}

/// Connections to the core system's master group, in creation order.
#[derive(Clone, Copy)]
enum MasterInput {
    Build(BuildId),
    Direct { voice: Voice, frame: Option<[f32; CHANNELS]> },
}

pub(super) type SoundHandle = Arc<NativeSound>;
/// A source Sound can outlive the global that first held it. Active channels
/// and in-flight release receivers retain it independently.
pub(super) struct NativeSound {
    id: u64,
    sample: Arc<Sample>,
    origin: SampleOrigin,
    format: Option<super::native_mono::Format>,
    loop_frames: Option<usize>,
    loop_start: usize,
    plays: std::sync::Mutex<Plays>,
    released: AtomicBool,
    generation: u64,
}

struct Playing {
    sample: Arc<Sample>,
    /// Retained with the sample when the bank changes during playback.
    origin: SampleOrigin,
    /// Resampler phase: source frames in the high word, fraction in the low word.
    position: u64,
    /// Source frames per output frame, in the same 32.32 format.
    step: u64,
    /// Passes left after the current one; `None` = loop forever.
    repeats: Option<u32>,
    route: Route,
    interpolation: Interpolation,
    mono: Option<Mono>,
    /// Same chunk/end planner as mono; channel math is separate.
    native_position: Option<Mono>,
}

#[derive(Clone, Copy)]
enum Interpolation {
    Weighted,
    Delta,
    FusedDelta,
}

impl Interpolation {
    fn for_channels(channels: u16) -> Self {
        // The linear resampler has specialized even-channel kernels.
        // The SSE3 form subtracts then scales; the AVX2/512 form
        // fuses the last multiply/add. Other layouts use the generic kernel.
        if matches!(channels, 2 | 4 | 6 | 8) {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            {
                return Self::for_cpu(
                    std::is_x86_feature_detected!("sse3"),
                    std::is_x86_feature_detected!("avx2"),
                    std::is_x86_feature_detected!("avx512f"),
                    std::is_x86_feature_detected!("fma"),
                );
            }
        }
        Self::Weighted
    }

    fn for_cpu(sse3: bool, avx2: bool, avx512: bool, fma: bool) -> Self {
        if (avx512 || avx2) && fma {
            Self::FusedDelta
        } else if sse3 {
            Self::Delta
        } else {
            Self::Weighted
        }
    }

    fn fraction(self, phase: u32) -> f32 {
        match self {
            Self::Weighted => phase as f32 * (1.0_f32 / PHASE_ONE as f32),
            // Both specialized kernels discard bit0 before float conversion.
            Self::Delta | Self::FusedDelta => (phase >> 1) as f32 * f32::from_bits(0x3000_0000),
        }
    }

    fn sample(self, a: f32, b: f32, fraction: f32) -> f32 {
        match self {
            Self::Weighted => b * fraction + a * (1.0 - fraction),
            Self::Delta => a + (b - a) * fraction,
            Self::FusedDelta => (b - a).mul_add(fraction, a),
        }
    }
}

const PHASE_ONE: u64 = 1 << 32;

/// The phase increment: the frequency/output-rate division is rounded to float32
/// before scaling by 2^32 and truncating to an integer.
fn sample_step(source_rate: u32, output_rate: u32) -> u64 {
    ((source_rate as f32 / output_rate as f32) * PHASE_ONE as f32) as u64
}

impl Playing {
    /// Next frame of the voice (one value per speaker), or `None` when it has finished.
    fn next(&mut self) -> Option<PerSpeaker> {
        if let Some(mono) = &mut self.mono {
            let value = mono.next(&self.sample.data, &mut self.position, self.step, &mut self.repeats)?;
            let front = f64::from(value * f32::from_bits(0x3f35_0481));
            return Some([front, front, 0.0, 0.0, 0.0, 0.0]);
        }
        let frames = self.sample.frames();
        if frames == 0 {
            return None;
        }
        let native = if let Some(native) = &mut self.native_position {
            Some(native.multichannel_indices(frames, &mut self.position, self.step, &mut self.repeats)?)
        } else {
            None
        };
        let end = frames as u64 * PHASE_ONE;
        if native.is_none() && self.position >= end {
            let loops = self.position / end;
            if let Some(repeats) = self.repeats {
                if loops > u64::from(repeats) {
                    return None;
                }
                self.repeats = Some(repeats - loops as u32);
            }
            self.position %= end;
        }
        if native.is_some_and(|value| value.0.is_none()) {
            return Some([0.; CHANNELS]);
        }
        let i = native.map_or((self.position >> 32) as usize, |value| value.0.unwrap_or(0));
        let fraction = native.map_or(self.position as u32, |value| value.2);
        let looping = self.repeats != Some(0);
        let j = if i + 1 < frames {
            Some(i + 1)
        } else if looping {
            Some(0)
        } else {
            None
        };
        let j = native.map_or(j, |value| value.1);
        if native.is_none() {
            self.position = self.position.wrapping_add(self.step);
        }
        if self.sample.channels == 1 && self.origin == SampleOrigin::Imported {
            // WAV widths without a specialized kernel use generic interpolation.
            let a = self.sample.data[i];
            let value = if fraction == 0 {
                a
            } else {
                let b = j.map_or(0.0, |j| self.sample.data[j]);
                self.interpolation.sample(a, b, self.interpolation.fraction(fraction))
            };
            // Imported mono samples: 1 -> 6 with an equal-power front pair only.
            // Synthesized samples use the six-speaker projection.
            let front = f64::from(value * f32::from_bits(0x3f35_0481));
            return Some([front, front, 0.0, 0.0, 0.0, 0.0]);
        }
        if matches!(self.sample.channels, 2 | 3 | 4 | 5 | 7 | 8) {
            let channels = usize::from(self.sample.channels);
            let a = &self.sample.data[i * channels..(i + 1) * channels];
            let b = j.map(|j| &self.sample.data[j * channels..(j + 1) * channels]);
            let fraction = self.interpolation.fraction(fraction);
            let mut frame = [0.0; 8];
            for (channel, value) in frame[..channels].iter_mut().enumerate() {
                *value = self.interpolation.sample(a[channel], b.map_or(0.0, |frame| frame[channel]), fraction);
            }
            return Some(multichannel_to_speakers(&frame[..channels]));
        }
        let a = speaker_frame(&self.sample, i);
        if fraction == 0 {
            return Some(a);
        }
        let b = j.map_or([0.0; CHANNELS], |j| speaker_frame(&self.sample, j));
        let fraction = self.interpolation.fraction(fraction);
        Some(std::array::from_fn(|k| f64::from(self.interpolation.sample(a[k] as f32, b[k] as f32, fraction))))
    }
}

/// The default connection matrix from eight sample channels into the six speaker buses.
/// This is a fixed down-mix matrix, not a directional-panning one.
fn surround_to_speakers(src: &[f32; 8]) -> PerSpeaker {
    let side_front = f32::from_bits(0x3ebc_0ebf); // 0.3673
    let side_rear = f32::from_bits(0x3f6e_147b); // 0.93
    let back_same = f32::from_bits(0x3f33_3333); // 0.7
    let back_opposite = f32::from_bits(0x3eeb_851f); // 0.46

    // The eight-input kernel combines corresponding lanes of its two four-float vectors,
    // then the lane pairs. Keep those separately rounded products and additions.
    [
        f64::from(src[4] * side_front + src[0]),
        f64::from(src[5] * side_front + src[1]),
        f64::from(src[2]),
        f64::from(src[3]),
        f64::from(src[4] * side_rear + (src[6] * back_same + src[7] * back_opposite)),
        f64::from(src[5] * side_rear + (src[6] * back_opposite + src[7] * back_same)),
    ]
}

/// Other sample layouts are compressed from their speaker masks before the connection
/// matrix is applied. Mono keeps the synthesized sound policy; six/eight-channel paths stay
/// separate. Seven-channel audio has no LFE and shifts the side/back channels down one index.
fn multichannel_to_speakers(src: &[f32]) -> PerSpeaker {
    let direct = |channel: usize| f64::from(src[channel]);
    match src.len() {
        2 => [direct(0), direct(1), 0.0, 0.0, 0.0, 0.0],
        3 => [direct(0), direct(1), direct(2), 0.0, 0.0, 0.0],
        4 => {
            let front = f32::from_bits(0x3f76_2b6b); // 0.9616
            let front_rear = f32::from_bits(0x3e8c_985f); // 0.2746
            let same_rear = f32::from_bits(0x3f75_d639); // 0.9603
            let opposite_rear = f32::from_bits(0x3ed8_5f07); // 0.4226
            let sum = |row: [f32; 4]| {
                let product: [f32; 4] = std::array::from_fn(|c| src[c] * row[c]);
                if matrix_uses_simd() {
                    (product[0] + product[1]) + (product[2] + product[3])
                } else {
                    product.into_iter().rev().fold(0.0, |sum, value| sum + value)
                }
            };
            [
                f64::from(sum([front, 0.0, 0.0, 0.0])),
                f64::from(sum([0.0, front, 0.0, 0.0])),
                f64::from(sum([0.0; 4])),
                f64::from(sum([0.0; 4])),
                f64::from(sum([front_rear, 0.0, same_rear, opposite_rear])),
                f64::from(sum([0.0, front_rear, opposite_rear, same_rear])),
            ]
        }
        5 => [direct(0), direct(1), direct(2), 0.0, direct(3), direct(4)],
        7 => {
            let side_front = f32::from_bits(0x3ebc_0ebf);
            let side_rear = f32::from_bits(0x3f6e_147b);
            let back_same = f32::from_bits(0x3f33_3333);
            let back_opposite = f32::from_bits(0x3eeb_851f);
            let rear = |side, back, other| {
                let (side, back, other) = (side * side_rear, back * back_same, other * back_opposite);
                // SSE combines lanes before horizontal sums; the generic scalar kernel
                // accumulates the compressed input channels in reverse order.
                if matrix_uses_simd() {
                    back + (other + side)
                } else {
                    (other + back) + side
                }
            };
            [
                f64::from(src[0] + src[3] * side_front),
                f64::from(src[1] + src[4] * side_front),
                direct(2),
                0.0,
                f64::from(rear(src[3], src[5], src[6])),
                f64::from(rear(src[4], src[6], src[5])),
            ]
        }
        8 => surround_to_speakers(src.try_into().expect("eight-channel frame")),
        _ => unreachable!("known multichannel sample layout"),
    }
}

fn matrix_uses_simd() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        std::is_x86_feature_detected!("sse3")
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        false
    }
}

/// Source frame `i` spread over the six speakers. Six-channel samples map directly;
/// other supported multichannel samples use the default connection matrix after
/// interpolation. Mono and unknown layouts retain the synthesized sound projection.
fn speaker_frame(s: &Sample, i: usize) -> PerSpeaker {
    let ch = usize::from(s.channels);
    let src = &s.data[i * ch..(i + 1) * ch];
    if ch == CHANNELS {
        std::array::from_fn(|k| f64::from(src[k]))
    } else if matches!(ch, 2 | 3 | 4 | 5 | 7 | 8) {
        multichannel_to_speakers(src)
    } else {
        let mono = src.iter().map(|&v| f64::from(v)).sum::<f64>() / ch as f64;
        [mono; CHANNELS]
    }
}

/// Frames per processing block.
const DSP_BLOCK_FRAMES: usize = 1024;

/// One build of the channel groups: a new
/// channel group per name with its mix levels, and a new low-pass filter. An old build is never
/// released or re-levelled, so voices keep the build they started in; the per-frame
/// DSP block only reaches the current build.
struct GroupBuild {
    levels: GroupLevels,
    /// The group list starts empty, before any successful explicit rebuild.
    initial_channel_list: bool,
    filters: PerGroup<Option<LowPass>>,
    /// Whether the filter is attached, independent of which filter kinds the mixer renders.
    attached: PerGroup<bool>,
    filter_types: PerGroup<i32>,
    /// Accepted float parameters survive a disabled filter and a later setter error.
    parameters: PerGroup<LowPass>,
    multiband_state: PerGroup<[Multiband; CHANNELS]>,
    /// Attachment snapshot for this software processing block. Metadata above
    /// still records each successful setter immediately, before audio catches up.
    rendering: PerGroup<bool>,
    /// An input-idle block is skipped, retaining filter memory.
    processing: PerGroup<bool>,
    block_position: usize,
    /// Sum of this frame's voices per group, before the filter.
    bus: PerGroup<PerSpeaker>,
    used: PerGroup<bool>,
}

impl GroupBuild {
    fn new(levels: GroupLevels) -> GroupBuild {
        GroupBuild {
            levels,
            initial_channel_list: false,
            filters: [None; GROUP_COUNT],
            attached: [false; GROUP_COUNT],
            filter_types: [1; GROUP_COUNT],
            parameters: [LowPass::default(); GROUP_COUNT],
            multiband_state: [[Multiband::default(); CHANNELS]; GROUP_COUNT],
            rendering: [false; GROUP_COUNT],
            processing: [false; GROUP_COUNT],
            block_position: 0,
            bus: [[0.0; CHANNELS]; GROUP_COUNT],
            used: [false; GROUP_COUNT],
        }
    }

    /// The rig's DSP block on this build: filter type, cutoff and Q per group.
    fn set_filters(&mut self, filters: &PerGroup<FilterSettings>, rate: f64) -> Result<(), EngineError> {
        let filters = filters.each_ref().map(|requested| NativeDspSettings {
            filter_type: Ok(i32::from(requested.low_pass)),
            cutoff_hz: Ok(requested.parameters.cutoff_hz),
            q: Ok(requested.parameters.q),
        });
        self.set_native_filters(&filters, rate)
    }

    fn set_native_filters(&mut self, filters: &PerGroup<NativeDspSettings>, rate: f64) -> Result<(), EngineError> {
        for group in Group::ALL {
            self.set_native_filter(group, &filters[group.index()], rate)?;
        }
        Ok(())
    }

    fn set_native_filter(&mut self, group: Group, requested: &NativeDspSettings, rate: f64) -> Result<(), EngineError> {
        for update in [
            DspUpdate::Remove,
            DspUpdate::Type(requested.filter_type.clone()),
            DspUpdate::Frequency(requested.cutoff_hz.clone()),
            DspUpdate::Q(requested.q.clone()),
            DspUpdate::Attach,
        ] {
            self.apply_dsp_step(group, &update, rate)?;
        }
        Ok(())
    }

    fn apply_dsp_step(&mut self, group: Group, update: &DspUpdate, rate: f64) -> Result<(), EngineError> {
        let g = group.index();
        let parameter_error =
            |parameter, error: &DspError| EngineError::DspParameter { group, parameter, error: error.clone() };
        match update {
            DspUpdate::Remove => {
                if self.initial_channel_list {
                    return Err(EngineError::DspParameter {
                        group,
                        parameter: "channel_group",
                        error: DspError {
                            class: "TypeError",
                            message: "list indices must be integers or slices, not str".into(),
                        },
                    });
                }
                self.filters[g] = None;
                self.attached[g] = false;
            }
            DspUpdate::Type(value) => {
                self.filter_types[g] = (*value.as_ref().map_err(|e| parameter_error("A_FILTER", e))?).clamp(0, 12);
            }
            DspUpdate::Frequency(value) => {
                let frequency = *value.as_ref().map_err(|e| parameter_error("A_FREQUENCY", e))?;
                if !(frequency as f32).is_finite() {
                    return Err(EngineError::InvalidDspFloat { group, parameter: "A_FREQUENCY" });
                }
                self.parameters[g].cutoff_hz = f64::from(frequency as f32).clamp(20.0, 22000.0);
            }
            DspUpdate::Q(value) => {
                let q = *value.as_ref().map_err(|e| parameter_error("A_Q", e))?;
                if !(q as f32).is_finite() {
                    return Err(EngineError::InvalidDspFloat { group, parameter: "A_Q" });
                }
                self.parameters[g].q = f64::from(q as f32).clamp(f64::from(0.1f32), 10.0);
            }
            DspUpdate::Attach => {
                self.set_filter(g, (self.filter_types[g] != 0).then_some(self.parameters[g]), rate);
                self.attached[g] = true;
            }
        }
        // Setters change the stored filter even if another in-flight call has
        // already reattached it. Attachment and parameter lifetime are independent.
        if self.attached[g] && matches!(update, DspUpdate::Type(_) | DspUpdate::Frequency(_) | DspUpdate::Q(_)) {
            self.set_filter(g, (self.filter_types[g] != 0).then_some(self.parameters[g]), rate);
        }
        Ok(())
    }

    fn set_filter(&mut self, group: usize, filter: Option<LowPass>, rate: f64) {
        self.filters[group] = filter;
        if let Some(parameters) = filter {
            for state in &mut self.multiband_state[group] {
                state.tune(self.filter_types[group], parameters.cutoff_hz, parameters.q, rate);
            }
        }
    }

    fn retune(&mut self, rate: f64) {
        for (g, parameters) in self.filters.iter().enumerate() {
            if let Some(parameters) = parameters {
                for state in &mut self.multiband_state[g] {
                    state.tune(self.filter_types[g], parameters.cutoff_hz, parameters.q, rate);
                }
            }
        }
    }

    fn add(&mut self, group: Group, frame: PerSpeaker) {
        let g = group.index();
        self.used[g] = true;
        for (bus, sample) in self.bus[g].iter_mut().zip(frame) {
            // Channels are first submixed in the group's float32
            // buffer. Its output matrix is applied to that completed sum.
            *bus = f64::from(*bus as f32 + sample as f32);
        }
    }

    /// Add this frame's groups (through their filters) to `out` and clear the buses.
    fn mix_into(&mut self, out: &mut PerSpeaker) {
        if self.block_position == 0 {
            for g in 0..GROUP_COUNT {
                self.rendering[g] = self.attached[g] && self.filters[g].is_some();
                // A block with idle input is not processed,
                // even with nonzero filter delay. Such a block also
                // leaves pending coefficient updates untouched.
                self.processing[g] = self.used[g];
                if self.rendering[g] && self.processing[g] {
                    for filter in &mut self.multiband_state[g] {
                        filter.begin_block(DSP_BLOCK_FRAMES);
                    }
                }
            }
        }
        for g in 0..GROUP_COUNT {
            let filtered = self.rendering[g];
            self.processing[g] |= self.used[g];
            if !self.used[g] && (!filtered || !self.processing[g]) {
                continue;
            }
            // A newly started software voice may enter partway through the
            // callback. No earlier silent block has initialized this DSP.
            if filtered && self.used[g] && !self.multiband_state[g][0].is_initialized() {
                for filter in &mut self.multiband_state[g] {
                    filter.begin_block(DSP_BLOCK_FRAMES - self.block_position);
                }
            }
            let levels = self.levels.of(Group::ALL[g]);
            for (k, o) in out.iter_mut().enumerate() {
                // The mix levels are float32 and form
                // a diagonal matrix for a six-channel group. The filter
                // follows that fader, so its input includes the rounded gain.
                let x = f64::from(self.bus[g][k] as f32 * levels[k] as f32);
                let mixed = if !filtered { x } else { self.multiband_state[g][k].process(x) };
                *o = f64::from(*o as f32 + mixed as f32);
            }
            self.bus[g] = [0.0; CHANNELS];
            self.used[g] = false;
        }
        self.block_position = (self.block_position + 1) % DSP_BLOCK_FRAMES;
    }

    /// No active voice or final partial input block can still reach this build.
    fn is_quiet(&self) -> bool {
        self.block_position == 0 || self.processing.iter().all(|active| !active)
    }
}

/// Engine metadata that readers can inspect while the mixer is rendering.
#[derive(Clone, Default)]
pub struct EngineStatus(Arc<AtomicBool>);

impl EngineStatus {
    pub fn is_fmod(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The mixer state.
pub struct Mixer {
    rate: u32,
    bank: SampleBank,
    builds: BTreeMap<BuildId, GroupBuild>,
    current: BuildId,
    voices: HashMap<Voice, Playing>,
    /// Inputs are appended to this list and summed
    /// from its head. HashMap order must not decide float sums.
    voice_order: Vec<Voice>,
    /// The held sounds, separate from currently playing channels.
    native_slots: HashMap<Voice, SoundHandle>,
    native_channels: HashMap<Voice, SoundHandle>,
    // A replaced, unreleased sound stays allocated
    // until the device closes.
    native_retired: Vec<SoundHandle>,
    next_native: u64,
    native_generation: u64,
    /// Speaker tests create fresh groups alongside the effect groups. A
    /// later profile rebuild appends its groups after an existing test.
    master_order: Vec<MasterInput>,
    engine: EngineVoice,
    /// Library and bank ownership survive closing an output; Studio instances do not.
    closed_engine: Option<super::fmod::FmodSource>,
    engine_status: EngineStatus,
}

impl Mixer {
    pub fn new(rate: u32, bank: SampleBank, levels: GroupLevels) -> Mixer {
        Self::with_build(rate, bank, GroupBuild::new(levels))
    }

    pub(super) fn new_unbuilt(rate: u32, bank: SampleBank) -> Mixer {
        Self::with_build(rate, bank, GroupBuild { initial_channel_list: true, ..GroupBuild::new(GroupLevels::empty()) })
    }

    fn with_build(rate: u32, bank: SampleBank, build: GroupBuild) -> Mixer {
        Mixer {
            rate,
            bank,
            builds: BTreeMap::from([(0, build)]),
            current: 0,
            voices: HashMap::new(),
            voice_order: Vec::new(),
            native_slots: HashMap::new(),
            native_channels: HashMap::new(),
            native_retired: Vec::new(),
            next_native: 1,
            native_generation: 0,
            master_order: vec![MasterInput::Build(0)],
            engine: EngineVoice::synth(rate),
            closed_engine: None,
            engine_status: EngineStatus::default(),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    pub(super) fn engine_mut(&mut self) -> &mut EngineVoice {
        &mut self.engine
    }

    pub fn engine_status(&self) -> EngineStatus {
        self.engine_status.clone()
    }

    /// Render at `rate` from now on (the rate the output device runs at).
    pub fn set_sample_rate(&mut self, rate: u32) {
        self.rate = rate;
        for v in self.voices.values_mut() {
            v.step = sample_step(v.sample.rate, rate);
            if let Some(mono) = &mut v.native_position {
                mono.rate_changed(v.sample.rate, rate);
            }
            if let Some(mono) = &mut v.mono {
                mono.rate_changed(v.sample.rate, rate);
            }
        }
        for b in self.builds.values_mut() {
            b.retune(f64::from(rate));
        }
        self.engine.set_sample_rate(rate);
        self.engine_status.0.store(self.engine.is_fmod(), Ordering::SeqCst);
    }

    /// A selected-device change tears down the audio graph and the Studio system.
    /// The bank and the effect state are kept; every owned audio node is released.
    pub(super) fn close_output(&mut self, rate: u32) {
        self.rate = rate;
        if let Some(source) = self.engine.release_for_reopen(self.rate) {
            self.closed_engine = Some(source);
        }
        self.engine_status.0.store(false, Ordering::SeqCst);
        self.voices.clear();
        self.voice_order.clear();
        for sound in self.native_slots.values().chain(self.native_channels.values()) {
            sound.released.store(true, Ordering::Relaxed);
        }
        self.native_channels.clear();
        self.native_retired.clear();
        self.native_generation += 1;
        // The group list stays empty until the first completed rebuild.
        let initial_channel_list = self.builds[&self.current].initial_channel_list;
        self.builds.clear();
        self.builds.insert(0, GroupBuild { initial_channel_list, ..GroupBuild::new(GroupLevels::empty()) });
        self.current = 0;
        self.master_order.clear();
        self.master_order.push(MasterInput::Build(0));
    }

    pub(super) fn reopen_engine(&mut self) {
        if let Some(source) = self.closed_engine.take() {
            match source.open(self.rate) {
                Ok(engine) => self.engine = EngineVoice::Fmod(Box::new(engine)),
                Err(error) => {
                    tracing::warn!(%error, "FMOD engine not available after device change");
                    self.closed_engine = Some(source);
                }
            }
        }
        self.engine_status.0.store(self.engine.is_fmod(), Ordering::SeqCst);
    }

    /// Rebuild the channel groups with new levels. Voices
    /// started from now on use them; voices already playing keep the levels and filter of the
    /// build they started in until they are stopped or restarted.
    pub fn set_levels(&mut self, levels: GroupLevels) {
        let block_position = self.builds[&self.current].block_position;
        self.current += 1;
        self.builds.insert(self.current, GroupBuild { block_position, ..GroupBuild::new(levels) });
        self.master_order.push(MasterInput::Build(self.current));
        self.drop_finished_builds();
    }

    /// Replace the sample bank; playing voices keep their sample and its source routing.
    pub fn set_bank(&mut self, bank: SampleBank) {
        self.bank = bank;
    }

    /// Apply a frame's actions in order. A DSP or engine action that fails
    /// ([`EngineError`](super::engine_sound::EngineError)) ends the frame: the actions after it
    /// are not applied.
    pub fn apply(&mut self, actions: &[Action]) -> Result<(), super::engine_sound::EngineError> {
        for action in actions {
            match action {
                Action::Stop(voice) => {
                    self.voices.remove(voice);
                    self.voice_order.retain(|slot| slot != voice);
                    self.master_order
                        .retain(|input| !matches!(input, MasterInput::Direct { voice: slot, .. } if slot == voice));
                }
                Action::Start { voice, sample, plays, group } => {
                    let route = Route::Group { build: self.current, group: *group };
                    self.start(*voice, *sample, *plays, route);
                }
                Action::Filters(f) => {
                    let rate = f64::from(self.rate);
                    if let Some(build) = self.builds.get_mut(&self.current) {
                        build.set_filters(f, rate)?;
                    }
                }
                Action::NativeDspStep { group, update } => {
                    let rate = f64::from(self.rate);
                    if let Some(build) = self.builds.get_mut(&self.current) {
                        build.apply_dsp_step(*group, update, rate)?;
                    }
                }
                Action::NativeGroupFilter { group, settings } => {
                    let rate = f64::from(self.rate);
                    if let Some(build) = self.builds.get_mut(&self.current) {
                        build.set_native_filter(*group, settings, rate)?;
                    }
                }
                Action::NativeFilters(f) => {
                    let rate = f64::from(self.rate);
                    if let Some(build) = self.builds.get_mut(&self.current) {
                        build.set_native_filters(f, rate)?;
                    }
                }
                Action::TractionFailure(error) => {
                    return Err(super::engine_sound::EngineError::TractionParameter { error: error.clone() })
                }
                Action::SuspensionFailure(error) => {
                    return Err(super::engine_sound::EngineError::SuspensionParameter { error: error.clone() })
                }
                Action::Engine(c) => self.engine.command(c)?,
                Action::PauseEngines => self.engine.pause()?,
                Action::PauseEngine(instance) => self.engine.pause_instance_live(*instance)?,
                Action::EngineUpdate => self.engine.submit()?,
            }
        }
        Ok(())
    }

    /// Play `sample` once with fixed speaker levels (the speaker test).
    pub fn play_direct(&mut self, voice: Voice, sample: SampleKey, levels: PerSpeaker) {
        self.start(voice, sample, Plays::Once, Route::Direct(levels));
    }

    fn start(&mut self, voice: Voice, key: SampleKey, plays: Plays, route: Route) {
        self.voices.remove(&voice);
        self.voice_order.retain(|slot| *slot != voice);
        self.master_order.retain(|input| !matches!(input, MasterInput::Direct { voice: slot, .. } if *slot == voice));
        let Some(sample) = self.bank.get(key) else {
            return;
        };
        let origin = self.bank.origin(key).expect("sample and origin share a bank entry");
        let playing = self.make_playing(
            sample,
            origin,
            self.bank.source_format(key),
            (self.bank.loop_frames(key), self.bank.loop_start(key)),
            plays,
            route,
        );
        self.voices.insert(voice, playing);
        self.voice_order.push(voice);
        if matches!(route, Route::Direct(_)) {
            self.master_order.push(MasterInput::Direct { voice, frame: None });
        }
    }

    /// Snapshot the selected Sound data into one independent playing channel.
    fn make_playing(
        &self,
        sample: Arc<Sample>,
        origin: SampleOrigin,
        format: Option<super::native_mono::Format>,
        (loop_frames, loop_start): (Option<usize>, usize),
        plays: Plays,
        route: Route,
    ) -> Playing {
        let repeats = match plays {
            Plays::Once => Some(0),
            Plays::Repeat(n) => Some(n),
            Plays::Forever => None,
        };
        let step = sample_step(sample.rate, self.rate);
        // PCM8/24/32 always use the generic source-format kernel, including
        // even layouts that have specialized PCM16 and float implementations.
        let interpolation = if format == Some(super::native_mono::Format::GenericInteger) {
            Interpolation::Weighted
        } else {
            Interpolation::for_channels(sample.channels)
        };
        let mut mono = format
            .filter(|_| sample.channels == 1)
            .map(|format| Mono::new(format, sample.rate, self.rate, self.builds[&self.current].block_position));
        let mut native_position = if origin == SampleOrigin::Imported && (2..=8).contains(&sample.channels) {
            Some(Mono::new(
                super::native_mono::Format::Float,
                sample.rate,
                self.rate,
                self.builds[&self.current].block_position,
            ))
        } else {
            None
        };
        for planner in [&mut mono, &mut native_position].into_iter().flatten() {
            planner.set_loop_frames(loop_frames);
            planner.set_loop_start(loop_start);
        }
        Playing { sample, origin, position: 0, step, repeats, route, interpolation, mono, native_position }
    }

    pub(super) fn native_slot(&self, voice: Voice) -> Option<SoundHandle> {
        self.native_slots.get(&voice).cloned()
    }
    pub(super) fn native_clear(&mut self, voice: Voice) {
        if let Some(sound) = self.native_slots.remove(&voice) {
            self.native_retired.push(sound);
        }
        self.collect_native_sounds();
    }
    pub(super) fn native_create(&mut self, voice: Voice, key: SampleKey, looping: bool) {
        if let Some(sample) = self.bank.get(key) {
            let id = self.next_native;
            self.next_native += 1;
            let sound = Arc::new(NativeSound {
                id,
                sample,
                origin: self.bank.origin(key).expect("known sample"),
                format: self.bank.source_format(key),
                loop_frames: self.bank.loop_frames(key),
                loop_start: self.bank.loop_start(key),
                plays: std::sync::Mutex::new(if looping { Plays::Forever } else { Plays::Once }),
                released: AtomicBool::new(false),
                generation: self.native_generation,
            });
            if let Some(old) = self.native_slots.insert(voice, sound) {
                self.native_retired.push(old);
            }
            self.collect_native_sounds();
        }
    }
    pub(super) fn native_set_loops(&mut self, voice: Voice, plays: Plays) -> Result<(), EngineError> {
        let sound = self
            .native_slot(voice)
            .ok_or_else(|| native_sound_error("AttributeError", "'NoneType' object has no attribute 'loop_count'"))?;
        if sound.generation != self.native_generation || sound.released.load(Ordering::Relaxed) {
            return Err(invalid_sound());
        }
        *sound.plays.lock().unwrap_or_else(|p| p.into_inner()) = plays;
        Ok(())
    }
    pub(super) fn native_release(&mut self, sound: &SoundHandle) -> Result<(), EngineError> {
        if sound.generation != self.native_generation || sound.released.swap(true, Ordering::Relaxed) {
            return Err(invalid_sound());
        }
        let removed: HashSet<_> =
            self.native_channels.iter().filter_map(|(voice, item)| (item.id == sound.id).then_some(*voice)).collect();
        self.native_channels.retain(|voice, _| !removed.contains(voice));
        self.voices.retain(|voice, _| !removed.contains(voice));
        self.voice_order.retain(|voice| !removed.contains(voice));
        self.master_order.retain(|input| !matches!(input,MasterInput::Direct{voice,..} if removed.contains(voice)));
        self.collect_native_sounds();
        Ok(())
    }
    pub(super) fn native_play(&mut self, voice: Voice, group: Group) -> Result<(), EngineError> {
        let sound = self.native_slot(voice).ok_or_else(|| {
            native_sound_error(
                "TypeError",
                "Bad type of passed argument (<class 'NoneType'>), expected <class 'pyfmodex.sound.Sound'>",
            )
        })?;
        if sound.generation != self.native_generation || sound.released.load(Ordering::Relaxed) {
            return Err(invalid_sound());
        }
        let plays = *sound.plays.lock().unwrap_or_else(|p| p.into_inner());
        let playing = self.make_playing(
            Arc::clone(&sound.sample),
            sound.origin,
            sound.format,
            (sound.loop_frames, sound.loop_start),
            plays,
            Route::Group { build: self.current, group },
        );
        // Playing allocates a channel; it does not release the previous
        // sound, nor its already playing channels.
        if let Some(old) = self.voices.remove(&voice) {
            let detached = Voice::Detached(self.next_native);
            self.next_native += 1;
            self.voices.insert(detached, old);
            for slot in &mut self.voice_order {
                if *slot == voice {
                    *slot = detached;
                }
            }
            for input in &mut self.master_order {
                if let MasterInput::Direct { voice: slot, .. } = input {
                    if *slot == voice {
                        *slot = detached;
                    }
                }
            }
            if let Some(sound) = self.native_channels.remove(&voice) {
                self.native_channels.insert(detached, sound);
            }
        }
        self.voices.insert(voice, playing);
        self.voice_order.push(voice);
        self.native_channels.insert(voice, sound);
        Ok(())
    }
    fn collect_native_sounds(&mut self) {
        self.native_channels.retain(|voice, _| self.voices.contains_key(voice));
        self.native_retired.retain(|sound| !sound.released.load(Ordering::Relaxed));
    }

    /// Forget old builds once their final active processing block has ended.
    fn drop_finished_builds(&mut self) {
        let in_use: HashSet<BuildId> = self
            .voices
            .values()
            .filter_map(|v| match v.route {
                Route::Group { build, .. } => Some(build),
                Route::Direct(_) => None,
            })
            .collect();
        let current = self.current;
        self.builds.retain(|id, b| *id == current || in_use.contains(id) || !b.is_quiet());
        self.collect_native_sounds();
        self.master_order.retain(|input| match input {
            MasterInput::Build(id) => self.builds.contains_key(id),
            MasterInput::Direct { voice, .. } => self.voices.contains_key(voice),
        });
    }

    /// Whether `voice` is still sounding.
    pub fn is_playing(&self, voice: Voice) -> bool {
        self.voices.contains_key(&voice)
    }

    pub fn engine_playing(&self) -> bool {
        self.engine.is_playing()
    }

    /// Play `engine` as the engine vibration from now on (the built-in one by default); it is
    /// switched to the mixer's rate.
    pub fn set_engine(&mut self, mut engine: EngineVoice) {
        engine.set_sample_rate(self.rate);
        self.engine = engine;
        self.closed_engine = None;
        self.engine_status.0.store(self.engine.is_fmod(), Ordering::SeqCst);
    }

    pub fn engine(&self) -> &EngineVoice {
        &self.engine
    }

    /// Fill `out` (interleaved, six channels in [`Speaker::ALL`] order) with the next frames.
    /// Levels are not clamped.
    pub fn render(&mut self, out: &mut [f32]) {
        self.render_layout(out, &SPEAKER_ORDER);
    }

    /// Fill `out` with the next frames, interleaved with one channel per entry of `layout`
    /// (a speaker bus, or silence for `None`). Levels are not clamped.
    pub fn render_layout(&mut self, out: &mut [f32], layout: &[Option<Speaker>]) {
        if layout.is_empty() {
            return;
        }
        for frame in out.chunks_exact_mut(layout.len()) {
            let mixed = self.next_frame();
            for (o, speaker) in frame.iter_mut().zip(layout) {
                *o = speaker.map_or(0.0, |s| mixed[s.index()] as f32);
            }
        }
        if self.builds.len() > 1 {
            self.drop_finished_builds();
        }
    }

    /// Convert the completed software mix into a physical device's speaker format.
    /// Raw speaker-bus callers continue to use [`Self::render`] or [`Self::render_layout`].
    pub fn render_output(&mut self, out: &mut [f32], matrix: &OutputMatrix) {
        if matrix.is_empty() {
            return;
        }
        for frame in out.chunks_exact_mut(matrix.len()) {
            matrix.write(&self.next_frame(), frame);
        }
        if self.builds.len() > 1 {
            self.drop_finished_builds();
        }
    }

    fn next_frame(&mut self) -> PerSpeaker {
        let mut out = [0.0f64; CHANNELS];
        let builds = &mut self.builds;
        let voices = &mut self.voices;
        let master_order = &mut self.master_order;
        for input in &mut *master_order {
            if let MasterInput::Direct { frame, .. } = input {
                *frame = None;
            }
        }
        self.voice_order.retain(|voice| {
            let Some(v) = voices.get_mut(voice) else { return false };
            let Some(s) = v.next() else {
                voices.remove(voice);
                return false;
            };
            match v.route {
                Route::Group { build, group } => {
                    if let Some(b) = builds.get_mut(&build) {
                        b.add(group, s);
                    }
                }
                Route::Direct(l) => {
                    for input in &mut *master_order {
                        if let MasterInput::Direct { voice: slot, frame } = input {
                            if slot == voice {
                                *frame = Some(std::array::from_fn(|k| s[k] as f32 * l[k] as f32));
                                break;
                            }
                        }
                    }
                }
            }
            true
        });
        master_order.retain(|input| match input {
            MasterInput::Build(id) => {
                if let Some(build) = builds.get_mut(id) {
                    build.mix_into(&mut out);
                    true
                } else {
                    false
                }
            }
            MasterInput::Direct { frame, .. } => {
                if let Some(frame) = frame {
                    for (bus, sample) in out.iter_mut().zip(frame) {
                        *bus = f64::from(*bus as f32 + sample);
                    }
                    true
                } else {
                    false
                }
            }
        });
        self.engine.render_into(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shakers::layout::Speaker;
    use crate::shakers::profiles::SpeakerProfile;
    use crate::shakers::samples::SampleFolder;
    use crate::shakers::tuning::ShakerTuning;

    fn mixer() -> Mixer {
        let profile = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        let levels = GroupLevels::compute(&profile, &ShakerTuning::default(), None);
        Mixer::new(44_100, SampleBank::synthesized(), levels)
    }

    fn start(voice: Voice, n: u8, plays: Plays, group: Group) -> Action {
        Action::Start { voice, sample: SampleKey::new(SampleFolder::Suspension, n), plays, group }
    }

    fn render(m: &mut Mixer, frames: usize) -> Vec<f32> {
        let mut buf = vec![0.0; frames * CHANNELS];
        m.render(&mut buf);
        buf
    }

    fn enabled_filter(parameters: LowPass) -> FilterSettings {
        FilterSettings { low_pass: true, parameters }
    }

    #[test]
    fn every_live_sound_key_has_a_builtin_fallback() {
        let bank = SampleBank::synthesized();
        for key in SampleKey::PLAYED {
            assert!(bank.get(key).is_some(), "{}", key.relative_path());
        }
    }

    #[test]
    fn source_sound_creation_survives_its_bank_being_replaced_before_play() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("transmission")).unwrap();
        let mut writer = hound::WavWriter::create(
            directory.path().join("transmission/1.ogg"),
            hound::WavSpec {
                channels: 1,
                sample_rate: 44100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..16 {
            writer.write_sample(12345_i16).unwrap();
        }
        writer.finalize().unwrap();
        let mut actual = mixer();
        actual.set_bank(SampleBank::import(directory.path()).unwrap().0);
        let key = SampleKey::new(SampleFolder::Transmission, 1);
        actual.native_create(Voice::Transmission, key, false);
        actual.set_bank(SampleBank::synthesized());
        actual.native_play(Voice::Transmission, Group::Gear).unwrap();
        let playing = &actual.voices[&Voice::Transmission];
        assert_eq!(playing.origin, SampleOrigin::Imported);
        assert_eq!(playing.sample.channels, 1);
        assert_eq!(playing.sample.frames(), 16);
        assert!(playing.mono.is_some());
    }

    #[test]
    fn native_sound_captures_import_format_but_resolves_group_when_played() {
        let mut actual = mixer();
        let key = SampleKey::new(SampleFolder::Transmission, 1);
        actual.native_create(Voice::Transmission, key, false);
        let handle = actual.native_slot(Voice::Transmission).unwrap();
        let sample = Arc::clone(&handle.sample);
        let mut replacement = SampleBank::synthesized();
        actual.set_bank(replacement.clone());
        actual.set_levels(actual.builds[&actual.current].levels.clone());
        let build = actual.current;
        actual.native_play(Voice::Transmission, Group::Gear).unwrap();
        let playing = &actual.voices[&Voice::Transmission];
        assert!(Arc::ptr_eq(&playing.sample, &sample));
        assert_eq!(playing.route, Route::Group { build, group: Group::Gear });
        replacement = SampleBank::synthesized();
        actual.set_bank(replacement);
        assert!(Arc::ptr_eq(&actual.voices[&Voice::Transmission].sample, &sample));
    }

    #[test]
    fn orphaned_sound_channels_end_but_unreleased_sound_retains_native_lifetime() {
        let mut actual = mixer();
        let key = SampleKey::new(SampleFolder::Transmission, 1);
        actual.native_create(Voice::Transmission, key, false);
        let old = actual.native_slot(Voice::Transmission).unwrap();
        let weak = Arc::downgrade(&old);
        actual.native_play(Voice::Transmission, Group::Gear).unwrap();
        drop(old);
        actual.native_create(Voice::Transmission, key, false);
        actual.native_play(Voice::Transmission, Group::Gear).unwrap();
        assert!(weak.upgrade().is_some());
        render(&mut actual, 5000);
        actual.collect_native_sounds();
        assert!(weak.upgrade().is_some(), "pyfmodex dropping its object never invokes Sound.release");
        assert!(actual.native_channels.is_empty());
        actual.close_output(44100);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn overwritten_source_sound_keeps_old_channels_until_its_own_release() {
        let mut actual = mixer();
        let key = SampleKey::new(SampleFolder::Transmission, 1);
        actual.native_create(Voice::Transmission, key, false);
        let first = actual.native_slot(Voice::Transmission).unwrap();
        actual.native_play(Voice::Transmission, Group::Gear).unwrap();
        actual.native_create(Voice::Transmission, key, false);
        let second = actual.native_slot(Voice::Transmission).unwrap();
        actual.native_play(Voice::Transmission, Group::Gear).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(actual.voices.len(), 2);
        actual.native_release(&second).unwrap();
        assert_eq!(actual.voices.len(), 1, "release current Sound must not stop escaped old Sound");
        actual.native_release(&first).unwrap();
        assert!(actual.voices.is_empty());
        assert!(actual.native_release(&first).is_err());
    }
    #[test]
    fn repeated_play_of_current_sound_is_independent_but_release_stops_all_instances() {
        let mut actual = mixer();
        let key = SampleKey::new(SampleFolder::Transmission, 1);
        actual.native_create(Voice::Transmission, key, false);
        let id = actual.native_slot(Voice::Transmission).unwrap();
        actual.native_play(Voice::Transmission, Group::Gear).unwrap();
        actual.native_play(Voice::Transmission, Group::Gear).unwrap();
        assert_eq!(actual.voices.len(), 2);
        actual.native_release(&id).unwrap();
        assert!(actual.voices.is_empty());
        actual.native_clear(Voice::Transmission);
        assert!(actual.native_set_loops(Voice::Transmission, Plays::Forever).is_err());
    }

    #[test]
    fn native_dsp_lookup_failure_detaches_only_current_group_and_recovers() {
        for failed_stage in 0..3 {
            let mut m = mixer();
            let old = LowPass { cutoff_hz: 400.0, q: 2.0 };
            m.apply(&[
                Action::Filters(Box::new([enabled_filter(old); GROUP_COUNT])),
                start(Voice::WheelSlipLf, 3, Plays::Forever, Group::WheelSlipLf),
            ])
            .unwrap();
            let mut requested =
                Group::ALL.map(|_| NativeDspSettings { filter_type: Ok(1), cutoff_hz: Ok(120.0), q: Ok(3.0) });
            requested[Group::Gear.index()].filter_type = Ok(13);
            let error = DspError { class: "KeyError", message: "'gear'".to_owned() };
            let gear = &mut requested[Group::Gear.index()];
            match failed_stage {
                0 => gear.filter_type = Err(error),
                1 => gear.cutoff_hz = Err(error),
                _ => gear.q = Err(error),
            }
            let error = m
                .apply(&[
                    Action::NativeFilters(Box::new(requested.clone())),
                    start(Voice::Impact, 6, Plays::Once, Group::Impact),
                ])
                .unwrap_err();
            assert!(matches!(error, EngineError::DspParameter { group: Group::Gear, parameter, error }
                if parameter == ["A_FILTER", "A_FREQUENCY", "A_Q"][failed_stage] && error.class == "KeyError"));
            let build = &m.builds[&m.current];
            assert_eq!(build.filters[Group::Engine.index()], Some(LowPass { cutoff_hz: 120.0, q: 3.0 }));
            assert_eq!(build.filters[Group::Gear.index()], None);
            assert!(build.attached[Group::Engine.index()]);
            assert!(!build.attached[Group::Gear.index()]);
            assert!(build.attached[Group::Afterburner.index()]);
            assert_eq!(build.filter_types[Group::Gear.index()], if failed_stage == 0 { 1 } else { 12 });
            assert_eq!(
                build.parameters[Group::Gear.index()],
                LowPass { cutoff_hz: if failed_stage == 2 { 120.0 } else { 400.0 }, q: 2.0 }
            );
            assert_eq!(build.filters[Group::Afterburner.index()], Some(old), "later groups were never removed");
            assert!(m.is_playing(Voice::WheelSlipLf), "early DSP failure does not stop held playback");
            assert!(!m.is_playing(Voice::Impact), "later actions were skipped");
            requested[Group::Gear.index()] = NativeDspSettings { filter_type: Ok(1), cutoff_hz: Ok(240.0), q: Ok(4.0) };
            m.apply(&[Action::NativeFilters(Box::new(requested))]).unwrap();
            assert_eq!(m.builds[&m.current].filters[Group::Gear.index()], Some(LowPass { cutoff_hz: 240.0, q: 4.0 }));
        }
    }

    #[test]
    fn native_dsp_int_clamps_after_ctypes_and_float_parameters_round_before_clamp() {
        for (value, expected) in [(i32::MIN, 0), (-1, 0), (0, 0), (1, 1), (12, 12), (13, 12), (i32::MAX, 12)] {
            let mut m = mixer();
            let requested = Group::ALL.map(|_| NativeDspSettings {
                filter_type: Ok(value),
                cutoff_hz: Ok(123.456789),
                q: Ok(1.23456789),
            });
            m.apply(&[Action::NativeFilters(Box::new(requested))]).unwrap();
            let build = &m.builds[&m.current];
            assert!(build.filter_types.iter().all(|kind| *kind == expected));
            assert!(build.attached.iter().all(|attached| *attached));
            assert!(build
                .parameters
                .iter()
                .all(|p| *p == LowPass { cutoff_hz: f64::from(123.456_79_f32), q: f64::from(1.234_567_9_f32) }));
        }
    }

    #[test]
    fn partial_dsp_calls_preserve_each_current_build_and_successful_parameter_prefix() {
        let group = Group::Gear;
        let g = group.index();
        let mut m = mixer();
        let apply = |m: &mut Mixer, update| m.apply(&[Action::NativeDspStep { group, update }]).unwrap();
        apply(&mut m, DspUpdate::Remove);
        apply(&mut m, DspUpdate::Type(Ok(3)));
        apply(&mut m, DspUpdate::Frequency(Ok(123.456789)));
        assert_eq!(m.builds[&m.current].filter_types[g], 3);
        assert_eq!(m.builds[&m.current].parameters[g].cutoff_hz, f64::from(123.456_79_f32));
        assert!(!m.builds[&m.current].attached[g]);
        let old = m.current;
        m.set_levels(m.builds[&old].levels.clone());
        apply(&mut m, DspUpdate::Q(Ok(2.5)));
        apply(&mut m, DspUpdate::Attach);
        assert!(!m.builds.get(&old).is_some_and(|build| build.attached[g]));
        let current = &m.builds[&m.current];
        assert!(current.attached[g]);
        assert_eq!(current.filter_types[g], 1, "new DSP keeps its own type default");
        assert_eq!(current.parameters[g].q, 2.5);
        assert_eq!(current.parameters[g].cutoff_hz, LowPass::default().cutoff_hz);
        let failure = m.apply(&[Action::NativeDspStep {
            group,
            update: DspUpdate::Frequency(Err(DspError { class: "ValueError", message: "bad".into() })),
        }]);
        assert!(failure.is_err());
        assert!(m.builds[&m.current].attached[g], "parameter failure alone never detaches DSP");
        assert_eq!(m.builds[&m.current].parameters[g].q, 2.5);
    }

    #[test]
    fn overlapping_dsp_updates_apply_late_parameters_to_an_already_reattached_filter() {
        let mut m = mixer();
        let group = Group::Gear;
        let g = group.index();
        let apply = |m: &mut Mixer, update| m.apply(&[Action::NativeDspStep { group, update }]).unwrap();
        apply(&mut m, DspUpdate::Remove);
        apply(&mut m, DspUpdate::Type(Ok(3)));
        // A second frame reaches the same DSP, completes all its setters, and attaches it.
        m.apply(&[Action::NativeGroupFilter {
            group,
            settings: NativeDspSettings { filter_type: Ok(1), cutoff_hz: Ok(444.0), q: Ok(1.5) },
        }])
        .unwrap();
        apply(&mut m, DspUpdate::Frequency(Ok(222.0)));
        assert_eq!(m.builds[&m.current].filters[g], Some(LowPass { cutoff_hz: 222.0, q: 1.5 }));
        apply(&mut m, DspUpdate::Q(Ok(2.5)));
        assert_eq!(m.builds[&m.current].filters[g], Some(LowPass { cutoff_hz: 222.0, q: 2.5 }));
        assert_eq!(m.builds[&m.current].filter_types[g], 1);
        apply(&mut m, DspUpdate::Attach);
        assert_eq!(m.builds[&m.current].filters[g], Some(LowPass { cutoff_hz: 222.0, q: 2.5 }));
    }

    #[test]
    fn dsp_nonfinite_filter_error_keeps_the_successful_setter_prefix() {
        for cutoff in [true, false] {
            for enabled in [true, false] {
                let mut m = mixer();
                let old = LowPass { cutoff_hz: 400.0, q: 2.0 };
                m.apply(&[Action::Filters(Box::new([enabled_filter(old); GROUP_COUNT]))]).unwrap();
                let mut requested = [enabled_filter(old); GROUP_COUNT];
                requested[Group::Engine.index()] = enabled_filter(LowPass { cutoff_hz: 120.0, q: 3.0 });
                requested[Group::Gear.index()] = FilterSettings {
                    low_pass: enabled,
                    parameters: LowPass { cutoff_hz: if cutoff { f64::NAN } else { 240.0 }, q: f64::NAN },
                };
                requested[Group::Afterburner.index()].low_pass = false;
                let error = m
                    .apply(&[Action::Filters(Box::new(requested)), start(Voice::Impact, 6, Plays::Once, Group::Impact)])
                    .unwrap_err();
                assert_eq!(error.to_string(), "INVALID FLOAT");
                assert!(matches!(error, EngineError::InvalidDspFloat { group: Group::Gear, parameter }
                if parameter == if cutoff { "A_FREQUENCY" } else { "A_Q" }));
                let build = &m.builds[&m.current];
                assert_eq!(build.filters[Group::Engine.index()], Some(requested[Group::Engine.index()].parameters));
                let retained = LowPass { cutoff_hz: if cutoff { 400.0 } else { 240.0 }, q: 2.0 };
                assert_eq!(build.filters[Group::Gear.index()], None, "failed group stays detached");
                assert_eq!(build.filter_types[Group::Gear.index()], i32::from(enabled));
                assert_eq!(build.parameters[Group::Gear.index()], retained);
                assert_eq!(build.filters[Group::Afterburner.index()], Some(old), "later groups stay unchanged");
                assert!(!m.is_playing(Voice::Impact), "later playback does not run");
            }
        }
    }

    #[test]
    fn dsp_nonfinite_first_frame_retains_descriptor_defaults_before_failed_calls() {
        for cutoff in [true, false] {
            let mut m = mixer();
            let mut requested = [FilterSettings::default(); GROUP_COUNT];
            requested[Group::Engine.index()] =
                enabled_filter(LowPass { cutoff_hz: if cutoff { f64::INFINITY } else { 120.0 }, q: f64::INFINITY });
            assert!(m.apply(&[Action::Filters(Box::new(requested))]).is_err());
            let build = &m.builds[&m.current];
            assert_eq!(
                build.parameters[Group::Engine.index()],
                LowPass { cutoff_hz: if cutoff { 8_000.0 } else { 120.0 }, ..LowPass::default() }
            );
            assert_eq!(build.filters[Group::Engine.index()], None, "add never follows the failed setter");
            assert_eq!(build.filters[Group::Gear.index()], None, "later groups were not attached");
        }
    }

    #[test]
    fn dsp_finite_float32_extremes_clamp_after_validation() {
        for (value, expected) in [
            (f64::from(f32::MAX), LowPass { cutoff_hz: 22_000.0, q: 10.0 }),
            (-f64::from(f32::MAX), LowPass { cutoff_hz: 20.0, q: f64::from(0.1_f32) }),
        ] {
            let mut m = mixer();
            let requested = enabled_filter(LowPass { cutoff_hz: value, q: value });
            m.apply(&[Action::Filters(Box::new([requested; GROUP_COUNT]))]).unwrap();
            let build = &m.builds[&m.current];
            assert!(build.parameters.iter().all(|parameters| *parameters == expected));
            assert!(build.filters.iter().all(|filter| *filter == Some(expected)));
        }
    }

    #[test]
    fn group_levels_route_speakers() {
        let mut m = mixer();
        // suspension_lf on '6 Speakers - 5.1' is FL only.
        m.apply(&[start(Voice::SuspensionLf, 6, Plays::Forever, Group::SuspensionLf)]).unwrap();
        let buf = render(&mut m, 2000);
        let peak = |k: usize| buf.chunks(6).map(|f| f[k].abs()).fold(0.0f32, f32::max);
        assert!(peak(Speaker::FrontLeft.index()) > 0.3);
        assert_eq!(peak(Speaker::FrontRight.index()), 0.0);
        assert_eq!(peak(Speaker::Sub.index()), 0.0);
    }

    #[test]
    fn native_sample_mix_rounds_the_output_gain_before_multiplication() {
        let profile = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        let mut tuning = ShakerTuning::default();
        tuning.volume[Speaker::FrontLeft.index()] = 7.0;
        let levels = GroupLevels::compute(&profile, &tuning, None);
        let gain = levels.of(Group::SuspensionLf)[Speaker::FrontLeft.index()];
        assert_eq!(gain, 0.7);
        let bank = SampleBank::synthesized();
        let sample = bank.get(SampleKey::new(SampleFolder::Suspension, 6)).unwrap();
        let mut mixer = Mixer::new(sample.rate, bank, levels);
        mixer.apply(&[start(Voice::SuspensionLf, 6, Plays::Forever, Group::SuspensionLf)]).unwrap();
        for (index, frame) in render(&mut mixer, 512).as_chunks::<CHANNELS>().0.iter().enumerate() {
            let expected = sample.data[index] * gain as f32;
            assert_eq!(frame[0].to_bits(), expected.to_bits(), "source frame {index}");
            assert_eq!(&frame[1..], &[0.0; 5]);
        }
    }

    #[test]
    fn native_sample_mix_sums_group_inputs_before_the_float32_gain() {
        let mut tuning = ShakerTuning::default();
        tuning.volume[Speaker::FrontLeft.index()] = 7.0;
        let profile = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        let levels = GroupLevels::compute(&profile, &tuning, None);
        let gain = levels.of(Group::SuspensionLf)[0] as f32;
        let bank = SampleBank::synthesized();
        let a = bank.get(SampleKey::new(SampleFolder::Suspension, 5)).unwrap();
        let b = bank.get(SampleKey::new(SampleFolder::Suspension, 6)).unwrap();
        assert_eq!(a.rate, b.rate);
        let mut mixer = Mixer::new(a.rate, bank, levels);
        mixer
            .apply(&[
                start(Voice::SuspensionLf, 5, Plays::Forever, Group::SuspensionLf),
                start(Voice::SuspensionRf, 6, Plays::Forever, Group::SuspensionLf),
            ])
            .unwrap();
        for (index, frame) in render(&mut mixer, 512).as_chunks::<CHANNELS>().0.iter().enumerate() {
            let expected = (a.data[index] + b.data[index]) * gain;
            assert_eq!(frame[0].to_bits(), expected.to_bits(), "source frame {index}");
        }
    }

    #[test]
    fn native_sample_mix_direct_route_uses_float32_gain_and_sum() {
        let mut mixer = mixer();
        let key = SampleKey::new(SampleFolder::Transmission, 1);
        let sample = mixer.bank.get(key).unwrap();
        mixer.play_direct(Voice::SpeakerTest, key, [0.7, 0.0, 0.0, 0.0, 0.0, 0.0]);
        for (index, frame) in render(&mut mixer, 512).as_chunks::<CHANNELS>().0.iter().enumerate() {
            assert_eq!(frame[0].to_bits(), (sample.data[index] * 0.7_f32).to_bits(), "source frame {index}");
            assert_eq!(&frame[1..], &[0.0; 5]);
        }
    }

    #[test]
    fn native_sample_mix_keeps_surround_channels_and_raw_level_metadata() {
        let sample = super::super::samples::decode(include_bytes!("testdata/layout_51.ogg"), "layout").unwrap();
        let mut profile = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        profile.levels[Group::Gear.index()] = [7.0, 3.0, 11.0, 13.0, 17.0, -9.0];
        let levels = GroupLevels::compute(&profile, &ShakerTuning::default(), None);
        let raw = levels.of(Group::Gear);
        let mut build = GroupBuild::new(levels);
        for index in 0..512 {
            build.add(Group::Gear, speaker_frame(&sample, index));
            let mut output = [0.0; CHANNELS];
            build.mix_into(&mut output);
            for k in 0..CHANNELS {
                let expected = sample.data[index * CHANNELS + k] * raw[k] as f32;
                assert_eq!(output[k], f64::from(expected), "frame {index}, speaker {k}");
            }
        }
        assert_eq!(build.levels.of(Group::Gear), raw, "API/profile metadata retains the native Python floats");
    }

    #[test]
    fn native_sample_mix_rounds_separate_groups_before_master_sum() {
        let mut profile = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        profile.levels[Group::Gear.index()] = [7.0; CHANNELS];
        profile.levels[Group::Canopy.index()] = [3.0; CHANNELS];
        let levels = GroupLevels::compute(&profile, &ShakerTuning::default(), None);
        let gear = levels.of(Group::Gear).map(|level| level as f32);
        let canopy = levels.of(Group::Canopy).map(|level| level as f32);
        let mut build = GroupBuild::new(levels);
        for (a, b) in [(0.13_f32, 0.37_f32), (-0.71, 0.23), (0.91, -0.43)] {
            build.add(Group::Gear, [f64::from(a); CHANNELS]);
            build.add(Group::Canopy, [f64::from(b); CHANNELS]);
            let mut output = [0.0; CHANNELS];
            build.mix_into(&mut output);
            assert_eq!(output, std::array::from_fn(|k| f64::from(a * gear[k] + b * canopy[k])));
        }
    }

    #[test]
    fn native_sample_mix_head_filter_receives_the_rounded_matrix_output() {
        let mut tuning = ShakerTuning::default();
        tuning.volume[0] = 7.0;
        let profile = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        let levels = GroupLevels::compute(&profile, &tuning, None);
        let gain = levels.of(Group::SuspensionLf)[0] as f32;
        let mut build = GroupBuild::new(levels);
        let mut filters = [FilterSettings::default(); GROUP_COUNT];
        filters[Group::SuspensionLf.index()] = enabled_filter(LowPass { cutoff_hz: 12_000.0, q: 0.5 });
        build.set_filters(&filters, 48_000.0).unwrap();
        let mut reference = Multiband::default();
        reference.tune(1, 12_000.0, 0.5, 48_000.0);
        reference.begin_block(DSP_BLOCK_FRAMES);
        for (a, b) in [(0.13_f32, 0.37_f32), (-0.71, 0.23), (0.91, -0.43)] {
            build.add(Group::SuspensionLf, [f64::from(a); CHANNELS]);
            build.add(Group::SuspensionLf, [f64::from(b); CHANNELS]);
            let mut output = [0.0; CHANNELS];
            build.mix_into(&mut output);
            let expected = reference.process(f64::from((a + b) * gain));
            assert_eq!(output[0], expected);
            assert_eq!(&output[1..], &[0.0; 5]);
        }
    }

    #[test]
    fn loop_counts() {
        let mut m = mixer();
        // 0.3 s sample played 3 times = 39690 frames at 44.1 kHz.
        m.apply(&[start(Voice::Impact, 6, Plays::Repeat(2), Group::Impact)]).unwrap();
        render(&mut m, 39_000);
        assert!(m.is_playing(Voice::Impact));
        render(&mut m, 1_000);
        assert!(!m.is_playing(Voice::Impact));
        m.apply(&[start(Voice::Brake, 6, Plays::Once, Group::Brake)]).unwrap();
        render(&mut m, 13_300);
        assert!(!m.is_playing(Voice::Brake));
        m.apply(&[start(Voice::RoadBump, 6, Plays::Forever, Group::RoadBump)]).unwrap();
        render(&mut m, 100_000);
        assert!(m.is_playing(Voice::RoadBump));
        m.apply(&[Action::Stop(Voice::RoadBump)]).unwrap();
        assert!(!m.is_playing(Voice::RoadBump));
    }

    #[test]
    fn low_pass_attenuates_highs() {
        let mut plain = mixer();
        let mut filtered = mixer();
        let mut f = [FilterSettings::default(); GROUP_COUNT];
        f[Group::SuspensionLf.index()] = enabled_filter(LowPass { cutoff_hz: 30.0, q: 0.707 });
        filtered.apply(&[Action::Filters(Box::new(f))]).unwrap();
        // suspension/5 is 130 Hz: well above a 30 Hz cutoff.
        for m in [&mut plain, &mut filtered] {
            m.apply(&[start(Voice::SuspensionLf, 5, Plays::Forever, Group::SuspensionLf)]).unwrap();
        }
        let energy = |b: Vec<f32>| b.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
        let e_plain = energy(render(&mut plain, 20_000));
        let e_filtered = energy(render(&mut filtered, 20_000));
        assert!(e_filtered < e_plain * 0.2);
    }

    #[test]
    fn native_filter_type_one_uses_the_same_source_coefficients_as_other_single_sections() {
        let mut build = GroupBuild::new(mixer().builds.remove(&0).unwrap().levels);
        let group = Group::SuspensionLf.index();
        let mut requested =
            [FilterSettings { low_pass: false, parameters: LowPass { cutoff_hz: 12_000.0, q: 0.5 } }; GROUP_COUNT];
        requested[group].low_pass = true;
        build.set_filters(&requested, 48_000.0).unwrap();
        // The sine polynomial at quarter-rate, Q 1/2, gives b0 one ULP above
        // the ideal 1/4; that float32 coefficient is used through rendering.
        build.bus[group][0] = 1.0;
        build.used[group] = true;
        let mut output = [0.0; CHANNELS];
        build.mix_into(&mut output);
        assert_eq!((output[0] as f32).to_bits(), 0x3e80_0001);
    }

    #[test]
    fn native_filter_changes_wait_for_the_next_audio_block_and_survive_bypass() {
        let mut m = mixer();
        let group = Group::SuspensionLf.index();
        let request = |kind| {
            let mut filters =
                Group::ALL.map(|_| NativeDspSettings { filter_type: Ok(0), cutoff_hz: Ok(12_000.0), q: Ok(0.5) });
            filters[group].filter_type = Ok(kind);
            Action::NativeFilters(Box::new(filters))
        };
        m.set_sample_rate(48_000);
        m.apply(&[request(1)]).unwrap();
        let tick = |m: &mut Mixer, value: f64| {
            let build = m.builds.get_mut(&m.current).unwrap();
            build.bus[group][0] = value;
            build.used[group] = true;
            let mut output = [0.0; CHANNELS];
            build.mix_into(&mut output);
            output[0]
        };
        assert_eq!((tick(&mut m, 1.0) as f32).to_bits(), 0x3e80_0001);
        m.apply(&[request(0)]).unwrap();
        assert!((tick(&mut m, 0.0) - 0.5).abs() < 1e-6, "the current processed block keeps its attached filter");
        for _ in 2..DSP_BLOCK_FRAMES {
            tick(&mut m, 0.0);
        }
        assert_eq!(tick(&mut m, 1.0), 1.0);
        m.apply(&[request(4)]).unwrap();
        assert_eq!(tick(&mut m, 1.0), 1.0, "the disabled block stays bypassed until its boundary");
        for _ in 2..DSP_BLOCK_FRAMES {
            tick(&mut m, 0.0);
        }
        // The old low-pass coefficient is used at n=0 of the newly enabled block.
        assert_eq!((tick(&mut m, 1.0) as f32).to_bits(), 0x3e80_0001);
        assert!(m.builds[&m.current].rendering[group]);
    }

    #[test]
    fn native_filter_transition_is_independent_of_device_callback_buffer_sizes() {
        fn configured() -> Mixer {
            let mut mixer = mixer();
            mixer.set_sample_rate(48_000);
            let filters =
                Group::ALL.map(|_| NativeDspSettings { filter_type: Ok(1), cutoff_hz: Ok(500.0), q: Ok(0.707) });
            mixer
                .apply(&[
                    Action::NativeFilters(Box::new(filters)),
                    start(Voice::Transmission, 1, Plays::Forever, Group::Gear),
                ])
                .unwrap();
            mixer
        }
        let mut whole = configured();
        let mut split = configured();
        assert_eq!(render(&mut whole, 1300), render(&mut split, 1300));
        let filters = Group::ALL.map(|_| NativeDspSettings { filter_type: Ok(4), cutoff_hz: Ok(180.0), q: Ok(0.5) });
        for mixer in [&mut whole, &mut split] {
            mixer.apply(&[Action::NativeFilters(Box::new(filters.clone()))]).unwrap();
        }
        let reference = render(&mut whole, 3000);
        let mut actual = Vec::new();
        for frames in [17, 751, 1, 1023, 101, 1107] {
            actual.extend(render(&mut split, frames));
        }
        assert_eq!(actual, reference);
    }

    #[test]
    fn native_new_channel_build_joins_the_current_processing_block() {
        let mut mixer = mixer();
        render(&mut mixer, 123);
        let old = mixer.current;
        let levels = mixer.builds[&old].levels.clone();
        mixer.set_levels(levels);
        assert_eq!(mixer.builds[&mixer.current].block_position, 123);
        render(&mut mixer, DSP_BLOCK_FRAMES - 123);
        assert_eq!(mixer.builds[&mixer.current].block_position, 0);
    }

    #[test]
    fn native_silent_filter_waits_for_input_before_initializing_coefficients() {
        let mut mixer = mixer();
        let group = Group::Gear.index();
        let filters = |cutoff_hz| {
            Box::new(Group::ALL.map(|_| NativeDspSettings {
                filter_type: Ok(1),
                cutoff_hz: Ok(cutoff_hz),
                q: Ok(0.707),
            }))
        };
        mixer.apply(&[Action::NativeFilters(filters(400.0))]).unwrap();
        render(&mut mixer, 1500);
        assert!(!mixer.builds[&mixer.current].multiband_state[group][0].is_initialized());
        mixer
            .apply(&[Action::NativeFilters(filters(200.0)), start(Voice::Transmission, 1, Plays::Forever, Group::Gear)])
            .unwrap();
        render(&mut mixer, 1);
        assert!(mixer.builds[&mixer.current].multiband_state[group][0].is_initialized());
    }

    #[test]
    fn native_input_idle_stop_and_resume_flows_through_actual_mixer_graph() {
        for kind in 1..=12 {
            let mut mixer = Mixer::new(48000, SampleBank::synthesized(), unity_levels());
            let filters =
                Group::ALL.map(|_| NativeDspSettings { filter_type: Ok(kind), cutoff_hz: Ok(80.0), q: Ok(10.0) });
            mixer
                .apply(&[
                    Action::NativeFilters(Box::new(filters)),
                    start(Voice::Impact, 6, Plays::Forever, Group::Impact),
                ])
                .unwrap();
            pin_sample(&mut mixer, Voice::Impact, 0.25);
            render(&mut mixer, DSP_BLOCK_FRAMES);
            let mut reference = mixer.builds[&mixer.current].multiband_state[Group::Impact.index()][0];
            mixer.apply(&[Action::Stop(Voice::Impact)]).unwrap();
            assert_eq!(
                render(&mut mixer, 2 * DSP_BLOCK_FRAMES),
                vec![0.0; 2 * DSP_BLOCK_FRAMES * CHANNELS],
                "kind {kind}"
            );
            mixer.apply(&[start(Voice::Impact, 6, Plays::Forever, Group::Impact)]).unwrap();
            pin_sample(&mut mixer, Voice::Impact, 0.25);
            reference.begin_block(DSP_BLOCK_FRAMES);
            for frame in render(&mut mixer, 32).as_chunks::<CHANNELS>().0 {
                assert_eq!(*frame, [reference.process(0.25) as f32; CHANNELS], "kind {kind}");
            }
        }
    }

    #[test]
    fn native_idle_block_sequence_matches_independent_c_query_and_ramp_reference() {
        let mut build = GroupBuild::new(unity_levels());
        let group = Group::Gear.index();
        let request =
            |kind| Group::ALL.map(|_| NativeDspSettings { filter_type: Ok(kind), cutoff_hz: Ok(12000.0), q: Ok(0.5) });
        build.set_native_filters(&request(1), 48000.0).unwrap();
        let mut hash = 0xcbf29ce484222325u64;
        let expected =
            [0x74e1b132d407074f, 0xae9cd80857cac74f, 0x6ffa3f4acb8e874f, 0x24ca6f8dda5295e9, 0x96da04cb1e2b7101];
        for (block, expected) in expected.into_iter().enumerate() {
            if block == 1 {
                build.set_native_filters(&request(4), 48000.0).unwrap();
            }
            for _ in 0..DSP_BLOCK_FRAMES {
                if block == 0 || block == 3 || block == 4 {
                    build.add(Group::Gear, [if block == 3 { 0.0 } else { 0.25 }; CHANNELS]);
                }
                let mut out = [0.0; CHANNELS];
                build.mix_into(&mut out);
                for byte in (out[0] as f32).to_le_bytes() {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                }
            }
            assert_eq!(hash, expected, "block {block} group {group}");
        }
    }

    #[test]
    fn native_input_idle_query_freezes_tail_and_pending_filter_until_sound_resumes() {
        let mut build = GroupBuild::new(mixer().builds.remove(&0).unwrap().levels);
        let group = Group::Gear.index();
        let request = |frequency| {
            [FilterSettings { low_pass: true, parameters: LowPass { cutoff_hz: frequency, q: 10.0 } }; GROUP_COUNT]
        };
        build.set_filters(&request(80.0), 48000.0).unwrap();
        for frame in 0..DSP_BLOCK_FRAMES {
            build.add(Group::Gear, [if frame == 0 { 1.0 } else { 0.0 }; CHANNELS]);
            build.mix_into(&mut [0.0; CHANNELS]);
        }
        let mut held = build.multiband_state[group][0];
        assert!(!held.is_quiet(), "reference needs a nonzero retained delay");
        let old_coefficients = held.coefficients_for_test();
        build.set_filters(&request(1000.0), 48000.0).unwrap();
        for frame in 0..2 * DSP_BLOCK_FRAMES {
            let mut out = [0.0; CHANNELS];
            build.mix_into(&mut out);
            assert_eq!(out, [0.0; CHANNELS], "idle query must bypass old delay at frame {frame}");
        }
        assert_eq!(build.multiband_state[group][0].coefficients_for_test(), old_coefficients);
        held.tune(1, 1000.0, 10.0, 48000.0);
        held.begin_block(DSP_BLOCK_FRAMES);
        for _ in 0..16 {
            build.add(Group::Gear, [0.0; CHANNELS]);
            let mut out = [0.0; CHANNELS];
            build.mix_into(&mut out);
            assert_eq!(out[0], held.process(0.0));
        }
    }

    #[test]
    fn native_input_idle_query_applies_to_all_filter_kinds_but_not_zero_valued_live_input() {
        for kind in 1..=12 {
            let mut build = GroupBuild::new(mixer().builds.remove(&0).unwrap().levels);
            let group = Group::Gear.index();
            let request = |frequency| {
                Group::ALL.map(|_| NativeDspSettings { filter_type: Ok(kind), cutoff_hz: Ok(frequency), q: Ok(2.0) })
            };
            build.set_native_filters(&request(300.0), 48000.0).unwrap();
            for frame in 0..DSP_BLOCK_FRAMES {
                build.add(Group::Gear, [if frame == 0 { 1.0 } else { 0.0 }; CHANNELS]);
                build.mix_into(&mut [0.0; CHANNELS]);
            }
            let mut reference = build.multiband_state[group][0];
            build.set_native_filters(&request(1000.0), 48000.0).unwrap();
            for _ in 0..DSP_BLOCK_FRAMES {
                let mut out = [0.0; CHANNELS];
                build.mix_into(&mut out);
                assert_eq!(out, [0.0; CHANNELS], "kind {kind}");
            }
            reference.tune(kind, 1000.0, 2.0, 48000.0);
            reference.begin_block(DSP_BLOCK_FRAMES);
            for _ in 0..32 {
                build.add(Group::Gear, [0.0; CHANNELS]);
                let mut out = [0.0; CHANNELS];
                build.mix_into(&mut out);
                assert_eq!(out[0], reference.process(0.0), "kind {kind}: live silence still processes");
            }
        }
    }

    #[test]
    fn native_input_idle_keeps_the_last_partial_block_and_drops_unreferenced_groups_after_it() {
        let mut mixer = mixer();
        mixer
            .apply(&[
                Action::Filters(Box::new([enabled_filter(LowPass { cutoff_hz: 80.0, q: 10.0 }); GROUP_COUNT])),
                start(Voice::Impact, 6, Plays::Forever, Group::Impact),
            ])
            .unwrap();
        render(&mut mixer, 317);
        mixer.apply(&[Action::Stop(Voice::Impact)]).unwrap();
        let old = mixer.current;
        mixer.set_levels(mixer.builds[&old].levels.clone());
        assert!(mixer.builds.contains_key(&old), "partly processed block retains old filter");
        render(&mut mixer, DSP_BLOCK_FRAMES - 317);
        assert!(!mixer.builds.contains_key(&old), "next input-idle block cannot ring");
        assert_eq!(render(&mut mixer, DSP_BLOCK_FRAMES), vec![0.0; DSP_BLOCK_FRAMES * CHANNELS]);
    }

    #[test]
    fn native_initialized_idle_filter_defers_midblock_request_until_the_next_boundary() {
        let mut build = GroupBuild::new(mixer().builds.remove(&0).unwrap().levels);
        let group = Group::Gear.index();
        let request = |frequency| {
            [FilterSettings { low_pass: true, parameters: LowPass { cutoff_hz: frequency, q: 0.5 } }; GROUP_COUNT]
        };
        let tick = |build: &mut GroupBuild, input: Option<f64>| {
            if let Some(input) = input {
                build.bus[group][0] = input;
                build.used[group] = true;
            }
            build.mix_into(&mut [0.0; CHANNELS]);
        };
        build.set_filters(&request(12_000.0), 48_000.0).unwrap();
        tick(&mut build, Some(1.0));
        for _ in 1..DSP_BLOCK_FRAMES {
            tick(&mut build, None);
        }
        let previous = build.multiband_state[group][0].coefficients_for_test();
        // This next block starts input-idle, before a pending setting and voice arrive.
        tick(&mut build, None);
        build.set_filters(&request(1000.0), 48_000.0).unwrap();
        tick(&mut build, Some(1.0));
        assert_eq!(build.multiband_state[group][0].coefficients_for_test(), previous);
        for _ in 2..DSP_BLOCK_FRAMES {
            tick(&mut build, Some(1.0));
        }
        assert_eq!(build.multiband_state[group][0].coefficients_for_test(), previous);
        tick(&mut build, Some(1.0));
        assert_ne!(build.multiband_state[group][0].coefficients_for_test(), previous);
    }

    #[test]
    fn native_disable_finishes_current_ramp_then_freezes_its_processed_state() {
        let mut build = GroupBuild::new(mixer().builds.remove(&0).unwrap().levels);
        let group = Group::Gear.index();
        let request = |enabled, frequency| {
            [FilterSettings { low_pass: enabled, parameters: LowPass { cutoff_hz: frequency, q: 0.5 } }; GROUP_COUNT]
        };
        let tick = |build: &mut GroupBuild| {
            build.bus[group][0] = 1.0;
            build.used[group] = true;
            build.mix_into(&mut [0.0; CHANNELS]);
        };
        build.set_filters(&request(true, 12_000.0), 48_000.0).unwrap();
        for _ in 0..DSP_BLOCK_FRAMES {
            tick(&mut build);
        }
        build.set_filters(&request(true, 1000.0), 48_000.0).unwrap();
        for _ in 0..17 {
            tick(&mut build);
        }
        let during = build.multiband_state[group][0].coefficients_for_test();
        build.set_filters(&request(false, 500.0), 48_000.0).unwrap();
        for _ in 17..DSP_BLOCK_FRAMES {
            tick(&mut build);
        }
        let completed = build.multiband_state[group][0].coefficients_for_test();
        assert_ne!(during, completed);
        for _ in 0..DSP_BLOCK_FRAMES {
            tick(&mut build);
        }
        assert_eq!(build.multiband_state[group][0].coefficients_for_test(), completed);
        build.set_filters(&request(true, 500.0), 48_000.0).unwrap();
        tick(&mut build);
        assert_ne!(build.multiband_state[group][0].coefficients_for_test(), completed);
    }

    #[test]
    fn native_multiband_filters_render_the_selected_profile_family() {
        let filtered = |kind, cutoff| {
            let mut m = mixer();
            let mut filters =
                Group::ALL.map(|_| NativeDspSettings { filter_type: Ok(0), cutoff_hz: Ok(130.0), q: Ok(0.707) });
            filters[Group::SuspensionLf.index()].filter_type = Ok(kind);
            filters[Group::SuspensionLf.index()].cutoff_hz = Ok(cutoff);
            m.apply(&[Action::NativeFilters(Box::new(filters))]).unwrap();
            let build = m.builds.get_mut(&m.current).unwrap();
            let mut output = Vec::new();
            for n in 0..12_288 {
                let input = (std::f64::consts::TAU * 130.0 * f64::from(n) / 44_100.0).sin();
                build.add(Group::SuspensionLf, [input; CHANNELS]);
                let mut frame = [0.0; CHANNELS];
                build.mix_into(&mut frame);
                if n >= 4_096 {
                    output.push(frame[0] as f32);
                }
            }
            output
        };
        let plain = filtered(0, 30.0);
        let energy = |signal: &[f32]| signal.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
        let reference = energy(&plain);
        for kind in [2, 3] {
            assert!(energy(&filtered(kind, 30.0)) < reference * 0.01, "low-pass type {kind} was bypassed");
        }
        for kind in [4, 5, 6] {
            assert!(energy(&filtered(kind, 1_000.0)) < reference * 0.01, "high-pass type {kind} was bypassed");
        }
        assert!(energy(&filtered(11, 130.0)) < reference * 0.01, "notch type 11 was bypassed");
        assert!(energy(&filtered(10, 1_000.0)) < reference * 0.05, "band-pass type 10 was bypassed");
        let allpass = filtered(12, 130.0);
        assert!((energy(&allpass) / reference - 1.0).abs() < 0.01);
        assert!(allpass.iter().zip(&plain).any(|(a, b)| (a - b).abs() > 0.1), "all-pass lost its phase response");
        for kind in [7, 8, 9] {
            assert_eq!(filtered(kind, 30.0), plain, "native untouched gain is zero dB for type {kind}");
        }
    }

    #[test]
    fn native_multiband_modes_keep_held_builds_and_failed_group_prefix() {
        let mut m = mixer();
        let filters = |kind| {
            Box::new(Group::ALL.map(|_| NativeDspSettings {
                filter_type: Ok(kind),
                cutoff_hz: Ok(120.0),
                q: Ok(0.707),
            }))
        };
        m.apply(&[
            Action::NativeFilters(filters(3)),
            start(Voice::SuspensionLf, 5, Plays::Forever, Group::SuspensionLf),
        ])
        .unwrap();
        let old = m.current;
        render(&mut m, 2_048);
        m.set_levels(m.builds[&old].levels.clone());
        m.apply(&[Action::NativeFilters(filters(5)), start(Voice::Transmission, 5, Plays::Forever, Group::Gear)])
            .unwrap();
        assert_eq!(m.builds[&old].filter_types[Group::SuspensionLf.index()], 3);
        let mut requested = filters(12);
        requested[Group::Gear.index()].q = Err(DspError { class: "KeyError", message: "'gear'".into() });
        assert!(m.apply(&[Action::NativeFilters(requested)]).is_err());
        let current = &m.builds[&m.current];
        assert!(current.attached[Group::Engine.index()]);
        assert_eq!(current.filter_types[Group::Engine.index()], 12);
        assert!(!current.attached[Group::Gear.index()]);
        assert_eq!(current.filters[Group::Gear.index()], None);
        assert!(current.attached[Group::Afterburner.index()]);
        assert_eq!(current.filter_types[Group::Afterburner.index()], 5);
        assert_eq!(m.builds[&old].filter_types[Group::SuspensionLf.index()], 3);
        assert!(m.is_playing(Voice::SuspensionLf));
        assert!(m.is_playing(Voice::Transmission));
        assert!(render(&mut m, 2_048).iter().any(|sample| sample.abs() > 0.01));
        m.apply(&[Action::NativeFilters(filters(12))]).unwrap();
        assert!(m.builds[&m.current].attached.iter().all(|attached| *attached));
        m.set_sample_rate(48_000);
        assert!(render(&mut m, 2_048).iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn resamples_and_direct_route() {
        let mut m = Mixer::new(
            48_000,
            SampleBank::synthesized(),
            GroupLevels::compute(
                &SpeakerProfile::built_in("1 Speaker - Center").unwrap(),
                &ShakerTuning::default(),
                None,
            ),
        );
        m.play_direct(
            Voice::SpeakerTest,
            SampleKey::new(SampleFolder::Transmission, 1),
            [0.0, 0.0, 0.0, 0.3, 0.0, 0.0],
        );
        // 0.0232 s at 44.1 kHz = 1023 source frames -> about 1114 output frames at 48 kHz.
        let buf = render(&mut m, 1_100);
        assert!(m.is_playing(Voice::SpeakerTest));
        assert!(buf.chunks(6).any(|f| f[3] != 0.0));
        assert!(buf.chunks(6).all(|f| f[0] == 0.0));
        render(&mut m, 20);
        assert!(!m.is_playing(Voice::SpeakerTest));
    }

    #[test]
    fn native_sample_resampler_uses_fixed_phase_and_float32_interpolation() {
        let mut m = mixer();
        m.set_sample_rate(48_000);
        let key = SampleKey::new(SampleFolder::Transmission, 1);
        let sample = m.bank.get(key).unwrap();
        assert_eq!((sample.rate, sample.channels), (44_100, 1));
        m.play_direct(Voice::SpeakerTest, key, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let actual = render(&mut m, 512);
        // The rate ratio is converted to f32 before
        // multiplying by 2^32 and truncating: 44100 / 48000 => 0xeb333300.
        for (n, frame) in actual.as_chunks::<CHANNELS>().0.iter().enumerate() {
            let position = n as u64 * 0xeb33_3300;
            let index = (position >> 32) as usize;
            let fraction = position as u32 as f32 * (1.0_f32 / 4_294_967_296.0);
            let expected = sample.data[index + 1] * fraction + sample.data[index] * (1.0 - fraction);
            assert_eq!(frame[0].to_bits(), expected.to_bits(), "source frame {n}");
            assert_eq!(&frame[1..], &[0.0; 5]);
        }
    }

    #[test]
    fn native_sample_resampler_retains_phase_through_long_normal_playback() {
        let bank = SampleBank::synthesized();
        for (key, rate, count, step) in [
            (SampleKey::new(SampleFolder::Transmission, 1), 48_000, 480_000_u64, 0xeb33_3300),
            (SampleKey::new(SampleFolder::Turbulence, 0), 44_100, 441_000, 0x1_16a3_b400),
        ] {
            let sample = bank.get(key).unwrap();
            let mut voice = Playing {
                sample: Arc::clone(&sample),
                origin: SampleOrigin::Synthesized,
                position: 0,
                step: sample_step(sample.rate, rate),
                repeats: None,
                route: Route::Direct([1.0; CHANNELS]),
                interpolation: Interpolation::for_channels(sample.channels),
                mono: None,
                native_position: None,
            };
            assert_eq!(voice.step, step);
            for _ in 0..count {
                voice.next().unwrap();
            }
            // Ten seconds of output keeps the truncated increment;
            // accumulating an f64 rate ratio reaches different source frames.
            let period = sample.frames() as u64 * PHASE_ONE;
            let expected_position = (count * step) % period;
            assert_eq!(voice.position, expected_position);
            let index = (expected_position >> 32) as usize;
            let fraction = expected_position as u32 as f32 / PHASE_ONE as f32;
            let next = (index + 1) % sample.frames();
            let expected = sample.data[next] * fraction + sample.data[index] * (1.0 - fraction);
            assert_eq!(voice.next().unwrap(), [f64::from(expected); CHANNELS]);
        }
    }

    #[test]
    fn native_sample_resampler_keeps_decoded_surround_channels_independent() {
        let sample =
            Arc::new(super::super::samples::decode(include_bytes!("testdata/layout_51.ogg"), "layout").unwrap());
        assert_eq!((sample.rate, sample.channels), (44_100, 6));
        let mut voice = Playing {
            sample: Arc::clone(&sample),
            origin: SampleOrigin::Synthesized,
            position: 0,
            step: sample_step(sample.rate, 48_000),
            repeats: Some(0),
            route: Route::Direct([1.0; CHANNELS]),
            interpolation: Interpolation::for_channels(sample.channels),
            mono: None,
            native_position: None,
        };
        for n in 0..1024_u64 {
            let position = n * 0xeb33_3300;
            let index = (position >> 32) as usize;
            let fraction = voice.interpolation.fraction(position as u32);
            let expected = std::array::from_fn(|channel| {
                let a = sample.data[index * CHANNELS + channel];
                let b = sample.data[(index + 1) * CHANNELS + channel];
                f64::from(match voice.interpolation {
                    Interpolation::Weighted => b * fraction + a * (1.0 - fraction),
                    Interpolation::Delta => a + (b - a) * fraction,
                    Interpolation::FusedDelta => (b - a).mul_add(fraction, a),
                })
            });
            assert_eq!(voice.next().unwrap(), expected);
        }
    }

    #[test]
    fn native_eight_channel_matrix_preserves_lfe_and_directional_inputs() {
        // The expected table for six destination/eight source channels,
        // including its rounded constants.
        let expected: [[f32; 8]; CHANNELS] = [
            [1.0, 0.0, 0.0, 0.0, 0.3673, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0, 0.0, 0.3673, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 0.0, 0.93, 0.0, 0.7, 0.46],
            [0.0, 0.0, 0.0, 0.0, 0.0, 0.93, 0.46, 0.7],
        ];
        for channel in 0..8 {
            let mut input = [0.0; 8];
            input[channel] = 1.0;
            assert_eq!(surround_to_speakers(&input), expected.map(|row| f64::from(row[channel])));
        }
    }

    #[test]
    fn native_eight_channel_resampling_precedes_connection_matrix() {
        let sample =
            Arc::new(super::super::samples::decode(include_bytes!("testdata/layout_71.ogg"), "layout").unwrap());
        assert_eq!((sample.rate, sample.channels, sample.frames()), (44_100, 8, 8820));
        // Expected values from an independent C implementation with the same matrix
        // and SSE horizontal sums after source-channel resampling.
        for (interpolation, expected) in [
            (Interpolation::Weighted, 0xacc1_7735_16ba_b1c1),
            (Interpolation::Delta, 0xd122_bbcb_ea0a_bc31),
            (Interpolation::FusedDelta, 0xf80b_f575_eb26_6fdb),
        ] {
            let mut voice = Playing {
                sample: Arc::clone(&sample),
                origin: SampleOrigin::Synthesized,
                position: 0,
                step: sample_step(sample.rate, 48_000),
                repeats: Some(0),
                route: Route::Direct([1.0; CHANNELS]),
                interpolation,
                mono: None,
                native_position: None,
            };
            let mut hash = 0xcbf2_9ce4_8422_2325_u64;
            for _ in 0..9601 {
                for value in voice.next().expect("source frame").map(|v| v as f32) {
                    for byte in value.to_le_bytes() {
                        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
                    }
                }
            }
            assert_eq!(hash, expected);
            assert!(voice.next().is_none());
        }
    }

    #[test]
    fn imported_eight_channel_sample_uses_native_path_through_split_render() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::create_dir(folder.path().join("suspension")).unwrap();
        std::fs::write(folder.path().join("suspension/0.ogg"), include_bytes!("testdata/layout_71.ogg")).unwrap();
        let (bank, errors) = SampleBank::import(folder.path()).unwrap();
        assert_eq!(errors.len(), SampleKey::PLAYED.len() - 1);
        let key = SampleKey::new(SampleFolder::Suspension, 0);
        let mut mixer = Mixer::new(48_000, bank, GroupLevels::empty());
        mixer.play_direct(Voice::SpeakerTest, key, [1.0; CHANNELS]);
        let mut output = render(&mut mixer, 317);
        output.extend(render(&mut mixer, 9601 - 317));
        let hash = output
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
        let expected = match Interpolation::for_channels(8) {
            Interpolation::Weighted => 0xacc1_7735_16ba_b1c1,
            Interpolation::Delta => 0xd122_bbcb_ea0a_bc31,
            Interpolation::FusedDelta => 0xf80b_f575_eb26_6fdb,
        };
        assert_eq!(hash, expected);
        assert_eq!(render(&mut mixer, 4), vec![0.0; 4 * CHANNELS]);
    }

    #[test]
    fn native_eight_channel_loop_and_rate_change_retain_fractional_phase() {
        let sample =
            Arc::new(super::super::samples::decode(include_bytes!("testdata/layout_71.ogg"), "layout").unwrap());
        for (interpolation, expected) in [
            (Interpolation::Weighted, 0x308e_54f7_c722_bbd1),
            (Interpolation::Delta, 0xcc5e_fade_adc3_031f),
            (Interpolation::FusedDelta, 0x1e86_82b4_a6de_e8c8),
        ] {
            let mut voice = Playing {
                sample: Arc::clone(&sample),
                origin: SampleOrigin::Synthesized,
                position: 8818 * PHASE_ONE,
                step: sample_step(sample.rate, 48_000),
                repeats: None,
                route: Route::Direct([1.0; CHANNELS]),
                interpolation,
                mono: None,
                native_position: None,
            };
            let mut hash = 0xcbf2_9ce4_8422_2325_u64;
            for frame in 0..1030 {
                if frame == 317 {
                    voice.step = sample_step(sample.rate, 44_100);
                }
                for value in voice.next().unwrap().map(|v| v as f32) {
                    for byte in value.to_le_bytes() {
                        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
                    }
                }
            }
            assert_eq!(voice.position, 0x03ea_3e66_2700);
            assert_eq!(hash, expected);
        }
    }

    #[test]
    fn imported_sample_layout_family_matches_native_connections() {
        // Expected values from an independent C implementation with the same connection tables:
        // it encodes these signed16 WAV frames and evaluates the separate source interpolation.
        for (channels, hashes) in [
            (2, [0x4203_9630_de6a_0af8, 0x7a30_d1ae_a1cf_865c, 0x67d9_7986_d000_2c50]),
            (3, [0x2561_33c2_ed56_bdc5; 3]),
            (4, [0x2e86_16bc_9793_fa11, 0xb8a2_17e2_663b_9633, 0x63ce_f444_6a0f_4fc9]),
            (5, [0xb6eb_cb7e_7667_6128; 3]),
            (7, [0xe66f_9df0_10d6_7d11; 3]),
        ] {
            let mut bytes = std::io::Cursor::new(Vec::new());
            let spec = hound::WavSpec {
                channels,
                sample_rate: 44_100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::new(&mut bytes, spec).unwrap();
            for frame in 0..67 {
                for channel in 0..channels {
                    writer.write_sample(((frame * 503 + i32::from(channel) * 2099) % 48001 - 24000) as i16).unwrap();
                }
            }
            writer.finalize().unwrap();
            let folder = tempfile::tempdir().unwrap();
            std::fs::create_dir(folder.path().join("suspension")).unwrap();
            std::fs::write(folder.path().join("suspension/0.ogg"), bytes.into_inner()).unwrap();
            let (bank, errors) = SampleBank::import(folder.path()).unwrap();
            assert_eq!(errors.len(), SampleKey::PLAYED.len() - 1);
            let sample = bank.get(SampleKey::new(SampleFolder::Suspension, 0)).unwrap();
            for (mode, requested) in
                [Interpolation::Weighted, Interpolation::Delta, Interpolation::FusedDelta].into_iter().enumerate()
            {
                let interpolation = if matches!(channels, 2 | 4) { requested } else { Interpolation::Weighted };
                let mut voice = Playing {
                    sample: Arc::clone(&sample),
                    origin: SampleOrigin::Synthesized,
                    position: 0,
                    step: sample_step(sample.rate, 48_000),
                    repeats: None,
                    route: Route::Direct([1.0; CHANNELS]),
                    interpolation,
                    mono: None,
                    native_position: None,
                };
                let mut hash = 0xcbf2_9ce4_8422_2325_u64;
                for _ in 0..1000 {
                    for value in voice.next().unwrap().map(|v| v as f32) {
                        for byte in value.to_le_bytes() {
                            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
                        }
                    }
                }
                let expected = if matrix_uses_simd() {
                    hashes[mode]
                } else {
                    match channels {
                        4 if mode == 0 => 0xf884_b4bf_0571_2b11,
                        7 => 0x3be3_8e4a_a099_d6cb,
                        _ => hashes[mode],
                    }
                };
                assert_eq!(hash, expected, "channels {channels}, mode {mode}");
            }
            let mut mixer = Mixer::new(48_000, bank, GroupLevels::empty());
            let key = SampleKey::new(SampleFolder::Suspension, 0);
            mixer.start(Voice::SpeakerTest, key, Plays::Forever, Route::Direct([1.0; CHANNELS]));
            let mut output = render(&mut mixer, 317);
            output.extend(render(&mut mixer, 683));
            let hash = output
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
            let index = match Interpolation::for_channels(channels) {
                Interpolation::Weighted => 0,
                Interpolation::Delta => 1,
                Interpolation::FusedDelta => 2,
            };
            let expected = if matrix_uses_simd() {
                match (channels, index) {
                    // The master's addition to its zero buffer clears negative
                    // zero in unused quad channels after the connection stage.
                    (4, 0) => 0xf884_b4bf_0571_2b11,
                    (4, 1) => 0xf05f_298f_bded_dd33,
                    (4, 2) => 0x0a6f_569c_e13d_2dc9,
                    _ => hashes[index],
                }
            } else {
                match channels {
                    4 => 0xf884_b4bf_0571_2b11,
                    7 => 0x3be3_8e4a_a099_d6cb,
                    _ => hashes[index],
                }
            };
            assert_eq!(hash, expected, "channels {channels} through split Mixer::render");
        }
    }

    fn imported_mono_bank() -> SampleBank {
        let mut bytes = std::io::Cursor::new(Vec::new());
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::new(&mut bytes, spec).unwrap();
        for frame in 0..67 {
            writer.write_sample((frame * 503 % 48001 - 24000) as i16).unwrap();
        }
        writer.finalize().unwrap();
        let folder = tempfile::tempdir().unwrap();
        std::fs::create_dir(folder.path().join("suspension")).unwrap();
        std::fs::write(folder.path().join("suspension/0.ogg"), bytes.into_inner()).unwrap();
        let (bank, errors) = SampleBank::import(folder.path()).unwrap();
        assert_eq!(errors.len(), SampleKey::PLAYED.len() - 1);
        bank
    }

    #[test]
    fn imported_mono_uses_native_front_connection_through_split_render_and_wrap() {
        let bank = imported_mono_bank();
        let key = SampleKey::new(SampleFolder::Suspension, 0);
        assert_eq!(bank.origin(key), Some(SampleOrigin::Imported));
        let mut mixer = Mixer::new(44_100, bank, GroupLevels::empty());
        mixer.start(Voice::SpeakerTest, key, Plays::Forever, Route::Direct([1.0; CHANNELS]));
        let mut output = render(&mut mixer, 317);
        output.extend(render(&mut mixer, 683));
        let hash = output
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
        // Expected values from an independent C implementation: the 1 -> 6 matrix applied to
        // these PCM16 frames at their own rate, added to the master buffer.
        assert_eq!(hash, 0xbe23_7afe_1f8c_6c6d);
        assert!(output.as_chunks::<CHANNELS>().0.iter().all(|frame| frame[0] == frame[1] && frame[2..] == [0.0; 4]));
        mixer.start(Voice::SpeakerTest, key, Plays::Once, Route::Direct([1.0; CHANNELS]));
        let once = render(&mut mixer, 68);
        assert_eq!(&once[..67 * CHANNELS], &output[..67 * CHANNELS]);
        assert_eq!(&once[67 * CHANNELS..], &[0.0; CHANNELS]);
        assert!(!mixer.is_playing(Voice::SpeakerTest));
    }

    #[test]
    fn sample_source_routing_is_retained_per_voice_and_keeps_synthesized_fallback() {
        let bank = imported_mono_bank();
        let key = SampleKey::new(SampleFolder::Suspension, 0);
        let fallback = SampleKey::new(SampleFolder::Transmission, 1);
        assert_eq!(bank.origin(fallback), Some(SampleOrigin::Synthesized));
        let mut mixer = Mixer::new(44_100, bank, GroupLevels::empty());
        mixer.start(Voice::SpeakerTest, key, Plays::Forever, Route::Direct([1.0; CHANNELS]));
        let imported = Arc::clone(&mixer.voices[&Voice::SpeakerTest].sample);
        render(&mut mixer, 1);
        mixer.set_bank(SampleBank::synthesized());
        let held = render(&mut mixer, 1);
        let expected = imported.data[1] * f32::from_bits(0x3f35_0481);
        assert_eq!(held, [expected, expected, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(mixer.voices[&Voice::SpeakerTest].origin, SampleOrigin::Imported);
        mixer.start(Voice::SpeakerTest, key, Plays::Forever, Route::Direct([1.0; CHANNELS]));
        assert_eq!(mixer.voices[&Voice::SpeakerTest].origin, SampleOrigin::Synthesized);
        let synthesized = mixer.bank.get(key).unwrap();
        let output = render(&mut mixer, 67);
        for (frame, &sample) in output.as_chunks::<CHANNELS>().0.iter().zip(&synthesized.data) {
            assert_eq!(frame, &[sample; CHANNELS]);
        }
        mixer.set_bank(imported_mono_bank());
        mixer.start(Voice::SpeakerTest, fallback, Plays::Once, Route::Direct([1.0; CHANNELS]));
        assert_eq!(mixer.voices[&Voice::SpeakerTest].origin, SampleOrigin::Synthesized);
        let output = render(&mut mixer, 67);
        assert!(output.as_chunks::<CHANNELS>().0.iter().all(|frame| frame.iter().all(|&value| value == frame[0])));
    }

    #[test]
    fn native_six_channel_interpolation_keeps_sse_and_fma_rounding() {
        // intrinsic implementation of the six-channel specialized
        // kernels (SSE and AVX/FMA). The last input pair
        // is exact PCM16 decoded to floats, as used by the imported Ogg path.
        for (a, b, phase, sse, fma) in [
            (0.123_f32, 0.987_f32, 0xb0cc_cccd_u32, 0x3f38_3e43, 0x3f38_3e42),
            (-0.3, 0.2, 0x7599_9980, 0xbd90_0002, 0xbd90_0002),
            (-1234.0 / 32768.0, 32456.0 / 32768.0, 0xdead_beef, 0x3f5b_4dbb, 0x3f5b_4dbb),
        ] {
            let sample =
                Arc::new(Sample { rate: 44_100, channels: 6, data: [vec![a; CHANNELS], vec![b; CHANNELS]].concat() });
            for (interpolation, expected) in [(Interpolation::Delta, sse), (Interpolation::FusedDelta, fma)] {
                let mut voice = Playing {
                    sample: Arc::clone(&sample),
                    origin: SampleOrigin::Synthesized,
                    position: u64::from(phase),
                    step: 0,
                    repeats: Some(0),
                    route: Route::Direct([1.0; CHANNELS]),
                    interpolation,
                    mono: None,
                    native_position: None,
                };
                assert_eq!(voice.next().unwrap().map(|v| (v as f32).to_bits()), [expected; CHANNELS]);
            }
        }
    }

    #[test]
    fn native_six_channel_fraction_discards_only_its_low_bit() {
        // shr1 before cvtdq2ps, then multiply by2^-31. Near zero this differs
        // from converting the full unsigned fraction directly.
        assert_eq!(Interpolation::Delta.fraction(1), 0.0);
        assert_eq!(Interpolation::FusedDelta.fraction(1), 0.0);
        assert_eq!(Interpolation::Weighted.fraction(1).to_bits(), 0x2f80_0000);
        assert_eq!(Interpolation::Delta.fraction(2).to_bits(), 0x3000_0000);
        assert_eq!(Interpolation::Delta.fraction(3), Interpolation::Delta.fraction(2));
        assert_eq!(Interpolation::Delta.fraction(u32::MAX), 1.0);
        assert!(matches!(Interpolation::for_cpu(false, false, false, false), Interpolation::Weighted));
        assert!(matches!(Interpolation::for_cpu(true, false, false, false), Interpolation::Delta));
        assert!(matches!(Interpolation::for_cpu(true, true, false, true), Interpolation::FusedDelta));
        assert!(matches!(Interpolation::for_cpu(true, false, true, true), Interpolation::FusedDelta));
        assert!(matches!(Interpolation::for_cpu(true, true, false, false), Interpolation::Delta));
    }

    #[test]
    fn native_six_channel_decoder_path_retains_dispatch_across_wrap_and_rate_change() {
        let sample =
            Arc::new(super::super::samples::decode(include_bytes!("testdata/layout_51.ogg"), "layout").unwrap());
        let interpolation = Interpolation::for_channels(sample.channels);
        let mut voice = Playing {
            sample: Arc::clone(&sample),
            origin: SampleOrigin::Synthesized,
            position: (sample.frames() as u64 - 1) * PHASE_ONE + 0xdead_beef,
            step: sample_step(sample.rate, 48_000),
            repeats: None,
            route: Route::Direct([1.0; CHANNELS]),
            interpolation,
            mono: None,
            native_position: None,
        };
        for step in [voice.step, sample_step(sample.rate, 96_000)] {
            voice.step = step;
            for _ in 0..257 {
                let normalized = voice.position % (sample.frames() as u64 * PHASE_ONE);
                let index = (normalized >> 32) as usize;
                let next = (index + 1) % sample.frames();
                let fraction = ((normalized as u32) >> 1) as f32 * (1.0 / 2_147_483_648.0);
                let expected = std::array::from_fn(|channel| {
                    let a = sample.data[index * CHANNELS + channel];
                    let b = sample.data[next * CHANNELS + channel];
                    match interpolation {
                        Interpolation::Weighted => {
                            let fraction = normalized as u32 as f32 * (1.0 / 4_294_967_296.0);
                            b * fraction + a * (1.0 - fraction)
                        }
                        Interpolation::Delta => a + (b - a) * fraction,
                        Interpolation::FusedDelta => (b - a).mul_add(fraction, a),
                    }
                });
                assert_eq!(voice.next().unwrap().map(|v| v as f32), expected);
            }
        }
    }

    #[test]
    fn imported_surround_sample_start_uses_the_native_cpu_path() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::create_dir(folder.path().join("suspension")).unwrap();
        std::fs::write(folder.path().join("suspension/0.ogg"), include_bytes!("testdata/layout_51.ogg")).unwrap();
        let (bank, _) = SampleBank::import(folder.path()).unwrap();
        let key = SampleKey::new(SampleFolder::Suspension, 0);
        let sample = bank.get(key).unwrap();
        let mut mixer = Mixer::new(48_000, bank, GroupLevels::empty());
        mixer.play_direct(Voice::SpeakerTest, key, [1.0; CHANNELS]);
        let output = render(&mut mixer, 1024);
        for (n, frame) in output.as_chunks::<CHANNELS>().0.iter().enumerate() {
            let position = n as u64 * 0xeb33_3300;
            let index = (position >> 32) as usize;
            let fraction = ((position as u32) >> 1) as f32 * (1.0 / 2_147_483_648.0);
            for (channel, actual) in frame.iter().enumerate() {
                let a = sample.data[index * CHANNELS + channel];
                let b = sample.data[(index + 1) * CHANNELS + channel];
                let expected = match Interpolation::for_channels(6) {
                    Interpolation::Weighted => {
                        let fraction = position as u32 as f32 / 4_294_967_296.0;
                        b * fraction + a * (1.0 - fraction)
                    }
                    Interpolation::Delta => a + (b - a) * fraction,
                    Interpolation::FusedDelta => (b - a).mul_add(fraction, a),
                };
                assert_eq!(actual.to_bits(), expected.to_bits(), "frame={n} channel={channel}");
            }
        }
    }

    #[test]
    fn native_voice_inputs_accumulate_in_attachment_order_and_restarts_append() {
        let mut mixer = mixer();
        let key = SampleKey::new(SampleFolder::Suspension, 0);
        let inputs =
            [(Voice::Impact, 1.0_f32), (Voice::RoadBump, f32::from_bits(0x3300_0000)), (Voice::Brake, -1.0_f32)];
        let pin_inputs = |mixer: &mut Mixer| {
            for (voice, value) in inputs {
                let playing = mixer.voices.get_mut(&voice).unwrap();
                playing.sample = Arc::new(Sample { rate: 44_100, channels: 6, data: vec![value; CHANNELS] });
                playing.position = 0;
                playing.step = 0;
            }
        };
        for (voice, _) in inputs {
            mixer.start(voice, key, Plays::Forever, Route::Direct([1.0; CHANNELS]));
        }
        pin_inputs(&mut mixer);
        assert_eq!(mixer.next_frame(), [0.0; CHANNELS], "(1+tiny)-1 rounds away tiny in native attachment order");
        mixer.start(Voice::RoadBump, key, Plays::Forever, Route::Direct([1.0; CHANNELS]));
        pin_inputs(&mut mixer);
        assert_eq!(mixer.next_frame(), [f64::from(inputs[1].1); CHANNELS], "restart appends: (1-1)+tiny");
        mixer.apply(&[Action::Stop(Voice::Impact)]).unwrap();
        assert_eq!(mixer.voice_order, [Voice::Brake, Voice::RoadBump]);
    }

    #[test]
    fn native_same_group_voice_order_survives_other_group_starts_and_auto_completion() {
        let mut mixer = mixer();
        let key = SampleKey::new(SampleFolder::Suspension, 0);
        let inputs = [(Voice::Impact, 1.0_f32), (Voice::RoadBump, f32::from_bits(0x3300_0000)), (Voice::Brake, -1.0)];
        for (voice, value) in inputs {
            mixer.start(voice, key, Plays::Forever, Route::Group { build: mixer.current, group: Group::Gear });
            let playing = mixer.voices.get_mut(&voice).unwrap();
            playing.sample = Arc::new(Sample { rate: 44_100, channels: 6, data: vec![value; CHANNELS] });
            playing.step = 0;
        }
        mixer.start(Voice::Canopy, key, Plays::Once, Route::Group { build: mixer.current, group: Group::Canopy });
        mixer.voices.get_mut(&Voice::Canopy).unwrap().sample =
            Arc::new(Sample { rate: 44_100, channels: 6, data: vec![0.0; CHANNELS] });
        assert_eq!(mixer.next_frame(), [0.0; CHANNELS]);
        assert_eq!(mixer.next_frame(), [0.0; CHANNELS]);
        assert!(!mixer.voices.contains_key(&Voice::Canopy));
        assert_eq!(mixer.voice_order, inputs.map(|(voice, _)| voice));
    }

    fn pin_sample(mixer: &mut Mixer, voice: Voice, value: f32) {
        let playing = mixer.voices.get_mut(&voice).unwrap();
        playing.sample = Arc::new(Sample { rate: 44_100, channels: 6, data: vec![value; CHANNELS] });
        playing.position = 0;
        playing.step = 0;
    }

    fn unity_levels() -> GroupLevels {
        let tuning = ShakerTuning {
            native_effect_gains: Some(Box::new([Some(1.0); 18])),
            native_group_bases: Some(Box::new(std::array::from_fn(|_| Ok([1.0; CHANNELS])))),
            ..ShakerTuning::default()
        };
        GroupLevels::compute(&SpeakerProfile::built_in("6 Speakers - 5.1").unwrap(), &tuning, None)
    }

    #[test]
    fn native_master_groups_precede_a_later_speaker_test() {
        let mut mixer = mixer();
        let key = SampleKey::new(SampleFolder::Transmission, 1);
        // These are all ordinary, unclipped sample amplitudes. The profile
        // gives each input unity gain; only the master addition order differs.
        mixer.set_levels(unity_levels());
        mixer.start(
            Voice::Transmission,
            key,
            Plays::Forever,
            Route::Group { build: mixer.current, group: Group::Gear },
        );
        mixer.start(Voice::Canopy, key, Plays::Forever, Route::Group { build: mixer.current, group: Group::Canopy });
        mixer.play_direct(Voice::SpeakerTest, key, [1.0; CHANNELS]);
        pin_sample(&mut mixer, Voice::Transmission, 1.0);
        pin_sample(&mut mixer, Voice::Canopy, -1.0);
        pin_sample(&mut mixer, Voice::SpeakerTest, f32::from_bits(0x3300_0000));
        assert_eq!(mixer.next_frame(), [f64::from(f32::from_bits(0x3300_0000)); CHANNELS]);
    }

    #[test]
    fn native_master_speaker_test_stays_between_old_and_new_group_builds() {
        let levels = unity_levels();
        let mut mixer = Mixer::new(44_100, SampleBank::synthesized(), levels.clone());
        let key = SampleKey::new(SampleFolder::Transmission, 1);
        mixer.start(
            Voice::Transmission,
            key,
            Plays::Forever,
            Route::Group { build: mixer.current, group: Group::Gear },
        );
        pin_sample(&mut mixer, Voice::Transmission, 1.0);
        mixer.play_direct(Voice::SpeakerTest, key, [1.0; CHANNELS]);
        pin_sample(&mut mixer, Voice::SpeakerTest, -1.0);
        mixer.set_levels(levels);
        mixer.start(Voice::Canopy, key, Plays::Forever, Route::Group { build: mixer.current, group: Group::Gear });
        pin_sample(&mut mixer, Voice::Canopy, f32::from_bits(0x3300_0000));
        assert_eq!(mixer.next_frame(), [f64::from(f32::from_bits(0x3300_0000)); CHANNELS]);

        // A new speaker test creates new test groups after both builds.
        mixer.play_direct(Voice::SpeakerTest, key, [1.0; CHANNELS]);
        pin_sample(&mut mixer, Voice::SpeakerTest, -1.0);
        assert_eq!(mixer.next_frame(), [0.0; CHANNELS]);
    }

    #[test]
    fn native_master_keeps_only_live_build_and_test_connections() {
        let mut mixer = mixer();
        let key = SampleKey::new(SampleFolder::Transmission, 1);
        for _ in 0..4 {
            mixer.play_direct(Voice::SpeakerTest, key, [1.0; CHANNELS]);
            pin_sample(&mut mixer, Voice::SpeakerTest, 0.5);
            mixer.voices.get_mut(&Voice::SpeakerTest).unwrap().position = PHASE_ONE;
            assert_eq!(mixer.next_frame(), [0.0; CHANNELS]);
            assert_eq!(mixer.master_order.len(), 1, "completed test group is no longer active");
            mixer.set_levels(GroupLevels::empty());
            assert_eq!(mixer.master_order.len(), 1, "silent unreferenced builds are no longer active");
        }
        mixer.play_direct(Voice::SpeakerTest, key, [1.0; CHANNELS]);
        mixer.apply(&[Action::Stop(Voice::SpeakerTest)]).unwrap();
        assert_eq!(mixer.master_order.len(), 1);
        // Changing a reused slot from direct to a regular group must also
        // remove its old direct master connection.
        mixer.play_direct(Voice::SpeakerTest, key, [1.0; CHANNELS]);
        mixer.start(Voice::SpeakerTest, key, Plays::Forever, Route::Group { build: mixer.current, group: Group::Gear });
        assert_eq!(mixer.master_order.len(), 1);
    }

    #[test]
    fn native_sample_resampler_keeps_repeat_and_terminal_edges() {
        let sample = Arc::new(Sample { rate: 2, channels: 1, data: vec![0.25, 1.0] });
        let make = |repeats, output_rate| Playing {
            sample: Arc::clone(&sample),
            origin: SampleOrigin::Synthesized,
            position: 0,
            step: sample_step(sample.rate, output_rate),
            repeats,
            route: Route::Direct([1.0; CHANNELS]),
            interpolation: Interpolation::for_channels(sample.channels),
            mono: None,
            native_position: None,
        };
        for (repeats, expected) in
            [(Some(0), vec![0.25, 0.625, 1.0, 0.5]), (Some(1), vec![0.25, 0.625, 1.0, 0.625, 0.25, 0.625, 1.0, 0.5])]
        {
            let mut voice = make(repeats, 4);
            for value in expected {
                assert_eq!(voice.next().unwrap(), [value; CHANNELS]);
            }
            assert!(voice.next().is_none());
        }
        let mut voice = make(None, 4);
        for value in [0.25, 0.625, 1.0, 0.625, 0.25] {
            assert_eq!(voice.next().unwrap(), [value; CHANNELS]);
        }
        // A short sound can cross multiple complete loops in one output
        // frame. Every crossed repetition counts, without indexing past PCM.
        let mut voice = make(Some(2), 1);
        voice.step = 5 * PHASE_ONE;
        assert_eq!(voice.next().unwrap(), [0.25; CHANNELS]);
        assert_eq!(voice.next().unwrap(), [1.0; CHANNELS]);
        assert!(voice.next().is_none());
    }

    #[test]
    fn native_sample_resampler_split_render_and_rate_change_keep_held_voice() {
        let prepare = || {
            let mut mixer = mixer();
            mixer.set_sample_rate(48_000);
            mixer.apply(&[start(Voice::SuspensionLf, 6, Plays::Forever, Group::SuspensionLf)]).unwrap();
            mixer
        };
        let mut whole = prepare();
        let mut split = prepare();
        let expected = render(&mut whole, 24_001);
        let mut actual = Vec::new();
        for size in [1, 511, 1024, 7, 8192, 14266] {
            actual.extend(render(&mut split, size));
        }
        assert_eq!(actual, expected);
        let position = whole.voices[&Voice::SuspensionLf].position;
        for mixer in [&mut whole, &mut split] {
            mixer.set_sample_rate(96_000);
            let voice = &mixer.voices[&Voice::SuspensionLf];
            assert_eq!(voice.position, position);
            assert_eq!(voice.step, 0x7599_9980);
            assert_eq!(voice.repeats, None);
        }
        let expected = render(&mut whole, 12_345);
        let mut actual = Vec::new();
        for size in [13, 123, 4096, 8113] {
            actual.extend(render(&mut split, size));
        }
        assert_eq!(actual, expected);
        assert_eq!(whole.voices[&Voice::SuspensionLf].position, split.voices[&Voice::SuspensionLf].position);
        assert!(whole.is_playing(Voice::SuspensionLf));
    }

    fn suspension_lf_frames(m: &mut Mixer, frames: usize) -> Vec<f32> {
        render(m, frames).chunks(CHANNELS).map(|f| f[Speaker::FrontLeft.index()]).collect()
    }

    #[test]
    fn load_channel_groups_leaves_playing_voices_on_their_build() {
        let profile = SpeakerProfile::built_in("6 Speakers - 5.1").unwrap();
        // Effect test on the engine: every suspension group gets level 0 in the new build.
        let muted = GroupLevels::compute(&profile, &ShakerTuning::default(), Some(crate::shakers::Effect::Engine));
        let loop_lf = start(Voice::SuspensionLf, 6, Plays::Forever, Group::SuspensionLf);
        let mut reference = mixer();
        reference.apply(std::slice::from_ref(&loop_lf)).unwrap();
        let mut rebuilt = mixer();
        rebuilt.apply(&[loop_lf]).unwrap();
        render(&mut reference, 500);
        render(&mut rebuilt, 500);
        rebuilt.set_levels(muted);
        // A low-pass from the next DSP block reaches only the new build.
        let mut f = [FilterSettings::default(); GROUP_COUNT];
        f[Group::SuspensionLf.index()] = enabled_filter(LowPass { cutoff_hz: 30.0, q: 0.707 });
        rebuilt.apply(&[Action::Filters(Box::new(f))]).unwrap();
        let a = suspension_lf_frames(&mut reference, 5_000);
        let b = suspension_lf_frames(&mut rebuilt, 5_000);
        assert_eq!(a, b);
        assert!(a.iter().any(|v| v.abs() > 0.3));
        // A voice started after the rebuild uses the new (zero) levels.
        rebuilt.apply(&[start(Voice::SuspensionRf, 6, Plays::Forever, Group::SuspensionRf)]).unwrap();
        let buf = render(&mut rebuilt, 2_000);
        assert!(buf.chunks(CHANNELS).all(|f| f[Speaker::FrontRight.index()] == 0.0));
        // Restarting the old voice moves it onto the new build: silent from then on.
        rebuilt.apply(&[start(Voice::SuspensionLf, 6, Plays::Forever, Group::SuspensionLf)]).unwrap();
        render(&mut rebuilt, 10);
        assert!(suspension_lf_frames(&mut rebuilt, 2_000).iter().all(|v| v.abs() < 1e-6));
    }

    #[test]
    fn old_builds_are_dropped_once_silent() {
        let mut m = mixer();
        m.apply(&[start(Voice::Impact, 6, Plays::Once, Group::Impact)]).unwrap();
        m.set_levels(m.builds[&0].levels.clone());
        assert_eq!(m.builds.len(), 2);
        render(&mut m, 20_000);
        assert!(!m.is_playing(Voice::Impact));
        assert_eq!(m.builds.keys().copied().collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn render_layout_places_buses_on_device_channels() {
        use crate::shakers::channel_map::{device_layout, ChannelOrder};
        let mut m = Mixer::new(
            44_100,
            SampleBank::synthesized(),
            GroupLevels::compute(
                &SpeakerProfile::built_in("1 Speaker - Center").unwrap(),
                &ShakerTuning::default(),
                None,
            ),
        );
        m.apply(&[start(Voice::Impact, 6, Plays::Forever, Group::Impact)]).unwrap();
        let layout = device_layout(ChannelOrder::Alsa, 6).unwrap();
        let mut buf = vec![0.0; 2_000 * 6];
        m.render_layout(&mut buf, &layout);
        // ALSA 5.1 is FL FR RL RR FC LFE: the centre speaker is channel 4.
        let peak = |k: usize| buf.chunks(6).map(|f| f[k].abs()).fold(0.0f32, f32::max);
        assert!(peak(4) > 0.1);
        for k in [0, 1, 2, 3, 5] {
            assert_eq!(peak(k), 0.0, "channel {k}");
        }
        // 7.1: two more, silent channels per frame.
        let layout = device_layout(ChannelOrder::Alsa, 8).unwrap();
        let mut buf = vec![1.0; 100 * 8];
        m.render_layout(&mut buf, &layout);
        assert!(buf.chunks(8).all(|f| f[6] == 0.0 && f[7] == 0.0));
        assert!(buf.chunks(8).any(|f| f[4] != 0.0));
    }

    #[test]
    fn device_output_matrix_follows_actual_imported_sample_and_preserves_bus_reads() {
        use crate::shakers::channel_map::ChannelOrder;
        let mut bytes = std::io::Cursor::new(Vec::new());
        let spec = hound::WavSpec {
            channels: 6,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::new(&mut bytes, spec).unwrap();
        for frame in 0..67 {
            for channel in 0..6 {
                writer.write_sample(((frame * 503 + channel * 2099) % 48001 - 24000) as i16).unwrap();
            }
        }
        writer.finalize().unwrap();
        let folder = tempfile::tempdir().unwrap();
        std::fs::create_dir(folder.path().join("suspension")).unwrap();
        std::fs::write(folder.path().join("suspension/0.ogg"), bytes.into_inner()).unwrap();
        let (bank, _) = SampleBank::import(folder.path()).unwrap();
        let prepare = || {
            let mut mixer = Mixer::new(44_100, bank.clone(), GroupLevels::empty());
            mixer.start(
                Voice::SpeakerTest,
                SampleKey::new(SampleFolder::Suspension, 0),
                Plays::Forever,
                Route::Direct([1.0; CHANNELS]),
            );
            mixer
        };
        for channels in [2, 4, 6, 8] {
            let matrix = OutputMatrix::new(ChannelOrder::Wave, channels).unwrap();
            let mut whole = prepare();
            let mut split = prepare();
            let mut expected = vec![0.0; 1000 * matrix.len()];
            whole.render_output(&mut expected, &matrix);
            let mut actual = vec![0.0; expected.len()];
            let (first, second) = actual.split_at_mut(317 * matrix.len());
            split.render_output(first, &matrix);
            split.render_output(second, &matrix);
            assert_eq!(actual, expected);
            let mut raw = prepare();
            let buses = render(&mut raw, 1000);
            for (frame, source) in expected.chunks(matrix.len()).zip(buses.as_chunks::<CHANNELS>().0) {
                let mut mapped = vec![0.0; matrix.len()];
                matrix.write(&source.map(f64::from), &mut mapped);
                assert_eq!(frame, mapped);
            }
        }
    }

    #[test]
    fn imported_integer_mono_matches_native_generic_kernels_at_every_width() {
        for (offset, cases) in [
            (0, include_str!("testdata/native_integer_mono_reference.txt")),
            (317, include_str!("testdata/native_integer_mono_offset_reference.txt")),
        ] {
            for line in cases.lines() {
                let fields: Vec<_> = line.split_whitespace().collect();
                let bits: u16 = fields[1].strip_prefix("fmt").unwrap().parse().unwrap();
                let loops: u8 = fields[2].strip_prefix("loops").unwrap().parse().unwrap();
                let rates: Vec<u32> = fields[3].split('>').map(|v| v.parse().unwrap()).collect();
                let expected = u64::from_str_radix(fields[4].strip_prefix("hash").unwrap(), 16).unwrap();
                let directory = tempfile::tempdir().unwrap();
                std::fs::create_dir(directory.path().join("suspension")).unwrap();
                let mut writer = hound::WavWriter::create(
                    directory.path().join("suspension/0.ogg"),
                    hound::WavSpec {
                        channels: 1,
                        sample_rate: rates[0],
                        bits_per_sample: bits,
                        sample_format: hound::SampleFormat::Int,
                    },
                )
                .unwrap();
                for i in 0..67u32 {
                    let value = match bits {
                        8 => i.wrapping_mul(71).wrapping_add(19) as i8 as i32,
                        24 => {
                            let bits = (i * 912367 + 91213) & 0xffffff;
                            if bits & 0x800000 != 0 {
                                bits as i32 - 0x1000000
                            } else {
                                bits as i32
                            }
                        }
                        32 => i.wrapping_mul(2654435761).wrapping_add(1234567) as i32,
                        _ => unreachable!(),
                    };
                    writer.write_sample(value).unwrap();
                }
                writer.finalize().unwrap();
                let bank = SampleBank::import(directory.path()).unwrap().0;
                let key = SampleKey::new(SampleFolder::Suspension, 0);
                assert_eq!(bank.source_format(key), Some(super::super::native_mono::Format::GenericInteger));
                let mut mixer = Mixer::new(rates[1], bank, GroupLevels::empty());
                render(&mut mixer, offset);
                let plays = match loops {
                    0 => Plays::Once,
                    1 => Plays::Repeat(2),
                    _ => Plays::Forever,
                };
                mixer.start(Voice::SpeakerTest, key, plays, Route::Direct([1.; CHANNELS]));
                let mut pcm = render(&mut mixer, 13);
                pcm.extend(render(&mut mixer, 2294));
                let hash = pcm
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
                assert_eq!(hash, expected, "offset {offset}: {line}");
            }
        }
    }

    #[test]
    fn imported_integer_multichannel_uses_generic_kernel_and_retains_format_per_voice() {
        let cases = if matrix_uses_simd() {
            include_str!("testdata/native_integer_multichannel_reference.txt")
        } else {
            include_str!("testdata/native_integer_multichannel_scalar_reference.txt")
        };
        // This generic kernel has no scalar/SIMD tail distinction. Independent
        // C output hashes are identical with and without the delayed start.
        for offset in [0, 317] {
            for line in cases.lines() {
                let fields: Vec<_> = line.split_whitespace().collect();
                let bits: u16 = fields[0].strip_prefix("fmt").unwrap().parse().unwrap();
                let channels: u16 = fields[1].strip_prefix("ch").unwrap().parse().unwrap();
                let loops: u8 = fields[2].strip_prefix("loops").unwrap().parse().unwrap();
                let rates: Vec<u32> = fields[3].split('>').map(|v| v.parse().unwrap()).collect();
                let expected = u64::from_str_radix(fields[4].strip_prefix("hash").unwrap(), 16).unwrap();
                let directory = tempfile::tempdir().unwrap();
                std::fs::create_dir(directory.path().join("suspension")).unwrap();
                let mut writer = hound::WavWriter::create(
                    directory.path().join("suspension/0.ogg"),
                    hound::WavSpec {
                        channels,
                        sample_rate: rates[0],
                        bits_per_sample: bits,
                        sample_format: hound::SampleFormat::Int,
                    },
                )
                .unwrap();
                for i in 0..67 * u32::from(channels) {
                    let value = match bits {
                        8 => i.wrapping_mul(71).wrapping_add(19) as i8 as i32,
                        24 => {
                            let value = (i * 912367 + 91213) & 0xffffff;
                            if value & 0x800000 != 0 {
                                value as i32 - 0x1000000
                            } else {
                                value as i32
                            }
                        }
                        32 => i.wrapping_mul(2654435761).wrapping_add(1234567) as i32,
                        _ => unreachable!(),
                    };
                    writer.write_sample(value).unwrap();
                }
                writer.finalize().unwrap();
                let bank = SampleBank::import(directory.path()).unwrap().0;
                let key = SampleKey::new(SampleFolder::Suspension, 0);
                assert_eq!(bank.source_format(key), Some(super::super::native_mono::Format::GenericInteger));
                let mut mixer = Mixer::new(rates[1], bank, GroupLevels::empty());
                render(&mut mixer, offset);
                let plays = match loops {
                    0 => Plays::Once,
                    1 => Plays::Repeat(2),
                    _ => Plays::Forever,
                };
                mixer.start(Voice::SpeakerTest, key, plays, Route::Direct([1.; CHANNELS]));
                assert!(mixer.voices[&Voice::SpeakerTest].mono.is_none());
                let mut pcm = render(&mut mixer, 13);
                mixer.set_bank(SampleBank::synthesized());
                pcm.extend(render(&mut mixer, 2294));
                let hash = pcm
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
                assert_eq!(hash, expected, "offset {offset}: {line}");
            }
        }
    }

    #[test]
    fn imported_short_multichannel_sources_count_one_repeat_per_native_boundary() {
        for channels in 2..=8 {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("suspension")).unwrap();
            let mut writer = hound::WavWriter::create(
                dir.path().join("suspension/0.ogg"),
                hound::WavSpec {
                    channels,
                    sample_rate: 96000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
            .unwrap();
            for channel in 0..channels {
                writer.write_sample(8000i16 + channel as i16 * 971).unwrap();
            }
            writer.finalize().unwrap();
            let bank = SampleBank::import(dir.path()).unwrap().0;
            let key = SampleKey::new(SampleFolder::Suspension, 0);
            for plays in [Plays::Once, Plays::Repeat(2), Plays::Forever] {
                let mut mixer = Mixer::new(32000, bank.clone(), GroupLevels::empty());
                mixer.start(Voice::SpeakerTest, key, plays, Route::Direct([1.; CHANNELS]));
                let frames: Vec<_> = (0..4).map(|_| mixer.next_frame()).collect();
                assert_ne!(frames[0], [0.; CHANNELS]);
                match plays {
                    Plays::Once => assert_eq!(&frames[1..], &[[0.; CHANNELS]; 3]),
                    Plays::Repeat(_) => {
                        assert_eq!(frames[1], frames[0]);
                        assert_eq!(frames[2], frames[0]);
                        assert_eq!(frames[3], [0.; CHANNELS]);
                    }
                    Plays::Forever => assert!(frames.iter().all(|frame| *frame == frames[0])),
                }
            }
            let make = || {
                let mut mixer = Mixer::new(32000, bank.clone(), GroupLevels::empty());
                mixer.start(Voice::SpeakerTest, key, Plays::Repeat(2), Route::Direct([1.; CHANNELS]));
                mixer
            };
            let mut whole = make();
            let expected = render(&mut whole, 5);
            let mut split = make();
            let mut actual = render(&mut split, 1);
            split.set_bank(SampleBank::synthesized());
            actual.extend(render(&mut split, 4));
            assert_eq!(actual, expected, "channels={channels}");
        }
    }

    #[test]
    fn imported_multichannel_reciprocal_boundary_preserves_prepared_padding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("suspension")).unwrap();
        let mut writer = hound::WavWriter::create(
            dir.path().join("suspension/0.ogg"),
            hound::WavSpec {
                channels: 2,
                sample_rate: 96000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..3 {
            writer.write_sample(16000i16).unwrap();
            writer.write_sample(-8000i16).unwrap();
        }
        writer.finalize().unwrap();
        let bank = SampleBank::import(dir.path()).unwrap().0;
        let key = SampleKey::new(SampleFolder::Suspension, 0);
        for (plays, count) in [(Plays::Once, 1), (Plays::Repeat(2), 6), (Plays::Forever, 7)] {
            let mut mixer = Mixer::new(32000, bank.clone(), GroupLevels::empty());
            mixer.start(Voice::SpeakerTest, key, plays, Route::Direct([1.; CHANNELS]));
            let frames: Vec<_> = (0..7).map(|_| mixer.next_frame()).collect();
            // The rounded reciprocal yields two frames per loop. The second
            // reads padding: zero for Once, prepared loop start for repeats.
            assert!(frames[..count].iter().all(|frame| *frame == frames[0]));
            assert!(frames[count..].iter().all(|frame| *frame == [0.; CHANNELS]));
        }
    }

    #[test]
    fn imported_short_chain_keeps_silent_tail_and_shortened_repeats_after_bank_replacement() {
        let records = include_bytes!("testdata/native_chain_links.bin");
        let mut input = records.as_slice();
        let mut links = Vec::new();
        for _ in 0..32 {
            let length = u32::from_le_bytes(input[..4].try_into().unwrap()) as usize;
            links.push(&input[4..4 + length]);
            input = &input[4 + length..];
        }
        // First six-channel64-block link, then two-channel128-block link.
        let bytes = [links[20], links[6]].concat();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("suspension")).unwrap();
        std::fs::write(dir.path().join("suspension/0.ogg"), bytes).unwrap();
        let bank = SampleBank::import(dir.path()).unwrap().0;
        let key = SampleKey::new(SampleFolder::Suspension, 0);
        let source = bank.get(key).unwrap();
        assert_eq!(source.frames(), 288);
        assert_eq!(bank.loop_frames(key), Some(159));
        for (plays, count) in [(Plays::Once, 288), (Plays::Repeat(1), 447), (Plays::Repeat(2), 606)] {
            let mut mixer = Mixer::new(44100, bank.clone(), GroupLevels::empty());
            mixer.start(Voice::SpeakerTest, key, plays, Route::Direct([1.; CHANNELS]));
            mixer.set_bank(SampleBank::synthesized());
            let voice = mixer.voices.get_mut(&Voice::SpeakerTest).unwrap();
            for frame in 0..count {
                let actual = voice.next().unwrap();
                let loop_count = match plays {
                    Plays::Repeat(n) => n as usize,
                    _ => 0,
                };
                let mut index = if frame < loop_count * 159 { frame % 159 } else { frame - loop_count * 159 };
                if loop_count > 0 && (159..167).contains(&index) {
                    index -= 159;
                }
                let expected: [f64; 6] = std::array::from_fn(|ch| f64::from(source.data[index * 6 + ch]));
                assert_eq!(actual, expected, "{plays:?} frame{frame}");
            }
            assert!(voice.next().is_none());
        }
    }

    #[test]
    fn replacing_output_discards_old_voices_and_graph_before_changing_rate() {
        let mut mixer = mixer();
        mixer.apply(&[start(Voice::Impact, 6, Plays::Forever, Group::Impact)]).unwrap();
        mixer.play_direct(Voice::SpeakerTest, SampleKey::new(SampleFolder::Transmission, 1), [1.; 6]);
        let held_sample = mixer.voices[&Voice::Impact].sample.clone();
        let levels = mixer.builds[&mixer.current].levels.clone();
        mixer.set_levels(levels.clone());
        render(&mut mixer, 317);
        assert!(!mixer.voice_order.is_empty());
        assert_eq!(mixer.builds.len(), 2);
        mixer.close_output(48000);
        assert!(mixer.voices.is_empty());
        assert!(mixer.voice_order.is_empty());
        assert_eq!(mixer.builds.len(), 1);
        assert_eq!(mixer.master_order.len(), 1);
        assert_eq!(mixer.builds[&mixer.current].block_position, 0);
        assert!(mixer.builds[&mixer.current].attached.iter().all(|value| !value));
        assert!(render(&mut mixer, 16).iter().all(|value| *value == 0.));
        mixer.set_sample_rate(96000);
        mixer.set_levels(levels.clone());
        mixer.apply(&[start(Voice::Impact, 6, Plays::Forever, Group::Impact)]).unwrap();
        assert!(Arc::ptr_eq(&held_sample, &mixer.voices[&Voice::Impact].sample));
        assert_eq!(mixer.voices[&Voice::Impact].position, 0);
        assert_eq!(mixer.voices[&Voice::Impact].step, sample_step(held_sample.rate, 96000));
        let mut fresh = Mixer::new(96000, mixer.bank.clone(), levels);
        fresh.apply(&[start(Voice::Impact, 6, Plays::Forever, Group::Impact)]).unwrap();
        let restarted = render(&mut mixer, 256);
        assert!(restarted.iter().any(|sample| *sample != 0.));
        assert_eq!(restarted, render(&mut fresh, 256), "replacement has fresh graph/voice phase");
    }

    #[test]
    fn sample_rate_change_rescales_playback() {
        let mut m = mixer();
        m.set_sample_rate(88_200);
        // transmission/1 is 0.0232 s: 1023 frames at 44.1 kHz, about 2046 at 88.2 kHz.
        m.apply(&[Action::Start {
            voice: Voice::Transmission,
            sample: SampleKey::new(SampleFolder::Transmission, 1),
            plays: Plays::Once,
            group: Group::Gear,
        }])
        .unwrap();
        render(&mut m, 2_000);
        assert!(m.is_playing(Voice::Transmission));
        render(&mut m, 60);
        assert!(!m.is_playing(Voice::Transmission));
    }
    fn fractional_mono_bank(float: bool) -> SampleBank {
        let mut bytes = std::io::Cursor::new(Vec::new());
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: if float { 32 } else { 16 },
            sample_format: if float { hound::SampleFormat::Float } else { hound::SampleFormat::Int },
        };
        let mut writer = hound::WavWriter::new(&mut bytes, spec).unwrap();
        for i in 0..67u32 {
            if float {
                writer
                    .write_sample(f32::from_bits(
                        (i.wrapping_mul(2654435761).wrapping_add(1234567) & 0x807fffff) | 0x3f000000,
                    ))
                    .unwrap();
            } else {
                writer.write_sample(((i * 31337 + 8711) % 65536) as i16).unwrap();
            }
        }
        writer.finalize().unwrap();
        let folder = tempfile::tempdir().unwrap();
        std::fs::create_dir(folder.path().join("suspension")).unwrap();
        std::fs::write(folder.path().join("suspension/0.ogg"), bytes.into_inner()).unwrap();
        SampleBank::import(folder.path()).unwrap().0
    }
    #[test]
    fn imported_fractional_mono_preserves_native_call_boundaries_across_split_render() {
        use super::super::native_mono::{Format, Kernel};
        for float in [false, true] {
            for (plays, looping) in [(Plays::Once, 0), (Plays::Repeat(2), 1), (Plays::Forever, 2)] {
                let bank = fractional_mono_bank(float);
                let format = if float { Format::Float } else { Format::Pcm16 };
                let key = SampleKey::new(SampleFolder::Suspension, 0);
                assert_eq!(bank.source_format(key), Some(format));
                let make = || {
                    let mut mixer = Mixer::new(48000, bank.clone(), GroupLevels::empty());
                    mixer.start(Voice::SpeakerTest, key, plays, Route::Direct([1.; CHANNELS]));
                    mixer
                };
                let mut whole = make();
                let expected = render(&mut whole, 2307);
                let mut split = make();
                let mut actual = Vec::new();
                for frames in [1, 13, 59, 997, 7, 1230] {
                    actual.extend(render(&mut split, frames));
                }
                assert_eq!(expected, actual, "float={float} plays={looping}");
                let mode = match Kernel::native() {
                    Kernel::Generic => 0,
                    Kernel::Sse => 1,
                    Kernel::Avx2 => 2,
                    Kernel::Avx512 => 3,
                };
                let line = include_str!("testdata/native_mono_reference.txt")
                    .lines()
                    .nth(mode * 30 + usize::from(float) * 15 + looping * 5)
                    .unwrap();
                let want =
                    u64::from_str_radix(line.split(" hash").nth(1).unwrap().split_whitespace().next().unwrap(), 16)
                        .unwrap();
                let hash = actual
                    .iter()
                    .flat_map(|x| x.to_le_bytes())
                    .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
                assert_eq!(hash, want, "{line}");
                if looping != 2 {
                    assert!(!split.is_playing(Voice::SpeakerTest));
                }
                // Bank replacement does not mutate a held source's format or origin.
                split.set_bank(SampleBank::synthesized());
                if looping == 2 {
                    assert_eq!(split.voices[&Voice::SpeakerTest].origin, SampleOrigin::Imported);
                }
            }
        }
    }
    #[test]
    fn imported_fractional_mono_after_midblock_start_keeps_aligned_callback_origin() {
        use super::super::native_mono::Kernel;
        let bank = fractional_mono_bank(true);
        let mut mixer = Mixer::new(48000, bank, GroupLevels::empty());
        render(&mut mixer, 317);
        mixer.start(
            Voice::SpeakerTest,
            SampleKey::new(SampleFolder::Suspension, 0),
            Plays::Forever,
            Route::Direct([1.; CHANNELS]),
        );
        let actual = render(&mut mixer, 2307);
        let mode = match Kernel::native() {
            Kernel::Generic => 0,
            Kernel::Sse => 1,
            Kernel::Avx2 => 2,
            Kernel::Avx512 => 3,
        };
        let line = include_str!("testdata/native_mono_offset_reference.txt").lines().nth(mode * 30 + 25).unwrap();
        let want =
            u64::from_str_radix(line.split(" hash").nth(1).unwrap().split_whitespace().next().unwrap(), 16).unwrap();
        let hash = actual
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
        assert_eq!(hash, want, "{line}");
    }
}
