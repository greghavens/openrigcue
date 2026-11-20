//! The sound half of the device monitor and the shaker sound-card notifications: the "Default Soundcard" warning
//! (the shaker card is the system's default output, so game sound would go to the shakers) and the surround
//! warning (the card has fewer channels than the speaker layout needs).
//!
//! The device-change check runs on its own thread, woken by a udev watcher for the `sound` subsystem: the card
//! list is the driver list (the output devices the Hardware page offers,
//! [`crate::shakers::list_soundcards`]), card hot-plug comes from udev's netlink broadcasts, the default output is
//! the PipeWire/PulseAudio default sink (`pactl`), and the channel count (the speaker
//! format the device is configured for) is the channel count of the sound server's
//! sink for the device. Also the "Fix for me" routines on the sound server
//! ([`set_soundcard_surround`], [`set_soundcard_default`]).

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use openrigcue_effects::shakers::profiles::surround_shortfall;

use crate::notifications::NotificationKind;
use crate::shakers::ShakersHandle;
use crate::state::{ServiceEvent, ServiceState};

/// Poll period without the event monitor.
pub const POLL_PERIOD: Duration = Duration::from_millis(1500);
/// After a wake-up, the pass waits until no further event arrives for this long.
pub const EVENT_QUIET: Duration = Duration::from_millis(300);
/// How long a pass waits for the card list to change after an event.
pub const EVENT_SETTLE: Duration = Duration::from_secs(1);
/// The card list re-read period while settling.
pub const EVENT_POLL: Duration = Duration::from_millis(200);

/// Title of the default-device notification.
pub const DEFAULT_CARD_TITLE: &str = "Shakers - Default Sound Device";
/// Text of the default-device notification.
pub const DEFAULT_CARD_MESSAGE: &str = "The sound device selected for the shakers is also the system default. Select a separate output for game audio so it does not play through the shakers.";

/// A raised-until-cleared flag that a waiting thread can block on.
#[derive(Debug, Default)]
pub struct AudioEvent {
    raised: Mutex<bool>,
    wake: Condvar,
}

impl AudioEvent {
    /// Raises the flag and wakes the waiters.
    pub fn set(&self) {
        *self.raised.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.wake.notify_all();
    }

    /// Whether the flag is raised, waiting up to `timeout` for it.
    pub fn wait(&self, timeout: Duration) -> bool {
        let raised = self.raised.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (raised, _) =
            self.wake.wait_timeout_while(raised, timeout, |r| !*r).unwrap_or_else(std::sync::PoisonError::into_inner);
        *raised
    }

    /// Lowers the flag.
    pub fn clear(&self) {
        *self.raised.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = false;
    }
}

/// Whether a udev monitor message reports a sound card being added or removed: subsystem `sound`, action `add` or
/// `remove`, device node `.../controlC<n>` (the card's control node; PCM and other nodes of the card are
/// ignored).
///
/// The message is in udev's
/// netlink format (`libudev\0`, magic 0xfeedcafe big-endian, native-endian offset and length of the
/// `KEY=VALUE\0` properties).
pub fn is_sound_card_change(message: &[u8]) -> bool {
    const PREFIX: &[u8] = b"libudev\0";
    const MAGIC: u32 = 0xfeed_cafe;
    let word = |at: usize| message.get(at..at + 4).map(|b| [b[0], b[1], b[2], b[3]]);
    if !message.starts_with(PREFIX) || word(8).map(u32::from_be_bytes) != Some(MAGIC) {
        return false;
    }
    let (Some(offset), Some(length)) = (word(16).map(u32::from_ne_bytes), word(20).map(u32::from_ne_bytes)) else {
        return false;
    };
    let Some(properties) = message.get(offset as usize..(offset as usize).saturating_add(length as usize)) else {
        return false;
    };
    let value = |key: &[u8]| {
        properties.split(|b| *b == 0).find_map(|entry| entry.strip_prefix(key).and_then(|rest| rest.strip_prefix(b"=")))
    };
    let node = value(b"DEVNAME").unwrap_or_default();
    let node_name = node.rsplit(|b| *b == b'/').next().unwrap_or_default();
    value(b"SUBSYSTEM") == Some(b"sound")
        && matches!(value(b"ACTION"), Some(b"add" | b"remove"))
        && node_name.starts_with(b"controlC")
}

/// Watches udev's netlink broadcasts and sets `event` on every sound card added or removed.
#[cfg(target_os = "linux")]
pub fn watch_udev(event: Arc<AudioEvent>, stop: Arc<AtomicBool>) -> std::io::Result<JoinHandle<()>> {
    use std::os::fd::AsRawFd;

    use nix::sys::socket::{
        bind, recv, setsockopt, socket, sockopt, AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType,
    };
    use nix::sys::time::{TimeVal, TimeValLike};

    /// Multicast group of udevd's processed events (libudev's `UDEV_MONITOR_UDEV`).
    const UDEV_GROUP: u32 = 2;
    let fd =
        socket(AddressFamily::Netlink, SockType::Datagram, SockFlag::SOCK_CLOEXEC, SockProtocol::NetlinkKObjectUEvent)?;
    bind(fd.as_raw_fd(), &NetlinkAddr::new(0, UDEV_GROUP))?;
    setsockopt(&fd, sockopt::ReceiveTimeout, &TimeVal::seconds(1))?;
    std::thread::Builder::new().name("usb_monitor_sound".into()).spawn(move || {
        let mut buffer = vec![0u8; 16 * 1024];
        while !stop.load(Ordering::SeqCst) {
            match recv(fd.as_raw_fd(), &mut buffer, MsgFlags::empty()) {
                Ok(n) if is_sound_card_change(&buffer[..n]) => {
                    tracing::info!("usb_device_monitor linux: audio card event");
                    event.set();
                }
                Ok(_) | Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINTR) => {}
                Err(error) => {
                    tracing::debug!(%error, "usb_device_monitor linux audio watch loop failed");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    })
}

/// Whether a line of `pactl subscribe` (run with `LC_ALL=C`) reports a change of the default output or of an
/// output's presence: `Event 'change' on server #<n>` (the default sink changed) and `Event 'new'`/`'remove'` on a
/// sink. Sink `change` events (volume, mute) and every other facility are ignored.
///
/// These are the sound server's notices of a changed default output device and of a changed device state.
pub fn is_default_output_event(line: &str) -> bool {
    let Some(rest) = line.trim().strip_prefix("Event '") else { return false };
    let Some((kind, target)) = rest.split_once("' on ") else { return false };
    match kind {
        "change" => target.starts_with("server #") || target == "server",
        "new" | "remove" => target.starts_with("sink #"),
        _ => false,
    }
}

/// Sets `event` for every default-output event among `lines` (until they end).
pub fn watch_server_events(lines: impl BufRead, event: &AudioEvent) {
    for line in lines.lines().map_while(Result::ok) {
        if is_default_output_event(&line) {
            tracing::debug!(%line, "usb_device_monitor linux: default output event");
            event.set();
        }
    }
}

/// How often a lost sound server is looked for again.
pub const SERVER_RETRY: Duration = Duration::from_secs(1);
/// A subscription that ends younger than this never worked; making the next one is not a restoration.
pub const SERVER_SETTLED: Duration = Duration::from_secs(2);

/// A running `pactl subscribe` whose default-output events set the audio event, so that default device
/// and surround configuration changes wake the audio scan.
///
/// `pactl subscribe` ends with its sound server
/// (a PipeWire or PulseAudio restart), so the watcher sets the audio event when the subscription ends, looks
/// for the server every [`SERVER_RETRY`] and subscribes again once it answers; `restored` then runs, because
/// every stream of the previous server is gone even when the outputs come back under the same names. It does
/// not run after a subscription that ended younger than [`SERVER_SETTLED`] (a server that was never there, or
/// one that refuses subscriptions): the audio event alone lets the device pass compare the card list.
pub struct ServerEvents {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    reader: Mutex<Option<JoinHandle<()>>>,
}

fn subscribe(pactl: &Path) -> std::io::Result<(Child, std::process::ChildStdout)> {
    let mut command = Command::new(pactl);
    command.arg("subscribe").env("LC_ALL", "C").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn()?;
    match child.stdout.take() {
        Some(stdout) => Ok((child, stdout)),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Err(std::io::Error::other("pactl subscribe has no output"))
        }
    }
}

/// Whether the sound server answers (`pactl info`), waiting at most two seconds for it.
fn server_answers(pactl: &Path, stop: &AtomicBool) -> bool {
    let mut command = Command::new(pactl);
    command.arg("info").env("LC_ALL", "C").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else { return false };
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline && !stop.load(Ordering::SeqCst) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn lock_child(child: &Mutex<Option<Child>>) -> std::sync::MutexGuard<'_, Option<Child>> {
    child.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ServerEvents {
    pub fn start(pactl: &Path, event: Arc<AudioEvent>) -> std::io::Result<ServerEvents> {
        ServerEvents::start_with(pactl, event, SERVER_RETRY, SERVER_SETTLED, Box::new(|| {}))
    }

    /// [`ServerEvents::start`] looking for a lost server every `retry` and running `restored` each time a
    /// subscription that lasted `settled` is made again.
    pub fn start_with(
        pactl: &Path,
        event: Arc<AudioEvent>,
        retry: Duration,
        settled: Duration,
        restored: Box<dyn Fn() + Send>,
    ) -> std::io::Result<ServerEvents> {
        let (first, stdout) = subscribe(pactl)?;
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(Some(first)));
        let (flag, slot, pactl) = (Arc::clone(&stop), Arc::clone(&child), pactl.to_path_buf());
        let spawned = std::thread::Builder::new().name("usb_monitor_default_output".into()).spawn(move || {
            let mut stdout = stdout;
            loop {
                let subscribed = Instant::now();
                watch_server_events(std::io::BufReader::new(stdout), &event);
                let worked = subscribed.elapsed() >= settled;
                if let Some(mut ended) = lock_child(&slot).take() {
                    let _ = ended.kill();
                    let _ = ended.wait();
                }
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                tracing::warn!("usb_device_monitor linux: sound server events ended; waiting for the sound server");
                event.set();
                stdout = loop {
                    let waited = Instant::now();
                    while waited.elapsed() < retry {
                        if flag.load(Ordering::SeqCst) {
                            return;
                        }
                        std::thread::sleep(retry.saturating_sub(waited.elapsed()).min(Duration::from_millis(50)));
                    }
                    if flag.load(Ordering::SeqCst) {
                        return;
                    }
                    if !server_answers(&pactl, &flag) {
                        continue;
                    }
                    let Ok((mut next, stdout)) = subscribe(&pactl) else { continue };
                    let mut slot = lock_child(&slot);
                    // `stop` kills the child it finds under this lock; one made after that is ended here.
                    if flag.load(Ordering::SeqCst) {
                        let _ = next.kill();
                        let _ = next.wait();
                        return;
                    }
                    *slot = Some(next);
                    break stdout;
                };
                if worked {
                    tracing::info!("usb_device_monitor linux: sound server events restored");
                    restored();
                }
                event.set();
            }
        });
        match spawned {
            Ok(reader) => Ok(ServerEvents { stop, child, reader: Mutex::new(Some(reader)) }),
            Err(error) => {
                if let Some(mut first) = lock_child(&child).take() {
                    let _ = first.kill();
                    let _ = first.wait();
                }
                Err(error)
            }
        }
    }

    /// Ends `pactl subscribe` (closing its output ends the reader) and joins the reader.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(mut child) = lock_child(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(reader) = self.reader.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            let _ = reader.join();
        }
    }

    /// The running `pactl subscribe`, if any.
    #[cfg(test)]
    fn child_id(&self) -> Option<u32> {
        lock_child(&self.child).as_ref().map(Child::id)
    }
}

