//! ESP-IDF glue: set-up, the main loop and the command
//! handlers, on top of the hardware-independent `obd2hud-core`.

mod ble;
mod hw;

use core::sync::atomic::{AtomicU32, Ordering};

use obd2hud_core::can::CanFrame;
use obd2hud_core::consts::*;
use obd2hud_core::eeprom::Eeprom;
use obd2hud_core::gate;
use obd2hud_core::messenger::{CmdMessenger, Dispatch, Print, MAX_STREAM_BUFFER};
use obd2hud_core::obd::Ecu;
use obd2hud_core::serial;
use obd2hud_core::telemetry::{slot, SLOT_COUNT};

use hw::{NvsEeprom, Uart0};

/// The float slots (POWER, SPEED, ...), written by the telemetry characteristic
/// in the NimBLE host task and read by the main loop; each slot is one 32-bit store.
pub static SLOTS: [AtomicU32; SLOT_COUNT] = [const { AtomicU32::new(0) }; SLOT_COUNT];

pub fn slot_value(i: usize) -> f32 {
    f32::from_bits(SLOTS[i].load(Ordering::Relaxed))
}

pub fn set_slot(i: usize, v: f32) {
    SLOTS[i].store(v.to_bits(), Ordering::Relaxed);
}

/// The firmware state owned by the loop task (messenger, serial number, the OBD state with the LED status).
struct Sketch {
    c: CmdMessenger,
    serial: Uart0,
    eeprom: Eeprom<NvsEeprom>,
    serial_number: [u8; 24],
    ecu: Ecu,
}

struct Out<'a>(&'a Uart0);
impl Print for Out<'_> {
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.0.write(bytes);
    }
}

/// The application entry: initialise the runtime, then create the loop task
/// pinned to core 1.
pub fn app_main() {
    hw::init_arduino();
    hw::spawn_loop_task(loop_task);
}

/// The loop task: set-up once, then the main loop forever. The loop task has no
/// watchdog and there is no serial-event hook.
extern "C" fn loop_task(_arg: *mut core::ffi::c_void) {
    let mut s = setup();
    loop {
        s.run_loop();
    }
}

/// Set-up of the firmware.
fn setup() -> Sketch {
    // The HUD power output starts low, the status LED on.
    hw::pin_mode_output(POWER_PIN);
    hw::digital_write(POWER_PIN, false);
    hw::pin_mode_output(LED_PIN);
    hw::digital_write(LED_PIN, true);
    // The serial link at 115200 baud, 8N1.
    let serial = Uart0::begin(UART_BAUD);
    // The CAN controller: 1000 kbit/s, TX GPIO23, RX GPIO22, a receive queue of 10 frames.
    hw::can_init();
    // The command handlers.
    let mut c = CmdMessenger::default();
    c.attach(cmd::WHO_ARE_YOU);
    c.attach(cmd::WRITE_LICENSE_KEY);
    c.attach_default();
    // The random generator is seeded from an analog reading.
    hw::random_seed(hw::analog_read_gpio27());
    // The emulated EEPROM and its identity bytes: MAC copy + salt.
    let mut eeprom = Eeprom::new(NvsEeprom::default());
    eeprom.begin(EEPROM_SIZE);
    let id = serial::unique_id(&hw::efuse_mac());
    serial::check_eeprom(&mut eeprom, &id, &mut hw::esp_random);
    // The serial number.
    let serial_number = serial::serial_number(&eeprom, &id);
    // No licence hash is computed: this firmware has no licensing.
    let mut s = Sketch { c, serial, eeprom, serial_number, ecu: Ecu::default() };
    {
        let mut out = Out(&s.serial);
        s.c.send_cmd_str(&mut out, cmd::LOGGER, READY_TEXT);
    }
    // BLE starts for the "licensed" status, which this firmware always
    // reports (LICENSE_STATUS_REPORTED); a logger line follows.
    if LICENSE_STATUS_REPORTED == 1 {
        ble::ble_setup(&s.serial_number);
        let mut out = Out(&s.serial);
        s.c.send_cmd_str(&mut out, cmd::LOGGER, BLE_STARTED_TEXT);
    }
    s
}

