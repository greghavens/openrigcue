//! Hardware layer: clocks, pins, the serial link, NVS and the CAN controller,
//! through ESP-IDF calls and direct register access.

use alloc::{format, vec::Vec};
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};
use esp_idf_sys::runtime::{Mutex, MutexGuard};

use esp_idf_sys as sys;
use obd2hud_core::can::{
    self, CanFrame, Regs, CAN_BASE, DEFAULT_FILTER, DPORT_CAN_BIT, DPORT_PERIP_CLK_EN, DPORT_PERIP_RST_EN,
};
use obd2hud_core::consts::*;
use obd2hud_core::eeprom::NvsBackend;

extern "C" {
    /// Requests a context switch at the end of an interrupt on Xtensa.
    fn _frxt_setup_switch();
    /// Reads a DPORT register on ESP32.
    fn esp_dport_access_reg_read(reg: u32) -> u32;
    fn srand(seed: u32);
    /// The IDF stack guard value (CONFIG_COMPILER_STACK_CHECK), set from the hardware
    /// random generator at start-up.
    static __stack_chk_guard: *mut c_void;
    // soc/rtc.h (ESP32), not in the generated bindings.
    fn rtc_clk_xtal_freq_get() -> u32;
    fn rtc_clk_cpu_freq_get_config(out: *mut RtcCpuFreqConfig);
    fn rtc_clk_cpu_freq_mhz_to_config(freq_mhz: u32, out: *mut RtcCpuFreqConfig) -> bool;
    fn rtc_clk_cpu_freq_set_config_fast(cfg: *const RtcCpuFreqConfig);
    fn rtc_clk_apb_freq_update(apb_freq: u32);
    fn esp_timer_impl_update_apb_freq(apb_ticks_per_us: u32);
    /// FreeRTOS Xtensa port: CPU cycles per tick.
    static mut _xt_tick_divisor: u32;
}

/// The IDF CPU frequency configuration { source, source_freq_mhz, div, freq_mhz }.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct RtcCpuFreqConfig {
    source: u32,
    source_freq_mhz: u32,
    div: u32,
    freq_mhz: u32,
}

const PD_TRUE: i32 = 1;
const QUEUE_TYPE_BASE: u8 = 0;
const QUEUE_TYPE_BINARY_SEMAPHORE: u8 = 3;
const QUEUE_SEND_TO_BACK: i32 = 0;
const PORT_MAX_DELAY: u32 = 0xffff_ffff;

// ---------------------------------------------------------------- start-up

/// EFUSE_BLK0_RDATA3_REG: bit 13 CHIP_CPU_FREQ_RATED, bit 12 CHIP_CPU_FREQ_LOW.
const EFUSE_BLK0_RDATA3_REG: usize = 0x3ff5_a00c;

/// APB frequency for a CPU config:
/// 80 MHz when freq_mhz >= 80, else source_freq_mhz * 1e6 / div.
fn apb_of(c: &RtcCpuFreqConfig) -> u32 {
    if c.freq_mhz > 79 {
        80_000_000
    } else {
        c.source_freq_mhz * 1_000_000 / c.div
    }
}

/// Sets the CPU frequency; false when the frequency is not available.
/// The APB-change callback list is empty at start-up (the serial link registers
/// its callback later), so no callback is triggered then; when callbacks exist they
/// are triggered before and after the change, whether or not the APB value changes.
pub fn set_cpu_frequency_mhz(mut cpu_freq_mhz: u32) -> bool {
    const FUNC: &str = "setCpuFrequencyMhz";
    unsafe {
        let xtal = rtc_clk_xtal_freq_get();
        if xtal != 0 {
            if xtal < 40 {
                if !(cpu_freq_mhz >= xtal || cpu_freq_mhz == xtal / 2) {
                    log_e(
                        HAL_CPU_C,
                        0x95,
                        FUNC,
                        &format!(
                            "Bad frequency: {cpu_freq_mhz} MHz! Options are: 240, 160, 80, {xtal} and {} MHz",
                            xtal / 2
                        ),
                    );
                    return false;
                }
            } else if !(cpu_freq_mhz >= xtal || cpu_freq_mhz == xtal / 2 || cpu_freq_mhz == xtal / 4) {
                log_e(
                    HAL_CPU_C,
                    0x99,
                    FUNC,
                    &format!(
                        "Bad frequency: {cpu_freq_mhz} MHz! Options are: 240, 160, 80, {xtal}, {} and {} MHz",
                        xtal / 2,
                        xtal / 4
                    ),
                );
                return false;
            }
        }
        if cpu_freq_mhz > xtal && cpu_freq_mhz != 240 && cpu_freq_mhz != 160 && cpu_freq_mhz != 80 {
            if xtal < 40 {
                log_e(
                    HAL_CPU_C,
                    0xa1,
                    FUNC,
                    &format!(
                        "Bad frequency: {cpu_freq_mhz} MHz! Options are: 240, 160, 80, {xtal} and {} MHz",
                        xtal / 2
                    ),
                );
            } else {
                log_e(
                    HAL_CPU_C,
                    0x9f,
                    FUNC,
                    &format!(
                        "Bad frequency: {cpu_freq_mhz} MHz! Options are: 240, 160, 80, {xtal}, {} and {} MHz",
                        xtal / 2,
                        xtal / 4
                    ),
                );
            }
            return false;
        }
        if cpu_freq_mhz == 240 {
            let r = ptr::read_volatile(EFUSE_BLK0_RDATA3_REG as *const u32);
            if r & (1 << 13) != 0 && ptr::read_volatile(EFUSE_BLK0_RDATA3_REG as *const u32) & 0x1000 != 0 {
                log_e(HAL_CPU_C, 0xaa, FUNC, "Can not switch to 240 MHz! Chip CPU frequency rated for 160MHz.");
                cpu_freq_mhz = 160;
            }
        }
        let mut cconf = RtcCpuFreqConfig::default();
        rtc_clk_cpu_freq_get_config(&mut cconf);
        if cconf.freq_mhz == cpu_freq_mhz {
            return true;
        }
        let mut conf = RtcCpuFreqConfig::default();
        if !rtc_clk_cpu_freq_mhz_to_config(cpu_freq_mhz, &mut conf) {
            log_e(HAL_CPU_C, 0xb6, FUNC, &format!("CPU clock could not be set to {cpu_freq_mhz} MHz"));
            return false;
        }
        let capb = apb_of(&cconf);
        let apb = apb_of(&conf);
        if apb_callbacks_present() {
            trigger_apb_change_callback(APB_BEFORE_CHANGE, capb, apb);
        }
        rtc_clk_cpu_freq_set_config_fast(&conf);
        if capb != apb {
            rtc_clk_apb_freq_update(apb);
            esp_timer_impl_update_apb_freq(apb / 1_000_000);
        }
        let hz = if conf.freq_mhz > 79 { conf.freq_mhz * 1_000_000 } else { apb };
        _xt_tick_divisor = hz / 1000;
        if apb_callbacks_present() {
            trigger_apb_change_callback(APB_AFTER_CHANGE, capb, apb);
        }
        true
    }
}