impl Drop for ServerEvents {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The id of card `number` (`/sys/class/sound/card<n>/id`).
fn card_id(sys_class_sound: &Path, number: u32) -> Option<String> {
    let id = std::fs::read_to_string(sys_class_sound.join(format!("card{number}")).join("id")).ok()?;
    Some(id.trim().to_owned())
}

/// The number of the card whose id is `id` (the `card<n>` of `/sys/class/sound` holding it).
fn card_number(sys_class_sound: &Path, id: &str) -> Option<u32> {
    std::fs::read_dir(sys_class_sound).ok()?.filter_map(Result::ok).find_map(|entry| {
        let number = entry.file_name().to_str()?.strip_prefix("card")?.parse().ok()?;
        (card_id(sys_class_sound, number).as_deref() == Some(id)).then_some(number)
    })
}

/// Which card an output device name (as the Hardware page lists it) plays on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceCard {
    /// `default`, `sysdefault`, `pulse`, `pipewire`: whatever the system's default output is.
    SystemDefault,
    /// A card by its id (`CARD=<id>`, or the id of `hw:<n>`).
    Card(String),
    Unknown,
}

/// The card of an ALSA device name: `surround51:CARD=Device,DEV=0` → `Device`, `plughw:1,0` → the id of card 1.
pub fn device_card(name: &str, sys_class_sound: &Path) -> DeviceCard {
    let (prefix, rest) = name.split_once(':').unwrap_or((name, ""));
    if rest.is_empty() && matches!(prefix, "default" | "sysdefault" | "pulse" | "pipewire") {
        return DeviceCard::SystemDefault;
    }
    let card = rest
        .split(',')
        .find_map(|part| part.strip_prefix("CARD="))
        .or_else(|| rest.split(',').next().filter(|p| !p.contains('=')));
    match card {
        Some(card) if !card.is_empty() => match card.parse::<u32>() {
            Ok(number) => card_id(sys_class_sound, number).map_or(DeviceCard::Unknown, DeviceCard::Card),
            Err(_) => DeviceCard::Card(card.to_owned()),
        },
        _ => DeviceCard::Unknown,
    }
}

/// The PCM device number of an ALSA device name: `DEV=<n>`, or the second field of `hw:<card>,<n>`.
fn device_number(name: &str) -> Option<u32> {
    let (_, rest) = name.split_once(':')?;
    rest.split(',').find_map(|part| part.strip_prefix("DEV=")).or_else(|| rest.split(',').nth(1))?.parse().ok()
}

/// The sound server's output a device name plays through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkTarget {
    /// The default sink (`default`, `pipewire`, ...).
    Default,
    /// The sink of ALSA card `card` (and PCM device `device` when the name gives one).
    Card { card: u32, device: Option<u32> },
    /// Stable PulseAudio/PipeWire sink name, not the shared ALSA card.
    Named(String),
}

/// The sink a device name plays through; `None` when the name tells no card.
pub fn sink_target(name: &str, sys_class_sound: &Path) -> Option<SinkTarget> {
    if let Some(name) = openrigcue_effects::shakers::output::pulse_sink(name) {
        return Some(SinkTarget::Named(name.into()));
    }
    match device_card(name, sys_class_sound) {
        DeviceCard::SystemDefault => Some(SinkTarget::Default),
        DeviceCard::Card(id) => {
            let card = card_number(sys_class_sound, &id)?;
            Some(SinkTarget::Card { card, device: device_number(name) })
        }
        DeviceCard::Unknown => None,
    }
}

/// A sink's (or card's) ALSA card number in its `pactl -f json` properties.
fn alsa_card(properties: &serde_json::Value) -> Option<u32> {
    ["alsa.card", "api.alsa.card"].iter().find_map(|key| properties.get(*key)?.as_str()?.trim().parse().ok())
}

/// A sink's ALSA PCM device number in its `pactl -f json` properties.
fn alsa_device(properties: &serde_json::Value) -> Option<u32> {
    ["alsa.device", "api.alsa.pcm.device"].iter().find_map(|key| properties.get(*key)?.as_str()?.trim().parse().ok())
}

/// The ALSA card number of sink `default_sink` in the JSON sink list of `pactl -f json list sinks`.
pub fn default_sink_card(default_sink: &str, sinks_json: &str) -> Option<u32> {
    let sinks: serde_json::Value = serde_json::from_str(sinks_json).ok()?;
    let sink = sinks.as_array()?.iter().find(|s| s.get("name").and_then(|n| n.as_str()) == Some(default_sink))?;
    alsa_card(sink.get("properties")?)
}

/// The sink of `target` in the JSON sink list of `pactl -f json list sinks`: the default sink by name, or the
/// first sink on the card (and PCM device, when given).
fn find_sink<'a>(
    target: &SinkTarget,
    default_sink: Option<&str>,
    sinks: &'a serde_json::Value,
) -> Option<&'a serde_json::Value> {
    let sinks = sinks.as_array()?;
    match target {
        SinkTarget::Default => {
            let name = default_sink?;
            sinks.iter().find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name))
        }
        SinkTarget::Card { card, device } => sinks.iter().find(|s| {
            let Some(properties) = s.get("properties") else { return false };
            alsa_card(properties) == Some(*card) && device.is_none_or(|d| alsa_device(properties) == Some(d))
        }),
        SinkTarget::Named(name) => sinks.iter().find(|s| s["name"].as_str() == Some(name)),
    }
}

/// A sink's channel count: the entries of its `channel_map`, else the `<n>ch` of its `sample_specification`.
fn sink_channel_count(sink: &serde_json::Value) -> Option<u16> {
    if let Some(map) = sink.get("channel_map").and_then(|m| m.as_str()).filter(|m| !m.trim().is_empty()) {
        return u16::try_from(map.split(',').count()).ok();
    }
    let spec = sink.get("sample_specification")?.as_str()?;
    spec.split_whitespace().find_map(|part| part.strip_suffix("ch")?.parse().ok())
}

/// The channel count of `target`'s sink in `pactl -f json list sinks` output.
pub fn sink_channels(target: &SinkTarget, default_sink: Option<&str>, sinks_json: &str) -> Option<u16> {
    let sinks: serde_json::Value = serde_json::from_str(sinks_json).ok()?;
    sink_channel_count(find_sink(target, default_sink, &sinks)?)
}

/// The name of `target`'s sink in `pactl -f json list sinks` output.
pub fn sink_name(target: &SinkTarget, default_sink: Option<&str>, sinks_json: &str) -> Option<String> {
    let sinks: serde_json::Value = serde_json::from_str(sinks_json).ok()?;
    Some(find_sink(target, default_sink, &sinks)?.get("name")?.as_str()?.to_owned())
}

/// The profiles of a card in `pactl -f json list cards` output, in the order pactl lists them (the speaker
/// configurations the speaker setup lists).
#[derive(Debug, Default)]
struct Profiles(Vec<(String, Profile)>);

#[derive(Debug, serde::Deserialize)]
struct Profile {
    #[serde(default)]
    description: String,
    #[serde(default = "available_by_default")]
    available: bool,
}

fn available_by_default() -> bool {
    true
}

impl<'de> serde::Deserialize<'de> for Profiles {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Profiles, D::Error> {
        struct InOrder;
        impl<'de> serde::de::Visitor<'de> for InOrder {
            type Value = Profiles;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a map of card profiles")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Profiles, A::Error> {
                let mut profiles = Vec::new();
                while let Some(entry) = map.next_entry::<String, Profile>()? {
                    profiles.push(entry);
                }
                Ok(Profiles(profiles))
            }
        }
        deserializer.deserialize_map(InOrder)
    }
}

#[derive(Debug, serde::Deserialize)]
struct Card {
    name: String,
    #[serde(default)]
    properties: serde_json::Value,
    #[serde(default)]
    profiles: Profiles,
    /// The profile in use, e.g. `output:analog-stereo+input:analog-stereo`.
    #[serde(default)]
    active_profile: Option<String>,
}

/// The surround fix's choice in the speaker setup: the card (by the sound server's name) of ALSA card
/// `number` and its first available profile whose name contains `5.1`,
/// in `pactl -f json list cards` output.
pub fn surround_profile(number: u32, cards_json: &str) -> Option<(String, String)> {
    let cards: Vec<Card> = serde_json::from_str(cards_json).ok()?;
    let card = cards.into_iter().find(|c| alsa_card(&c.properties) == Some(number))?;
    let surround = || card.profiles.0.iter().filter(|(_, p)| p.available && p.description.contains("5.1"));
    // The speaker setup should change the playback side only. A card profile names both sides, and the
    // output-only 5.1 profile is listed first: taking it would switch the card's microphone off.
    let input =
        card.active_profile.as_deref().and_then(|active| active.split('+').find(|side| side.starts_with("input:")));
    let keeps_input = |name: &str| input.is_some_and(|input| name.split('+').any(|side| side == input));
    let (profile, _) = surround().find(|(name, _)| keeps_input(name)).or_else(|| surround().next())?;
    Some((card.name.clone(), profile.clone()))
}

/// The sound server: PipeWire/PulseAudio through `pactl`
/// (default playback device, the speaker configuration of each device).
pub trait SoundServer: Send {
    /// `pactl get-default-sink`.
    fn default_sink(&mut self) -> Option<String>;
    /// `pactl -f json list sinks`.
    fn sinks(&mut self) -> Option<String>;
    /// `pactl -f json list cards`.
    fn cards(&mut self) -> Option<String>;
    /// `pactl <args>` (`set-card-profile`, `set-default-sink`).
    fn command(&mut self, args: &[&str]) -> Result<(), String>;
}

/// The PipeWire/PulseAudio server, through `pactl`.
pub struct Pactl {
    pactl: PathBuf,
}

impl Pactl {
    pub fn new(pactl: PathBuf) -> Pactl {
        Pactl { pactl }
    }

    fn output(&self, args: &[&str]) -> Result<String, String> {
        let output = Command::new(&self.pactl)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }
}

impl SoundServer for Pactl {
    fn default_sink(&mut self) -> Option<String> {
        self.output(&["get-default-sink"]).ok().map(|sink| sink.trim().to_owned())
    }

    fn sinks(&mut self) -> Option<String> {
        self.output(&["-f", "json", "list", "sinks"]).ok()
    }

    fn cards(&mut self) -> Option<String> {
        self.output(&["-f", "json", "list", "cards"]).ok()
    }

    fn command(&mut self, args: &[&str]) -> Result<(), String> {
        self.output(args).map(drop)
    }
}

/// No sound server (no `pactl`).
pub struct NoSoundServer;

impl SoundServer for NoSoundServer {
    fn default_sink(&mut self) -> Option<String> {
        None
    }

    fn sinks(&mut self) -> Option<String> {
        None
    }

    fn cards(&mut self) -> Option<String> {
        None
    }

    fn command(&mut self, _: &[&str]) -> Result<(), String> {
        Err("no sound server (pactl not found)".into())
    }
}

/// A change of the shown notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Raise a red notification of this kind with a title and message.
    Raise { kind: NotificationKind, title: String, message: String },
    /// Withdraw the notifications of this kind.
    Withdraw(NotificationKind),
}

/// What one check reads.
#[derive(Debug, Clone)]
pub struct CheckInput<'a> {
    /// `SHAKEBOX.soundcard`.
    pub sound_card: &'a str,
    /// `SHAKEBOX.profile`.
    pub profile: &'a str,
    /// `SHAKEBOX.default_soundcard` as a boolean.
    pub check_default: bool,
    /// `SHAKEBOX.check_surround` as a boolean.
    pub check_surround: bool,
    /// Whether the selected card plays on the system's default output.
    pub selected_is_default: bool,
    /// The default output.
    pub default_output: Option<String>,
    /// The selected card's channel count, or why there is none.
    pub channels: Result<u16, String>,
}

/// What the checks remember between passes (the previous default output, channel count, sound card and
/// profile).
#[derive(Debug, Clone)]
pub struct DeviceChecks {
    /// `None` before the first pass (equal to no device).
    previous_default: Option<Option<String>>,
    /// `-1` before the first pass.
    previous_channels: i64,
    previous_sound_card: String,
    previous_profile: String,
}

impl Default for DeviceChecks {
    fn default() -> Self {
        DeviceChecks {
            previous_default: None,
            previous_channels: -1,
            previous_sound_card: String::new(),
            previous_profile: String::new(),
        }
    }
}

