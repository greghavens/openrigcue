#!/usr/bin/env node
// Finalize the Arduino flash layout using only Node/Rust tooling.
'use strict';
const fs=require('fs'),crypto=require('crypto');
const [bootFile,appFile,partitionFile,otaFile,mergedFile]=process.argv.slice(2);
if(!mergedFile)throw Error('Usage: esp32-image.js BOOT APP PARTITIONS OTA MERGED');
function image(file){
 const b=fs.readFileSync(file);
 if(b.length<24||b[0]!==0xe9||![0,1].includes(b[23]))throw Error('Invalid ESP32 image '+file);
 const end=b[23]===1?b.length-32:b.length;
 if(b[23]===1&&!crypto.createHash('sha256').update(b.subarray(0,end)).digest().equals(b.subarray(end)))throw Error('Invalid image SHA256 '+file);
 // IDF 3.3 extended headers have no maximum chip-revision restriction.
 b[17]=0;b[18]=0;
 if(b[23]===1)crypto.createHash('sha256').update(b.subarray(0,end)).digest().copy(b,end);
 fs.writeFileSync(file,b);return b;
}
const boot=image(bootFile),app=image(appFile),partitions=fs.readFileSync(partitionFile),ota=fs.readFileSync(otaFile);
if(boot.length>0x7000||partitions.length>0x1000||ota.length!==0x2000||app.length>0x140000)throw Error('Image exceeds original flash layout');
const merged=Buffer.alloc(0x10000+app.length,255);
boot.copy(merged,0x1000);partitions.copy(merged,0x8000);ota.copy(merged,0xe000);app.copy(merged,0x10000);
fs.writeFileSync(mergedFile,merged);
