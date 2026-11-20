#![no_std]
#![feature(alloc_error_handler)]
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]
mod raw {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}
pub use raw::*;
pub mod runtime;

pub use ets_printf as esp_rom_printf;
pub use gpio_matrix_in as esp_rom_gpio_connect_in_signal;
pub use gpio_matrix_out as esp_rom_gpio_connect_out_signal;
pub type nvs_handle_t = nvs_handle;
pub const nvs_open_mode_t_NVS_READWRITE: nvs_open_mode = nvs_open_mode_NVS_READWRITE;
pub const periph_interrupt_t_ETS_UART0_INTR_SOURCE: u32 = ETS_UART0_INTR_SOURCE;
pub const configTICK_RATE_HZ: u32 = 1000;

#[inline(always)]
pub unsafe fn gpio_set_level(pin: i32, value: u32) -> i32 {
    raw::gpio_set_level(pin as _, value)
}
#[inline(always)]
pub unsafe fn gpio_set_direction(pin: i32, mode: gpio_mode_t) -> i32 {
    raw::gpio_set_direction(pin as _, mode)
}
#[inline(always)]
pub unsafe fn esp_rom_gpio_pad_select_gpio(pin: u32) {
    raw::gpio_pad_select_gpio(pin as _)
}
#[inline(always)]
pub unsafe fn esp_partition_erase_range(partition: *const esp_partition_t, start: usize, size: usize) -> i32 {
    raw::esp_partition_erase_range(partition, start as _, size as _)
}

#[inline(always)]
pub unsafe fn xQueueReceive(queue: QueueHandle_t, buffer: *mut core::ffi::c_void, ticks: u32) -> i32 {
    xQueueGenericReceive(queue, buffer, ticks, 0)
}
#[inline(always)]
pub unsafe fn xQueueSemaphoreTake(queue: QueueHandle_t, ticks: u32) -> i32 {
    xQueueGenericReceive(queue, core::ptr::null_mut(), ticks, 0)
}

pub fn link_patches() {}
