//! The CmdMessenger link: receiving, parsing and sending of commands.
//!
//! Layout of the messenger object:
//!
//! | off | field | off | field |
//! |---|---|---|---|
//! | 0x000 | startCommand | 0x249 | messageState |
//! | 0x001 | lastCommandId | 0x24a | dumped |
//! | 0x002 | bufferIndex | 0x24b | ArgOk |
//! | 0x003 | bufferLength (0x40) | 0x24c | current (char *) |
//! | 0x004 | bufferLastIndex (0x3f) | 0x250 | last (char *) |
//! | 0x005 | ArglastChar | 0x254 | prevChar (never written, 0) |
//! | 0x006 | CmdlastChar | 0x258 | comms |
//! | 0x007 | pauseProcessing | 0x25c/d/e | command/field separator, escape |
//! | 0x008 | print_newlines | 0x260 | default_callback |
//! | 0x009 | commandBuffer[64] | 0x264 | callbackList[50] |
//! | 0x049 | streamBuffer[512] | | |
//!
//! The argument scanner (find next, split) walks raw memory until an
//! unescaped NUL or field separator, so an escaped NUL lets it run past the end of
//! the command buffer into the stream buffer (reachable after a
//! dropped 63-byte message whose last byte is an unpaired `'/'`, which leaves
//! the last command character as `'/'` for the next message). For that reason the bytes at
//! offsets 0x009..0x258 are one array (`mem`) and the message state, the dumped and
//! argument-ok flags and the `current` and `last` pointers live in it, the two pointers stored as
//! little-endian device addresses. How far the walk can get: only the command id field
//! starts with that escape mismatch (its number is 0, so only the who-are-you handler
//! runs, whose string argument is not unescaped), and a receive pass never
//! reads more than the 256 queued + 128 FIFO bytes that can be available, so
//! the stream buffer's bytes 384..512 stay 0 and end every walk. Fields after the stream buffer
//! are modelled anyway; a pointer outside the object reads as 0 here.

use crate::consts::{COMMAND_SEPARATOR, ESCAPE_CHARACTER, FIELD_SEPARATOR};

/// The length of the command buffer and its last index.
pub const BUFFER_LENGTH: usize = 0x40;
pub const BUFFER_LAST_INDEX: u8 = 0x3f;
/// The number of command ids that can have a callback.
pub const MAX_CALLBACKS: u8 = 0x32;
/// A receive pass reads at most this many bytes into the stream buffer.
pub const MAX_STREAM_BUFFER: usize = 0x200;

/// The device address of the messenger object.
pub const C_ADDR: u32 = 0x3ffc_19d0;
/// Object offsets of the fields kept in `mem`.
const OFF_COMMAND_BUFFER: u32 = 0x009;
const OFF_STREAM_BUFFER: u32 = 0x049;
const OFF_MESSAGE_STATE: u32 = 0x249;
const OFF_DUMPED: u32 = 0x24a;
const OFF_ARG_OK: u32 = 0x24b;
const OFF_CURRENT: u32 = 0x24c;
const OFF_LAST: u32 = 0x250;
/// End of the modelled bytes (the stream pointer starts here).
const OFF_MEM_END: u32 = 0x258;
pub const MEM_LEN: usize = (OFF_MEM_END - OFF_COMMAND_BUFFER) as usize;
/// The device address of the first byte of the command buffer.
pub const COMMAND_BUFFER_ADDR: u32 = C_ADDR + OFF_COMMAND_BUFFER;

const K_PROCESSING_MESSAGE: u8 = 0;
const K_END_OF_MESSAGE: u8 = 1;
const K_PROCESSING_ARGUMENTS: u8 = 2;

/// The output of the messenger: raw bytes.
pub trait Print {
    fn write_bytes(&mut self, bytes: &[u8]);
}

/// Which handler a completed message selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    Callback(u8),
    Default,
}

pub struct CmdMessenger {
    pub field_separator: u8,
    pub command_separator: u8,
    pub escape_character: u8,
    pub print_newlines: bool,
    buffer_index: u8,
    buffer_last_index: u8,
    arg_last_char: u8,
    cmd_last_char: u8,
    pub last_command_id: u8,
    pub start_command: bool,
    pub pause_processing: bool,
    /// Object bytes 0x009..0x258 (command buffer, stream buffer, message state,
    /// dumped flag, argument-ok flag, the `current` and `last` pointers, the previous character + padding).
    mem: [u8; MEM_LEN],
    callbacks: [bool; MAX_CALLBACKS as usize],
    default_callback: bool,
}

