#!/usr/bin/env node
'use strict';
const fs=require('fs'),cp=require('child_process'),path=require('path');
const [runtime,belt,obd]=process.argv.slice(2);if(!obd)throw Error('check-target.js RUNTIME BELT_ELF OBD_ELF');
const prefix=path.join(runtime,'xtensa-esp32-elf/bin/xtensa-esp32-elf-');
const results={};for(const [name,elf]of [['belt',belt],['obd2hud',obd]]){
 const symbols=cp.execFileSync(prefix+'nm',['-n',elf],{encoding:'utf8'}).split('\n').flatMap(line=>{let m=line.match(/^([a-f0-9]+)\s+\w\s+(\S+)$/);return m?[{name:m[2],addr:parseInt(m[1],16)}]:[];});
 const entries={};for(const [suffix,frame,iram]of [['8uart_isr',48,true],['7can_isr',64,false]]){let s=symbols.filter(x=>x.name.endsWith(suffix));if(s.length!==1)throw Error('Native ISR boundary missing '+suffix);let asm=cp.execFileSync(prefix+'objdump',['-d','--start-address='+s[0].addr,'--stop-address='+(s[0].addr+3),elf],{encoding:'utf8'});if(!asm.includes('entry\ta1, '+frame)&&!new RegExp('entry\\s+a1,\\s*'+frame).test(asm))throw Error('Actual ISR owned frame differs '+asm);if(iram&&!(s[0].addr>=0x40080000&&s[0].addr<0x400c0000))throw Error('UART wrapper escaped IRAM');entries[suffix]={address:s[0].addr,frame};}
 const names=new Set(symbols.map(x=>x.name));for(const n of ['adc2_get_raw','adc2_config_channel_atten','adc_set_clk_div','adc1_config_width','esp_timer_impl_update_apb_freq'])if(!names.has(n))throw Error('Original runtime API missing '+n);
 for(const n of ['adc_oneshot_new_unit','adc_oneshot_read','esp_nimble_init'])if(names.has(n))throw Error('Modern IDF API unexpectedly linked '+n);
 for(const [n,addr]of [['__truncdfsf2',0x40002b90],['__divsf3',0x4000234c]])if(symbols.find(x=>x.name===n)?.addr!==addr)throw Error('Original ROM numerical boundary differs '+n);
 if(name==='obd2hud')for(const n of ['nimble_port_init','esp_nimble_hci_init','ble_svc_gatt_init','atof'])if(!names.has(n))throw Error('Original NimBLE/newlib API absent '+n);
 const bytes=fs.readFileSync(elf);if(!bytes.includes(Buffer.from('v3.3.5-1-g85c43024c')))throw Error('Original IDF release absent');results[name]={entries,originalRuntimeVersion:true,originalAdcApis:true,originalRomNumerics:true};
}console.log(JSON.stringify(results,null,2));
