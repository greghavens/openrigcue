//! Linux joystick input through the evdev interface (`/dev/input/event*`), feeding [`JoystickInputs`].
//!
//! An alternative to SDL2 joystick polling. Axis and button indices follow
//! the numbering SDL2's Linux backend gives the same device, so a profile refers to the same inputs; axis values
//! are rescaled to SDL's -32768..=32767.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use evdev::{AbsInfo, AbsoluteAxisCode, Device, EventSummary, KeyCode};
use rustix::event::{PollFd, PollFlags, Timespec};

use super::JoystickInputs;

/// How often new controllers are looked for.
const RESCAN_INTERVAL: Duration = Duration::from_secs(1);
/// Longest a reader waits for events before it checks for a stop request.
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// Absolute axes SDL turns into hats instead of axes (`ABS_HAT0X..=ABS_HAT3Y`).
const HAT_AXES: std::ops::RangeInclusive<u16> = 0x10..=0x17;
/// Axes SDL considers (`0..ABS_MAX`).
const ABS_MAX: u16 = 0x3f;
/// First joystick button code (`BTN_JOYSTICK`) and the end of the key code space (`KEY_MAX`).
const BTN_JOYSTICK: u16 = 0x120;
const KEY_MAX: u16 = 0x2ff;
/// Joystick and gamepad buttons (`BTN_JOYSTICK..BTN_DIGI`), trigger-happy buttons.
const JOYSTICK_BUTTONS: std::ops::Range<u16> = 0x120..0x140;
const TRIGGER_HAPPY_BUTTONS: std::ops::Range<u16> = 0x2c0..0x2e8;
/// Codes that mark touchpads, pens and mice, which also report absolute axes or buttons.
const BTN_LEFT: u16 = 0x110;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOUCH: u16 = 0x14a;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

/// SDL axis number of each absolute axis code: ascending codes below `ABS_MAX`, hats skipped.
fn axis_numbers(codes: &BTreeSet<u16>) -> Vec<(u16, u32)> {
    codes.iter().copied().filter(|code| *code < ABS_MAX && !HAT_AXES.contains(code)).zip(0..).collect()
}

/// SDL button number of each key code: `BTN_JOYSTICK..KEY_MAX` first, then the codes below `BTN_JOYSTICK`.
fn button_numbers(codes: &BTreeSet<u16>) -> Vec<(u16, u32)> {
    let joystick_first = codes.range(BTN_JOYSTICK..KEY_MAX).chain(codes.range(..BTN_JOYSTICK));
    joystick_first.copied().zip(0..).collect()
}

/// Rescales a raw axis value from the device range to -32768..=32767.
fn scale_axis(value: i32, minimum: i32, maximum: i32) -> i32 {
    if maximum <= minimum {
        return 0;
    }
    let span = f64::from(maximum) - f64::from(minimum);
    let scaled = (f64::from(value) - f64::from(minimum)) * 65535.0 / span - 32768.0;
    scaled.round().clamp(-32768.0, 32767.0) as i32
}

/// Whether a device with these key and axis codes is a game controller (joystick, gamepad, wheel, pedals).
/// Public so that other input readers (the hotkey runtime) classify devices the same way.
pub fn is_controller(keys: &BTreeSet<u16>, axes: &BTreeSet<u16>) -> bool {
    let has_buttons = keys.iter().any(|k| JOYSTICK_BUTTONS.contains(k) || TRIGGER_HAPPY_BUTTONS.contains(k));
    let pointer = keys.contains(&BTN_TOUCH) || keys.contains(&BTN_TOOL_PEN) || keys.contains(&BTN_LEFT);
    let has_stick = axes.contains(&ABS_X) || axes.contains(&ABS_Y) || !axes.is_empty() && has_buttons;
    !pointer && (has_buttons || has_stick)
}

fn key_codes(device: &Device) -> BTreeSet<u16> {
    device.supported_keys().map(|keys| keys.iter().map(|k: KeyCode| k.0).collect()).unwrap_or_default()
}

fn axis_codes(device: &Device) -> BTreeSet<u16> {
    device
        .supported_absolute_axes()
        .map(|axes| axes.iter().map(|a: AbsoluteAxisCode| a.0).collect())
        .unwrap_or_default()
}

/// One open controller and how its codes map to SDL numbers.
struct Controller {
    name: String,
    device: Device,
    axes: Vec<(u16, u32)>,
    buttons: Vec<(u16, u32)>,
    ranges: Vec<(u16, AbsInfo)>,
}