// ------------------------------------------------------ APB change callbacks

/// The APB change events.
const APB_BEFORE_CHANGE: u32 = 0;
const APB_AFTER_CHANGE: u32 = 1;

/// An APB change callback: argument, event, old and new APB frequency.
type ApbChangeCb = unsafe fn(arg: usize, ev_type: u32, old_apb: u32, new_apb: u32);

/// The APB change callbacks, behind their lock:
/// element 0 is the list head; a new callback is prepended.
static APB_CALLBACKS: Mutex<Vec<(usize, ApbChangeCb)>> = Mutex::new(Vec::new());

fn apb_lock() -> MutexGuard<'static, Vec<(usize, ApbChangeCb)>> {
    APB_CALLBACKS.lock()
}

/// Whether any APB change callback is registered.
fn apb_callbacks_present() -> bool {
    !apb_lock().is_empty()
}

/// Triggers the APB change callbacks: BEFORE runs head to
/// tail, AFTER runs tail to head, under the list lock.
fn trigger_apb_change_callback(ev_type: u32, old_apb: u32, new_apb: u32) {
    let list = apb_lock();
    if ev_type == APB_BEFORE_CHANGE {
        for (arg, cb) in list.iter() {
            unsafe { cb(*arg, APB_BEFORE_CHANGE, old_apb, new_apb) };
        }
    } else {
        for (arg, cb) in list.iter().rev() {
            unsafe { cb(*arg, ev_type, old_apb, new_apb) };
        }
    }
}

/// Adds an APB change callback: a (cb, arg) pair already present logs
/// a duplicate error and returns false; else the entry is prepended.
fn add_apb_change_callback(arg: usize, cb: ApbChangeCb) -> bool {
    let mut list = apb_lock();
    if list.iter().any(|(a, c)| *a == arg && *c as usize == cb as usize) {
        drop(list);
        log_e(HAL_CPU_C, 0x5d, "addApbChangeCallback", &format!("duplicate func={:08X} arg={:08X}", cb as usize, arg));
        return false;
    }
    list.insert(0, (arg, cb));
    true
}

/// Start-up of the runtime: CPU at 240 MHz, log level ERROR, NVS init (erase
/// the partition when it reports NO_FREE_PAGES). Bluetooth is in use, so the
/// controller memory is kept; the BLE set-up
/// releases only the Classic part later.
pub fn init_arduino() {
    unsafe {
        set_cpu_frequency_mhz(240);
        sys::esp_log_level_set(c"*".as_ptr(), sys::esp_log_level_t_ESP_LOG_ERROR);
        let mut err = sys::nvs_flash_init();
        if err == ESP_ERR_NVS_NO_FREE_PAGES {
            let p = sys::esp_partition_find_first(
                sys::esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
                sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_DATA_NVS,
                ptr::null(),
            );
            if !p.is_null() {
                err = sys::esp_partition_erase_range(p, 0, (*p).size as usize);
                if err == 0 {
                    err = sys::nvs_flash_init();
                } else {
                    // The format error is logged; the NVS error below then
                    // follows with the erase result.
                    log_e(HAL_MISC_C, 0xd5, "initArduino", "Failed to format the broken NVS partition!");
                }
            }
        }
        if err != 0 {
            log_e(HAL_MISC_C, 0xda, "initArduino", &format!("Failed to initialize NVS! Error: {}", err as u32));
        }
        // Bluetooth is in use: the controller memory is not released here.
    }
}

/// The file names in error log lines.
const HAL_MISC_C: &str = "esp32-hal-misc.c";
const HAL_CPU_C: &str = "esp32-hal-cpu.c";

