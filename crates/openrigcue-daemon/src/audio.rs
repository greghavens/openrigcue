//! Sound output. cpal streams cannot move between threads, so each one lives on its own keeper
//! thread for as long as its owner holds the [`Keeper`].

use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use openrigcue_effects::accessories::alerts::{BeepCommand, BeepPlayer};
use openrigcue_effects::shakers::samples::{decode, Sample};
use openrigcue_effects::shakers::ShakerSystem;

/// A thread that keeps one stream open until the keeper is dropped.
pub struct Keeper {
    release: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Keeper {
    /// Runs `open` on a new thread and keeps what it returns there until dropped; the error when `open` fails.
    fn spawn<T: 'static>(
        name: &str,
        open: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<Keeper, String> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || match open() {
                Ok(stream) => {
                    let _ = ready_tx.send(Ok(()));
                    let _ = release_rx.recv();
                    drop(stream);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Keeper { release: Some(release_tx), thread: Some(thread) }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(crate::i18n::sentence("^name^: the output thread ended", &[("name", name)])),
        }
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        self.release.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Why an output could not be opened, as a sentence with its values ([`crate::i18n::sentence`]) for the UI to
/// show in the selected language; the device's name and the sound system's own text are values.
fn output_failure(error: &openrigcue_effects::shakers::output::OutputError) -> String {
    use openrigcue_effects::shakers::output::OutputError;
    match error {
        OutputError::NoDevice(name) => crate::i18n::sentence("no output device named ^name^", &[("name", name)]),
        OutputError::Device(text) => crate::i18n::sentence("audio device: ^error^", &[("error", text)]),
        OutputError::NoDefault | OutputError::NoFormat => error.to_string(),
    }
}

/// Opens `sound_card` for the shaker mixer with the channel count it is
/// configured for (`configured`, FMOD mixing onto the device in its own speaker format; see
/// [`openrigcue_effects::shakers::output::open`]); the error when it cannot be opened. The flag is raised when
/// the open stream loses its device ([`openrigcue_effects::shakers::output::Output::lost`]).
pub fn open_shaker_output(
    sound_card: &str,
    system: &ShakerSystem,
    configured: Option<u16>,
) -> Result<(Keeper, Arc<AtomicBool>), String> {
    let mixer = system.mixer();
    let card = sound_card.to_owned();
    let (lost_tx, lost_rx) = mpsc::channel();
    let keeper = Keeper::spawn("shaker_output", move || {
        let output = openrigcue_effects::shakers::output::open(Some(&card), mixer, configured)
            .map_err(|e| output_failure(&e))?;
        tracing::info!(sound_card = %card, channels = output.channels, rate = output.sample_rate, "shaker output open");
        let _ = lost_tx.send(output.lost());
        Ok(output)
    })?;
    let lost = lost_rx
        .try_recv()
        .map_err(|_| crate::i18n::sentence("^name^: the output thread ended", &[("name", "shaker_output")]))?;
    Ok((keeper, lost))
}

/// The shift beep on the default sound device: one sound file looped asynchronously until stopped.
pub struct LoopingBeep {
    sound: Arc<Sample>,
    playing: Option<Keeper>,
}

impl LoopingBeep {
    /// The beep file's contents (`bytes`, read from `name`) decoded, or `None` when they do not decode.
    pub fn decode(bytes: &[u8], name: &str) -> Option<LoopingBeep> {
        let sound = decode(bytes, name).ok()?;
        Some(LoopingBeep { sound: Arc::new(sound), playing: None })
    }
}

impl BeepPlayer for LoopingBeep {
    fn apply(&mut self, command: BeepCommand) {
        self.playing = None;
        if command == BeepCommand::StartLoop {
            let sound = Arc::clone(&self.sound);
            self.playing = Keeper::spawn("shift_beep", move || open_loop(sound))
                .inspect_err(|error| tracing::warn!(%error, "cannot open the shift beep output"))
                .ok();
        }
    }
}

/// Which source channel each device channel plays (`None`: silent), as a system mixer places a sound on the
/// default device: channel k of the file on device channel k (a stereo file on the front
/// left/right pair of a surround device, which comes first in both the ALSA and the WAVE order), a mono file on
/// the first two channels; a device with fewer channels than the file gets the mean of all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Direct,
    MonoFront,
    Downmix,
}

fn placement(source_channels: usize, device_channels: usize) -> Placement {
    if source_channels == 1 {
        Placement::MonoFront
    } else if source_channels <= device_channels {
        Placement::Direct
    } else {
        Placement::Downmix
    }
}

/// One device frame from one source frame.
fn place(frame: &[f32], out: &mut [f32], placement: Placement) {
    match placement {
        Placement::Direct => {
            for (k, slot) in out.iter_mut().enumerate() {
                *slot = frame.get(k).copied().unwrap_or(0.0);
            }
        }
        Placement::MonoFront => {
            for (k, slot) in out.iter_mut().enumerate() {
                *slot = if k < 2 { frame[0] } else { 0.0 };
            }
        }
        Placement::Downmix => out.fill(frame.iter().sum::<f32>() / frame.len() as f32),
    }
}

/// The source frame at fractional `position` of a looping sound: linear interpolation between frame
/// `floor(position)` and the next one, the last frame leading back to the first (`SND_LOOP` replays the file).
fn loop_frame(data: &[f32], channels: usize, position: f64, frame: &mut [f32]) {
    let frames = data.len() / channels;
    let index = position as usize % frames;
    let next = (index + 1) % frames;
    let fraction = (position - position.floor()) as f32;
    for (k, slot) in frame.iter_mut().enumerate().take(channels) {
        let a = data[index * channels + k];
        let b = data[next * channels + k];
        *slot = a + (b - a) * fraction;
    }
}

/// The stream configuration for a sound of `rate` Hz: the default output configuration at the sound's own rate
/// when the device offers it with the same channel count and sample format (the system mixer then
/// resamples), else the default configuration as it is.
fn loop_config(device: &cpal::Device, rate: u32) -> Result<cpal::StreamConfig, String> {
    let default = device.default_output_config().map_err(|e| e.to_string())?;
    let own_rate = device.supported_output_configs().ok().and_then(|mut ranges| {
        ranges.find(|range| {
            range.channels() == default.channels()
                && range.sample_format() == default.sample_format()
                && (range.min_sample_rate().0..=range.max_sample_rate().0).contains(&rate)
        })
    });
    Ok(match own_rate {
        Some(range) => range.with_sample_rate(cpal::SampleRate(rate)).config(),
        None => default.config(),
    })
}

/// Loop `sound` on the default output device ([`place`] decides which channel plays what). A device that does
/// not take the sound's rate gets it resampled by linear interpolation ([`loop_frame`]), as the shaker mixer does.
fn open_loop(sound: Arc<Sample>) -> Result<cpal::Stream, String> {
    let device = cpal::default_host().default_output_device().ok_or("no default output device")?;
    let config = loop_config(&device, sound.rate)?;
    let channels = usize::from(config.channels);
    let source_channels = usize::from(sound.channels.max(1));
    let placement = placement(source_channels, channels);
    let step = f64::from(sound.rate) / f64::from(config.sample_rate.0);
    let frames = sound.frames().max(1);
    if sound.data.len() < frames * source_channels {
        return Err("empty shift beep sound".into());
    }
    let mut position = 0.0_f64;
    let mut frame = vec![0.0_f32; source_channels];
    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _| {
                for out in data.chunks_mut(channels) {
                    loop_frame(&sound.data, source_channels, position, &mut frame);
                    place(&frame, out, placement);
                    position = (position + step) % frames as f64;
                }
            },
            |error| tracing::warn!(%error, "shift beep stream error"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stereo beep on a 5.1 default device plays on front left/right only; on a stereo device as is; a mono
    /// sound on the front pair; more channels than the device has are averaged.
    #[test]
    fn beep_channel_placement() {
        let mut out = [9.0; 6];
        place(&[0.25, 0.5], &mut out, placement(2, 6));
        assert_eq!(out, [0.25, 0.5, 0.0, 0.0, 0.0, 0.0]);
        let mut out = [9.0; 2];
        place(&[0.25, 0.5], &mut out, placement(2, 2));
        assert_eq!(out, [0.25, 0.5]);
        let mut out = [9.0; 6];
        place(&[0.5], &mut out, placement(1, 6));
        assert_eq!(out, [0.5, 0.5, 0.0, 0.0, 0.0, 0.0]);
        let mut out = [9.0; 1];
        place(&[0.25, 0.75], &mut out, placement(2, 1));
        assert_eq!(out, [0.5]);
    }

    /// Resampling the looping beep interpolates linearly between neighbouring frames, the last one leading back
    /// to the first; whole positions give the frames themselves.
    #[test]
    fn beep_resampling_interpolates() {
        let data = [0.0, 1.0, 1.0, -1.0, 0.5, 0.5];
        let mut frame = [0.0; 2];
        loop_frame(&data, 2, 1.0, &mut frame);
        assert_eq!(frame, [1.0, -1.0]);
        loop_frame(&data, 2, 0.25, &mut frame);
        assert_eq!(frame, [0.25, 0.5]);
        loop_frame(&data, 2, 2.5, &mut frame);
        assert_eq!(frame, [0.25, 0.75]);
    }
}