impl Controller {
    fn axis(&self, code: u16) -> Option<u32> {
        self.axes.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
    }

    fn button(&self, code: u16) -> Option<u32> {
        self.buttons.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
    }

    fn scaled(&self, code: u16, value: i32) -> i32 {
        match self.ranges.iter().find(|(c, _)| *c == code) {
            Some((_, info)) => scale_axis(value, info.minimum(), info.maximum()),
            None => value,
        }
    }

    /// Publishes the current position of every axis (SDL's initial axis state).
    fn publish_initial(&self, inputs: &JoystickInputs) {
        for (code, info) in &self.ranges {
            if let Some(number) = self.axis(*code) {
                inputs.set_axis(&self.name, number, scale_axis(info.value(), info.minimum(), info.maximum()));
            }
        }
    }

    /// Reads events until the device goes away or a stop is requested (checked at least every
    /// [`STOP_CHECK_INTERVAL`]), then forgets the controller.
    fn run(mut self, inputs: &JoystickInputs, stop: &StopSignal) {
        if let Err(error) = self.device.set_nonblocking(true) {
            tracing::warn!(name = %self.name, %error, "joystick not readable");
            return;
        }
        inputs.connect(&self.name);
        self.publish_initial(inputs);
        while !stop.is_requested() {
            match wait_readable(&self.device, STOP_CHECK_INTERVAL) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            let events: Vec<_> = match self.device.fetch_events() {
                Ok(events) => events.collect(),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(_) => break,
            };
            for event in events {
                match event.destructure() {
                    EventSummary::AbsoluteAxis(_, code, value) => {
                        if let Some(number) = self.axis(code.0) {
                            inputs.axis_event(&self.name, number, self.scaled(code.0, value));
                        } else if HAT_AXES.contains(&code.0) {
                            inputs.hat_event(&self.name, u32::from(code.0 - HAT_AXES.start()) / 2);
                        }
                    }
                    EventSummary::Key(_, code, value @ (0 | 1)) => {
                        if let Some(number) = self.button(code.0) {
                            inputs.button_event(&self.name, number, value == 1);
                        }
                    }
                    _ => {}
                }
            }
        }
        inputs.remove(&self.name);
    }
}