/// Logs an error line of the form `"[E][file:line] func(): ...\r\n"` on the console,
/// holding UART0's lock once the serial link exists.
fn log_e(file: &str, line: u32, func: &str, msg: &str) {
    let mut buf = format!("[E][{file}:{line}] {func}(): {msg}\r\n").into_bytes();
    buf.push(0);
    let _g = if UART_QUEUE.load(Ordering::Acquire).is_null() { None } else { Some(UART_LOCK.lock()) };
    unsafe { sys::esp_rom_printf(c"%s".as_ptr(), buf.as_ptr() as *const core::ffi::c_char) };
}

/// Delays the task for `ms` milliseconds (1 kHz tick).
pub fn delay(ms: u32) {
    unsafe { sys::vTaskDelay(ms * sys::configTICK_RATE_HZ / 1000) }
}

/// Prints text on the C standard output (the UART0 console, `\n` -> `\r\n`):
/// the text is formatted in Rust and printed with `"%s"`.
pub fn printf(text: &str) {
    let mut buf = Vec::with_capacity(text.len() + 1);
    buf.extend_from_slice(text.as_bytes());
    buf.push(0);
    unsafe {
        sys::printf(c"%s".as_ptr(), buf.as_ptr() as *const core::ffi::c_char);
    }
}

/// Creates the loop task: 0x2000 bytes of stack, priority 1, pinned to core 1.
pub fn spawn_loop_task(f: extern "C" fn(*mut c_void)) {
    unsafe {
        let mut handle: sys::TaskHandle_t = ptr::null_mut();
        sys::xTaskCreatePinnedToCore(
            Some(f),
            c"loopTask".as_ptr(),
            LOOP_TASK_STACK,
            ptr::null_mut(),
            LOOP_TASK_PRIORITY,
            &mut handle,
            LOOP_TASK_CORE,
        );
    }
}

pub fn millis() -> u32 {
    unsafe { (sys::esp_timer_get_time() / 1000) as u32 }
}

pub fn esp_random() -> u32 {
    unsafe { sys::esp_random() }
}

/// Seeds the C random generator, unless the seed is 0.
pub fn random_seed(seed: u32) {
    if seed != 0 {
        unsafe { srand(seed) }
    }
}

/// The GPIO27 attachment and ADC2 read, before BLE start-up.
pub fn analog_read_gpio27() -> u32 {
    obd2hud_core::analog::read_gpio27(&mut Analog27)
}

struct Analog27;
impl obd2hud_core::analog::Io for Analog27 {
    fn read(&mut self, address: usize) -> u32 {
        unsafe { ptr::read_volatile(address as *const u32) }
    }
    fn write(&mut self, address: usize, value: u32) {
        unsafe { ptr::write_volatile(address as *mut u32, value) }
    }
    fn attenuation(&mut self, channel: u8, value: u8) {
        unsafe { sys::adc2_config_channel_atten(channel as _, value as _) };
    }
    fn clock_divider(&mut self, value: u8) {
        unsafe { sys::adc_set_clk_div(value) };
    }
    fn width(&mut self, value: u8) {
        unsafe { sys::adc1_config_width(value as _) };
    }
    fn adc2_raw(&mut self, channel: u8, width: u8, value: &mut i32) -> i32 {
        unsafe { sys::adc2_get_raw(channel as _, width as _, value) }
    }
    fn read_error(&mut self, error: i32) {
        let suffix = match error {
            0x103 => ": ADC2 not initialized yet.",
            0x107 => ": ADC2 is in use by Wi-Fi.",
            _ => "",
        };
        let line = match error {
            0x103 => 0x85,
            0x107 => 0x87,
            _ => 0x89,
        };
        let description = unsafe { core::ffi::CStr::from_ptr(sys::esp_err_to_name(error)) }.to_string_lossy();
        log_e("esp32-hal-adc.c", line, "__analogRead", &format!("GPIO27: {description}{suffix}"));
    }
}

/// The factory MAC address from the eFuse.
pub fn efuse_mac() -> [u8; 6] {
    let mut mac = [0u8; 6];
    unsafe {
        sys::esp_efuse_mac_get_default(mac.as_mut_ptr());
    }
    mac
}

// ---------------------------------------------------------------- GPIO2 LED, GPIO19 HUD power

/// The RTC pad of GPIO2: register (RTC_IO_TOUCH_PAD2_REG), mux, pull-up, pull-down
/// (GPIO19 has no RTC pad).
const RTC_GPIO2_REG: usize = 0x3ff4_849c;
const RTC_GPIO2_MUX: u32 = 0x0008_0000;
const RTC_GPIO2_PULLUP: u32 = 0x0800_0000;
const RTC_GPIO2_PULLDOWN: u32 = 0x1000_0000;
/// The IO_MUX registers of the pins (IO_MUX base + 0x40 for GPIO2, + 0x74 for GPIO19).
const IO_MUX_GPIO2_REG: usize = 0x3ff4_9040;
const IO_MUX_GPIO19_REG: usize = 0x3ff4_9074;
/// GPIO_ENABLE_W1TS_REG, GPIO_PINn_REG (0x3ff44088 + 4n).
const GPIO_ENABLE_W1TS_REG: usize = 0x3ff4_4024;
const GPIO_PIN2_REG: usize = 0x3ff4_4090;
const GPIO_PIN19_REG: usize = 0x3ff4_40d4;
/// GPIO_OUT_W1TS_REG / GPIO_OUT_W1TC_REG.
const GPIO_OUT_W1TS_REG: usize = 0x3ff4_4008;
const GPIO_OUT_W1TC_REG: usize = 0x3ff4_400c;
/// FUN_DRV = 2, FUN_IE, MCU_SEL = 2 (OUTPUT without pulls).
const IO_MUX_OUTPUT_FUNC: u32 = 0x2a00;