impl Default for CmdMessenger {
    fn default() -> Self {
        Self::new(FIELD_SEPARATOR, COMMAND_SEPARATOR, ESCAPE_CHARACTER)
    }
}

impl CmdMessenger {
    /// A new messenger
    /// (the object is all zero before it is initialised).
    pub fn new(fld: u8, cmd: u8, esc: u8) -> Self {
        let mut c = Self {
            field_separator: fld,
            command_separator: cmd,
            escape_character: esc,
            print_newlines: false,
            buffer_index: 0,
            buffer_last_index: BUFFER_LAST_INDEX,
            arg_last_char: 0,
            cmd_last_char: 0,
            last_command_id: 0,
            start_command: false,
            pause_processing: false,
            mem: [0; MEM_LEN],
            callbacks: [false; MAX_CALLBACKS as usize],
            default_callback: false,
        };
        c.set_field_u32(OFF_CURRENT, 0);
        c.set_field_u32(OFF_LAST, 0);
        c.set_field(OFF_DUMPED, 1);
        c.set_field(OFF_MESSAGE_STATE, K_PROCESSING_MESSAGE);
        c
    }

    /// Attaches a callback to a command id (ids >= 50 are ignored).
    pub fn attach(&mut self, msg_id: u8) {
        if msg_id < MAX_CALLBACKS {
            self.callbacks[msg_id as usize] = true;
        }
    }

    /// Attaches the default callback.
    pub fn attach_default(&mut self) {
        self.default_callback = true;
    }

    // ------------------------------------------------------------ object memory

    fn index_of(addr: u32) -> Option<usize> {
        let i = addr.wrapping_sub(COMMAND_BUFFER_ADDR) as usize;
        (i < MEM_LEN).then_some(i)
    }

    /// The byte at `addr`. Addresses outside the modelled object read as 0.
    fn byte(&self, addr: u32) -> u8 {
        Self::index_of(addr).map_or(0, |i| self.mem[i])
    }

    /// Stores `v` at `addr`. Writes outside the modelled object are dropped.
    fn set(&mut self, addr: u32, v: u8) {
        if let Some(i) = Self::index_of(addr) {
            self.mem[i] = v;
        }
    }

    fn field(&self, off: u32) -> u8 {
        self.byte(C_ADDR + off)
    }

    fn set_field(&mut self, off: u32, v: u8) {
        self.set(C_ADDR + off, v);
    }

    fn field_u32(&self, off: u32) -> u32 {
        let a = C_ADDR + off;
        u32::from_le_bytes([self.byte(a), self.byte(a + 1), self.byte(a + 2), self.byte(a + 3)])
    }

    fn set_field_u32(&mut self, off: u32, v: u32) {
        for (k, b) in v.to_le_bytes().into_iter().enumerate() {
            self.set(C_ADDR + off + k as u32, b);
        }
    }

    /// The argument-ok flag (0x24b).
    pub fn arg_ok(&self) -> bool {
        self.field(OFF_ARG_OK) != 0
    }

    /// `current` (0x24c) as a device address (0 = NULL).
    pub fn current_ptr(&self) -> u32 {
        self.field_u32(OFF_CURRENT)
    }

    /// `last` (0x250) as a device address (0 = NULL).
    pub fn last_ptr(&self) -> u32 {
        self.field_u32(OFF_LAST)
    }

    /// The stream buffer (0x049, 512 bytes), which a receive pass fills.
    pub fn stream_buffer_mut(&mut self) -> &mut [u8] {
        let s = (OFF_STREAM_BUFFER - OFF_COMMAND_BUFFER) as usize;
        &mut self.mem[s..s + MAX_STREAM_BUFFER]
    }

    /// Byte `i` of the stream buffer, read from memory on every iteration of the
    /// receive loop (callbacks may have rewritten it).
    pub fn stream_byte(&self, i: usize) -> u8 {
        self.byte(C_ADDR + OFF_STREAM_BUFFER + i as u32)
    }

    /// The command buffer (0x009, 64 bytes).
    pub fn command_buffer(&self) -> &[u8] {
        &self.mem[..BUFFER_LENGTH]
    }

    /// The modelled object bytes 0x009..0x258.
    pub fn object_bytes(&self) -> &[u8; MEM_LEN] {
        &self.mem
    }

