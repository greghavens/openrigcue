//! One mixer processing block, independent of the output device's callback size.

use std::sync::Mutex;

use super::{lock, Mixer, OutputMatrix};

/// Frames per processing block.
pub(super) const FRAMES: usize = 1024;

/// An output owns its cache. Commands arriving while the device consumes a
/// completed block affect the next block: queued commands are applied
/// before a block is processed. Closing an output drops its cache.
pub(super) struct BlockRenderer {
    samples: Vec<f32>,
    offset: usize,
    matrix: OutputMatrix,
}

impl BlockRenderer {
    pub(super) fn new(matrix: OutputMatrix) -> Self {
        let samples = vec![0.0; FRAMES * matrix.len()];
        let offset = samples.len();
        Self { samples, offset, matrix }
    }

    pub(super) fn render(&mut self, mixer: &Mutex<Mixer>, out: &mut [f32]) {
        if self.samples.is_empty() {
            return;
        }
        let mut written = 0;
        while written < out.len() {
            if self.offset == self.samples.len() {
                lock(mixer).render_output(&mut self.samples, &self.matrix);
                self.offset = 0;
            }
            let count = (out.len() - written).min(self.samples.len() - self.offset);
            out[written..written + count].copy_from_slice(&self.samples[self.offset..self.offset + count]);
            written += count;
            self.offset += count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shakers::{
        channel_map::ChannelOrder,
        levels::GroupLevels,
        rig::{Action, Plays, Voice},
        samples::{SampleBank, SampleFolder, SampleKey},
        Group, ShakerTuning, SpeakerProfile,
    };

    fn levels() -> GroupLevels {
        GroupLevels::compute(&SpeakerProfile::built_in("6 Speakers - 5.1").unwrap(), &ShakerTuning::default(), None)
    }

    fn start() -> Action {
        Action::Start {
            voice: Voice::Impact,
            sample: SampleKey::new(SampleFolder::Suspension, 6),
            plays: Plays::Forever,
            group: Group::Impact,
        }
    }

    fn mixer(rate: u32) -> Mutex<Mixer> {
        let mut mixer = Mixer::new(rate, SampleBank::synthesized(), levels());
        mixer.apply(&[start()]).unwrap();
        Mutex::new(mixer)
    }

    #[test]
    fn output_callback_partitions_preserve_complete_native_blocks() {
        for rate in [44100, 48000, 96000] {
            for channels in [2, 4, 6, 8] {
                let matrix = OutputMatrix::new(ChannelOrder::Wave, channels).unwrap();
                let reference = mixer(rate);
                let mut expected = vec![0.0; FRAMES * 3 * usize::from(channels)];
                lock(&reference).render_output(&mut expected, &matrix);
                let actual_mixer = mixer(rate);
                let mut renderer = BlockRenderer::new(matrix);
                let mut actual = Vec::new();
                for frames in [1, 17, 317, 1, 997, 3, 1736] {
                    let mut part = vec![0.0; frames * usize::from(channels)];
                    renderer.render(&actual_mixer, &mut part);
                    actual.extend(part);
                }
                assert!(actual == expected, "rate{rate} channels{channels}");
            }
        }
    }

    #[test]
    fn imported_44100_and_48000_sources_keep_their_resampling_across_output_partitions() {
        for source_rate in [44100, 48000] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::create_dir(directory.path().join("suspension")).unwrap();
            let mut writer = hound::WavWriter::create(
                directory.path().join("suspension/6.ogg"),
                hound::WavSpec {
                    channels: 6,
                    sample_rate: source_rate,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
            .unwrap();
            for sample in 0..1117 * 6 {
                writer.write_sample((sample * 719 % 40001 - 20000) as i16).unwrap();
            }
            writer.finalize().unwrap();
            let bank = SampleBank::import(directory.path()).unwrap().0;
            for rate in [44100, 48000] {
                let make = || {
                    let mut mixer = Mixer::new(rate, bank.clone(), levels());
                    mixer.apply(&[start()]).unwrap();
                    Mutex::new(mixer)
                };
                let matrix = OutputMatrix::new(ChannelOrder::Alsa, 6).unwrap();
                let mut expected = vec![0.0; 3 * FRAMES * 6];
                lock(&make()).render_output(&mut expected, &matrix);
                let mixer = make();
                let mut renderer = BlockRenderer::new(matrix);
                let mut output = Vec::new();
                for frames in [13, 1000, 1019, 17, 1023] {
                    let mut chunk = vec![0.0; frames * 6];
                    renderer.render(&mixer, &mut chunk);
                    output.extend(chunk);
                }
                assert!(output == expected, "source{source_rate} output{rate}");
            }
        }
    }

    #[test]
    fn stop_and_rebuild_cannot_rewrite_the_remainder_of_a_completed_output_block() {
        let matrix = OutputMatrix::new(ChannelOrder::Wave, 6).unwrap();
        let reference = mixer(48000);
        let mut expected = vec![0.0; FRAMES * 6];
        lock(&reference).render_output(&mut expected, &matrix);
        let actual_mixer = mixer(48000);
        let mut renderer = BlockRenderer::new(matrix);
        let mut actual = vec![0.0; 137 * 6];
        renderer.render(&actual_mixer, &mut actual);
        lock(&actual_mixer).apply(&[Action::Stop(Voice::Impact)]).unwrap();
        lock(&actual_mixer).set_levels(GroupLevels::empty());
        let mut rest = vec![0.0; (FRAMES - 137) * 6];
        renderer.render(&actual_mixer, &mut rest);
        actual.extend(rest);
        assert!(actual == expected, "remaining cached frames changed after stop/rebuild");
        let mut next = vec![1.0; FRAMES * 6];
        renderer.render(&actual_mixer, &mut next);
        assert!(next.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn active_loop_restart_enters_next_block_with_fresh_source_phase() {
        let matrix = OutputMatrix::new(ChannelOrder::Wave, 6).unwrap();
        let reference = mixer(48000);
        let mut expected = vec![0.0; FRAMES * 6];
        lock(&reference).render_output(&mut expected, &matrix);
        let mixer = mixer(48000);
        let mut renderer = BlockRenderer::new(matrix);
        let mut output = vec![0.0; 317 * 6];
        renderer.render(&mixer, &mut output);
        lock(&mixer).apply(&[start()]).unwrap();
        let mut rest = vec![0.0; (FRAMES - 317) * 6];
        renderer.render(&mixer, &mut rest);
        output.extend(rest);
        assert!(output == expected, "restart changed cached frame remainder");
        let mut restarted = vec![0.0; FRAMES * 6];
        renderer.render(&mixer, &mut restarted);
        assert!(restarted == expected, "restarted loop begins at source phase zero");
    }

    #[test]
    fn a_new_voice_after_an_idle_partial_callback_enters_the_next_native_block() {
        let matrix = OutputMatrix::new(ChannelOrder::Wave, 6).unwrap();
        let mixer = Mutex::new(Mixer::new(48000, SampleBank::synthesized(), levels()));
        let mut renderer = BlockRenderer::new(matrix.clone());
        renderer.render(&mixer, &mut [0.0; 6]);
        lock(&mixer).apply(&[start()]).unwrap();
        let mut rest = vec![1.0; (FRAMES - 1) * 6];
        renderer.render(&mixer, &mut rest);
        assert!(rest.iter().all(|sample| *sample == 0.0));
        let mut expected = vec![0.0; FRAMES * 6];
        lock(&super::tests::mixer(48000)).render_output(&mut expected, &matrix);
        let mut next = vec![0.0; FRAMES * 6];
        renderer.render(&mixer, &mut next);
        assert!(next == expected, "new source begins at next block");
    }

    #[test]
    fn cached_remainder_does_not_reacquire_the_mixer_or_advance_it() {
        let matrix = OutputMatrix::new(ChannelOrder::Wave, 6).unwrap();
        let mixer = std::sync::Arc::new(mixer(48000));
        let mut renderer = BlockRenderer::new(matrix);
        renderer.render(&mixer, &mut [0.0; 6]);
        let locked = lock(&mixer);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_mixer = mixer.clone();
        let worker = std::thread::spawn(move || {
            let mut rest = vec![0.0; (FRAMES - 1) * 6];
            renderer.render(&worker_mixer, &mut rest);
            done_tx.send(rest.iter().any(|sample| *sample != 0.0)).unwrap();
        });
        let completed = done_rx.recv_timeout(std::time::Duration::from_secs(2));
        drop(locked);
        worker.join().unwrap();
        assert!(completed.expect("cached remainder must not block on the mixer"));
    }

    #[test]
    fn recreated_output_discards_cached_audio_from_the_previous_device() {
        let matrix = OutputMatrix::new(ChannelOrder::Wave, 6).unwrap();
        let mixer = mixer(44100);
        let mut old = BlockRenderer::new(matrix.clone());
        old.render(&mixer, &mut [0.0; 6]);
        lock(&mixer).apply(&[Action::Stop(Voice::Impact)]).unwrap();
        drop(old);
        let mut new = BlockRenderer::new(matrix);
        let mut output = vec![1.0; FRAMES * 6];
        new.render(&mixer, &mut output);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }
}
