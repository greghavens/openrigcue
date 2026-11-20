//! SDL2 joystick enumeration and polling.
//! Polling runs on the joystick polling worker; tests use an in-memory backend and never initialize SDL.

mod ffi;

use std::collections::BTreeMap;

use super::{ControllerView, JoystickEvent, JoystickInputs};

#[derive(Debug, Clone)]
struct Device {
    guid: String,
    name: String,
    instance: i32,
    handle: usize,
    axes: i32,
    initial: BTreeMap<u32, i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Added { index: i32 },
    Removed { instance: i32 },
    Axis { instance: i32, index: u32, value: i32 },
    Hat { instance: i32, index: u32 },
    Button { instance: i32, index: u32, pressed: bool },
    Other { instance: i32 },
}

trait Backend {
    fn devices(&mut self) -> Result<Vec<Device>, String>;
    fn event(&mut self) -> Option<Event>;
    fn axis(&self, device: &Device, index: u32) -> i32;
}

/// An SDL joystick session. Construct, update and drop it on the joystick polling worker.
/// SDL initialization is deliberately absent from library/symbol inspection tests.
pub struct SdlJoysticks(Driver<ffi::Backend>);

impl SdlJoysticks {
    pub fn open(inputs: JoystickInputs) -> Result<Self, String> {
        let mut backend = ffi::Backend::load()?;
        backend.initialize();
        Ok(Self(Driver::new(backend, inputs)))
    }

    pub fn update(&mut self) -> Result<(), String> {
        self.0.update()
    }
}

struct Driver<B> {
    backend: B,
    inputs: JoystickInputs,
    devices: Vec<Device>,
}

impl<B: Backend> Driver<B> {
    fn new(backend: B, inputs: JoystickInputs) -> Self {
        // A session starts with an empty registry. SDL's initial Added events
        // cause enumeration during update, as do subsequent hotplug events.
        let driver = Self { backend, inputs, devices: Vec::new() };
        driver.publish(BTreeMap::new(), BTreeMap::new(), Vec::new());
        driver
    }

    fn reload(&mut self) -> Result<(), String> {
        let mut devices: Vec<Device> = Vec::new();
        for device in self.backend.devices()? {
            // A device whose GUID is already listed replaces that entry in place,
            // so the entry keeps the position of the first device with that GUID.
            if let Some(previous) = devices.iter_mut().find(|entry| entry.guid == device.guid) {
                *previous = device;
            } else {
                devices.push(device);
            }
        }
        self.devices = devices;
        Ok(())
    }

    fn update(&mut self) -> Result<(), String> {
        let mut frame: BTreeMap<String, ControllerView> = BTreeMap::new();
        let mut last_axes = BTreeMap::new();
        let mut events = Vec::new();
        while let Some(event) = self.backend.event() {
            match event {
                Event::Added { index } => {
                    self.reload()?;
                    let controller = usize::try_from(index)
                        .ok()
                        .and_then(|index| self.devices.get(index))
                        .map(|device| device.name.clone())
                        .unwrap_or_default();
                    events.push(JoystickEvent::Added { controller });
                    continue;
                }
                Event::Removed { instance } => {
                    let controller = self
                        .devices
                        .iter()
                        .find(|device| device.instance == instance)
                        .map(|device| device.name.clone())
                        .unwrap_or_default();
                    self.reload()?;
                    events.push(JoystickEvent::Removed { controller });
                    continue;
                }
                _ => {}
            }
            let instance = match event {
                Event::Axis { instance, .. }
                | Event::Hat { instance, .. }
                | Event::Button { instance, .. }
                | Event::Other { instance } => instance,
                Event::Added { .. } | Event::Removed { .. } => unreachable!(),
            };
            let Some(device) = self.devices.iter().find(|device| device.instance == instance) else {
                self.reload()?;
                continue;
            };
            let controller = device.name.clone();
            events.push(match event {
                Event::Axis { index, value, .. } => {
                    // The controller's last axis state holds this axis alone; earlier axes are dropped.
                    last_axes.insert(controller.clone(), BTreeMap::from([(index, value)]));
                    JoystickEvent::Axis { controller, index, value }
                }
                Event::Hat { index, .. } => JoystickEvent::Hat { controller, index },
                Event::Button { index, pressed, .. } => {
                    frame.entry(controller.clone()).or_default().buttons.insert(index, pressed);
                    JoystickEvent::Button { controller, index, pressed }
                }
                Event::Other { .. } => JoystickEvent::Other { controller },
                _ => unreachable!(),
            });
        }
        // Every axis is polled after draining events. Current values
        // can therefore differ from the value an earlier motion event left in `last_axes`.
        for device in &self.devices {
            for index in 0..device.axes {
                frame
                    .entry(device.name.clone())
                    .or_default()
                    .axes
                    .insert(index as u32, self.backend.axis(device, index as u32));
            }
        }
        self.publish(frame, last_axes, events);
        Ok(())
    }

