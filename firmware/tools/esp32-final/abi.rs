#![no_std]
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]
include!(env!("ABI_BINDINGS"));
use core::mem::{offset_of, size_of as s};
#[no_mangle]
#[link_section = ".rodata.orc_abi"]
pub static ORC_ABI: [u32; 33] = [
    s::<*mut u8>() as u32,
    s::<portMUX_TYPE>() as u32,
    offset_of!(portMUX_TYPE, owner) as u32,
    offset_of!(portMUX_TYPE, count) as u32,
    s::<QueueHandle_t>() as u32,
    s::<TickType_t>() as u32,
    s::<esp_bt_controller_config_t>() as u32,
    offset_of!(esp_bt_controller_config_t, controller_task_stack_size) as u32,
    offset_of!(esp_bt_controller_config_t, hci_uart_baudrate) as u32,
    offset_of!(esp_bt_controller_config_t, normal_adv_size) as u32,
    offset_of!(esp_bt_controller_config_t, controller_debug_flag) as u32,
    offset_of!(esp_bt_controller_config_t, mode) as u32,
    offset_of!(esp_bt_controller_config_t, auto_latency) as u32,
    offset_of!(esp_bt_controller_config_t, magic) as u32,
    s::<ble_gatt_svc_def>() as u32,
    offset_of!(ble_gatt_svc_def, uuid) as u32,
    offset_of!(ble_gatt_svc_def, characteristics) as u32,
    s::<ble_gatt_chr_def>() as u32,
    offset_of!(ble_gatt_chr_def, access_cb) as u32,
    offset_of!(ble_gatt_chr_def, flags) as u32,
    offset_of!(ble_gatt_chr_def, val_handle) as u32,
    s::<ble_gap_adv_params>() as u32,
    offset_of!(ble_gap_adv_params, itvl_min) as u32,
    offset_of!(ble_gap_adv_params, filter_policy) as u32,
    s::<ble_hs_adv_fields>() as u32,
    offset_of!(ble_hs_adv_fields, name) as u32,
    offset_of!(ble_hs_adv_fields, appearance) as u32,
    offset_of!(ble_hs_adv_fields, mfg_data) as u32,
    s::<os_mbuf>() as u32,
    offset_of!(os_mbuf, om_data) as u32,
    s::<ble_uuid128_t>() as u32,
    s::<ble_gatt_access_ctxt>() as u32,
    offset_of!(ble_gatt_access_ctxt, om) as u32,
];