    // ------------------------------------------------------------ receiving

    /// Whether the character is escaped by the one before it.
    fn is_escaped(&self, cur: u8, escape_char: u8, last_char: &mut u8) -> bool {
        let prev = *last_char;
        *last_char = cur;
        if self.escape_character == cur && prev == escape_char {
            *last_char = 0;
        }
        prev == escape_char
    }

    /// Resets the parser: buffer index 0, `current` and `last` NULL, the dumped flag set.
    fn reset(&mut self) {
        self.buffer_index = 0;
        self.set_field_u32(OFF_CURRENT, 0);
        self.set_field_u32(OFF_LAST, 0);
        self.set_field(OFF_DUMPED, 1);
    }

    /// Processes one received byte.
    /// Returns true when a complete message is waiting.
    pub fn process_byte(&mut self, serial_char: u8) -> bool {
        self.set_field(OFF_MESSAGE_STATE, K_PROCESSING_MESSAGE);
        let mut last = self.cmd_last_char;
        let escaped = self.is_escaped(serial_char, self.escape_character, &mut last);
        self.cmd_last_char = last;
        let idx = self.buffer_index;
        let at = COMMAND_BUFFER_ADDR + idx as u32;
        if serial_char == self.command_separator && !escaped {
            self.set(at, 0);
            if idx > 0 {
                self.set_field(OFF_MESSAGE_STATE, K_END_OF_MESSAGE);
                self.cmd_last_char = 0;
            }
            self.reset();
        } else {
            self.set(at, serial_char);
            self.buffer_index = idx.wrapping_add(1);
            if self.buffer_index >= self.buffer_last_index {
                self.reset();
            }
        }
        self.field(OFF_MESSAGE_STATE) == K_END_OF_MESSAGE
    }

    /// Handles a completed message: reads the command id and
    /// picks the handler. Call only after `process_byte` returned true.
    pub fn handle_message(&mut self) -> Option<Dispatch> {
        let id = self.read_int16_arg() as u8;
        self.last_command_id = id;
        if id < MAX_CALLBACKS && self.arg_ok() && self.callbacks[id as usize] {
            Some(Dispatch::Callback(id))
        } else if self.default_callback {
            Some(Dispatch::Default)
        } else {
            None
        }
    }

    /// The length of the field starting at `start`, up to the next unescaped field separator or NUL.
    fn find_next(&mut self, start: u32) -> u32 {
        self.arg_last_char = 0;
        let mut p = start;
        loop {
            let ch = self.byte(p);
            let mut l = self.arg_last_char;
            let escaped = self.is_escaped(ch, self.escape_character, &mut l);
            self.arg_last_char = l;
            if (ch == 0 && !escaped) || (ch == self.field_separator && !escaped) {
                break;
            }
            p = p.wrapping_add(1);
        }
        p.wrapping_sub(start)
    }

    /// Splits off the next field, continuing from `last` when `s` is 0 (pointer 0 = NULL).
    fn split_r(&mut self, s: u32) -> u32 {
        let mut s = if s == 0 { self.field_u32(OFF_LAST) } else { s };
        while self.find_next(s) == 0 && self.byte(s) != 0 {
            s = s.wrapping_add(1);
        }
        if self.byte(s) == 0 {
            return 0;
        }
        let n = self.find_next(s);
        let mut p = s.wrapping_add(n);
        if self.byte(p) != 0 {
            self.set(p, 0);
            p = p.wrapping_add(1);
        }
        self.set_field_u32(OFF_LAST, p);
        s
    }

    /// Advances `current` to the next argument; false when there is none.
    fn next(&mut self) -> bool {
        let mut temp = 0;
        match self.field(OFF_MESSAGE_STATE) {
            K_PROCESSING_MESSAGE => return false,
            K_END_OF_MESSAGE => {
                temp = COMMAND_BUFFER_ADDR;
                self.set_field(OFF_MESSAGE_STATE, K_PROCESSING_ARGUMENTS);
            }
            _ => {}
        }
        if self.field(OFF_DUMPED) != 0 {
            let cur = self.split_r(temp);
            self.set_field_u32(OFF_CURRENT, cur);
        }
        if self.field_u32(OFF_CURRENT) != 0 {
            self.set_field(OFF_DUMPED, 1);
            return true;
        }
        false
    }