    fn publish(
        &self,
        frame: BTreeMap<String, ControllerView>,
        last_axes: BTreeMap<String, BTreeMap<u32, i32>>,
        events: Vec<JoystickEvent>,
    ) {
        let mut state = self.inputs.lock();
        state.devices = self.devices.iter().map(|device| device.name.clone()).collect();
        state.native_joysticks = Some(
            self.devices
                .iter()
                .map(|device| {
                    (
                        device.guid.clone(),
                        serde_json::json!({
                            "name": device.name, "joystick": device.handle, "num_axis": device.axes,
                            "index": device.instance, "initial_axis_value": device.initial
                        }),
                    )
                })
                .collect(),
        );
        state.controllers = frame;
        state.last_axes.extend(last_axes);
        // Motion/recorder readers inspect the latest shared pass, while the
        // shortcut worker consumes the complete ordered event stream.
        state.readers.retain(|reader| reader.strong_count() != 0);
        for event in events {
            state.publish(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Fake {
        devices: Vec<Device>,
        events: VecDeque<Event>,
        values: BTreeMap<(usize, u32), i32>,
        enumerations: usize,
    }

    impl Backend for Fake {
        fn devices(&mut self) -> Result<Vec<Device>, String> {
            self.enumerations += 1;
            Ok(self.devices.clone())
        }
        fn event(&mut self) -> Option<Event> {
            self.events.pop_front()
        }
        fn axis(&self, device: &Device, index: u32) -> i32 {
            self.values.get(&(device.handle, index)).copied().unwrap_or(0)
        }
    }

    fn device(guid: &str, name: &str, instance: i32, handle: usize) -> Device {
        Device {
            guid: guid.into(),
            name: name.into(),
            instance,
            handle,
            axes: 2,
            initial: BTreeMap::from([(0, -32768)]),
        }
    }

    #[test]
    fn enumeration_uses_sdl_names_and_last_guid_value_in_first_key_position() {
        let inputs = JoystickInputs::default();
        let events = inputs.events();
        let fake = Fake {
            devices: vec![
                device("guid-a", "SDL Wheel old", 10, 0),
                device("guid-b", "SDL Pedals", 20, 1),
                device("guid-a", "SDL Wheel", 30, 2),
            ],
            events: VecDeque::from([
                Event::Added { index: 0 },
                Event::Button { instance: 10, index: 1, pressed: true },
                Event::Button { instance: 30, index: 2, pressed: true },
            ]),
            ..Default::default()
        };
        let mut driver = Driver::new(fake, inputs.clone());
        assert!(inputs.devices().is_empty(), "constructor does not enumerate");
        driver.update().unwrap();
        assert_eq!(inputs.devices(), ["SDL Wheel", "SDL Pedals"]);
        let page = inputs.page_snapshot();
        let registry = page["joysticks"].as_object().unwrap();
        assert_eq!(registry.keys().map(String::as_str).collect::<Vec<_>>(), ["guid-a", "guid-b"]);
        assert_eq!(registry["guid-a"]["index"], 30);
        assert_eq!(registry["guid-a"]["initial_axis_value"], serde_json::json!({"0": -32768}));
        assert_eq!(driver.backend.enumerations, 2, "missing duplicate instance reloads without a synthetic hotplug");
        assert_eq!(
            events.take(),
            [
                JoystickEvent::Added { controller: "SDL Wheel".into() },
                JoystickEvent::Button { controller: "SDL Wheel".into(), index: 2, pressed: true }
            ]
        );
    }

    #[test]
    fn pass_drains_ordered_events_then_polls_axes_and_replaces_transient_buttons() {
        let inputs = JoystickInputs::default();
        let events = inputs.events();
        let reader = inputs.reader();
        let delayed_reader = inputs.reader();
        let fake = Fake {
            devices: vec![device("guid", "Wheel", 5, 0)],
            events: VecDeque::from([
                Event::Added { index: 0 },
                Event::Axis { instance: 5, index: 0, value: 100 },
                Event::Axis { instance: 5, index: 1, value: 200 },
                Event::Button { instance: 5, index: 7, pressed: true },
                Event::Other { instance: 5 },
                Event::Hat { instance: 5, index: 2 },
            ]),
            values: BTreeMap::from([((0, 0), 700), ((0, 1), -300)]),
            ..Default::default()
        };
        let mut driver = Driver::new(fake, inputs.clone());
        driver.update().unwrap();
        let pass = reader.take();
        assert_eq!(pass["Wheel"].axes, BTreeMap::from([(0, 700), (1, -300)]));
        assert_eq!(pass["Wheel"].button_events, BTreeMap::from([(7, true)]));
        assert_eq!(inputs.page_snapshot()["last_state"], serde_json::json!({"Wheel": {"axis": {"1": 200}}}));
        assert_eq!(
            events.take(),
            [
                JoystickEvent::Added { controller: "Wheel".into() },
                JoystickEvent::Axis { controller: "Wheel".into(), index: 0, value: 100 },
                JoystickEvent::Axis { controller: "Wheel".into(), index: 1, value: 200 },
                JoystickEvent::Button { controller: "Wheel".into(), index: 7, pressed: true },
                JoystickEvent::Other { controller: "Wheel".into() },
                JoystickEvent::Hat { controller: "Wheel".into(), index: 2 }
            ]
        );
        driver.backend.values.insert((0, 0), 900);
        driver.update().unwrap();
        assert_eq!(inputs.snapshot()["Wheel"].axes[&0], 900);
        assert!(inputs.snapshot()["Wheel"].buttons.is_empty());
        assert!(reader.take()["Wheel"].button_events.is_empty());
        assert!(
            delayed_reader.take()["Wheel"].button_events.is_empty(),
            "native readers see the latest pass, not an old button queue"
        );
        assert_eq!(driver.backend.enumerations, 1, "ordinary passes do not rescan");
        assert!(events.take().is_empty(), "axis polling does not invent motion events");
    }

    #[test]
    fn removal_reloads_registry_and_retains_last_state() {
        let inputs = JoystickInputs::default();
        let events = inputs.events();
        let fake = Fake {
            devices: vec![device("guid", "Wheel", 9, 0)],
            events: VecDeque::from([Event::Added { index: 0 }, Event::Axis { instance: 9, index: 0, value: 88 }]),
            ..Default::default()
        };
        let mut driver = Driver::new(fake, inputs.clone());
        driver.update().unwrap();
        events.take();
        driver.backend.devices.clear();
        driver.backend.events.push_back(Event::Removed { instance: 9 });
        driver.update().unwrap();
        assert!(inputs.devices().is_empty());
        assert!(inputs.snapshot().is_empty());
        assert_eq!(inputs.page_snapshot()["last_state"], serde_json::json!({"Wheel": {"axis": {"0": 88}}}));
        assert_eq!(events.take(), [JoystickEvent::Removed { controller: "Wheel".into() }]);
    }
}