/// Makes one of the two output pins
/// an output: an RTC pad (GPIO2) first gets its RTC mux bit cleared, then pull-up and
/// pull-down cleared; then `GPIO.enable_w1ts = 1 << pin; IO_MUX = 0x2a00; GPIO.pin[pin] = 0`.
pub fn pin_mode_output(pin: u32) {
    let (io_mux, gpio_pin) = match pin {
        2 => (IO_MUX_GPIO2_REG, GPIO_PIN2_REG),
        19 => (IO_MUX_GPIO19_REG, GPIO_PIN19_REG),
        _ => unreachable!("only GPIO2 and GPIO19 are outputs in this sketch"),
    };
    unsafe {
        if pin == 2 {
            let rtc = RTC_GPIO2_REG as *mut u32;
            let v = ptr::read_volatile(rtc);
            ptr::write_volatile(rtc, !RTC_GPIO2_MUX & v);
            let v = ptr::read_volatile(rtc);
            ptr::write_volatile(rtc, !(RTC_GPIO2_PULLUP | RTC_GPIO2_PULLDOWN) & v);
        }
        ptr::write_volatile(GPIO_ENABLE_W1TS_REG as *mut u32, 1 << pin);
        ptr::write_volatile(io_mux as *mut u32, IO_MUX_OUTPUT_FUNC);
        ptr::write_volatile(gpio_pin as *mut u32, 0);
    }
}

/// Sets the level of a pin below 32:
/// `GPIO.out_w1ts = 1 << pin` when high, else `GPIO.out_w1tc = 1 << pin`. Called from
/// the loop task and from the NimBLE host task (disconnect handling).
#[inline(always)]
pub fn digital_write(pin: u32, high: bool) {
    let reg = if high { GPIO_OUT_W1TS_REG } else { GPIO_OUT_W1TC_REG };
    unsafe { ptr::write_volatile(reg as *mut u32, 1 << pin) }
}

// ---------------------------------------------------------------- UART0 / Serial
//
// The serial link drives the UART registers directly (the IDF UART driver
// is not used).
// RX: the ISR moves the hardware FIFO into a 256-entry byte queue and drops
// bytes when the queue is full; the available count = queue count + FIFO count.
// TX: bytes go straight into the FIFO (busy wait while it holds 127).
// Writes take a mutex: the loop task (messenger) and the NimBLE
// host task (the lines of the OTA handler) both write to the serial link.
// Reads happen in the loop task only.

/// The lock of the serial link, taken per write call.
static UART_LOCK: Mutex<()> = Mutex::new(());

/// UART0 register block (on the DPORT bus; RX
/// FIFO reads and TX FIFO writes go through offset 0 of this address).
const UART0_BASE: usize = 0x3ff4_0000;
const UART_FIFO: usize = 0x00;
const UART_INT_ENA: usize = 0x0c;
const UART_INT_CLR: usize = 0x10;
const UART_CLKDIV: usize = 0x14;
const UART_STATUS: usize = 0x1c;
const UART_CONF0: usize = 0x20;
const UART_CONF1: usize = 0x24;
const UART_IDLE_CONF: usize = 0x40;
const UART_RS485_CONF: usize = 0x44;
const UART_MEM_RX_STATUS: usize = 0x60;
/// int_ena/int_clr bits: rxfifo_full, frm_err, rxfifo_tout.
const UART_INT_RXFIFO_FULL: u32 = 0x001;
const UART_INT_FRM_ERR: u32 = 0x008;
const UART_INT_RXFIFO_TOUT: u32 = 0x100;
/// DPORT_UART_CLK_EN / DPORT_UART_RST (bit 2).
const DPORT_UART0_BIT: u32 = 0x4;
/// The IO_MUX registers of GPIO1 (base + 0x88) and GPIO3 (base + 0x84).
const IO_MUX_GPIO1_REG: usize = 0x3ff4_9088;
const IO_MUX_GPIO3_REG: usize = 0x3ff4_9084;
const GPIO_ENABLE_W1TC_REG: usize = 0x3ff4_4028;
const GPIO_PIN1_REG: usize = 0x3ff4_408c;
const GPIO_PIN3_REG: usize = 0x3ff4_4094;
/// U0RXD_IN_IDX / U0TXD_OUT_IDX.
const U0RXD_IN_IDX: u32 = 14;
const U0TXD_OUT_IDX: u32 = 14;
/// conf1: rxfifo_full_thrhd = 112, rx_tout_thrhd = 2, rx_tout_en.
const UART_RX_FULL_THRHD: u32 = 112;
const UART_RX_TOUT_THRHD: u32 = 2;
/// status.txfifo_cnt value at which a write waits.
const UART_TX_FIFO_FULL: u32 = 0x7f;

static UART_QUEUE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

#[inline(always)]
fn uart_rd(off: usize) -> u32 {
    unsafe { ptr::read_volatile((UART0_BASE + off) as *const u32) }
}