    /// The C string at `addr` (up to its NUL, at most to the end of the modelled object).
    fn cstr_at(&self, addr: u32) -> &[u8] {
        let Some(start) = Self::index_of(addr) else { return &[] };
        let tail = &self.mem[start..];
        let n = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
        &tail[..n]
    }

    /// The next argument as a decimal integer truncated to 16 bits.
    pub fn read_int16_arg(&mut self) -> i16 {
        if self.next() {
            let cur = self.field_u32(OFF_CURRENT);
            self.set_field(OFF_DUMPED, 1);
            self.set_field(OFF_ARG_OK, 1);
            return atoi(self.cstr_at(cur)) as i16;
        }
        self.set_field(OFF_ARG_OK, 0);
        0
    }

    /// The next argument as the raw (not unescaped) C string, or NULL.
    pub fn read_string_arg(&mut self) -> Option<&[u8]> {
        if self.next() {
            self.set_field(OFF_DUMPED, 1);
            self.set_field(OFF_ARG_OK, 1);
            let cur = self.field_u32(OFF_CURRENT);
            return Some(self.cstr_at(cur));
        }
        self.set_field(OFF_ARG_OK, 0);
        None
    }

    /// Starts a command: prints the id in decimal.
    pub fn send_cmd_start<P: Print>(&mut self, out: &mut P, cmd_id: u8) {
        if !self.start_command {
            self.start_command = true;
            self.pause_processing = true;
            let mut buf = [0u8; 3];
            out.write_bytes(fmt_decimal_u8(cmd_id, &mut buf));
        }
    }

    /// Sends a text argument:
    /// separator + the text, unescaped.
    pub fn send_cmd_arg_str<P: Print>(&mut self, out: &mut P, arg: &[u8]) {
        if self.start_command {
            out.write_bytes(&[self.field_separator]);
            let n = arg.iter().position(|&b| b == 0).unwrap_or(arg.len());
            out.write_bytes(&arg[..n]);
        }
    }

    /// Sends a one-byte binary argument: separator + the escaped byte.
    pub fn send_cmd_bin_arg_u8<P: Print>(&mut self, out: &mut P, arg: u8) {
        if self.start_command {
            out.write_bytes(&[self.field_separator]);
            self.print_esc(out, arg);
        }
    }

    /// Prints a byte, preceded by the escape character when it is a separator, the escape character or NUL.
    fn print_esc<P: Print>(&self, out: &mut P, c: u8) {
        if c == self.field_separator || c == self.command_separator || c == self.escape_character || c == 0 {
            out.write_bytes(&[self.escape_character]);
        }
        out.write_bytes(&[c]);
    }

    /// Sends a whole command with one text argument, only when no command is being sent.
    pub fn send_cmd_str<P: Print>(&mut self, out: &mut P, cmd_id: u8, arg: &[u8]) -> bool {
        if self.start_command {
            return false;
        }
        self.send_cmd_start(out, cmd_id);
        self.send_cmd_arg_str(out, arg);
        self.send_cmd_end(out)
    }

    /// Ends a command. No send in the firmware
    /// requests an acknowledge, so there is no acknowledge wait.
    pub fn send_cmd_end<P: Print>(&mut self, out: &mut P) -> bool {
        if self.start_command {
            out.write_bytes(&[self.command_separator]);
            if self.print_newlines {
                out.write_bytes(b"\r\n");
            }
        }
        self.pause_processing = false;
        self.start_command = false;
        false
    }
}

fn fmt_decimal_u8(v: u8, buf: &mut [u8; 3]) -> &[u8] {
    let mut n = 0;
    if v >= 100 {
        buf[n] = b'0' + v / 100;
        n += 1;
    }
    if v >= 10 {
        buf[n] = b'0' + (v / 10) % 10;
        n += 1;
    }
    buf[n] = b'0' + v % 10;
    n += 1;
    &buf[..n]
}

/// The decimal integer at the start of `s` (saturating on overflow).
pub fn atoi(s: &[u8]) -> i32 {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r') {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let mut acc: i64 = 0;
    let mut overflow = false;
    while i < s.len() && s[i].is_ascii_digit() {
        acc = acc * 10 + (s[i] - b'0') as i64;
        if acc > i32::MAX as i64 + 1 {
            overflow = true;
            acc = i32::MAX as i64 + 1;
        }
        i += 1;
    }
    let v = if neg { -acc } else { acc };
    if overflow || v > i32::MAX as i64 || v < i32::MIN as i64 {
        return if neg { i32::MIN } else { i32::MAX };
    }
    v as i32
}
