//! Playback of the mixer through cpal. The device's channel count and order
//! decide where the speaker buses go (see [`super::channel_map`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};
use thiserror::Error;

use super::channel_map::{choose_format, device_layout, ChannelOrder, DeviceLayout, FormatOption, OutputMatrix};
use super::mixer::Mixer;

mod block;
use block::BlockRenderer;

mod convert;
use convert::{OutputSample, Pcm16Rounding};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod pulse;

/// Stable routing identity and the operating system's display name are separate.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct OutputDevice {
    pub id: String,
    pub name: String,
}

pub fn devices() -> Result<Vec<OutputDevice>, OutputError> {
    #[cfg(target_os = "linux")]
    {
        linux::devices()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(device_names()?.into_iter().map(|name| OutputDevice { id: name.clone(), name }).collect())
    }
}

/// Explicitly retain a previous ALSA choice without displaying its implementation alias.
pub fn legacy_device_label(id: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        linux::legacy_label(id)
    }
    #[cfg(not(target_os = "linux"))]
    {
        id.to_owned()
    }
}

pub fn pulse_sink(id: &str) -> Option<&str> {
    id.strip_prefix("pulse:").filter(|name| !name.is_empty())
}

#[derive(Debug, Error)]
pub enum OutputError {
    #[error("no output device named {0:?}")]
    NoDevice(String),
    #[error("no default output device")]
    NoDefault,
    #[error("the device offers no 2, 4, 6 or 8 channel output in a usable sample format")]
    NoFormat,
    #[error("audio device: {0}")]
    Device(String),
}

/// The name listed for a device whose name cannot be read.
pub const UNKNOWN_DEVICE: &str = "Unknown";

/// Output device names of the default host, in its order
/// (a device whose name cannot be read is [`UNKNOWN_DEVICE`]).
pub fn device_names() -> Result<Vec<String>, OutputError> {
    #[cfg(target_os = "linux")]
    {
        Ok(devices()?.into_iter().map(|device| device.id).collect())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let host = cpal::default_host();
        let devices = host.output_devices().map_err(|e| OutputError::Device(e.to_string()))?;
        Ok(devices.map(|d| d.name().unwrap_or_else(|_| UNKNOWN_DEVICE.to_owned())).collect())
    }
}

fn find_device(host: &cpal::Host, name: &str) -> Result<cpal::Device, OutputError> {
    host.output_devices()
        .map_err(|e| OutputError::Device(e.to_string()))?
        .find(|d| d.name().map(|n| n == name).unwrap_or(false))
        .ok_or_else(|| OutputError::NoDevice(name.to_string()))
}

fn format_options(ranges: &[cpal::SupportedStreamConfigRange]) -> Vec<FormatOption> {
    ranges
        .iter()
        .map(|r| FormatOption {
            channels: r.channels(),
            min_rate: r.min_sample_rate().0,
            max_rate: r.max_sample_rate().0,
            format_rank: format_rank(r.sample_format()),
        })
        .collect()
}

/// The channel count [`open`] uses for `device` when its configured count is not known:
/// [`choose_format`]'s preference among the formats the device offers. CPAL may open PCM handles
/// while querying supported configurations; no output stream is started.
pub fn offered_channels(device: &str, wanted_rate: u32) -> Result<u16, OutputError> {
    #[cfg(target_os = "linux")]
    if let Some(sink) = pulse_sink(device) {
        return Ok(linux::sink(sink)?.channels);
    }
    let host = cpal::default_host();
    let dev = find_device(&host, device)?;
    let ranges: Vec<_> = dev.supported_output_configs().map_err(|e| OutputError::Device(e.to_string()))?.collect();
    choose_format(&format_options(&ranges), wanted_rate, None).map(|c| c.channels).ok_or(OutputError::NoFormat)
}

/// A running output stream; dropping it stops playback.
pub struct Output {
    _stream: Backend,
    pub channels: u16,
    pub sample_rate: u32,
    /// The speaker bus on each device channel.
    pub layout: DeviceLayout,
    lost: Arc<AtomicBool>,
}

impl Output {
    /// Raised once the stream can no longer reach its device (the endpoint was removed or
    /// reconfigured, or the sound server went away). The stream is pinned to its endpoint and
    /// never plays again; the owner reopens the configured device.
    pub fn lost(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.lost)
    }
}

enum Backend {
    Cpal {
        _stream: cpal::Stream,
    },
    #[cfg(target_os = "linux")]
    Pulse {
        _stream: pulse::Playback,
    },
}

/// Channel order of a cpal host: ALSA's own maps on ALSA, WAVE order elsewhere.
fn host_order(host: &cpal::Host) -> ChannelOrder {
    if host.id().name() == "ALSA" {
        ChannelOrder::Alsa
    } else {
        ChannelOrder::Wave
    }
}

/// Preference of a sample format openrigcue can write (lower is better).
fn format_rank(format: SampleFormat) -> Option<u8> {
    match format {
        SampleFormat::F32 => Some(0),
        SampleFormat::I32 => Some(1),
        SampleFormat::I16 => Some(2),
        SampleFormat::U16 => Some(3),
        _ => None,
    }
}