#[inline(always)]
fn uart_wr(off: usize, v: u32) {
    unsafe { ptr::write_volatile((UART0_BASE + off) as *mut u32, v) }
}

/// status.rxfifo_cnt (bits 0-7).
#[inline(always)]
fn uart_rx_fifo_cnt() -> u32 {
    uart_rd(UART_STATUS) & 0xff
}

/// Whether the RX write and read addresses of mem_rx_status differ (bits 13-23 / 2-12); the register is read twice.
#[inline(always)]
fn uart_rx_addr_differs() -> bool {
    let wr = (uart_rd(UART_MEM_RX_STATUS) >> 13) & 0x7ff;
    let rd = (uart_rd(UART_MEM_RX_STATUS) >> 2) & 0x7ff;
    wr != rd
}

/// One byte from the RX FIFO (byte read at the register block base).
#[inline(always)]
fn uart_fifo_read() -> u8 {
    unsafe { ptr::read_volatile((UART0_BASE + UART_FIFO) as *const u8) }
}

/// The APB change callback of the serial link.
/// BEFORE: lock, interrupts off and cleared, drain the RX FIFO into the queue
/// (waiting one tick; bytes are read and dropped while there is no queue), unlock, wait
/// for txfifo_cnt and st_utx_out to reach 0. AFTER: lock, `clkdiv = (new_apb << 4) /
/// ((old_apb << 4) / clkdiv_reg)`, re-enable rxfifo_full / frm_err / rxfifo_tout, clear all,
/// unlock.
unsafe fn uart_on_apb_change(_arg: usize, ev_type: u32, old_apb: u32, new_apb: u32) {
    if ev_type == APB_BEFORE_CHANGE {
        {
            let _g = UART_LOCK.lock();
            uart_wr(UART_INT_ENA, 0);
            uart_wr(UART_INT_CLR, 0xffff_ffff);
            while uart_rx_fifo_cnt() != 0 || uart_rx_addr_differs() {
                let c = uart_fifo_read();
                let q = UART_QUEUE.load(Ordering::Acquire);
                if !q.is_null() {
                    sys::xQueueGenericSend(
                        q as sys::QueueHandle_t,
                        &c as *const u8 as *const c_void,
                        1,
                        QUEUE_SEND_TO_BACK,
                    );
                }
            }
        }
        while (uart_rd(UART_STATUS) >> 16) & 0xff != 0 || (uart_rd(UART_STATUS) >> 24) & 0xf != 0 {}
        return;
    }
    let _g = UART_LOCK.lock();
    let reg = uart_rd(UART_CLKDIV);
    let cur = ((reg & 0x000f_ffff) << 4) | ((reg >> 20) & 0xf);
    let clk_div = (new_apb << 4) / ((old_apb << 4) / cur);
    uart_wr(UART_CLKDIV, (uart_rd(UART_CLKDIV) & 0xfff0_0000) | ((clk_div >> 4) & 0x000f_ffff));
    uart_wr(UART_CLKDIV, (uart_rd(UART_CLKDIV) & 0xff0f_ffff) | ((clk_div & 0xf) << 20));
    uart_wr(UART_INT_ENA, uart_rd(UART_INT_ENA) | UART_INT_RXFIFO_FULL);
    uart_wr(UART_INT_ENA, uart_rd(UART_INT_ENA) | UART_INT_FRM_ERR);
    uart_wr(UART_INT_ENA, uart_rd(UART_INT_ENA) | UART_INT_RXFIFO_TOUT);
    uart_wr(UART_INT_CLR, 0xffff_ffff);
}

/// Sets the direction of the non-RTC pins 1 and 3:
/// INPUT -> GPIO.enable_w1tc, OUTPUT -> GPIO.enable_w1ts; IO_MUX = FUN_DRV 2 |
/// FUN_IE | MCU_SEL 2 (0x2a00, no pull bits); GPIO.pin[pin] = 0.
fn pin_mode_plain(pin: u32, output: bool, io_mux_reg: usize, gpio_pin_reg: usize) {
    unsafe {
        if output {
            ptr::write_volatile(GPIO_ENABLE_W1TS_REG as *mut u32, 1 << pin);
        } else {
            ptr::write_volatile(GPIO_ENABLE_W1TC_REG as *mut u32, 1 << pin);
        }
        ptr::write_volatile(io_mux_reg as *mut u32, IO_MUX_OUTPUT_FUNC);
        ptr::write_volatile(gpio_pin_reg as *mut u32, 0);
    }
}

/// The UART interrupt handler (IRAM; only UART0 has an interrupt in this firmware).
#[link_section = ".iram1.obd2hud_uart_isr"]
#[unsafe(naked)]
unsafe extern "C" fn uart_isr(_arg: *mut c_void) {
    core::arch::naked_asm!("entry a1,48","mov a10,a1","call8 {body}","retw.n",body=sym uart_isr_body);
}

