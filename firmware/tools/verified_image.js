#!/usr/bin/env node
// Publication guard shared with the approved product firmware manifest. Builds
// never copy an imported image or accept a newly changed candidate
// just because its compiler/linker completed successfully.
'use strict';
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const root = path.resolve(__dirname, '../..');
const [mode, device, candidate] = process.argv.slice(2);
function fail(message) {
    console.error(message);
    process.exit(1);
}
if (!['approved', 'check'].includes(mode) || !device || (mode === 'check' && !candidate)) {
    fail('Usage: verified_image.js approved DEVICE | check DEVICE CANDIDATE_HEX');
}
const rows = fs.readFileSync(path.join(root, 'packaging/firmware/images.tsv'), 'utf8')
    .split(/\r?\n/).filter(line => line && !line.startsWith('#'))
    .map(line => line.split('\t')).filter(row => row[0] === device);
if (rows.length !== 1 || rows[0].length !== 3 || !/^[a-f0-9]{64}$/.test(rows[0][2])) {
    fail(`${device}: no unique approved whole-stream firmware hash; no existing output changed`);
}
if (mode === 'check') {
    const actual = crypto.createHash('sha256').update(fs.readFileSync(candidate)).digest('hex');
    if (actual !== rows[0][2]) {
        fail(`${device}: fresh Rust HEX ${actual} differs from verified ${rows[0][2]}; no existing output changed`);
    }
    console.log(`${device}: freshly compiled Rust HEX matches verified whole-stream image ${actual}`);
}