/// Waits until the device has events or `timeout` passes; `Ok(false)` on timeout or an interrupted wait.
fn wait_readable(device: &Device, timeout: Duration) -> io::Result<bool> {
    let timeout = Timespec::try_from(timeout).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    match rustix::event::poll(&mut [PollFd::new(device, PollFlags::IN)], Some(&timeout)) {
        Ok(ready) => Ok(ready > 0),
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// A stop request shared by the scanner and the readers.
#[derive(Debug, Default)]
struct StopSignal {
    requested: Mutex<bool>,
    changed: Condvar,
}

impl StopSignal {
    fn lock(&self) -> MutexGuard<'_, bool> {
        self.requested.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn request(&self) {
        *self.lock() = true;
        self.changed.notify_all();
    }

    fn is_requested(&self) -> bool {
        *self.lock()
    }

    /// Sleeps up to `timeout`, waking early on a stop request; returns whether a stop was requested.
    fn wait(&self, timeout: Duration) -> bool {
        let guard = self.changed.wait_timeout_while(self.lock(), timeout, |requested| !*requested);
        *guard.unwrap_or_else(PoisonError::into_inner).0
    }
}

/// Reader threads still to be joined.
type Readers = Arc<Mutex<Vec<JoinHandle<()>>>>;

/// Background readers for every connected controller. Dropping it stops the scanner and every reader (each within
/// [`STOP_CHECK_INTERVAL`]) and waits for them, closing the devices.
pub struct EvdevJoysticks {
    stop: Arc<StopSignal>,
    scanner: Option<JoinHandle<()>>,
    readers: Readers,
}

impl EvdevJoysticks {
    /// Starts watching `/dev/input` for controllers.
    pub fn start(inputs: JoystickInputs) -> Self {
        let stop = Arc::new(StopSignal::default());
        let readers = Readers::default();
        let (signal, spawned) = (stop.clone(), readers.clone());
        let scanner =
            thread::Builder::new().name("joystick_scan".into()).spawn(move || scan(&inputs, &signal, &spawned)).ok();
        EvdevJoysticks { stop, scanner, readers }
    }
}

impl Drop for EvdevJoysticks {
    fn drop(&mut self) {
        self.stop.request();
        if let Some(scanner) = self.scanner.take() {
            let _ = scanner.join();
        }
        let readers = std::mem::take(&mut *self.readers.lock().unwrap_or_else(PoisonError::into_inner));
        for reader in readers {
            let _ = reader.join();
        }
    }
}

/// Opens newly connected controllers, each in its own reader thread, every [`RESCAN_INTERVAL`] until a stop is
/// requested.
fn scan(inputs: &JoystickInputs, stop: &Arc<StopSignal>, readers: &Readers) {
    let open: Arc<Mutex<BTreeSet<PathBuf>>> = Arc::default();
    while !stop.is_requested() {
        for (path, device) in evdev::enumerate() {
            let (keys, axes) = (key_codes(&device), axis_codes(&device));
            let already_open = open.lock().unwrap_or_else(PoisonError::into_inner).contains(&path);
            if already_open || !is_controller(&keys, &axes) {
                continue;
            }
            open.lock().unwrap_or_else(PoisonError::into_inner).insert(path.clone());
            let controller = Controller {
                name: device.name().unwrap_or_default().to_owned(),
                ranges: device
                    .get_absinfo()
                    .map(|infos| infos.map(|(code, info)| (code.0, info)).collect())
                    .unwrap_or_default(),
                axes: axis_numbers(&axes),
                buttons: button_numbers(&keys),
                device,
            };
            tracing::info!(name = %controller.name, path = %path.display(), "joystick connected");
            let (inputs, stop, open) = (inputs.clone(), stop.clone(), open.clone());
            let spawned = thread::Builder::new().name("joystick_read".into()).spawn(move || {
                controller.run(&inputs, &stop);
                open.lock().unwrap_or_else(PoisonError::into_inner).remove(&path);
            });
            match spawned {
                Ok(reader) => {
                    let mut readers = readers.lock().unwrap_or_else(PoisonError::into_inner);
                    readers.retain(|reader| !reader.is_finished());
                    readers.push(reader);
                }
                Err(error) => tracing::warn!(%error, "could not start a joystick reader"),
            }
        }
        if stop.wait(RESCAN_INTERVAL) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(codes: &[u16]) -> BTreeSet<u16> {
        codes.iter().copied().collect()
    }

    #[test]
    fn axes_are_numbered_in_code_order_without_hats() {
        assert_eq!(
            axis_numbers(&set(&[0x00, 0x01, 0x02, 0x10, 0x11, 0x05, 0x3f])),
            vec![(0x00, 0), (0x01, 1), (0x02, 2), (0x05, 3)]
        );
    }

    #[test]
    fn buttons_start_at_btn_joystick() {
        assert_eq!(
            button_numbers(&set(&[0x2c0, 0x121, 0x120, 0x0a])),
            vec![(0x120, 0), (0x121, 1), (0x2c0, 2), (0x0a, 3)]
        );
    }

    #[test]
    fn axis_values_span_the_sdl_range() {
        assert_eq!(scale_axis(0, 0, 65535), -32768);
        assert_eq!(scale_axis(65535, 0, 65535), 32767);
        assert_eq!(scale_axis(0, -32768, 32767), 0);
        assert_eq!(scale_axis(-1, -1, 1), -32768);
        assert_eq!(scale_axis(5, 3, 3), 0);
    }

    #[test]
    fn stop_signal_wakes_a_waiting_scanner() {
        let stop = Arc::new(StopSignal::default());
        assert!(!stop.wait(Duration::from_millis(1)), "times out without a request");
        let waiter = {
            let stop = stop.clone();
            thread::spawn(move || stop.wait(Duration::from_secs(30)))
        };
        stop.request();
        assert!(waiter.join().unwrap());
        assert!(stop.is_requested());
    }

    #[test]
    fn controllers_are_told_apart_from_pointers() {
        assert!(is_controller(&set(&[0x120]), &set(&[0x00, 0x01])), "joystick");
        assert!(is_controller(&set(&[0x130]), &set(&[])), "gamepad buttons only");
        assert!(is_controller(&set(&[]), &set(&[0x01, 0x02])), "pedals");
        assert!(!is_controller(&set(&[BTN_TOUCH, BTN_LEFT]), &set(&[0x00, 0x01])), "touchpad");
        assert!(!is_controller(&set(&[0x1e, 0x1f]), &set(&[])), "keyboard");
    }
}