impl DeviceChecks {
    /// The notification part of one device pass (run while a sound card is selected).
    ///
    /// Default check: when the default output or the selected card changed, the notification is raised if the
    /// selected card is the default and withdrawn otherwise. Surround check: when the channel count or the
    /// profile changed, a profile without requirement withdraws it, one whose minimum the card meets withdraws
    /// it, otherwise it is raised naming the requirement. A channel count that cannot be read ends the checks
    /// with the error. [`DeviceChecks::remember`] follows.
    pub fn check(&self, input: &CheckInput<'_>) -> (Vec<Notice>, Option<String>) {
        let mut notices = Vec::new();
        let error = self.checks(input, &mut notices).err();
        (notices, error)
    }

    /// The end of the pass, after the checks: the default output and the channel count are remembered,
    /// each from a fresh read (a channel read that fails
    /// keeps the old value), then `SHAKEBOX.soundcard` and `SHAKEBOX.profile` as the configuration holds them
    /// now.
    pub fn remember(
        &mut self,
        default_output: Option<String>,
        channels: Result<u16, String>,
        card: &str,
        profile: &str,
    ) {
        self.previous_default = Some(default_output);
        if let Ok(channels) = channels {
            self.previous_channels = i64::from(channels);
        }
        card.clone_into(&mut self.previous_sound_card);
        profile.clone_into(&mut self.previous_profile);
    }

    /// [`DeviceChecks::check`] then [`DeviceChecks::remember`] with the same readings.
    #[cfg(test)]
    fn pass(&mut self, input: &CheckInput<'_>) -> (Vec<Notice>, Option<String>) {
        let outcome = self.check(input);
        self.remember(input.default_output.clone(), input.channels.clone(), input.sound_card, input.profile);
        outcome
    }

    fn checks(&self, input: &CheckInput<'_>, notices: &mut Vec<Notice>) -> Result<(), String> {
        if input.check_default {
            let default_changed = self.previous_default.as_ref() != Some(&input.default_output);
            if default_changed || input.sound_card != self.previous_sound_card {
                tracing::debug!(selected_is_default = input.selected_is_default, "defaultsoundcard state changed");
                notices.push(if input.selected_is_default {
                    Notice::Raise {
                        kind: NotificationKind::DefaultSoundCard,
                        title: DEFAULT_CARD_TITLE.into(),
                        message: DEFAULT_CARD_MESSAGE.into(),
                    }
                } else {
                    Notice::Withdraw(NotificationKind::DefaultSoundCard)
                });
            } else {
                tracing::debug!("defaultsoundcard check: no change detected, skipping notification update");
            }
        }
        if input.check_surround {
            let channels = input.channels.clone()?;
            if i64::from(channels) == self.previous_channels && input.profile == self.previous_profile {
                tracing::debug!("surround check: no change detected, skipping notification update");
                return Ok(());
            }
            notices.push(match surround_shortfall(input.profile, channels) {
                Some(requirement) => {
                    let suffix = if requirement.suggest_stereo { " or switch to a 2-channel profile such as \"2 Speakers - Front Left / Right\"." } else { "." };
                    Notice::Raise {
                        kind: NotificationKind::SurroundRequired {
                            profile: input.profile.to_owned(),
                            label: requirement.label.to_owned(),
                            suggest_stereo: requirement.suggest_stereo,
                        },
                        title: format!("Shakers - {} Required", requirement.label),
                        message: format!(
                            "The \"{}\" profile needs your sound device set to {} in the system sound settings. Click \"Fix automatically\" to have that done for you{suffix}",
                            input.profile, requirement.label
                        ),
                    }
                }
                _ => Notice::Withdraw(NotificationKind::surround()),
            });
        }
        Ok(())
    }
}

/// The driver list.
pub type SoundcardList = Arc<dyn Fn() -> Vec<String> + Send + Sync>;
/// The channel count a device is opened with when the sound server does not tell its configuration.
pub type OfferedChannels = Box<dyn Fn(&str) -> Result<u16, String> + Send>;

/// Where the monitor reads the system.
pub struct DeviceSources {
    pub sys_class_sound: PathBuf,
    pub server: Box<dyn SoundServer>,
    /// `pactl`, for the default-output events of the event-driven scan.
    pub pactl: Option<PathBuf>,
    /// The driver list: `'None'` and the output devices ([`crate::shakers::list_soundcards`]).
    pub soundcards: SoundcardList,
    /// The channel count of a device the sound server has no sink for
    /// ([`openrigcue_effects::shakers::output::offered_channels`]).
    pub offered_channels: OfferedChannels,
}

impl DeviceSources {
    /// `/sys/class/sound`, `pactl` from `PATH`, the output devices of the default audio host.
    pub fn system() -> DeviceSources {
        let sys = PathBuf::from("/sys/class/sound");
        let path = std::env::var_os("PATH").unwrap_or_default();
        let pactl = crate::voice::find_program(&["pactl"], &path);
        let server: Box<dyn SoundServer> = match &pactl {
            Some(pactl) => Box::new(Pactl::new(pactl.clone())),
            None => Box::new(NoSoundServer),
        };
        DeviceSources {
            sys_class_sound: sys,
            server,
            pactl,
            soundcards: Arc::new(crate::shakers::list_soundcards),
            offered_channels: Box::new(|name| {
                openrigcue_effects::shakers::output::offered_channels(name, crate::shakers::MIXER_RATE)
                    .map_err(|e| e.to_string())
            }),
        }
    }

    /// The default output device: the card id of the default sink; `None` when there is no default output on an
    /// ALSA card, or it cannot be told.
    pub fn default_card(&mut self) -> Option<String> {
        let sink = self.server.default_sink()?;
        let sinks = self.server.sinks()?;
        let number = default_sink_card(&sink, &sinks)?;
        card_id(&self.sys_class_sound, number)
    }

    /// The channel count `sound_card` is configured for: the channels of the sound server's sink for it (the
    /// speaker setup of the device in the system's sound settings); `None` when the server has no sink for it.
    pub fn configured_channels(&mut self, sound_card: &str) -> Option<u16> {
        let target = sink_target(sound_card, &self.sys_class_sound)?;
        let default = match target {
            SinkTarget::Default => self.server.default_sink(),
            SinkTarget::Card { .. } | SinkTarget::Named(_) => None,
        };
        sink_channels(&target, default.as_deref(), &self.server.sinks()?)
    }
}

/// The channel count of `sound_card`: the card is looked up in the driver list `soundcards`, then the
/// channel count the device is configured for is read, whether or not
/// the shakers are initialised. A card that is not in the list is an error naming the card and the list.
pub fn get_sound_channels(sound_card: &str, soundcards: &[String], sources: &mut DeviceSources) -> Result<u16, String> {
    if sound_card == "None" || !soundcards.iter().any(|card| card == sound_card) {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_model::python_repr::{repr, repr_str};

        // The error text lists the cards afresh, after the lookup, as
        // [index, name] rows, including [0, 'None'].
        let listed = (sources.soundcards)()
            .into_iter()
            .enumerate()
            .map(|(index, name)| LegacyValue::List(vec![LegacyValue::Int(index as i64), LegacyValue::Str(name)]))
            .collect();
        return Err(format!(
            "get_sound_channels: soundcard {} not found in FMOD driver list {}",
            repr_str(sound_card),
            repr(&LegacyValue::List(listed)),
        ));
    }
    let channels = match sources.configured_channels(sound_card) {
        Some(channels) => Ok(channels),
        None => (sources.offered_channels)(sound_card),
    };
    tracing::debug!(result = ?channels, "get_sound_channels result");
    channels
}

/// "Fix for me" of the surround notification: gives the device its first speaker configuration naming `5.1`.
/// On the sound server: the card
/// of `sound_card` gets its first available profile whose name contains `5.1` (`pactl set-card-profile`).
pub fn set_soundcard_surround(sound_card: &str, sources: &mut DeviceSources) -> Result<(), String> {
    let card = match sink_target(sound_card, &sources.sys_class_sound) {
        Some(SinkTarget::Card { card, .. }) => Some(card),
        Some(SinkTarget::Default) => {
            let sink = sources.server.default_sink();
            sink.zip(sources.server.sinks()).and_then(|(sink, sinks)| default_sink_card(&sink, &sinks))
        }
        Some(SinkTarget::Named(name)) => sources.server.sinks().and_then(|sinks| default_sink_card(&name, &sinks)),
        None => None,
    };
    let card = card.ok_or_else(|| format!("OpenRigCue cannot find the sound card of {sound_card:?}"))?;
    let cards = sources.server.cards().ok_or("Sound is taking too much time to respond")?;
    let (name, profile) =
        surround_profile(card, &cards).ok_or_else(|| format!("no 5.1 configuration for {sound_card:?}"))?;
    sources.server.command(&["set-card-profile", &name, &profile])
}

/// "Fix for me" of the default-device notification (with the
/// card chosen in "Set Soundcard as Default"): makes the device the default playback device. On the sound server:
/// its sink becomes the default sink (`pactl set-default-sink`).
pub fn set_soundcard_default(sound_card: &str, sources: &mut DeviceSources) -> Result<(), String> {
    let target = sink_target(sound_card, &sources.sys_class_sound)
        .filter(|target| *target != SinkTarget::Default)
        .ok_or_else(|| format!("OpenRigCue cannot find the sound card of {sound_card:?}"))?;
    let sinks = sources.server.sinks().ok_or("Sound is taking too much time to respond")?;
    let sink = sink_name(&target, None, &sinks).ok_or_else(|| format!("no output of {sound_card:?}"))?;
    sources.server.command(&["set-default-sink", &sink])
}

/// The running device check thread and the watchers that wake it.
pub struct DeviceMonitor {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
    /// The udev watcher (card hot-plug).
    card_watcher: Option<JoinHandle<()>>,
    /// `pactl subscribe` (default output changes).
    server_events: Option<ServerEvents>,
}

impl DeviceMonitor {
    /// Only the sound-server subscription helper is stopped; the hardware loops are untouched.
    pub fn stop_native_helper(&self) {
        if let Some(events) = &self.server_events {
            events.stop();
        }
    }

    /// Starts the thread when `SHAKEBOX.auto_refresh_soundcard` is on. Under
    /// `CUSTOM_FEATURES.enable_event_driven_device_scan` the udev watcher (card hot-plug) and `pactl subscribe`
    /// (default output changes) wake it; without the udev watcher it
    /// polls every 1.5 s.
    pub fn start(
        state: ServiceState,
        shakers: ShakersHandle,
        sources: DeviceSources,
    ) -> std::io::Result<Option<DeviceMonitor>> {
        let (enabled, event_driven) = {
            let config = state.config.read();
            let settings = config.settings();
            (settings.shakers.auto_refresh_sound_card, settings.features.event_driven_device_scan)
        };
        if !enabled {
            return Ok(None);
        }
        let stop = Arc::new(AtomicBool::new(false));
        // One reselection per event: a lost output, a renamed output and a returned sound server raise this
        // flag and the next pass asks for the reload once (see `card_list_pass`).
        let reselect = Arc::new(AtomicBool::new(false));
        shakers.lock().set_device_monitor(Some(Arc::clone(&reselect)));
        let mut card_watcher = None;
        let mut server_events = None;
        let event = if event_driven {
            let event = Arc::new(AudioEvent::default());
            match watch_udev(Arc::clone(&event), Arc::clone(&stop)) {
                Ok(watcher) => {
                    card_watcher = Some(watcher);
                    tracing::debug!("check_device_changed: event-driven audio scan enabled");
                    server_events = sources.pactl.as_deref().and_then(|pactl| {
                        // A restarted sound server has none of the previous streams, also when its
                        // outputs return under the names the card list already holds.
                        let reopen = Arc::clone(&reselect);
                        let restored = Box::new(move || reopen.store(true, Ordering::SeqCst));
                        match ServerEvents::start_with(
                            pactl,
                            Arc::clone(&event),
                            SERVER_RETRY,
                            SERVER_SETTLED,
                            restored,
                        ) {
                            Ok(events) => Some(events),
                            Err(error) => {
                                tracing::warn!(%error, "no default output events (pactl subscribe)");
                                None
                            }
                        }
                    });
                    // The first check comes once the application has loaded ([`ShakersHandle::app_loaded`]).
                    shakers.lock().set_device_event(Some(Arc::clone(&event)));
                    if shakers.lock().is_app_loaded() {
                        event.set();
                    }
                    Some(event)
                }
                Err(error) => {
                    tracing::warn!(%error, "no sound device event monitor; polling every 1.5 s");
                    None
                }
            }
        } else {
            None
        };
        let flag = Arc::clone(&stop);
        let spawned = std::thread::Builder::new().name("check_device_changed".into()).spawn(move || {
            run(&state, &shakers, sources, event.as_deref(), &reselect, &flag);
            shakers.lock().set_device_monitor(None);
        });
        match spawned {
            Ok(handle) => Ok(Some(DeviceMonitor { stop, handle, card_watcher, server_events })),
            Err(error) => {
                stop.store(true, Ordering::SeqCst);
                if let Some(events) = server_events {
                    events.stop();
                }
                if let Some(watcher) = card_watcher {
                    let _ = watcher.join();
                }
                Err(error)
            }
        }
    }