#[inline(never)]
#[link_section = ".iram1.orc_uart_body"]
unsafe extern "C" fn uart_isr_body(native: *mut u8) {
    let woken = native.add(12).cast::<i32>();

    uart_wr(UART_INT_CLR, uart_rd(UART_INT_CLR) | UART_INT_RXFIFO_FULL);
    uart_wr(UART_INT_CLR, uart_rd(UART_INT_CLR) | UART_INT_FRM_ERR);
    uart_wr(UART_INT_CLR, uart_rd(UART_INT_CLR) | UART_INT_RXFIFO_TOUT);
    let q = UART_QUEUE.load(Ordering::Relaxed);
    while uart_rx_fifo_cnt() != 0 || uart_rx_addr_differs() {
        let c = uart_fifo_read();
        if !q.is_null() {
            // A full queue rejects the byte: it is lost.
            sys::xQueueGenericSendFromISR(
                q as sys::QueueHandle_t,
                &c as *const u8 as *const c_void,
                woken,
                QUEUE_SEND_TO_BACK,
            );
        }
    }
    if native_stack_word(woken.cast()) != 0 {
        _frxt_setup_switch();
    }
}

pub struct Uart0;

impl Uart0 {
    /// Begins the serial link on UART0: 8N1, RX on GPIO3, TX on GPIO1,
    /// a receive queue of 0x100 bytes.
    pub fn begin(baud: u32) -> Self {
        unsafe {
            let q = sys::xQueueGenericCreate(UART_RX_QUEUE_LEN as u32, 1, QUEUE_TYPE_BASE);
            UART_QUEUE.store(q as *mut c_void, Ordering::Release);
            // The UART clock is enabled and its reset released.
            let clk = esp_dport_access_reg_read(DPORT_PERIP_CLK_EN as u32);
            ptr::write_volatile(DPORT_PERIP_CLK_EN as *mut u32, clk | DPORT_UART0_BIT);
            let rst = esp_dport_access_reg_read(DPORT_PERIP_RST_EN as u32);
            ptr::write_volatile(DPORT_PERIP_RST_EN as *mut u32, rst & !DPORT_UART0_BIT);
        }
        // Flush the transmitter: wait for txfifo_cnt and st_utx_out to be 0.
        while (uart_rd(UART_STATUS) >> 16) & 0xff != 0 || (uart_rd(UART_STATUS) >> 24) & 0xf != 0 {}
        // The baud rate: clk_div = (APB frequency << 4) / baud.
        let mut cconf = RtcCpuFreqConfig::default();
        unsafe { rtc_clk_cpu_freq_get_config(&mut cconf) };
        let clk_div = (apb_of(&cconf) << 4) / baud;
        uart_wr(UART_CLKDIV, (uart_rd(UART_CLKDIV) & 0xfff0_0000) | ((clk_div >> 4) & 0x000f_ffff));
        uart_wr(UART_CLKDIV, (uart_rd(UART_CLKDIV) & 0xff0f_ffff) | ((clk_div & 0xf) << 20));
        // conf0 = config; two stop bits -> one + rs485 dl1_en (not the case for 8N1).
        uart_wr(UART_CONF0, UART_CONFIG_8N1);
        if (uart_rd(UART_CONF0) >> 4) & 3 == 3 {
            uart_wr(UART_CONF0, (uart_rd(UART_CONF0) & !0x30) | 0x10);
            uart_wr(UART_RS485_CONF, uart_rd(UART_RS485_CONF) | 0x4);
        }
        // idle_conf.tx_idle_num = 0
        uart_wr(UART_IDLE_CONF, uart_rd(UART_IDLE_CONF) & 0xfff0_03ff);
        // RX: the pin is an input routed to the UART0 RX signal;
        // then the receive interrupts are enabled.
        pin_mode_plain(UART_RX_PIN as u32, false, IO_MUX_GPIO3_REG, GPIO_PIN3_REG);
        unsafe { sys::esp_rom_gpio_connect_in_signal(UART_RX_PIN as u32, U0RXD_IN_IDX, false) };
        uart_wr(UART_CONF1, (uart_rd(UART_CONF1) & !0x7f) | UART_RX_FULL_THRHD);
        uart_wr(UART_CONF1, (uart_rd(UART_CONF1) & 0x80ff_ffff) | (UART_RX_TOUT_THRHD << 24));
        uart_wr(UART_CONF1, uart_rd(UART_CONF1) | 0x8000_0000);
        uart_wr(UART_INT_ENA, uart_rd(UART_INT_ENA) | UART_INT_RXFIFO_FULL);
        uart_wr(UART_INT_ENA, uart_rd(UART_INT_ENA) | UART_INT_FRM_ERR);
        uart_wr(UART_INT_ENA, uart_rd(UART_INT_ENA) | UART_INT_RXFIFO_TOUT);
        uart_wr(UART_INT_CLR, 0xffff_ffff);
        unsafe {
            sys::esp_intr_alloc(
                sys::periph_interrupt_t_ETS_UART0_INTR_SOURCE as i32,
                sys::ESP_INTR_FLAG_IRAM as i32,
                Some(uart_isr),
                ptr::null_mut(),
                ptr::null_mut(),
            );
        }
        // TX: the pin is an output driven by the UART0 TX signal.
        pin_mode_plain(UART_TX_PIN as u32, true, IO_MUX_GPIO1_REG, GPIO_PIN1_REG);
        unsafe { sys::esp_rom_gpio_connect_out_signal(UART_TX_PIN as u32, U0TXD_OUT_IDX, false, false) };
        // The APB change callback is registered. There is one UART,
        // so its register base serves as the callback argument.
        add_apb_change_callback(UART0_BASE, uart_on_apb_change);
        Uart0
    }

    fn queue(&self) -> sys::QueueHandle_t {
        UART_QUEUE.load(Ordering::Acquire) as sys::QueueHandle_t
    }

