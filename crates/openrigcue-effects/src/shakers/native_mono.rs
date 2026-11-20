//! Sample resampling chunk boundaries and mono interpolation, including scalar alignment and tails.
//! Multichannel callers reuse the frame planner with their own channel kernels and matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Format {
    /// PCM8/24/32 dispatch to the generic full-phase weighted kernel on every CPU.
    GenericInteger,
    Pcm16,
    Float,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kernel {
    Generic,
    Sse,
    Avx2,
    Avx512,
}
impl Kernel {
    pub(super) fn native() -> Self {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            if std::is_x86_feature_detected!("fma") {
                if std::is_x86_feature_detected!("avx512f") {
                    return Self::Avx512;
                }
                if std::is_x86_feature_detected!("avx2") {
                    return Self::Avx2;
                }
            }
            if std::is_x86_feature_detected!("sse3") {
                return Self::Sse;
            }
        }
        Self::Generic
    }
}
#[derive(Clone, Copy)]
pub(super) struct Mono {
    format: Format,
    kernel: Kernel,
    inverse: u64,
    callback_remaining: usize,
    callback_offset: usize,
    chunk_remaining: usize,
    chunk_offset: usize,
    vector_start: usize,
    vector_end: usize,
    ends_source: bool,
    done: bool,
    // Sound's prepared padding survives the channel's final repeat decrement.
    loop_padding: Option<bool>,
    step: Option<u64>,
    loop_frames: Option<usize>,
    /// The Sound's loop start (0 unless a WAV smpl loop moved it); repeats return here.
    loop_start: usize,
}
impl Mono {
    pub(super) fn new(format: Format, source_rate: u32, output_rate: u32, offset: usize) -> Self {
        Self::with_kernel(
            format,
            if format == Format::GenericInteger { Kernel::Generic } else { Kernel::native() },
            source_rate,
            output_rate,
            offset,
        )
    }
    fn with_kernel(format: Format, kernel: Kernel, source_rate: u32, output_rate: u32, offset: usize) -> Self {
        Self {
            format,
            kernel,
            inverse: inverse(source_rate, output_rate),
            // The caller passes this voice's first active frame in the DSP block.
            // Start-delay trimming reduces the count first,
            // while the kernel still receives the aligned buffer's beginning.
            callback_remaining: 1024 - offset % 1024,
            callback_offset: 0,
            chunk_remaining: 0,
            chunk_offset: 0,
            vector_start: 0,
            vector_end: 0,
            ends_source: false,
            done: false,
            loop_padding: None,
            step: None,
            loop_frames: None,
            loop_start: 0,
        }
    }
    pub(super) fn set_loop_frames(&mut self, frames: Option<usize>) {
        self.loop_frames = frames.filter(|frames| *frames > 0);
    }
    /// A loop start after frame 0 (with its loop end set by [`Self::set_loop_frames`]).
    pub(super) fn set_loop_start(&mut self, start: usize) {
        self.loop_start = start;
    }
    pub(super) fn rate_changed(&mut self, source_rate: u32, output_rate: u32) {
        self.inverse = inverse(source_rate, output_rate);
        // Preserve elapsed frames and source phase when the caller changes its rate.
        // A fresh chunk re-evaluates the new step without changing the callback offset.
        self.chunk_remaining = 0;
        self.step = None;
    }
    pub(super) fn next(
        &mut self,
        samples: &[f32],
        position: &mut u64,
        step: u64,
        repeats: &mut Option<u32>,
    ) -> Option<f32> {
        if self.done || samples.is_empty() {
            return None;
        }
        let (i, phase, looping, vector) = self.prepare(samples.len(), position, step, repeats)?;
        let a = self.sample_index(i, samples.len(), looping).map_or(0.0, |i| samples[i]);
        let b = self.sample_index(i.saturating_add(1), samples.len(), looping).map_or(0.0, |i| samples[i]);
        let value =
            if step as u32 == 0 && phase == 0 { a } else { interpolate(a, b, phase, self.format, self.kernel, vector) };
        self.advance(position, step, repeats, samples.len());
        Some(value)
    }
    pub(super) fn multichannel_indices(
        &mut self,
        frames: usize,
        position: &mut u64,
        step: u64,
        repeats: &mut Option<u32>,
    ) -> Option<(Option<usize>, Option<usize>, u32)> {
        let (index, phase, looping, _) = self.prepare(frames, position, step, repeats)?;
        let next = self.sample_index(index.saturating_add(1), frames, looping);
        let index = self.sample_index(index, frames, looping);
        self.advance(position, step, repeats, frames);
        Some((index, next, phase))
    }
    fn sample_index(&self, index: usize, frames: usize, looping: bool) -> Option<usize> {
        if looping {
            let end = self.loop_frames.unwrap_or(frames);
            // Loop padding is a copy of at most eight frames from the loop
            // start. It remains in the PCM buffer after the
            // final repeat starts.
            let start = self.loop_start.min(end);
            if index >= end && index - end < 8.min(end - start) {
                return Some(start + index - end);
            }
        }
        if index < frames {
            Some(index)
        } else if looping && self.loop_frames.is_none() {
            Some(index % frames)
        } else {
            None
        }
    }
    fn prepare(
        &mut self,
        frames: usize,
        position: &mut u64,
        step: u64,
        repeats: &mut Option<u32>,
    ) -> Option<(usize, u32, bool, bool)> {
        if self.done || frames == 0 {
            return None;
        }
        if self.step != Some(step) {
            self.chunk_remaining = 0;
            self.step = Some(step);
        }
        while self.chunk_remaining == 0 {
            if self.callback_remaining == 0 {
                self.callback_remaining = 1024;
                self.callback_offset = 0;
            }
            // This step uses the separately rounded reciprocal rate;
            // dividing by the forward increment is observably different.
            let boundary_frames = if *repeats == Some(0) { frames } else { self.loop_frames.unwrap_or(frames) };
            let end = (boundary_frames as u64) << 32;
            let distance = end.saturating_sub(*position);
            let boundary = if step > 0x100 { count(distance, self.inverse) } else { u64::MAX };
            self.ends_source = boundary <= self.callback_remaining as u64;
            self.chunk_remaining = if self.ends_source { boundary as usize } else { self.callback_remaining };
            self.chunk_offset = 0;
            let alignment = match self.kernel {
                Kernel::Generic => 1,
                Kernel::Sse => 4,
                _ => 8,
            };
            self.vector_start = ((alignment - self.callback_offset % alignment) % alignment).min(self.chunk_remaining);
            let width = if self.kernel == Kernel::Avx512 && self.format == Format::Float { 16 } else { alignment };
            self.vector_end = self.vector_start + (self.chunk_remaining - self.vector_start) / width * width;
            // An exhausted source may arrive here only after an exact boundary or a rate change.
            if self.chunk_remaining == 0 {
                self.finish(position, repeats, frames);
                if self.done {
                    return None;
                }
                continue;
            }
        }
        let i = (*position >> 32) as usize;
        let phase = *position as u32;
        let looping = *self.loop_padding.get_or_insert(*repeats != Some(0));
        let vector = self.chunk_offset >= self.vector_start && self.chunk_offset < self.vector_end;
        Some((i, phase, looping, vector))
    }
    fn advance(&mut self, position: &mut u64, step: u64, repeats: &mut Option<u32>, frames: usize) {
        *position = position.wrapping_add(step);
        self.chunk_offset += 1;
        self.chunk_remaining -= 1;
        self.callback_offset += 1;
        self.callback_remaining -= 1;
        if self.chunk_remaining == 0 && self.ends_source {
            self.finish(position, repeats, frames);
        }
    }
    fn finish(&mut self, position: &mut u64, repeats: &mut Option<u32>, frames: usize) {
        let loop_frames = self.loop_frames.unwrap_or(frames);
        let start = self.loop_start.min(loop_frames);
        match repeats {
            Some(0) => {
                self.done = true;
                *position = (frames as u64) << 32;
            }
            remaining => {
                let length = (loop_frames - start).max(1) as u64;
                let index = start as u64 + (*position >> 32).saturating_sub(loop_frames as u64) % length;
                *position = (index << 32) | (*position as u32 as u64);
                if let Some(n) = remaining {
                    *n -= 1;
                }
            }
        }
    }
}
fn inverse(source_rate: u32, output_rate: u32) -> u64 {
    ((output_rate as f32 / source_rate as f32) * 4_294_967_296.0_f32) as u64
}
fn count(distance: u64, inverse: u64) -> u64 {
    ((((u128::from(distance) * u128::from(inverse)) >> 32) + u128::from(u32::MAX)) >> 32) as u64
}
// SSE's power-of-two PCM16 normalization commutes exactly for these finite
// inputs. AVX's scalar and vector association differs, so keep both paths.
fn interpolate(a: f32, b: f32, phase: u32, format: Format, kernel: Kernel, vector: bool) -> f32 {
    if kernel == Kernel::Generic {
        let fraction = phase as f32 * 2.328_306_4e-10_f32;
        return a * (1.0 - fraction) + b * fraction;
    }
    let phase = (phase >> 1) as f32;
    let scale = f32::from_bits(0x30000000);
    if kernel == Kernel::Sse {
        return ((b - a) * phase) * scale + a;
    }
    if vector {
        let fraction = phase * scale;
        if format == Format::Pcm16 {
            let (a, b) = (a * 32768.0, b * 32768.0);
            (b - a).mul_add(fraction, a) * f32::from_bits(0x38000000)
        } else {
            (b - a).mul_add(fraction, a)
        }
    } else {
        ((b - a) * phase).mul_add(scale, a)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_preload_preserves_advertised_tail_and_default_loop_region() {
        let data: Vec<f32> = (1..=24).map(|value| if value <= 10 { value as f32 } else { 0.0 }).collect();
        let lines: Vec<Vec<_>> = include_str!("testdata/native_short_loop_reference.txt")
            .lines()
            .map(|line| line.split(',').collect())
            .collect();
        for loops in 0..3 {
            for divisor in [1, 4] {
                let mut state = Mono::with_kernel(Format::Float, Kernel::Generic, 44100, 44100 * divisor, 0);
                state.set_loop_frames(Some(10));
                let mut position = 0;
                let mut repeats = Some(loops);
                let step = (1u64 << 32) / u64::from(divisor);
                for line in lines.iter().filter(|line| line[0] == loops.to_string() && line[1] == divisor.to_string()) {
                    let expected: f32 = line[3].parse().unwrap();
                    assert_eq!(state.next(&data, &mut position, step, &mut repeats), Some(expected), "{line:?}");
                }
                assert!(state.next(&data, &mut position, step, &mut repeats).is_none());
            }
        }
    }

    /// A loop start after frame 0 (a WAV smpl loop): the first pass plays from frame 0 to the
    /// loop end, each repeat returns to the loop start, and the padding after the loop end holds
    /// the frames from the loop start, also during the final full pass.
    #[test]
    fn loop_start_bounds_each_repeat_and_its_padding() {
        let data: Vec<f32> = (1..=24).map(|value| value as f32).collect();
        let mut state = Mono::with_kernel(Format::Float, Kernel::Generic, 44100, 44100, 0);
        state.set_loop_frames(Some(10));
        state.set_loop_start(4);
        let (mut position, mut repeats) = (0, Some(1));
        let mut out = Vec::new();
        while let Some(value) = state.next(&data, &mut position, 1u64 << 32, &mut repeats) {
            out.push(value);
        }
        let expected: Vec<f32> =
            (1..=10).chain(5..=10).chain(5..=10).chain(17..=24).map(|value| value as f32).collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn multichannel_boundaries_match_independent_native_lengths() {
        for line in include_str!("testdata/native_multichannel_boundaries.txt").lines() {
            let fields: Vec<u32> = line.split(',').map(|v| v.parse().unwrap()).collect();
            let (frames, source, output, remaining, expected) = (fields[0], fields[1], fields[2], fields[3], fields[4]);
            let mut state = Mono::new(Format::Float, source, output, 0);
            let mut position = 0;
            let mut repeats = Some(remaining);
            let step = ((source as f32 / output as f32) * 4_294_967_296.0f32) as u64;
            let mut produced = 0;
            while let Some((current, next, _)) =
                state.multichannel_indices(frames as usize, &mut position, step, &mut repeats)
            {
                assert!(current.is_none_or(|i| i < frames as usize));
                assert!(next.is_none_or(|i| i < frames as usize));
                produced += 1;
                assert!(produced <= expected, "{line}");
            }
            assert_eq!(produced, expected, "{line}");
        }
    }
    fn data(index: usize, format: Format) -> f32 {
        match format {
            Format::GenericInteger | Format::Pcm16 => (((index as u32 * 31337 + 8711) % 65536) as i16) as f32 / 32768.0,
            Format::Float => f32::from_bits(
                ((index as u32).wrapping_mul(2654435761).wrapping_add(1234567) & 0x807fffff) | 0x3f000000,
            ),
        }
    }
    #[test]
    fn independent_native_c_all_paths() {
        let log = include_str!("testdata/native_mono_reference.txt");
        let rates = [(44100, 48000), (48000, 44100), (8000, 96000), (96000, 8000), (44100, 44100)];
        let mut lines = log.lines();
        for kernel in [Kernel::Generic, Kernel::Sse, Kernel::Avx2, Kernel::Avx512] {
            for format in [Format::Pcm16, Format::Float] {
                for looping in 0..3 {
                    for (src, dst) in rates {
                        let expected = lines.next().unwrap();
                        let expectedhash = expected.split(" hash").nth(1).unwrap().split_whitespace().next().unwrap();
                        let data: Vec<_> = (0..67).map(|i| data(i, format)).collect();
                        let mut state = Mono::with_kernel(format, kernel, src, dst, 0);
                        let mut pos = 0;
                        let mut repeats = match looping {
                            0 => Some(0),
                            1 => Some(2),
                            _ => None,
                        };
                        let step = ((src as f32 / dst as f32) * 4_294_967_296.0f32) as u64;
                        let mut h = 0xcbf29ce484222325u64;
                        for _ in 0..2307 {
                            let value = state.next(&data, &mut pos, step, &mut repeats).unwrap_or(0.0)
                                * f32::from_bits(0x3f35_0481);
                            for x in [value + 0.0, value + 0.0, 0.0, 0.0, 0.0, 0.0] {
                                for b in x.to_le_bytes() {
                                    h = (h ^ b as u64).wrapping_mul(0x100000001b3);
                                }
                            }
                        }
                        assert_eq!(format!("{h:016x}"), expectedhash, "{expected}");
                    }
                }
            }
        }
    }
    #[test]
    fn boundaries_keep_native_unsigned_loop_rules() {
        // Forward loop: the early boundary clamps integer underflow,
        // removes every whole loop from an overshoot, decrements count only once.
        let mut mono = Mono::with_kernel(Format::Pcm16, Kernel::Avx2, 44100, 48000, 0);
        let mut repeats = Some(2);
        let mut phase = (66u64 << 32) | 0x87654321;
        mono.finish(&mut phase, &mut repeats, 67);
        assert_eq!(phase, 0x87654321);
        assert_eq!(repeats, Some(1));
        phase = (205u64 << 32) | 0x12345678;
        mono.finish(&mut phase, &mut repeats, 67);
        assert_eq!(phase, (4u64 << 32) | 0x12345678);
        assert_eq!(repeats, Some(0));
        mono.finish(&mut phase, &mut repeats, 67);
        assert_eq!(phase, 67u64 << 32);
        assert!(mono.done);
        assert_eq!(count(5708727517396, inverse(8000, 96000)), 15951);
        let step = ((8000f32 / 96000f32) * 4294967296f32) as u64;
        assert_eq!(5708727517396u64.div_ceil(step), 15950);
    }
    #[test]
    fn stationary_phase_does_not_trigger_false_source_boundary() {
        let mut mono = Mono::new(Format::Float, 44100, 48000, usize::MAX);
        let mut phase = 0;
        let mut repeats = Some(0);
        for _ in 0..4097 {
            assert_eq!(mono.next(&[0.25, 0.5], &mut phase, 0, &mut repeats), Some(0.25));
        }
        assert_eq!(phase, 0);
        assert!(!mono.done);
    }
    #[test]
    fn changed_rate_retains_phase_and_elapsed_callback_frames() {
        let mut mono = Mono::new(Format::Float, 44100, 48000, 0);
        let mut phase = 0;
        let mut repeats = None;
        for _ in 0..13 {
            mono.next(&[0.25, 0.5, 0.75], &mut phase, 0xeb333300, &mut repeats).unwrap();
        }
        let old_phase = phase;
        let old_left = mono.callback_remaining;
        let old_offset = mono.callback_offset;
        mono.rate_changed(44100, 96000);
        assert_eq!(phase, old_phase);
        assert_eq!((mono.callback_remaining, mono.callback_offset), (old_left, old_offset));
        mono.next(&[0.25, 0.5, 0.75], &mut phase, 0x75999980, &mut repeats).unwrap();
        assert_eq!(mono.callback_remaining, old_left - 1);
    }
}
