//! Linux output identities. Enumeration only reads metadata; it never opens a PCM.
use std::collections::BTreeSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{OutputDevice, OutputError};
use crate::shakers::channel_map::{DeviceLayout, OutputMatrix};
use crate::shakers::layout::Speaker;

#[derive(Debug, Clone)]
pub(super) struct Sink {
    pub name: String,
    pub description: String,
    pub channels: u16,
    pub rate: u32,
    pub positions: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct Plan {
    pub sink: String,
    pub rate: u32,
    pub layout: DeviceLayout,
    pub matrix: OutputMatrix,
    pub positions: Vec<String>,
}

/// `pactl` talks to PulseAudio and PipeWire's Pulse compatibility server. A failed
/// connection permits the ALSA-only fallback; an empty live sink list does not.
fn sinks() -> Result<Vec<Sink>, OutputError> {
    let mut child = Command::new("pactl")
        .args(["--format=json", "list", "sinks"])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| OutputError::Device(e.to_string()))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(2 * 1024 * 1024).read_to_end(&mut bytes).map(|_| bytes)
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() < Duration::from_secs(2) => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| OutputError::Device("sound server reader ended".into()))?
        .map_err(|e| OutputError::Device(e.to_string()))?;
    if !status.is_some_and(|s| s.success()) {
        return Err(OutputError::Device("sound server is unavailable".into()));
    }
    parse_sinks(&bytes)
}

fn parse_sinks(bytes: &[u8]) -> Result<Vec<Sink>, OutputError> {
    let values: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| OutputError::Device(e.to_string()))?;
    let values = values.as_array().ok_or_else(|| OutputError::Device("invalid sound server sink list".into()))?;
    let mut result = Vec::new();
    for value in values {
        // A physically absent jack is not offered by the desktop's output picker either.
        let active = value["active_port"].as_str();
        let unavailable = value["ports"].as_array().into_iter().flatten().any(|port| {
            port["name"].as_str() == active && matches!(port["availability"].as_str(), Some("not available" | "no"))
        });
        if unavailable {
            continue;
        }
        let Some(name) = value["name"].as_str().filter(|s| !s.is_empty()) else { continue };
        let description = value["description"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| value["properties"]["device.description"].as_str())
            .unwrap_or("Audio output");
        let positions: Vec<String> = value["channel_map"]
            .as_str()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let rate = value["sample_specification"]
            .as_str()
            .unwrap_or("")
            .split_whitespace()
            .find_map(|s| s.strip_suffix("Hz").and_then(|n| n.parse().ok()))
            .unwrap_or(48_000);
        result.push(Sink {
            name: name.into(),
            description: description.into(),
            channels: positions.len() as u16,
            rate,
            positions,
        });
    }
    Ok(result)
}

fn descriptors(sinks: Vec<Sink>) -> Vec<OutputDevice> {
    let mut counts = std::collections::BTreeMap::new();
    for sink in &sinks {
        *counts.entry(sink.description.clone()).or_insert(0usize) += 1;
    }
    let mut ordinal = std::collections::BTreeMap::new();
    sinks
        .into_iter()
        .map(|sink| {
            let index = ordinal.entry(sink.description.clone()).or_insert(0usize);
            *index += 1;
            let name = if counts[&sink.description] > 1 {
                format!("{} ({})", sink.description, index)
            } else {
                sink.description
            };
            OutputDevice { id: format!("pulse:{}", sink.name), name }
        })
        .collect()
}

pub(super) fn devices() -> Result<Vec<OutputDevice>, OutputError> {
    if let Ok(sinks) = sinks() {
        return Ok(descriptors(sinks));
    }
    let hints = alsa::device_name::HintIter::new_str(None, "pcm").map_err(|e| OutputError::Device(e.to_string()))?;
    let mut seen = BTreeSet::new();
    Ok(hints
        .filter(|hint| hint.direction != Some(alsa::Direction::Capture))
        .filter_map(|hint| {
            let id = hint.name?;
            if matches!(id.as_str(), "null" | "default" | "sysdefault" | "pulse" | "pipewire") {
                return None;
            }
            // Expose one hardware-backed PCM per endpoint, not front/surround/upmix aliases.
            if id.contains(':')
                && !["hw:", "plughw:", "sysdefault:", "iec958:", "hdmi:"].iter().any(|prefix| id.starts_with(prefix))
            {
                return None;
            }
            if !seen.insert(id.clone()) {
                return None;
            }
            let name = hint
                .desc
                .map(|s| s.lines().collect::<Vec<_>>().join(" — "))
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "ALSA audio output".into());
            Some(OutputDevice { id, name })
        })
        .collect())
}

pub(super) fn legacy_label(id: &str) -> String {
    if super::pulse_sink(id).is_some() {
        return "Previously selected output (unavailable)".into();
    }
    if matches!(id, "default" | "sysdefault" | "pulse" | "pipewire") {
        return "System default output (previous selection)".into();
    }
    alsa::device_name::HintIter::new_str(None, "pcm")
        .ok()
        .and_then(|mut hints| hints.find(|hint| hint.name.as_deref() == Some(id)).and_then(|hint| hint.desc))
        .map(|s| format!("{} (previous selection)", s.lines().collect::<Vec<_>>().join(" — ")))
        .unwrap_or_else(|| "Previously selected audio output".into())
}