    /// The number of bytes available: queue count + status.rxfifo_cnt.
    pub fn available(&self) -> usize {
        let q = self.queue();
        if q.is_null() {
            return 0;
        }
        let n = unsafe { sys::uxQueueMessagesWaiting(q) } as usize;
        n + uart_rx_fifo_cnt() as usize
    }

    /// Moves the RX FIFO to the queue: interrupts off, drain the FIFO into the queue
    /// (without waiting), interrupts back on.
    fn rx_fifo_to_queue(&self) {
        // This whole register sequence is serialized with writes from
        // the loop task and NimBLE. In particular, no OTA log may enter the shared FIFO
        // while the RX drain owns the UART lock.
        let _lock = UART_LOCK.lock();
        let q = self.queue();
        uart_wr(UART_INT_ENA, 0);
        uart_wr(UART_INT_CLR, 0xffff_ffff);
        while uart_rx_fifo_cnt() != 0 || uart_rx_addr_differs() {
            let c = uart_fifo_read();
            unsafe { sys::xQueueGenericSend(q, &c as *const u8 as *const c_void, 0, QUEUE_SEND_TO_BACK) };
        }
        uart_wr(UART_INT_ENA, uart_rd(UART_INT_ENA) | UART_INT_RXFIFO_FULL);
        uart_wr(UART_INT_ENA, uart_rd(UART_INT_ENA) | UART_INT_FRM_ERR);
        uart_wr(UART_INT_ENA, uart_rd(UART_INT_ENA) | UART_INT_RXFIFO_TOUT);
        uart_wr(UART_INT_CLR, 0xffff_ffff);
    }

    /// Reads one byte: -1 when nothing is available.
    pub fn read(&self) -> i32 {
        if self.available() == 0 {
            return -1;
        }
        let q = self.queue();
        unsafe {
            if sys::uxQueueMessagesWaiting(q) == 0 && uart_rx_fifo_cnt() != 0 {
                self.rx_fifo_to_queue();
            }
            let mut c: u8 = 0;
            if sys::xQueueReceive(q, &mut c as *mut u8 as *mut c_void, 0) == 0 {
                return 0;
            }
            c as i32
        }
    }

    /// Reads bytes into `buf` with a timeout per byte (re-armed
    /// for each byte), and stops at the first timeout.
    pub fn read_bytes(&self, buf: &mut [u8], timeout_ms: u32) -> usize {
        let mut count = 0;
        for b in buf.iter_mut() {
            let start = millis();
            let c = loop {
                let c = self.read();
                if c >= 0 {
                    break c;
                }
                if millis().wrapping_sub(start) >= timeout_ms {
                    break -1;
                }
            };
            if c < 0 {
                break;
            }
            *b = c as u8;
            count += 1;
        }
        count
    }

    /// Writes bytes: take the lock, then per
    /// byte wait while status.txfifo_cnt is 127 and store it into the FIFO.
    pub fn write(&self, bytes: &[u8]) {
        let _lock = UART_LOCK.lock();
        for &b in bytes {
            while (uart_rd(UART_STATUS) >> 16) & 0xff == UART_TX_FIFO_FULL {}
            unsafe { ptr::write_volatile((UART0_BASE + UART_FIFO) as *mut u8, b) };
        }
    }
}

// ---------------------------------------------------------------- EEPROM (NVS)

#[derive(Default)]
pub struct NvsEeprom {
    handle: sys::nvs_handle_t,
}

fn cstr(s: &str, buf: &mut [u8; 16]) -> *const core::ffi::c_char {
    let n = s.len().min(15);
    buf[..n].copy_from_slice(&s.as_bytes()[..n]);
    buf[n] = 0;
    buf.as_ptr() as *const core::ffi::c_char
}

impl NvsBackend for NvsEeprom {
    fn open(&mut self, namespace: &str) -> i32 {
        let mut b = [0u8; 16];
        unsafe { sys::nvs_open(cstr(namespace, &mut b), sys::nvs_open_mode_t_NVS_READWRITE, &mut self.handle) }
    }

    fn get_blob(&mut self, key: &str, out: Option<&mut [u8]>, len: &mut usize) -> i32 {
        let mut b = [0u8; 16];
        let p = match out {
            Some(o) => o.as_mut_ptr() as *mut c_void,
            None => ptr::null_mut(),
        };
        unsafe { sys::nvs_get_blob(self.handle, cstr(key, &mut b), p, len) }
    }

    fn set_blob(&mut self, key: &str, data: &[u8]) -> i32 {
        let mut b = [0u8; 16];
        unsafe { sys::nvs_set_blob(self.handle, cstr(key, &mut b), data.as_ptr() as *const c_void, data.len()) }
    }

    fn erase_key(&mut self, key: &str) -> i32 {
        let mut b = [0u8; 16];
        unsafe { sys::nvs_erase_key(self.handle, cstr(key, &mut b)) }
    }

    fn commit(&mut self) -> i32 {
        unsafe { sys::nvs_commit(self.handle) }
    }

    fn close(&mut self) {
        unsafe { sys::nvs_close(self.handle) }
        self.handle = 0;
    }
}

// ---------------------------------------------------------------- CAN

static RX_QUEUE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static TX_SEM: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

struct Mmio;