    /// Stops the check thread and its watchers and waits for all of them.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(events) = self.server_events {
            events.stop();
        }
        let _ = self.handle.join();
        if let Some(watcher) = self.card_watcher {
            let _ = watcher.join();
        }
    }
}

/// Waits for the next pass. Event-driven: waits for the audio event
/// with no timeout, clears it, then keeps waiting [`EVENT_QUIET`] and clearing for as
/// long as events keep coming. Otherwise sleeps [`POLL_PERIOD`]. Returns `false` when stopped.
fn wait_for_pass(event: Option<&AudioEvent>, stop: &AtomicBool) -> bool {
    let Some(event) = event else {
        std::thread::sleep(POLL_PERIOD);
        tracing::debug!("check_device_changed: woke up from 1.5s poll (event monitor disabled)");
        return !stop.load(Ordering::SeqCst);
    };
    // An unbounded wait, in slices so that the monitor can be stopped.
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        if event.wait(Duration::from_millis(500)) {
            break;
        }
    }
    event.clear();
    tracing::debug!("check_device_changed: woke up from OS audio event");
    while event.wait(EVENT_QUIET) {
        event.clear();
        if stop.load(Ordering::SeqCst) {
            return false;
        }
    }
    !stop.load(Ordering::SeqCst)
}

/// Re-reads the card list every [`EVENT_POLL`] until it differs from `cards` or [`EVENT_SETTLE`] has passed.
/// The wait also ends when a reselection is pending (`reselect`, see [`card_list_pass`]).
fn settle(current: &mut Vec<String>, cards: &[String], list: &dyn Fn() -> Vec<String>, reselect: &AtomicBool) {
    let deadline = Instant::now() + EVENT_SETTLE;
    while current.as_slice() == cards && Instant::now() < deadline && !reselect.load(Ordering::SeqCst) {
        std::thread::sleep(EVENT_POLL);
        *current = list();
    }
}

/// The card-list part of a pass: the list is read; after an event
/// the pass waits up to [`EVENT_SETTLE`] for it to change; a changed list asks the UI to reload it.
/// After an event with no change, the list is read again
/// (the audio host enumerates its devices afresh on every listing, so this is a
/// plain re-read) and, under the Windows rules,
/// the pass settles once more. Returns the list the next pass compares with.
///
/// `reselect`: the open output was lost, the selected output was renamed or the sound server came back
/// ([`DeviceMonitor`]'s reselection flag, taken here). The pass then asks for the reload at once and once,
/// whether or not the list differs: the list comparison alone would ask a second time for the same event when
/// it does differ (a second failed `set_soundcard` and its message), and not at all when it does not. A flag
/// raised while the pass is settling ends the settling.
fn card_list_pass(
    cards: &[String],
    event_driven: bool,
    windows: bool,
    reselect: &AtomicBool,
    list: &dyn Fn() -> Vec<String>,
    refresh: &mut dyn FnMut(),
) -> Vec<String> {
    let mut current = list();
    let reselected = |current: &[String], refresh: &mut dyn FnMut()| {
        let asked = reselect.swap(false, Ordering::SeqCst);
        if asked {
            tracing::debug!(from = ?cards, to = ?current, "soundcard reselection requested");
            refresh();
        }
        asked
    };
    if reselected(&current, refresh) {
        return current;
    }
    if event_driven {
        settle(&mut current, cards, list, reselect);
        if reselected(&current, refresh) {
            return current;
        }
    }
    if current.as_slice() != cards {
        tracing::debug!(from = ?cards, to = ?current, "soundcard list changed");
        refresh();
    } else if event_driven {
        current = list();
        if windows && current.as_slice() == cards {
            settle(&mut current, cards, list, reselect);
        }
        if reselected(&current, refresh) {
            return current;
        }
        if current.as_slice() != cards {
            tracing::debug!(from = ?cards, to = ?current, "soundcard list changed after FMOD cache refresh");
            refresh();
        } else {
            tracing::debug!(cards = ?current, "audio device event received, but FMOD soundcard list did not change");
        }
    }
    current
}

fn run(
    state: &ServiceState,
    shakers: &ShakersHandle,
    mut sources: DeviceSources,
    event: Option<&AudioEvent>,
    reselect: &AtomicBool,
    stop: &AtomicBool,
) {
    let soundcards = Arc::clone(&sources.soundcards);
    let list = move || soundcards();
    let mut cards = list();
    let mut checks = DeviceChecks::default();
    let mut selected = None;
    follow_selected_output(state, sources.server.as_mut(), &mut selected);
    while wait_for_pass(event, stop) {
        let mut refresh = || shakers.request_soundcard_refresh();
        if follow_selected_output(state, sources.server.as_mut(), &mut selected) {
            reselect.store(true, Ordering::SeqCst);
        }
        cards = card_list_pass(&cards, event.is_some(), state.platform.is_windows(), reselect, &list, &mut refresh);
        if device_pass(state, shakers, &mut sources, &mut checks) {
            // The channel count read at the end of the pass lists the cards again.
            cards = list();
        }
    }
}

/// The selected sound-server output as the monitor last saw it: its sink and the outputs present with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedOutput {
    sink: String,
    present: Vec<String>,
}

/// The device part of a sound-server sink name: `alsa_output.<device>.<profile>` without the profile.
fn sink_device(name: &str) -> Option<&str> {
    name.rsplit_once('.').map(|(device, _)| device).filter(|device| device.contains('.'))
}

/// The names of the sinks in `pactl -f json list sinks` output.
fn sink_names(sinks_json: &str) -> Option<Vec<String>> {
    let sinks: serde_json::Value = serde_json::from_str(sinks_json).ok()?;
    Some(sinks.as_array()?.iter().filter_map(|sink| sink["name"].as_str().map(str::to_owned)).collect())
}

/// The sink a vanished selection became.
/// The sound server names a sink after its card profile, so the same change (the surround "Fix for me",
/// [`set_soundcard_surround`], or the system's sound settings) removes the selected sink and adds another one
/// for the same device. That one is the answer when it is the only new output of the device; an output that
/// was already there next to the selection is another connector and is never taken.
pub fn renamed_output(seen: &SelectedOutput, now: &[String]) -> Option<String> {
    if now.contains(&seen.sink) {
        return None;
    }
    let device = sink_device(&seen.sink)?;
    let mut new = now.iter().filter(|name| sink_device(name) == Some(device) && !seen.present.contains(name));
    let renamed = new.next()?;
    new.next().is_none().then(|| renamed.clone())
}

/// Keeps `SHAKEBOX.soundcard` on the selected output across a sink rename ([`renamed_output`]): the new name
/// is stored in memory and in the settings file, as selecting it in the dropdown does. Returns whether the
/// selection moved; the caller asks the UI to load the card list again, which opens it.
fn follow_selected_output(
    state: &ServiceState,
    server: &mut dyn SoundServer,
    selected: &mut Option<SelectedOutput>,
) -> bool {
    let sound_card = state.config.read().settings().shakers.sound_card.clone();
    let Some(sink) = openrigcue_effects::shakers::output::pulse_sink(&sound_card) else {
        *selected = None;
        return false;
    };
    let Some(now) = server.sinks().as_deref().and_then(sink_names) else { return false };
    if now.iter().any(|name| name == sink) {
        *selected = Some(SelectedOutput { sink: sink.to_owned(), present: now });
        return false;
    }
    let Some(renamed) = selected.as_ref().filter(|seen| seen.sink == sink).and_then(|seen| renamed_output(seen, &now))
    else {
        return false;
    };
    let value = openrigcue_model::native_value::LegacyValue::Str(format!("pulse:{renamed}"));
    if let Err(error) = state.config.write().write_native_setting("SHAKEBOX", "soundcard", &value) {
        tracing::error!(%error, "cannot keep the shaker sound device selected after its output was renamed");
        return false;
    }
    tracing::info!(from = %sink, to = %renamed, "the selected shaker output was reconfigured; following it");
    *selected = Some(SelectedOutput { sink: renamed, present: now });
    true
}

/// The notification part of a pass (run under the Windows rules, with a sound card selected, once the
/// application has loaded), on the service's settings, the driver list and the sound server; whether it ran.
fn device_pass(
    state: &ServiceState,
    shakers: &ShakersHandle,
    sources: &mut DeviceSources,
    checks: &mut DeviceChecks,
) -> bool {
    let (sound_card, profile, check_default, check_surround) = {
        let config = state.config.read();
        let s = &config.settings().shakers;
        (s.sound_card.clone(), s.speaker_layout.clone(), s.use_default_sound_card, s.check_surround)
    };
    if !state.platform.is_windows() || sound_card == "None" || !shakers.lock().is_app_loaded() {
        return false;
    }
    // The default output device is read only for the default check.
    let named = openrigcue_effects::shakers::output::pulse_sink(&sound_card);
    let default_output = if !check_default {
        None
    } else if named.is_some() {
        sources.server.default_sink()
    } else {
        sources.default_card()
    };
    let selected_is_default = if let Some(named) = named {
        default_output.as_deref() == Some(named)
    } else {
        match device_card(&sound_card, &sources.sys_class_sound) {
            DeviceCard::SystemDefault => true,
            DeviceCard::Card(id) => default_output.as_deref() == Some(id.as_str()),
            DeviceCard::Unknown => false,
        }
    };
    if check_default {
        tracing::debug!(current_default = ?default_output, selected_soundcard = %sound_card, "defaultsoundcard check");
    }
    let channels = get_sound_channels(&sound_card, &(sources.soundcards)(), sources);
    let input = CheckInput {
        sound_card: &sound_card,
        profile: &profile,
        check_default,
        check_surround,
        selected_is_default,
        default_output,
        channels,
    };
    let (notices, error) = checks.check(&input);
    if let Some(error) = error {
        tracing::error!(%error, "Error during check_surround");
    }
    if apply(state, notices) {
        state.announce(ServiceEvent::Notifications);
    }
    let default_now = if named.is_some() { sources.server.default_sink() } else { sources.default_card() };
    let card_for_channels = state.config.read().settings().shakers.sound_card.clone();
    let channels_now = get_sound_channels(&card_for_channels, &(sources.soundcards)(), sources);
    let (card_now, profile_now) = {
        let config = state.config.read();
        let s = &config.settings().shakers;
        (s.sound_card.clone(), s.speaker_layout.clone())
    };
    checks.remember(default_now, channels_now, &card_now, &profile_now);
    true
}

