/* Compile with the pinned GCC5 and the pinned IDF3/NimBLE headers. */
#include <stddef.h>
#include "../../esp-idf33-sys/wrapper.h"
#define S(t) sizeof(t)
#define O(t,m) offsetof(t,m)
const unsigned __attribute__((section(".rodata.orc_abi"),used)) orc_abi[]={
 S(void*),S(portMUX_TYPE),O(portMUX_TYPE,owner),O(portMUX_TYPE,count),S(QueueHandle_t),S(TickType_t),
 S(esp_bt_controller_config_t),O(esp_bt_controller_config_t,controller_task_stack_size),O(esp_bt_controller_config_t,hci_uart_baudrate),O(esp_bt_controller_config_t,normal_adv_size),O(esp_bt_controller_config_t,controller_debug_flag),O(esp_bt_controller_config_t,mode),O(esp_bt_controller_config_t,auto_latency),O(esp_bt_controller_config_t,magic),
 S(struct ble_gatt_svc_def),O(struct ble_gatt_svc_def,uuid),O(struct ble_gatt_svc_def,characteristics),
 S(struct ble_gatt_chr_def),O(struct ble_gatt_chr_def,access_cb),O(struct ble_gatt_chr_def,flags),O(struct ble_gatt_chr_def,val_handle),
 S(struct ble_gap_adv_params),O(struct ble_gap_adv_params,itvl_min),O(struct ble_gap_adv_params,filter_policy),
 S(struct ble_hs_adv_fields),O(struct ble_hs_adv_fields,name),O(struct ble_hs_adv_fields,appearance),O(struct ble_hs_adv_fields,mfg_data),
 S(struct os_mbuf),O(struct os_mbuf,om_data),S(ble_uuid128_t),S(struct ble_gatt_access_ctxt),O(struct ble_gatt_access_ctxt,om)
};