impl Sketch {
    /// One pass of the main loop: serial input, the HUD power output, then a received CAN frame, if any.
    fn run_loop(&mut self) {
        self.feedin_serial_data();
        // The HUD power output is high when the power slot is 1.0.
        hw::digital_write(POWER_PIN, slot_value(slot::POWER) == 1.0);
        let mut frame = CanFrame::default();
        if hw::can_receive(&mut frame, CAN_RX_WAIT_TICKS) {
            self.flash_led();
            let rsp = self.ecu.handle_live(&frame, slot_value);
            // Mode 9 prints before it sends, mode 1 only prints for unknown PIDs (before the
            // send) and the 0x7E0 branch prints before its flush: printed lines come first.
            for l in &rsp.printf {
                hw::printf(l);
            }
            for f in &rsp.frames {
                hw::can_write_frame(f);
            }
        }
    }

    /// Feeds the available serial bytes to the messenger and dispatches completed messages.
    fn feedin_serial_data(&mut self) {
        loop {
            if self.c.pause_processing {
                return;
            }
            // The available count is taken twice: once for the test against 0 and
            // again for the number of bytes to read (at most 0x200).
            if self.serial.available() == 0 {
                return;
            }
            let n = self.serial.available().min(MAX_STREAM_BUFFER);
            // The number of bytes read is ignored.
            self.serial.read_bytes(&mut self.c.stream_buffer_mut()[..n], STREAM_TIMEOUT_MS);
            for i in 0..n {
                let ch = self.c.stream_byte(i);
                if self.c.process_byte(ch) {
                    if let Some(d) = self.c.handle_message() {
                        self.dispatch(d);
                    }
                }
            }
        }
    }

    fn dispatch(&mut self, d: Dispatch) {
        match d {
            Dispatch::Callback(cmd::WHO_ARE_YOU) => self.on_who_are_you(),
            Dispatch::Callback(cmd::WRITE_LICENSE_KEY) => self.on_write_license_key(),
            Dispatch::Callback(_) | Dispatch::Default => self.on_unknown_command(),
        }
    }

    /// Toggles the status LED.
    fn flash_led(&mut self) {
        let next = !self.ecu.led_status;
        hw::digital_write(LED_PIN, next);
        self.ecu.led_status = next;
    }

    /// Answers `who_are_you` with `i_am`.
    fn on_who_are_you(&mut self) {
        let mut hs = [0u8; 64];
        let hs_len = match self.c.read_string_arg() {
            Some(s) => {
                let n = s.len().min(hs.len());
                hs[..n].copy_from_slice(&s[..n]);
                n
            }
            None => 0,
        };
        let open_gate = gate::open_gate(&hs[..hs_len], LICENSE_STATUS_REPORTED, &mut hw::esp_random);
        let mut out = Out(&self.serial);
        self.c.send_cmd_start(&mut out, cmd::I_AM);
        self.c.send_cmd_arg_str(&mut out, &open_gate);
        self.c.send_cmd_bin_arg_u8(&mut out, PRODUCT);
        self.c.send_cmd_bin_arg_u8(&mut out, MODEL);
        self.c.send_cmd_bin_arg_u8(&mut out, VERSION);
        self.c.send_cmd_bin_arg_u8(&mut out, I_AM_LICENSE_BYTE);
        self.c.send_cmd_arg_str(&mut out, &self.serial_number);
        self.c.send_cmd_end(&mut out);
    }

    /// Stores the key of a `write_license_key` command in the EEPROM and commits it (the command
    /// is accepted for the host's sake; nothing reads the key back).
    fn on_write_license_key(&mut self) {
        let mut key = [0u8; 256];
        let len = match self.c.read_string_arg() {
            Some(s) => {
                let n = s.len().min(key.len());
                key[..n].copy_from_slice(&s[..n]);
                n
            }
            None => 0,
        };
        self.eeprom.write_string(EEPROM_LICENSE_OFFSET, &key[..len]);
    }

    /// Answers a command without a handler with an error message.
    fn on_unknown_command(&mut self) {
        let mut out = Out(&self.serial);
        self.c.send_cmd_str(&mut out, cmd::ERROR, UNKNOWN_COMMAND_TEXT);
    }
}