/// Applies the notices to the shown notifications; whether anything changed.
fn apply(state: &ServiceState, notices: Vec<Notice>) -> bool {
    let mut notifications = state.notifications.write();
    let mut changed = false;
    for notice in notices {
        changed |= match notice {
            Notice::Raise { kind, title, message } => {
                tracing::info!(?kind, "sending create_notification");
                notifications.raise_device(kind, &title, message)
            }
            Notice::Withdraw(kind) => {
                tracing::info!(?kind, "sending destroy_notification");
                notifications.withdraw(&kind)
            }
        };
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn udev_message(properties: &[&str]) -> Vec<u8> {
        let mut props = Vec::new();
        for p in properties {
            props.extend_from_slice(p.as_bytes());
            props.push(0);
        }
        let mut m = b"libudev\0".to_vec();
        m.extend_from_slice(&0xfeed_cafeu32.to_be_bytes());
        m.extend_from_slice(&40u32.to_ne_bytes());
        m.extend_from_slice(&40u32.to_ne_bytes());
        m.extend_from_slice(&(props.len() as u32).to_ne_bytes());
        m.resize(40, 0);
        m.extend(props);
        m
    }

    /// The udev watcher reacts only to add/remove of a card's `controlC` node.
    #[test]
    fn sound_card_filter() {
        assert!(is_sound_card_change(&udev_message(&["ACTION=add", "SUBSYSTEM=sound", "DEVNAME=/dev/snd/controlC4"])));
        assert!(is_sound_card_change(&udev_message(&[
            "SUBSYSTEM=sound",
            "ACTION=remove",
            "DEVNAME=/dev/snd/controlC0"
        ])));
        assert!(!is_sound_card_change(&udev_message(&["ACTION=add", "SUBSYSTEM=sound", "DEVNAME=/dev/snd/pcmC4D0p"])));
        assert!(
            !is_sound_card_change(&udev_message(&["ACTION=add", "SUBSYSTEM=sound"])),
            "the card device itself has no node"
        );
        assert!(!is_sound_card_change(&udev_message(&[
            "ACTION=change",
            "SUBSYSTEM=sound",
            "DEVNAME=/dev/snd/controlC4"
        ])));
        assert!(!is_sound_card_change(&udev_message(&["ACTION=add", "SUBSYSTEM=tty", "DEVNAME=/dev/ttyUSB0"])));
        assert!(!is_sound_card_change(b"add@/devices/x/sound/card4/controlC4\0ACTION=add\0SUBSYSTEM=sound\0"));
    }

    fn sys(cards: &[(u32, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for (n, id) in cards {
            let dir = tmp.path().join(format!("card{n}"));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("id"), format!("{id}\n")).unwrap();
        }
        std::fs::create_dir(tmp.path().join("controlC0")).unwrap();
        tmp
    }

    #[test]
    fn cards_and_device_names() {
        let tmp = sys(&[(10, "Ten"), (4, "X4"), (0, "Audio")]);
        assert_eq!(card_number(tmp.path(), "X4"), Some(4));
        assert_eq!(card_number(tmp.path(), "Nope"), None);
        assert_eq!(
            sink_target("surround51:CARD=X4,DEV=0", tmp.path()),
            Some(SinkTarget::Card { card: 4, device: Some(0) })
        );
        assert_eq!(sink_target("plughw:4,1", tmp.path()), Some(SinkTarget::Card { card: 4, device: Some(1) }));
        assert_eq!(sink_target("hw:CARD=Ten", tmp.path()), Some(SinkTarget::Card { card: 10, device: None }));
        assert_eq!(sink_target("pipewire", tmp.path()), Some(SinkTarget::Default));
        assert_eq!(sink_target("hw:CARD=Nope,DEV=0", tmp.path()), None);
        assert_eq!(sink_target("CM106 Like Sound Device Analog Surround 5.1", tmp.path()), None);
        assert_eq!(device_card("surround51:CARD=X4,DEV=0", tmp.path()), DeviceCard::Card("X4".into()));
        assert_eq!(device_card("hw:CARD=Audio,DEV=1", tmp.path()), DeviceCard::Card("Audio".into()));
        assert_eq!(device_card("plughw:4,0", tmp.path()), DeviceCard::Card("X4".into()));
        assert_eq!(device_card("hw:7", tmp.path()), DeviceCard::Unknown);
        assert_eq!(device_card("default", tmp.path()), DeviceCard::SystemDefault);
        assert_eq!(device_card("pipewire", tmp.path()), DeviceCard::SystemDefault);
        assert_eq!(device_card("CM106 Like Sound Device Analog Surround 5.1", tmp.path()), DeviceCard::Unknown);
    }

    #[test]
    fn default_sink_from_pactl_json() {
        let json = r#"[{"name":"alsa_output.usb-Generic.HiFi__Speaker__sink","properties":{"alsa.card":"0","device.api":"alsa"}},
                      {"name":"alsa_output.usb-Creative_X4.analog-stereo","properties":{"api.alsa.card":"4"}},
                      {"name":"bluez_output.00_11","properties":{"device.api":"bluez5"}}]"#;
        assert_eq!(default_sink_card("alsa_output.usb-Creative_X4.analog-stereo", json), Some(4));
        assert_eq!(default_sink_card("alsa_output.usb-Generic.HiFi__Speaker__sink", json), Some(0));
        assert_eq!(default_sink_card("bluez_output.00_11", json), None);
        assert_eq!(default_sink_card("missing", json), None);
        assert_eq!(default_sink_card("x", "not json"), None);
    }

    fn input<'a>(profile: &'a str, channels: Result<u16, String>, selected_is_default: bool) -> CheckInput<'a> {
        CheckInput {
            sound_card: "surround51:CARD=Device,DEV=0",
            profile,
            check_default: true,
            check_surround: true,
            selected_is_default,
            default_output: Some(if selected_is_default { "Device".into() } else { "X4".into() }),
            channels,
        }
    }

    fn kinds(notices: &[Notice]) -> Vec<(bool, NotificationKind)> {
        notices
            .iter()
            .map(|n| match n {
                Notice::Raise { kind, .. } => (true, kind.clone()),
                Notice::Withdraw(kind) => (false, kind.clone()),
            })
            .collect()
    }

    /// The default check: decided on the first pass and whenever the default output or the
    /// selected card changes; raised when the selected card is the default.
    #[test]
    fn default_card_check() {
        let mut checks = DeviceChecks::default();
        let (notices, _) = checks.pass(&input("SRS UShake6", Ok(6), true));
        assert_eq!(
            notices[0],
            Notice::Raise {
                kind: NotificationKind::DefaultSoundCard,
                title: DEFAULT_CARD_TITLE.into(),
                message: DEFAULT_CARD_MESSAGE.into()
            }
        );
        let (notices, _) = checks.pass(&input("SRS UShake6", Ok(6), true));
        assert!(notices.is_empty(), "no change detected");
        let (notices, _) = checks.pass(&input("SRS UShake6", Ok(6), false));
        assert_eq!(kinds(&notices), [(false, NotificationKind::DefaultSoundCard)]);
        let mut other_card = input("SRS UShake6", Ok(6), false);
        other_card.sound_card = "hw:CARD=X4,DEV=0";
        other_card.selected_is_default = true;
        let (notices, _) = checks.pass(&other_card);
        assert_eq!(kinds(&notices), [(true, NotificationKind::DefaultSoundCard)]);
    }

    /// The surround check: the requirement table of `surround_requirement`, the two message
    /// endings, `channels >= requirement` withdraws, a profile without requirement withdraws, no change skips.
    #[test]
    fn surround_check() {
        let mut checks = DeviceChecks::default();
        let mut i = input("SRS UShake6", Ok(2), false);
        i.check_default = false;
        let (notices, _) = checks.pass(&i);
        assert_eq!(
            notices,
            [Notice::Raise {
                kind: NotificationKind::SurroundRequired {
                    profile: "SRS UShake6".into(),
                    label: "6-channel (5.1 Surround)".into(),
                    suggest_stereo: true,
                },
                title: "Shakers - 6-channel (5.1 Surround) Required".into(),
                message: "The \"SRS UShake6\" profile needs your sound device set to 6-channel (5.1 Surround) in the system sound settings. Click \"Fix automatically\" to have that done for you or switch to a 2-channel profile such as \"2 Speakers - Front Left / Right\".".into(),
            }]
        );
        assert!(checks.pass(&i).0.is_empty());
        i.profile = "4 Speakers - FL,FR,RL,RR";
        let (notices, _) = checks.pass(&i);
        let Notice::Raise { title, message, .. } = &notices[0] else { panic!("{notices:?}") };
        assert_eq!(title, "Shakers - 4-channel (Quadraphonic) Required");
        assert!(message.ends_with("to have that done for you."), "{message}");
        i.channels = Ok(4);
        assert_eq!(kinds(&checks.pass(&i).0), [(false, NotificationKind::surround())]);
        i.profile = "2 Speakers - Front Left / Right";
        i.channels = Ok(2);
        assert_eq!(kinds(&checks.pass(&i).0), [(false, NotificationKind::surround())]);
    }

    /// A failing channel count read: logged, notifications untouched, the previous
    /// channel count kept; the default check before it still ran.
    #[test]
    fn unreadable_channels() {
        let mut checks = DeviceChecks::default();
        checks.pass(&input("SRS UShake6", Ok(6), false));
        let (notices, error) = checks.pass(&input("SRS UShake6 Enhanced", Err("not open".into()), true));
        assert_eq!(kinds(&notices), [(true, NotificationKind::DefaultSoundCard)]);
        assert_eq!(error.as_deref(), Some("not open"));
        let (notices, _) = checks.pass(&input("SRS UShake6 Enhanced", Ok(6), true));
        assert!(notices.is_empty(), "profile remembered although the channels were unreadable");
    }

    /// Raise and withdraw notices on the service's notification list:
    /// every raise stacks a new notification, all are removed by
    /// their kind, kept whatever game runs.
    #[test]
    fn notices_reach_the_notification_list() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        let state = ServiceState::new(
            ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()),
            OriginalPlatform::Windows,
        );
        let raise = |message: &str| Notice::Raise {
            kind: NotificationKind::SurroundRequired {
                profile: "2 Speakers - Center / Sub".into(),
                label: "5.1 Surround".into(),
                suggest_stereo: false,
            },
            title: "Shakers - 5.1 Surround Required".into(),
            message: message.into(),
        };
        assert!(apply(&state, vec![raise("a")]));
        assert!(apply(&state, vec![raise("a")]));
        assert!(apply(&state, vec![raise("b")]));
        let items = state.notifications.read().items.clone();
        assert_eq!(items.iter().map(|i| i.message.as_str()).collect::<Vec<_>>(), ["a", "a", "b"]);
        state.notifications.write().tidy(Some("acc"), None, true, &std::collections::BTreeSet::new());
        assert_eq!(state.notifications.read().items.len(), 3);
        assert!(apply(&state, vec![Notice::Withdraw(NotificationKind::surround())]));
        assert!(!apply(&state, vec![Notice::Withdraw(NotificationKind::surround())]));
    }

    /// Default-output events on `pactl subscribe` lines: the
    /// server change (default sink) and sink new/remove wake the scan; volume changes and other facilities do not.
    #[test]
    fn default_output_event_filter() {
        assert!(is_default_output_event("Event 'change' on server #4294967295"));
        assert!(is_default_output_event("Event 'new' on sink #57\n"));
        assert!(is_default_output_event("Event 'remove' on sink #57"));
        assert!(!is_default_output_event("Event 'change' on sink #57"));
        assert!(!is_default_output_event("Event 'new' on sink-input #120"));
        assert!(!is_default_output_event("Event 'change' on source #58"));
        assert!(!is_default_output_event("Event 'new' on client #301"));
        assert!(!is_default_output_event("Ereignis 'change' auf Server #1"));
    }

    #[cfg(target_os = "linux")]
    fn fake_subscription(root: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = root.join("pactl");
        // exec makes the controlled sleep process the direct owned child.
        std::fs::write(&script, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        script
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn server_events_drop_and_explicit_stop_reap_only_the_owned_child() {
        let temp = tempfile::tempdir().unwrap();
        let script = fake_subscription(temp.path());
        let first = ServerEvents::start(&script, Arc::new(AudioEvent::default())).unwrap();
        let second = ServerEvents::start(&script, Arc::new(AudioEvent::default())).unwrap();
        let first_pid = first.child_id().unwrap();
        let second_pid = second.child_id().unwrap();
        drop(first);
        assert!(!Path::new(&format!("/proc/{first_pid}")).exists());
        assert!(Path::new(&format!("/proc/{second_pid}")).exists());
        second.stop();
        second.stop();
        assert!(!Path::new(&format!("/proc/{second_pid}")).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn server_events_immediate_exit_helper() {
        let Some(root) = std::env::var_os("OPENRIGCUE_SUBSCRIPTION_TEST_ROOT") else { return };
        let root = Path::new(&root);
        let events = ServerEvents::start(&root.join("pactl"), Arc::new(AudioEvent::default())).unwrap();
        let pid = events.child_id().unwrap();
        std::fs::write(root.join("child.pid"), pid.to_string()).unwrap();
        events.stop();
        std::process::exit(0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn server_events_cleanup_precedes_immediate_process_exit() {
        let temp = tempfile::tempdir().unwrap();
        fake_subscription(temp.path());
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sound_devices::tests::server_events_immediate_exit_helper", "--nocapture"])
            .env("OPENRIGCUE_SUBSCRIPTION_TEST_ROOT", temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        let pid = std::fs::read_to_string(temp.path().join("child.pid")).unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "subscription outlived its exited parent");
    }

    /// A default-output
    /// change wakes the device pass's event wait; without an event the wait goes on (it has no
    /// timeout) until stopped.
    #[test]
    fn default_output_change_wakes_the_pass() {
        let event = AudioEvent::default();
        let stop = AtomicBool::new(false);
        watch_server_events(
            std::io::Cursor::new("Event 'change' on sink #57\nEvent 'change' on server #4294967295\n"),
            &event,
        );
        let started = Instant::now();
        assert!(wait_for_pass(Some(&event), &stop));
        assert!(started.elapsed() < Duration::from_millis(1400), "woken by the event, not a poll");
        assert!(started.elapsed() >= EVENT_QUIET, "quiet debounce after the wake-up");
        assert!(!event.wait(Duration::from_millis(1)), "event consumed");
        watch_server_events(std::io::Cursor::new("Event 'change' on sink #57\n"), &event);
        assert!(!event.wait(Duration::from_millis(1)), "volume change does not wake");
        let waiter = std::thread::scope(|scope| {
            let handle = scope.spawn(|| wait_for_pass(Some(&event), &stop));
            std::thread::sleep(Duration::from_millis(700));
            assert!(!handle.is_finished(), "no timeout on the event wait");
            stop.store(true, Ordering::SeqCst);
            handle.join().unwrap()
        });
        assert!(!waiter);
    }

    /// The quiet wait has no upper bound: events closer together than
    /// [`EVENT_QUIET`] keep the pass waiting past [`EVENT_SETTLE`].
    #[test]
    fn quiet_debounce_is_unbounded() {
        let event = Arc::new(AudioEvent::default());
        let stop = AtomicBool::new(false);
        event.set();
        let feeder = Arc::clone(&event);
        let started = Instant::now();
        let burst = std::thread::spawn(move || {
            for _ in 0..8 {
                std::thread::sleep(Duration::from_millis(150));
                feeder.set();
            }
        });
        assert!(wait_for_pass(Some(&event), &stop));
        burst.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(1200 + 300), "{:?}", started.elapsed());
    }

    fn lister(lists: Vec<Vec<&'static str>>) -> impl Fn() -> Vec<String> {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        move || {
            let n = calls.fetch_add(1, Ordering::SeqCst).min(lists.len() - 1);
            lists[n].iter().map(|s| (*s).to_owned()).collect()
        }
    }

    /// The card-list pass: after an event the list is re-read every 0.2 s for up to 1 s until it
    /// changes; a changed list sets `refresh_soundcard`. An unchanged list is read again
    /// and, under the Windows rules, settles once more; polling mode reads it once.
    #[test]
    fn card_list_settles_after_an_event() {
        let cards = vec!["0 Audio".to_owned()];
        let mut refreshed = 0;
        let started = Instant::now();
        let changed = lister(vec![vec!["0 Audio"], vec!["0 Audio"], vec!["0 Audio", "4 X4"]]);
        let now = card_list_pass(&cards, true, true, &AtomicBool::new(false), &changed, &mut || refreshed += 1);
        assert_eq!(now, ["0 Audio", "4 X4"]);
        assert_eq!(refreshed, 1);
        let took = started.elapsed();
        assert!(took >= 2 * EVENT_POLL && took < EVENT_SETTLE, "{took:?}");

        let started = Instant::now();
        let same = lister(vec![vec!["0 Audio"]]);
        assert_eq!(card_list_pass(&cards, true, true, &AtomicBool::new(false), &same, &mut || refreshed += 1), cards);
        assert_eq!(refreshed, 1);
        assert!(started.elapsed() >= 2 * EVENT_SETTLE, "two settles on Windows: {:?}", started.elapsed());

        let started = Instant::now();
        assert_eq!(card_list_pass(&cards, true, false, &AtomicBool::new(false), &same, &mut || refreshed += 1), cards);
        let took = started.elapsed();
        assert!(took >= EVENT_SETTLE && took < 2 * EVENT_SETTLE, "one settle elsewhere: {took:?}");

        let started = Instant::now();
        let later = lister(vec![vec!["0 Audio"], vec!["0 Audio", "4 X4"]]);
        assert_eq!(
            card_list_pass(&cards, false, true, &AtomicBool::new(false), &later, &mut || refreshed += 1),
            cards,
            "polling: one read"
        );
        assert!(started.elapsed() < EVENT_POLL);
        let changed = lister(vec![vec!["4 X4"]]);
        card_list_pass(&cards, false, true, &AtomicBool::new(false), &changed, &mut || refreshed += 1);
        assert_eq!(refreshed, 2);
    }

    const SINKS: &str = r#"[
        {"name":"alsa_output.usb-Generic.analog-stereo","properties":{"alsa.card":"0","alsa.device":"0"},
         "sample_specification":"s16le 2ch 48000Hz","channel_map":"front-left,front-right"},
        {"name":"alsa_output.usb-X4.hdmi","properties":{"alsa.card":"4","alsa.device":"3"},
         "sample_specification":"s32le 8ch 48000Hz","channel_map":""},
        {"name":"alsa_output.usb-X4.analog-surround-51","properties":{"api.alsa.card":"4","alsa.device":"0"},
         "channel_map":"front-left,front-right,rear-left,rear-right,front-center,lfe"}]"#;

    /// A sound server answering from fixed `pactl` outputs and recording its commands.
    struct FakeServer {
        default_sink: Option<String>,
        sinks: String,
        cards: String,
        default_reads: Arc<std::sync::atomic::AtomicUsize>,
        commands: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl FakeServer {
        fn new(default_sink: &str) -> FakeServer {
            FakeServer {
                default_sink: Some(default_sink.into()),
                sinks: SINKS.into(),
                cards: String::new(),
                default_reads: Arc::default(),
                commands: Arc::default(),
            }
        }
    }

    impl SoundServer for FakeServer {
        fn default_sink(&mut self) -> Option<String> {
            self.default_reads.fetch_add(1, Ordering::SeqCst);
            self.default_sink.clone()
        }
        fn sinks(&mut self) -> Option<String> {
            Some(self.sinks.clone())
        }
        fn cards(&mut self) -> Option<String> {
            Some(self.cards.clone())
        }
        fn command(&mut self, args: &[&str]) -> Result<(), String> {
            self.commands.lock().unwrap().push(args.iter().map(|a| (*a).to_owned()).collect());
            Ok(())
        }
    }

    fn sources(sys: &Path, server: FakeServer, cards: &[&str]) -> DeviceSources {
        let cards: Vec<String> = cards.iter().map(|c| (*c).to_owned()).collect();
        DeviceSources {
            sys_class_sound: sys.to_path_buf(),
            server: Box::new(server),
            pactl: None,
            soundcards: Arc::new(move || cards.clone()),
            offered_channels: Box::new(|_| Ok(6)),
        }
    }

    /// The sink of a device name and its channel count: the default sink for the system default, the first sink
    /// on the card (and PCM device) otherwise; `channel_map` entries, else the `sample_specification`.
    #[test]
    fn sink_channels_from_pactl_json() {
        let default = Some("alsa_output.usb-Generic.analog-stereo");
        assert_eq!(sink_channels(&SinkTarget::Default, default, SINKS), Some(2));
        assert_eq!(sink_channels(&SinkTarget::Card { card: 4, device: Some(0) }, None, SINKS), Some(6));
        assert_eq!(sink_channels(&SinkTarget::Card { card: 4, device: Some(3) }, None, SINKS), Some(8));
        assert_eq!(sink_channels(&SinkTarget::Card { card: 4, device: None }, None, SINKS), Some(8));
        assert_eq!(sink_channels(&SinkTarget::Card { card: 7, device: None }, None, SINKS), None);
        assert_eq!(
            sink_name(&SinkTarget::Card { card: 4, device: Some(0) }, None, SINKS).as_deref(),
            Some("alsa_output.usb-X4.analog-surround-51")
        );
    }

    #[test]
    fn named_sound_device_keeps_two_outputs_on_one_card_distinct() {
        let temp = tempfile::tempdir().unwrap();
        let mut server = FakeServer::new("usb.speakers");
        server.sinks = r#"[
            {"name":"usb.spdif","channel_map":"front-left,front-right","properties":{"alsa.card":"0","alsa.device":"3"}},
            {"name":"usb.speakers","channel_map":"front-left,front-right,front-center,lfe,rear-left,rear-right","properties":{"alsa.card":"0","alsa.device":"0"}}
        ]"#.into();
        let commands = server.commands.clone();
        let mut sources = sources(temp.path(), server, &["None", "pulse:usb.spdif", "pulse:usb.speakers"]);
        assert_eq!(sources.configured_channels("pulse:usb.spdif"), Some(2));
        assert_eq!(sources.configured_channels("pulse:usb.speakers"), Some(6));
        set_soundcard_default("pulse:usb.spdif", &mut sources).unwrap();
        assert_eq!(*commands.lock().unwrap(), [vec!["set-default-sink".to_owned(), "usb.spdif".to_owned()]]);
        assert!(set_soundcard_default("pulse:missing", &mut sources).is_err());
        assert_eq!(commands.lock().unwrap().len(), 1);
    }

    #[test]
    fn linux_sound_device_default_check_compares_endpoints_not_shared_card() {
        use crate::shakers::SilentBeep;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_effects::shakers::SampleBank;
        let state = ServiceState::new(
            ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()),
            OriginalPlatform::Windows,
        );
        state.config.write().update(|config| config.settings.shakers.sound_card = "pulse:usb.spdif".into());
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let temp = tempfile::tempdir().unwrap();
        let mut server = FakeServer::new("usb.speakers");
        server.sinks = r#"[{"name":"usb.spdif","channel_map":"front-left,front-right","properties":{"alsa.card":"0"}},{"name":"usb.speakers","channel_map":"front-left,front-right","properties":{"alsa.card":"0"}}]"#.into();
        let mut sources = sources(temp.path(), server, &["None", "pulse:usb.spdif", "pulse:usb.speakers"]);
        shakers.app_loaded();
        let mut checks = DeviceChecks::default();
        assert!(device_pass(&state, &shakers, &mut sources, &mut checks));
        assert!(!state.notifications.read().items.iter().any(|notice| notice.title == DEFAULT_CARD_TITLE));
        state.config.write().update(|config| config.settings.shakers.sound_card = "pulse:usb.speakers".into());
        assert!(device_pass(&state, &shakers, &mut sources, &mut checks));
        assert!(state.notifications.read().items.iter().any(|notice| notice.title == DEFAULT_CARD_TITLE));
    }

    /// The channel count of a card: a card missing from the driver list (or `None`) is an error; otherwise the channels the
    /// device is configured for (its sink), whether or not a card is open; without a sink, the count the device
    /// is opened with.
    #[test]
    fn get_sound_channels_reads_the_configured_speaker_setup() {
        let tmp = sys(&[(0, "Generic"), (4, "X4")]);
        let list = ["None", "hw:CARD=X4,DEV=0", "pipewire", "hw:CARD=Generic,DEV=1"];
        let mut s = sources(tmp.path(), FakeServer::new("alsa_output.usb-Generic.analog-stereo"), &list);
        let cards = (s.soundcards)();
        assert_eq!(get_sound_channels("hw:CARD=X4,DEV=0", &cards, &mut s), Ok(6));
        assert_eq!(get_sound_channels("pipewire", &cards, &mut s), Ok(2));
        assert_eq!(get_sound_channels("hw:CARD=Generic,DEV=1", &cards, &mut s), Ok(6), "no sink: offered count");
        let missing = get_sound_channels("hw:CARD=Gone,DEV=0", &cards, &mut s).unwrap_err();
        assert!(missing.starts_with("get_sound_channels: soundcard 'hw:CARD=Gone,DEV=0' not found in FMOD driver list"));
        assert!(get_sound_channels("None", &cards, &mut s).is_err());
    }

    #[test]
    fn native_sound_channel_errors_use_current_indexed_list_and_python_repr() {
        use std::sync::atomic::AtomicUsize;

        let tmp = sys(&[]);
        for selected in ["None", "Missing"] {
            let server = FakeServer::new("unused");
            let server_reads = server.default_reads.clone();
            let mut sources = sources(tmp.path(), server, &[]);
            let reads = Arc::new(AtomicUsize::new(0));
            let count = reads.clone();
            sources.soundcards = Arc::new(move || {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    vec!["None".into(), "Old".into()]
                } else {
                    vec!["None".into(), "Missing".into(), "O'Brien".into(), "A\" B".into(), "tab\tÄ".into()]
                }
            });
            sources.offered_channels = Box::new(|_| panic!("an invalid index must not query channels"));
            let lookup = (sources.soundcards)();
            let error = get_sound_channels(selected, &lookup, &mut sources).unwrap_err();
            let expected = format!(
                "get_sound_channels: soundcard '{selected}' not found in FMOD driver list [[0, 'None'], [1, 'Missing'], [2, \"O'Brien\"], [3, 'A\" B'], [4, 'tab\\tÄ']]"
            );
            assert_eq!(error, expected);
            assert_eq!(reads.load(Ordering::SeqCst), 2, "error formatting re-reads the driver list");
            assert_eq!(server_reads.load(Ordering::SeqCst), 0, "the index guard precedes server queries");
        }
    }

    #[test]
    fn native_sound_channel_valid_index_does_not_read_the_error_list() {
        let tmp = sys(&[]);
        let mut sources = sources(tmp.path(), FakeServer::new("unused"), &[]);
        sources.soundcards = Arc::new(|| panic!("the error list is only evaluated for a negative index"));
        sources.offered_channels = Box::new(|_| Ok(4));
        assert_eq!(get_sound_channels("Valid", &["None".into(), "Valid".into()], &mut sources), Ok(4));
    }

    /// "Fix for me": the surround fix gives the card its first available `5.1` configuration in the order the
    /// server lists them; the default fix makes the chosen device's sink the default one.
    #[test]
    fn fix_for_me_routines() {
        let tmp = sys(&[(0, "Generic"), (4, "X4")]);
        let mut server = FakeServer::new("alsa_output.usb-Generic.analog-stereo");
        server.cards = r#"[
            {"name":"alsa_card.usb-Generic","properties":{"alsa.card":"0"},"profiles":{"output:analog-stereo":{"description":"Analog Stereo Output"}}},
            {"name":"alsa_card.usb-X4","properties":{"api.alsa.card":"4"},"profiles":{
                "output:analog-stereo":{"description":"Analog Stereo Output","available":true},
                "output:z-surround-51":{"description":"Analog Surround 5.1 Output","available":false},
                "output:y-surround-51":{"description":"Digital Surround 5.1 (HDMI) Output","available":true},
                "output:a-surround-51":{"description":"Analog Surround 5.1 Output","available":true}}}]"#
            .into();
        let commands = Arc::clone(&server.commands);
        let mut s = sources(tmp.path(), server, &["None"]);
        set_soundcard_surround("surround51:CARD=X4,DEV=0", &mut s).unwrap();
        assert!(set_soundcard_surround("hw:CARD=Generic,DEV=0", &mut s).is_err(), "no 5.1 configuration");
        set_soundcard_default("hw:CARD=X4,DEV=0", &mut s).unwrap();
        assert!(set_soundcard_default("pipewire", &mut s).is_err());
        assert_eq!(
            *commands.lock().unwrap(),
            [
                vec!["set-card-profile", "alsa_card.usb-X4", "output:y-surround-51"],
                vec!["set-default-sink", "alsa_output.usb-X4.analog-surround-51"],
            ]
        );
    }

    /// The surround fix keeps the card's recording side: with a microphone profile active it takes the 5.1
    /// profile with the same input, not the output-only one listed before it (which switched the input off).
    #[test]
    fn surround_profile_keeps_the_active_input() {
        let cards = |active: &str| {
            format!(
                r#"[{{"name":"alsa_card.usb-X4","properties":{{"alsa.card":"4"}},"active_profile":"{active}","profiles":{{
                "input:analog-stereo":{{"description":"Analog Stereo Input"}},
                "output:analog-stereo":{{"description":"Analog Stereo Output"}},
                "output:analog-stereo+input:analog-stereo":{{"description":"Analog Stereo Duplex"}},
                "output:analog-surround-51":{{"description":"Analog Surround 5.1 Output"}},
                "output:analog-surround-51+input:analog-stereo":{{"description":"Analog Surround 5.1 Output + Analog Stereo Input"}},
                "output:analog-surround-51+input:iec958-stereo":{{"description":"Analog Surround 5.1 Output + Digital Stereo (IEC958) Input","available":false}},
                "off":{{"description":"Off"}}}}}}]"#
            )
        };
        let chosen = |active: &str| surround_profile(4, &cards(active)).map(|(_, profile)| profile);
        assert_eq!(
            chosen("output:analog-stereo+input:analog-stereo").as_deref(),
            Some("output:analog-surround-51+input:analog-stereo")
        );
        assert_eq!(chosen("output:analog-stereo").as_deref(), Some("output:analog-surround-51"), "no input to keep");
        assert_eq!(chosen("off").as_deref(), Some("output:analog-surround-51"));
        assert_eq!(
            chosen("output:analog-stereo+input:iec958-stereo").as_deref(),
            Some("output:analog-surround-51"),
            "the matching 5.1 profile is unavailable"
        );
        let without_active = cards("x").replace(r#""active_profile":"x","#, "");
        assert_eq!(surround_profile(4, &without_active).unwrap().1, "output:analog-surround-51");
    }

    /// The notification part runs only under the Windows rules, with a sound card selected and once the
    /// application has loaded; it reads
    /// the default device and the channel count again at its end. The channel count is the configured one, read
    /// while no card is open.
    #[test]
    fn notification_part_waits_for_app_loaded() {
        use crate::shakers::SilentBeep;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_effects::shakers::SampleBank;
        let state = ServiceState::new(
            ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()),
            OriginalPlatform::Windows,
        );
        state.config.write().update(|c| {
            c.settings.shakers.sound_card = "hw:CARD=X4,DEV=0".into();
            c.settings.shakers.speaker_layout = "SRS UShake6".into();
        });
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let tmp = sys(&[(4, "X4")]);
        let server = FakeServer::new("alsa_output.usb-X4.analog-surround-51");
        let reads = Arc::clone(&server.default_reads);
        let mut sources = sources(tmp.path(), server, &["None", "hw:CARD=X4,DEV=0"]);
        let mut checks = DeviceChecks::default();
        assert!(!device_pass(&state, &shakers, &mut sources, &mut checks), "app not loaded yet");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        shakers.app_loaded();
        assert!(device_pass(&state, &shakers, &mut sources, &mut checks));
        assert_eq!(reads.load(Ordering::SeqCst), 2, "read for the check, then again at the end");
        assert_eq!(checks.previous_default, Some(Some("X4".into())));
        assert_eq!(checks.previous_sound_card, "hw:CARD=X4,DEV=0");
        assert_eq!(checks.previous_channels, 6, "configured channels with no card open");
        let kinds: Vec<_> = state.notifications.read().items.iter().map(|i| i.title.clone()).collect();
        assert_eq!(kinds, [DEFAULT_CARD_TITLE], "default card raised; 6 channels meet the UShake6 requirement");
        state.config.write().update(|c| c.settings.shakers.use_default_sound_card = false);
        assert!(device_pass(&state, &shakers, &mut sources, &mut checks));
        assert_eq!(reads.load(Ordering::SeqCst), 3, "no read for a disabled check, one at the end");
    }

    /// The application load, a sound card selection and a surround recheck set the monitor's event (the last two
    /// only under the Windows rules); `refresh_soundcard` stays set until the UI takes it.
    #[test]
    fn shakers_wake_the_event_driven_monitor() {
        use crate::shakers::SilentBeep;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_effects::shakers::SampleBank;
        let state = ServiceState::new(
            ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()),
            OriginalPlatform::Windows,
        );
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let event = Arc::new(AudioEvent::default());
        shakers.lock().set_device_event(Some(Arc::clone(&event)));
        shakers.app_loaded();
        assert!(event.wait(Duration::ZERO));
        event.clear();
        shakers.trigger_surround_recheck(&state);
        assert!(event.wait(Duration::ZERO), "trigger_surround_recheck");
        event.clear();
        state.config.write().update(|c| c.settings.shakers.speaker_layout = "SRS UShake6".into());
        let accessories = openrigcue_effects::accessories::dispatcher::Accessories::new(Instant::now());
        crate::shakers::shaker_pass(&state, &shakers, &accessories);
        assert!(!event.wait(Duration::ZERO), "the shaker loop sets no event; the Hardware page's handler does");
        shakers.set_soundcard(&state).unwrap();
        assert!(event.wait(Duration::ZERO), "set_soundcard('None')");
        event.clear();
        shakers.lock().request_soundcard_refresh();
        assert!(shakers.lock().status().refresh_soundcard);
        assert!(shakers.lock().take_soundcard_refresh());
        assert!(!shakers.lock().take_soundcard_refresh());
    }

    /// A sound server restart ends `pactl subscribe`. The watcher wakes the device pass, waits until the
    /// server answers again, subscribes anew and reports the restoration; before, the reader thread ended
    /// with the first subscription and no later output change was seen.
    #[cfg(target_os = "linux")]
    #[test]
    fn server_events_resubscribe_after_the_sound_server_returns() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let script = root.join("pactl");
        // `up` stands for the running sound server; a subscription records its process and prints one event.
        // `refuse` makes the next subscription end by itself at once.
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n[ -e '{root}/up' ] || exit 1\n[ \"$1\" = info ] && exit 0\n\
                 if [ -e '{root}/refuse' ]; then rm '{root}/refuse'; exit 1; fi\necho $$ > '{root}/pid'\n\
                 echo \"Event 'new' on sink #1\"\nexec sleep 60\n",
                root = root.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(root.join("up"), "").unwrap();
        let event = Arc::new(AudioEvent::default());
        let restorations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (retry, settled) = (Duration::from_millis(40), Duration::from_millis(600));
        let events = loop {
            let counted = Arc::clone(&restorations);
            let restored = Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            });
            match ServerEvents::start_with(&script, Arc::clone(&event), retry, settled, restored) {
                // A parallel test forked while the script was still open for writing.
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                started => break started.unwrap(),
            }
        };
        assert!(event.wait(Duration::from_secs(5)), "the first subscription's event");
        event.clear();
        let first = events.child_id().unwrap();
        std::thread::sleep(settled);

        // The server goes away: the subscription ends by itself.
        std::fs::remove_file(root.join("up")).unwrap();
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(first as i32), nix::sys::signal::Signal::SIGTERM).unwrap();
        assert!(event.wait(Duration::from_secs(5)), "a lost server wakes the device pass");
        event.clear();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(events.child_id(), None, "nothing is subscribed while the server is away");
        assert_eq!(restorations.load(Ordering::SeqCst), 0);
        assert!(!event.wait(Duration::ZERO), "retries do not wake the pass");

        // It returns: one restoration, a new subscription whose events arrive again.
        std::fs::write(root.join("up"), "").unwrap();
        assert!(event.wait(Duration::from_secs(5)), "the returned server wakes the device pass");
        let deadline = Instant::now() + Duration::from_secs(5);
        while events.child_id().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let second = events.child_id().expect("subscribed again");
        assert_ne!(second, first);
        assert_eq!(restorations.load(Ordering::SeqCst), 1);
        std::thread::sleep(settled);
        event.clear();

        // The next subscription is refused and ends at once: replacing the one that worked is a
        // restoration, replacing the refused one wakes the pass but restores nothing.
        std::fs::write(root.join("refuse"), "").unwrap();
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(second as i32), nix::sys::signal::Signal::SIGTERM).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while (root.join("refuse").exists() || events.child_id().is_none_or(|id| id == second))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let third = events.child_id().expect("subscribed again after the refused subscription");
        assert!(event.wait(Duration::from_secs(5)));
        assert_eq!(restorations.load(Ordering::SeqCst), 2);

        // Stopping during an outage ends the watcher without another subscription.
        std::fs::remove_file(root.join("up")).unwrap();
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(third as i32), nix::sys::signal::Signal::SIGTERM).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        events.stop();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(events.child_id(), None);
        assert!(!Path::new(&format!("/proc/{third}")).exists());
        assert_eq!(restorations.load(Ordering::SeqCst), 2);
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// A card profile change renames the selected sink. Only a single new output of the same device is the
    /// renamed selection; outputs that existed next to it, other devices and ambiguous sets are not.
    #[test]
    fn renamed_output_is_the_only_new_output_of_the_same_device() {
        let stereo = "alsa_output.usb-X4-03.analog-stereo";
        let surround = "alsa_output.usb-X4-03.analog-surround-51";
        let hdmi = "alsa_output.pci-0000_01_00.1.hdmi-stereo";
        let seen = SelectedOutput { sink: stereo.into(), present: names(&[stereo, hdmi]) };
        assert_eq!(renamed_output(&seen, &names(&[hdmi, surround])).as_deref(), Some(surround));
        assert_eq!(renamed_output(&seen, &names(&[stereo, hdmi, surround])), None, "still present");
        assert_eq!(renamed_output(&seen, &names(&[hdmi])), None, "unplugged");
        assert_eq!(
            renamed_output(&seen, &names(&[hdmi, "alsa_output.pci-0000_01_00.1.hdmi-surround"])),
            None,
            "a new output of another device"
        );
        assert_eq!(
            renamed_output(
                &seen,
                &names(&[hdmi, "alsa_output.usb-X4-03.pro-output-0", "alsa_output.usb-X4-03.pro-output-1"])
            ),
            None,
            "two new outputs are ambiguous"
        );
        let speaker = "alsa_output.usb-Generic-00.HiFi__Speaker__sink";
        let spdif = "alsa_output.usb-Generic-00.HiFi__SPDIF__sink";
        let seen = SelectedOutput { sink: speaker.into(), present: names(&[speaker, spdif]) };
        assert_eq!(renamed_output(&seen, &names(&[spdif])), None, "another connector of the card is not taken");
        let plain =
            SelectedOutput { sink: "openrigcue_test_private".into(), present: names(&["openrigcue_test_private"]) };
        assert_eq!(renamed_output(&plain, &names(&["openrigcue_test_other"])), None, "no device part in the name");
    }

    /// The surround "Fix for me" on a sound-server output: `set-card-profile` replaces the selected sink by
    /// the 5.1 one of the same card. The monitor moves `SHAKEBOX.soundcard` to it and asks for the card list
    /// to be loaded again; before, the stored selection named a sink that no longer existed.
    #[test]
    fn surround_fix_keeps_the_reconfigured_output_selected() {
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        let state = ServiceState::new(
            ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()),
            OriginalPlatform::Windows,
        );
        let stereo = "pulse:alsa_output.usb-X4-03.analog-stereo";
        state.config.write().update(|config| config.settings.shakers.sound_card = stereo.into());
        let sink = |name: &str, channels: &str| {
            format!(r#"{{"name":"{name}","channel_map":"{channels}","properties":{{"alsa.card":"4"}}}}"#)
        };
        let other = sink("alsa_output.pci-0000_01_00.1.hdmi-stereo", "front-left,front-right");
        let mut server = FakeServer::new("alsa_output.pci-0000_01_00.1.hdmi-stereo");
        server.sinks = format!("[{},{other}]", sink("alsa_output.usb-X4-03.analog-stereo", "front-left,front-right"));
        server.cards = r#"[{"name":"alsa_card.usb-X4-03","properties":{"alsa.card":"4"},"profiles":{
            "output:analog-stereo":{"description":"Analog Stereo Output","available":true},
            "output:analog-surround-51":{"description":"Analog Surround 5.1 Output","available":true}}}]"#
            .into();
        let commands = server.commands.clone();
        let temp = tempfile::tempdir().unwrap();
        let mut sources = sources(temp.path(), server, &["None", stereo]);
        let mut selected = None;
        assert!(!follow_selected_output(&state, sources.server.as_mut(), &mut selected));
        assert_eq!(selected.as_ref().map(|seen| seen.sink.as_str()), Some("alsa_output.usb-X4-03.analog-stereo"));

        set_soundcard_surround(stereo, &mut sources).unwrap();
        assert_eq!(
            *commands.lock().unwrap(),
            [vec![
                "set-card-profile".to_owned(),
                "alsa_card.usb-X4-03".to_owned(),
                "output:analog-surround-51".to_owned()
            ]]
        );
        // What the sound server lists once the profile is active.
        let surround = "front-left,front-right,front-center,lfe,rear-left,rear-right";
        let mut after = FakeServer::new("alsa_output.pci-0000_01_00.1.hdmi-stereo");
        after.sinks = format!("[{other},{}]", sink("alsa_output.usb-X4-03.analog-surround-51", surround));
        assert!(follow_selected_output(&state, &mut after, &mut selected));
        let moved = "pulse:alsa_output.usb-X4-03.analog-surround-51";
        assert_eq!(state.config.read().settings().shakers.sound_card, moved);
        assert_eq!(
            openrigcue_model::native_value::LegacyValue::Dict(state.config.read().settings().native_snapshot())
                .item("SHAKEBOX")
                .unwrap()
                .item("soundcard")
                .unwrap()
                .as_str(),
            Some(moved)
        );
        let mut sources = self::sources(temp.path(), after, &["None", moved]);
        assert_eq!(get_sound_channels(moved, &["None".into(), moved.into()], &mut sources), Ok(6));
        assert!(!follow_selected_output(&state, sources.server.as_mut(), &mut selected), "nothing moves twice");

        // An unplugged device keeps its stored name; another selection kind forgets the memory.
        let mut gone = FakeServer::new("alsa_output.pci-0000_01_00.1.hdmi-stereo");
        gone.sinks = format!("[{other}]");
        assert!(!follow_selected_output(&state, &mut gone, &mut selected));
        assert_eq!(state.config.read().settings().shakers.sound_card, moved);
        state.config.write().update(|config| config.settings.shakers.sound_card = "None".into());
        assert!(!follow_selected_output(&state, &mut gone, &mut selected));
        assert_eq!(selected, None);
    }

    /// One event, one reselection. A lost stream, a renamed output or a returned sound server raises the
    /// monitor's flag; the pass then asks for the reload once, at once, whether the card list differs (the
    /// sound server went away: the comparison would ask again) or not (an output replaced under the same name:
    /// the comparison would settle for two seconds and not ask).
    #[test]
    fn reselection_is_asked_once_per_pass() {
        let cards = vec!["None".to_owned(), "pulse:selected".to_owned()];
        let gone = || vec!["None".to_owned()];
        let same = || vec!["None".to_owned(), "pulse:selected".to_owned()];
        let mut asked = 0;
        assert_eq!(card_list_pass(&cards, true, true, &AtomicBool::new(true), &gone, &mut || asked += 1), gone());
        assert_eq!(asked, 1, "lost stream and changed list");
        let started = Instant::now();
        assert_eq!(card_list_pass(&cards, true, true, &AtomicBool::new(true), &same, &mut || asked += 1), cards);
        assert_eq!(asked, 2, "lost stream, unchanged list");
        assert!(started.elapsed() < EVENT_POLL, "no settling when the reselection is already known");
        assert_eq!(card_list_pass(&cards, false, true, &AtomicBool::new(true), &same, &mut || asked += 1), cards);
        assert_eq!(asked, 3, "polling monitor");
        // A stream reported lost while the pass settles on an unchanged list ends the settling and is
        // answered by that pass, with the flag taken.
        let late = Arc::new(AtomicBool::new(false));
        let raise = Arc::clone(&late);
        let raiser = std::thread::spawn(move || {
            std::thread::sleep(EVENT_POLL / 2);
            raise.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        assert_eq!(card_list_pass(&cards, true, true, &late, &same, &mut || asked += 1), cards);
        raiser.join().unwrap();
        assert_eq!(asked, 4);
        assert!(started.elapsed() < EVENT_SETTLE, "{:?}", started.elapsed());
        assert!(!late.load(Ordering::SeqCst));
    }

    /// The whole monitor on one outage: the open stream is lost and the card list loses the selected output
    /// at the same moment (the sound server exits). The reload is requested once; when the server and its
    /// outputs return, once more. Each request stands for one dropdown reload by the UI main loop.
    #[test]
    fn one_outage_asks_for_one_reselection_each_way() {
        use crate::shakers::SilentBeep;
        use crate::state::OriginalPlatform;
        use crate::store::ConfigStore;
        use openrigcue_effects::shakers::SampleBank;
        let state = ServiceState::new(
            ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()),
            OriginalPlatform::Windows,
        );
        let shakers = ShakersHandle::new(&state, SampleBank::synthesized(), Box::new(SilentBeep), false);
        let present = Arc::new(AtomicBool::new(true));
        let listed = Arc::clone(&present);
        let temp = tempfile::tempdir().unwrap();
        let mut sources = sources(temp.path(), FakeServer::new("other"), &[]);
        sources.soundcards = Arc::new(move || {
            let mut cards = vec!["None".to_owned()];
            if listed.load(Ordering::SeqCst) {
                cards.push("pulse:selected".to_owned());
            }
            cards
        });
        let event = Arc::new(AudioEvent::default());
        let reselect = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        shakers.lock().set_device_event(Some(Arc::clone(&event)));
        shakers.lock().set_device_monitor(Some(Arc::clone(&reselect)));
        let monitor = {
            let (state, shakers, event, reselect, stop) =
                (state.clone(), shakers.clone(), Arc::clone(&event), Arc::clone(&reselect), Arc::clone(&stop));
            std::thread::spawn(move || run(&state, &shakers, sources, Some(&event), &reselect, &stop))
        };
        // The UI main loop: every request is taken and counted.
        let requests = |window: Duration| {
            let (mut count, until) = (0, Instant::now() + window);
            while Instant::now() < until {
                if shakers.lock().take_soundcard_refresh() {
                    count += 1;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            count
        };
        std::thread::sleep(Duration::from_millis(50));
        // The server exits: the stream reports its loss, the subscription ends, the list changes.
        present.store(false, Ordering::SeqCst);
        reselect.store(true, Ordering::SeqCst);
        event.set();
        event.set();
        assert_eq!(requests(EVENT_QUIET + Duration::from_millis(900)), 1, "the outage");
        // It returns with its outputs: the restored subscription raises the flag and the event.
        present.store(true, Ordering::SeqCst);
        reselect.store(true, Ordering::SeqCst);
        event.set();
        assert_eq!(requests(EVENT_QUIET + Duration::from_millis(900)), 1, "the return");
        // An output replaced under its own name: no list change, the lost stream alone asks.
        reselect.store(true, Ordering::SeqCst);
        event.set();
        assert_eq!(requests(EVENT_QUIET + Duration::from_millis(900)), 1, "same-name replacement");
        stop.store(true, Ordering::SeqCst);
        monitor.join().unwrap();
    }

    /// The audio event: set before wait is kept, wait does not clear, clear does.
    #[test]
    fn audio_event() {
        let event = AudioEvent::default();
        assert!(!event.wait(Duration::from_millis(1)));
        event.set();
        assert!(event.wait(Duration::from_millis(1)));
        assert!(event.wait(Duration::from_millis(1)));
        event.clear();
        assert!(!event.wait(Duration::from_millis(1)));
    }
}