pub(super) fn sink(name: &str) -> Result<Sink, OutputError> {
    sinks()?.into_iter().find(|sink| sink.name == name).ok_or_else(|| OutputError::NoDevice(format!("pulse:{name}")))
}

pub(super) fn plan(sink: &Sink) -> Result<Plan, OutputError> {
    use Speaker::*;
    if !matches!(sink.channels, 2 | 4 | 6 | 8) || !(1..=384_000).contains(&sink.rate) {
        return Err(OutputError::NoFormat);
    }
    let rear_pair = sink.positions.iter().any(|p| p == "rear-left" || p == "rear-right");
    let layout = sink
        .positions
        .iter()
        .map(|position| match position.as_str() {
            "front-left" => Some(FrontLeft),
            "front-right" => Some(FrontRight),
            "front-center" => Some(Center),
            "lfe" => Some(Sub),
            "rear-left" => Some(RearLeft),
            "rear-right" => Some(RearRight),
            "side-left" if !rear_pair => Some(RearLeft),
            "side-right" if !rear_pair => Some(RearRight),
            _ => None,
        })
        .collect();
    let matrix = OutputMatrix::from_positions(&sink.positions).ok_or(OutputError::NoFormat)?;
    Ok(Plan { sink: sink.name.clone(), rate: sink.rate, layout, matrix, positions: sink.positions.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn os_endpoints_keep_routing_separate_from_names_and_hide_disconnected_ports() {
        let values = serde_json::json!([
            {"name":"usb.spdif","description":"USB Audio S/PDIF Output","channel_map":"front-left,front-right","sample_specification":"float32le 2ch 48000Hz"},
            {"name":"usb.speakers","description":"USB Audio Speakers","channel_map":"front-left,front-right"},
            {"name":"usb.headphones","description":"USB Audio Front Headphones","active_port":"phones","ports":[{"name":"phones","availability":"not available"}]}
        ]);
        let sinks = parse_sinks(&serde_json::to_vec(&values).unwrap()).unwrap();
        assert_eq!(
            descriptors(sinks.clone()),
            [
                OutputDevice { id: "pulse:usb.spdif".into(), name: "USB Audio S/PDIF Output".into() },
                OutputDevice { id: "pulse:usb.speakers".into(), name: "USB Audio Speakers".into() }
            ]
        );
        assert_eq!(plan(&sinks[0]).unwrap().sink, "usb.spdif");
        assert_eq!(plan(&sinks[1]).unwrap().sink, "usb.speakers");
    }

    #[test]
    fn native_channel_map_preserves_center_sub_and_rear_routing() {
        let values = br#"[{"name":"x4","description":"Sound Blaster X4","channel_map":"front-left,front-right,front-center,lfe,rear-left,rear-right,side-left,side-right","sample_specification":"s32le 8ch 44100Hz"}]"#;
        let plan = plan(&parse_sinks(values).unwrap()[0]).unwrap();
        use Speaker::*;
        assert_eq!(
            plan.layout,
            [Some(FrontLeft), Some(FrontRight), Some(Center), Some(Sub), Some(RearLeft), Some(RearRight), None, None]
        );
        assert_eq!(plan.rate, 44100);
        let mut output = [0.0; 8];
        plan.matrix.write(&[0.0, 0.0, 0.0, 0.0, 1.0, 0.0], &mut output);
        assert_eq!(output, [0.0, 0.0, 0.0, 0.0, f32::from_bits(0x3ef0_7c85), 0.0, f32::from_bits(0x3f61_ff2e), 0.0]);
    }

    #[test]
    fn captured_desktop_outputs_match_the_os_picker() {
        let sinks = parse_sinks(include_bytes!("fixtures/sinks.json")).unwrap();
        let devices = descriptors(sinks);
        assert_eq!(
            devices.iter().map(|device| device.name.as_str()).collect::<Vec<_>>(),
            [
                "USB Audio S/PDIF Output",
                "USB Audio Speakers",
                "Sound Blaster X4 Analog Stereo",
                "GB202 High Definition Audio Controller Digital Stereo (HDMI)"
            ]
        );
        assert!(devices.iter().all(|device| device.id.starts_with("pulse:alsa_output.")));
        assert_ne!(devices[0].id, devices[1].id);
    }

    #[test]
    fn five_one_side_pair_carries_rear_buses_without_upmix() {
        let sinks = parse_sinks(
            br#"[{"name":"surround","channel_map":"front-left,front-right,front-center,lfe,side-left,side-right"}]"#,
        )
        .unwrap();
        let plan = plan(&sinks[0]).unwrap();
        assert_eq!(&plan.layout[4..], [Some(Speaker::RearLeft), Some(Speaker::RearRight)]);
        let mut output = [0.0; 6];
        plan.matrix.write(&[0.0, 0.0, 0.0, 0.0, 1.0, 0.5], &mut output);
        assert_eq!(output, [0.0, 0.0, 0.0, 0.0, 1.0, 0.5]);
    }

    #[test]
    fn host_output_inventory_read_only() {
        let Some(path) = std::env::var_os("OPENRIGCUE_OUTPUT_INVENTORY") else { return };
        let outputs = devices().unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(&outputs).unwrap()).unwrap();
        assert!(!outputs.is_empty());
    }
}
