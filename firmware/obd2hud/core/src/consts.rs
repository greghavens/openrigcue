//! Constants of the firmware.

/// The product id (first byte argument of `i_am`).
pub const PRODUCT: u8 = 3;
/// The model id.
pub const MODEL: u8 = 1;
/// The version.
pub const VERSION: u8 = 1;

/// CmdMessenger command ids. Commands 0 and 7 have handlers, the rest go to the default
/// handler; the firmware sends 1 (`i_am`), 5 (logger) and 8 (error).
pub mod cmd {
    pub const WHO_ARE_YOU: u8 = 0;
    pub const I_AM: u8 = 1;
    pub const LOGGER: u8 = 5;
    pub const WRITE_LICENSE_KEY: u8 = 7;
    pub const ERROR: u8 = 8;
}

/// The field separator, command separator and escape character of the messenger.
pub const FIELD_SEPARATOR: u8 = b',';
pub const COMMAND_SEPARATOR: u8 = b';';
pub const ESCAPE_CHARACTER: u8 = b'/';

/// The text of the answer to an unknown command.
pub const UNKNOWN_COMMAND_TEXT: &[u8] = b"Command without callback.";
/// The logger lines sent during set-up.
pub const READY_TEXT: &[u8] = b"***READY****";
pub const BLE_STARTED_TEXT: &[u8] = b"***BLESTARTED****";

/// The XTEA key of the handshake (16 bytes).
pub const XTEA_KEY: [u8; 16] =
    [0xe2, 0x6d, 0x6f, 0xa9, 0x6e, 0xc2, 0x55, 0xd6, 0x00, 0x10, 0x4a, 0x33, 0xff, 0x18, 0x48, 0x4f];
/// The number of XTEA cycles of the handshake.
pub const XTEA_ROUNDS: u32 = 32;

/// The licence status in the handshake answer. This firmware
/// has no licensing and always reports the "licensed" value.
pub const LICENSE_STATUS_REPORTED: u8 = 1;
/// The fourth byte argument of `i_am`, always 0.
pub const I_AM_LICENSE_BYTE: u8 = 0;

/// The size of the emulated EEPROM.
pub const EEPROM_SIZE: usize = 0x49;
/// The NVS namespace and blob key of the EEPROM, and the key of its resize probe.
pub const EEPROM_NVS_NAME: &str = "eeprom";
pub const EEPROM_NVS_EXPAND_KEY: &str = "expand";
/// EEPROM[0..6]: copy of the factory MAC; EEPROM[6..12]: random salt bytes.
pub const EEPROM_MAC_OFFSET: usize = 0;
pub const EEPROM_SALT_OFFSET: usize = 6;
pub const EEPROM_ID_LEN: usize = 6;
/// The EEPROM offset of the licence key string.
pub const EEPROM_LICENSE_OFFSET: i32 = 12;

/// Status LED: an output, switched on at set-up, toggled per received CAN frame.
pub const LED_PIN: u32 = 2;
/// The LED status starts as on.
pub const LED_STATUS_INITIAL: bool = true;
/// HUD power output: an output, initially low; the main loop drives it with
/// the telemetry power flag, the BLE disconnect callback switches it off.
pub const POWER_PIN: u32 = 19;
/// The pin whose analog reading seeds the random generator at set-up.
pub const ADC_SEED_PIN: i32 = 27;

/// The CAN TX and RX pins.
pub const CAN_TX_PIN: i32 = 23;
pub const CAN_RX_PIN: i32 = 22;
/// The CAN speed in kbit/s.
pub const CAN_SPEED: u32 = 1000;
/// The receive queue: 10 frames of 16 bytes.
pub const CAN_RX_QUEUE_LEN: u32 = 10;
pub const CAN_FRAME_SIZE: u32 = 16;
/// How many ticks the main loop waits on the receive queue.
pub const CAN_RX_WAIT_TICKS: u32 = 3;
/// The CAN interrupt source (ETS_CAN_INTR_SOURCE).
pub const CAN_INTR_SOURCE: i32 = 0x2d;
/// The GPIO matrix signals of CAN TX and RX.
pub const CAN_TX_SIGNAL: u32 = 0x7b;
pub const CAN_RX_SIGNAL: u32 = 0x5e;

/// The serial link: UART0 at 115200 baud (TX GPIO1, RX GPIO3).
pub const UART_BAUD: u32 = 115_200;
/// The UART configuration word for 8N1.
pub const UART_CONFIG_8N1: u32 = 0x0800_001c;
pub const UART_TX_PIN: i32 = 1;
pub const UART_RX_PIN: i32 = 3;
/// The length of the serial receive queue (256 bytes).
pub const UART_RX_QUEUE_LEN: usize = 256;
/// The timeout of a serial read, 1000 ms.
pub const STREAM_TIMEOUT_MS: u32 = 1000;

/// The loop task: 0x2000 bytes of stack, priority 1, pinned to core 1.
pub const LOOP_TASK_STACK: u32 = 0x2000;
pub const LOOP_TASK_PRIORITY: u32 = 1;
pub const LOOP_TASK_CORE: i32 = 1;

/// NVS initialisation results after which NVS is erased, and the not-found error.
pub const ESP_ERR_NVS_NO_FREE_PAGES: i32 = 0x110d;
pub const ESP_ERR_NVS_NEW_VERSION_FOUND: i32 = 0x1110;
pub const ESP_ERR_NVS_NOT_FOUND: i32 = 0x1102;