/// Open `device` (or the default device) and render the mixer into it. The format is chosen by
/// [`choose_format`]: the channel count the device is configured for (`configured`)
/// when it offers it, else 5.1, 7.1, quad or stereo, at the mixer's
/// sample rate if possible (the mixer is switched to the device's rate otherwise).
pub fn open(device: Option<&str>, mixer: Arc<Mutex<Mixer>>, configured: Option<u16>) -> Result<Output, OutputError> {
    #[cfg(target_os = "linux")]
    if let Some(sink) = device.and_then(pulse_sink) {
        let sink = linux::sink(sink)?;
        let plan = linux::plan(&sink)?;
        lock(&mixer).set_sample_rate(plan.rate);
        let lost = Arc::new(AtomicBool::new(false));
        let stream = pulse::Playback::open(plan.clone(), mixer, Arc::clone(&lost))?;
        return Ok(Output {
            _stream: Backend::Pulse { _stream: stream },
            channels: plan.layout.len() as u16,
            sample_rate: plan.rate,
            layout: plan.layout,
            lost,
        });
    }
    let host = cpal::default_host();
    let dev = match device {
        Some(name) => find_device(&host, name)?,
        None => host.default_output_device().ok_or(OutputError::NoDefault)?,
    };
    let ranges: Vec<_> = dev.supported_output_configs().map_err(|e| OutputError::Device(e.to_string()))?.collect();
    let options = format_options(&ranges);
    let wanted_rate = lock(&mixer).sample_rate();
    let choice = choose_format(&options, wanted_rate, configured).ok_or(OutputError::NoFormat)?;
    let layout = device_layout(host_order(&host), choice.channels).ok_or(OutputError::NoFormat)?;
    let matrix = OutputMatrix::new(host_order(&host), choice.channels).ok_or(OutputError::NoFormat)?;
    let range = ranges[choice.index];
    let format = range.sample_format();
    let config = range.with_sample_rate(cpal::SampleRate(choice.rate)).config();
    if choice.rate != wanted_rate {
        lock(&mixer).set_sample_rate(choice.rate);
    }
    let lost = Arc::new(AtomicBool::new(false));
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&dev, &config, mixer, matrix, Arc::clone(&lost)),
        SampleFormat::I32 => build::<i32>(&dev, &config, mixer, matrix, Arc::clone(&lost)),
        SampleFormat::I16 => build::<i16>(&dev, &config, mixer, matrix, Arc::clone(&lost)),
        SampleFormat::U16 => build::<u16>(&dev, &config, mixer, matrix, Arc::clone(&lost)),
        _ => return Err(OutputError::NoFormat),
    }?;
    stream.play().map_err(|e| OutputError::Device(e.to_string()))?;
    Ok(Output {
        _stream: Backend::Cpal { _stream: stream },
        channels: choice.channels,
        sample_rate: choice.rate,
        layout,
        lost,
    })
}

fn lock(mixer: &Mutex<Mixer>) -> std::sync::MutexGuard<'_, Mixer> {
    mixer.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A stream of sample type `T` that converts the completed mix to the device format.
fn build<T>(
    dev: &cpal::Device,
    config: &cpal::StreamConfig,
    mixer: Arc<Mutex<Mixer>>,
    matrix: OutputMatrix,
    lost: Arc<AtomicBool>,
) -> Result<cpal::Stream, OutputError>
where
    T: SizedSample + OutputSample,
{
    let mut mixed: Vec<f32> = Vec::new();
    let mut renderer = BlockRenderer::new(matrix);
    let rounding = Pcm16Rounding::native();
    dev.build_output_stream(
        config,
        move |data: &mut [T], _| {
            mixed.resize(data.len(), 0.0);
            renderer.render(&mixer, &mut mixed);
            for (out, v) in data.iter_mut().zip(&mixed) {
                *out = T::from_mix(*v, rounding);
            }
        },
        move |e| {
            tracing::error!("shaker audio stream: {e}");
            if stream_lost(&e) {
                lost.store(true, Ordering::SeqCst);
            }
        },
        None,
    )
    .map_err(|e| OutputError::Device(e.to_string()))
}

/// Whether a stream error means the device is gone for good; an xrun or another backend report
/// leaves the stream running.
fn stream_lost(error: &cpal::StreamError) -> bool {
    matches!(error, cpal::StreamError::DeviceNotAvailable)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// ALSA can advertise a PCM without loading its plugin. The deliberately nonexistent plugin
    /// makes this fail if enumeration tries to open a device, as CPAL's ALSA iterator does.
    #[test]
    fn lists_output_hints_without_opening_devices() {
        const CHILD: &str = "OPENRIGCUE_AUDIO_HINT_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(
                devices().unwrap().iter().map(|device| device.name.as_str()).collect::<Vec<_>>(),
                ["Output hint", "Duplex hint"]
            );
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("alsa.conf");
        std::fs::write(
            &config,
            r#"
pcm.enumeration_only {
    type openrigcue_nonexistent_plugin
    hint { show on description "Output hint" ioid "Output" }
}
pcm.both_directions {
    type openrigcue_nonexistent_plugin
    hint { show on description "Duplex hint" }
}
pcm.null {
    type null
    hint { show on description "Null hint" }
}
"#,
        )
        .unwrap();
        // ALSA scans control devices even when only software hints are requested.
        // Give the child a private /dev so detected host cards cannot require
        // ctl.hw entries in this deliberately minimal software-only fixture.
        let executable = std::env::current_exe().unwrap();
        let mut command = if std::path::Path::new("/dev/snd").exists() {
            let bwrap = ["/usr/bin/bwrap", "/bin/bwrap"]
                .into_iter()
                .find(|path| std::path::Path::new(path).exists())
                .expect("hardware hosts need bubblewrap for the software-only ALSA fixture");
            let mut command = std::process::Command::new(bwrap);
            command.args(["--ro-bind", "/", "/", "--dev", "/dev", "--die-with-parent"]);
            command.arg(&executable);
            command
        } else {
            std::process::Command::new(&executable)
        };
        let status = command
            .args(["--exact", "shakers::output::tests::lists_output_hints_without_opening_devices", "--nocapture"])
            .env(CHILD, "1")
            .env("ALSA_CONFIG_PATH", config)
            .env("PATH", directory.path())
            .status()
            .unwrap();
        assert!(status.success(), "isolated ALSA enumeration check failed");
    }
}