impl Regs for Mmio {
    fn read(&mut self, offset: usize) -> u32 {
        unsafe { ptr::read_volatile((CAN_BASE + offset) as *const u32) }
    }
    fn write(&mut self, offset: usize, value: u32) {
        unsafe { ptr::write_volatile((CAN_BASE + offset) as *mut u32, value) }
    }
}

/// Creates the receive queue and initialises the CAN controller.
pub fn can_init() {
    unsafe {
        let q = sys::xQueueGenericCreate(CAN_RX_QUEUE_LEN, CAN_FRAME_SIZE, QUEUE_TYPE_BASE);
        RX_QUEUE.store(q as *mut c_void, Ordering::Release);

        // DPORT clock enable / reset release.
        let clk = esp_dport_access_reg_read(DPORT_PERIP_CLK_EN as u32);
        ptr::write_volatile(DPORT_PERIP_CLK_EN as *mut u32, clk | DPORT_CAN_BIT);
        let rst = esp_dport_access_reg_read(DPORT_PERIP_RST_EN as u32);
        ptr::write_volatile(DPORT_PERIP_RST_EN as *mut u32, rst & !DPORT_CAN_BIT);

        // TX pin: level 1, output, matrix signal 0x7b; RX pin: input, signal 0x5e.
        sys::gpio_set_level(CAN_TX_PIN, 1);
        sys::gpio_set_direction(CAN_TX_PIN, sys::gpio_mode_t_GPIO_MODE_OUTPUT);
        sys::esp_rom_gpio_connect_out_signal(CAN_TX_PIN as u32, CAN_TX_SIGNAL, false, false);
        sys::esp_rom_gpio_pad_select_gpio(CAN_TX_PIN as u32);
        sys::gpio_set_direction(CAN_RX_PIN, sys::gpio_mode_t_GPIO_MODE_INPUT);
        sys::esp_rom_gpio_connect_in_signal(CAN_RX_PIN as u32, CAN_RX_SIGNAL, false);
        sys::esp_rom_gpio_pad_select_gpio(CAN_RX_PIN as u32);

        let mut r = Mmio;
        can::init_controller(&mut r, CAN_SPEED, &DEFAULT_FILTER);

        sys::esp_intr_alloc(CAN_INTR_SOURCE, 0, Some(can_isr), ptr::null_mut(), ptr::null_mut());
        let sem = sys::xQueueGenericCreate(1, 0, QUEUE_TYPE_BINARY_SEMAPHORE);
        TX_SEM.store(sem as *mut c_void, Ordering::Release);

        can::start_controller(&mut r);
    }
}

/// The CAN interrupt handler.
#[unsafe(naked)]
unsafe extern "C" fn can_isr(_arg: *mut c_void) {
    core::arch::naked_asm!("entry a1,64","mov a10,a1","call8 {body}","retw.n",body=sym can_isr_body);
}

#[inline(never)]
unsafe extern "C" fn can_isr_body(native: *mut u8) {
    let woken = native.add(8).cast::<i32>();
    ptr::write_volatile(woken, 0);
    let q = RX_QUEUE.load(Ordering::Acquire);
    let mut r = Mmio;
    let guard = ptr::read_volatile(ptr::addr_of!(__stack_chk_guard)) as u32;
    // The frame on the stack supplies the stack bytes a short frame leaves unwritten.
    ptr::write_volatile(native.add(28).cast::<u32>(), guard);
    let frame = native.add(12);
    let mut stale = [0u8; 8];
    for (index, value) in stale.iter_mut().enumerate() {
        *value = native_stack_byte(frame.add(8 + index));
    }
    let mut storage = NativeCanStorage { frame };
    let o = can::isr_with_storage(&mut r, !q.is_null(), guard, stale, &mut storage);
    if o.rx.is_some() {
        sys::xQueueGenericSendFromISR(q as sys::QueueHandle_t, frame as *const c_void, woken, QUEUE_SEND_TO_BACK);
    }
    if o.release_rx {
        can::release_rx(&mut r);
    }
    if o.give_tx {
        sys::xQueueGiveFromISR(TX_SEM.load(Ordering::Acquire) as sys::QueueHandle_t, woken);
    }
    if native_stack_word(woken.cast()) != 0 {
        _frxt_setup_switch();
    }
    if native_stack_word(native.add(28)) != guard {
        // Stack-check failure: print the message, then abort.
        sys::esp_rom_printf(c"\r\nStack smashing protect failure!\r\n\r\n".as_ptr());
        sys::abort();
    }
}

/// Waits up to `ticks` for a frame from the receive queue.
pub fn can_receive(frame: &mut CanFrame, ticks: u32) -> bool {
    let q = RX_QUEUE.load(Ordering::Acquire);
    unsafe { sys::xQueueReceive(q as sys::QueueHandle_t, frame as *mut CanFrame as *mut c_void, ticks) == PD_TRUE }
}

/// Transmits a frame: returns -1 without a semaphore, else fills
/// the TX buffer, requests transmission and blocks until the ISR gives the semaphore.
pub fn can_write_frame(frame: &CanFrame) -> i32 {
    let sem = TX_SEM.load(Ordering::Acquire);
    if sem.is_null() {
        return -1;
    }
    let mut r = Mmio;
    can::write_frame(&mut r, frame);
    unsafe {
        sys::xQueueSemaphoreTake(sem as sys::QueueHandle_t, PORT_MAX_DELAY);
    }
    0
}

include!("../../../../esp32-common/native_stack.rs");
