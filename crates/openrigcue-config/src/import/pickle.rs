//! A pickle reader restricted to plain data.
//!
//! Every `profile.db`/`metadata.db` value is a Python pickle. This reader admits only the globals
//! `builtins.{dict,list,str,int,float,bool,set,tuple}` and `datetime.datetime`,
//! and can only *construct* those types: it has no way to call arbitrary code, so a
//! hostile file can at worst be rejected.

use super::value::{
    BigInt, DateTimeParts, Dict, Key, LegacyValue, NativeBufferView, NativeGlobal, NativeObject, PythonString,
};
use num_traits::ToPrimitive;
use openrigcue_model::python_compare::native_equal;

mod codecs;
mod newobj;
mod text_number;
mod text_string;
mod write;
pub use codecs::{backslashreplace, utf8_strict, NativeException, UnicodeErrorAttributes, UnicodeErrorObject};
pub use write::{encode, encode_checked};

#[cfg(test)]
mod datetime_tests;
#[cfg(test)]
mod frame_tests;
#[cfg(test)]
mod memo_tests;
#[cfg(test)]
mod native_tests;
#[cfg(test)]
mod stack_tests;
#[cfg(test)]
mod text_number_tests;

/// Why a pickle was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PickleError {
    #[error("pickle data ended unexpectedly")]
    Exhausted,
    #[error("pickle data ended unexpectedly")]
    Truncated,
    #[error("FRAME length exceeds system's maximum of 9223372036854775807 bytes")]
    FrameTooLarge,
    #[error("read would overflow (invalid bytecode)")]
    ReadOverflow,
    #[error("unsupported pickle protocol: {0}")]
    UnsupportedProtocol(u8),
    #[error("unsupported pickle opcode 0x{0:02x} at byte {1}")]
    UnsupportedOpcode(u8, usize),
    #[error("forbidden global {0}.{1}")]
    ForbiddenGlobal(String, String),
    #[error("malformed pickle: {0}")]
    Malformed(String),
    #[error("{class}: {message}")]
    Constructor { class: &'static str, message: String },
    #[error("{0}")]
    Native(NativeException),
}

impl PickleError {
    /// Classified decode errors. Unsupported value/opcode kinds remain
    /// unclassified rather than being mislabeled as malformed data.
    pub fn native_exception(&self) -> Option<(&'static str, String)> {
        match self {
            Self::Exhausted => Some(("EOFError", "Ran out of input".into())),
            Self::Truncated => Some(("UnpicklingError", "pickle data was truncated".into())),
            Self::FrameTooLarge => Some(("OverflowError", self.to_string())),
            Self::ReadOverflow => Some(("UnpicklingError", self.to_string())),
            Self::UnsupportedProtocol(version) => {
                Some(("ValueError", format!("unsupported pickle protocol: {version}")))
            }
            Self::ForbiddenGlobal(module, name) => {
                Some(("UnpicklingError", format!("forbidden global: {module}.{name}")))
            }
            Self::UnsupportedOpcode(byte, _) => {
                // The pickle opcode set. A valid but unimplemented
                // opcode is a limit of this reader, not an invalid byte.
                const VALID: &[u8] = &[
                    40, 41, 46, 48, 49, 50, 66, 67, 70, 71, 73, 74, 75, 76, 77, 78, 80, 81, 82, 83, 84, 85, 86, 88, 93,
                    97, 98, 99, 100, 101, 103, 104, 105, 106, 108, 111, 112, 113, 114, 115, 116, 117, 125, 128, 129,
                    130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149,
                    150, 151, 152,
                ];
                if VALID.contains(byte) {
                    return None;
                }
                let shown = if (32..127).contains(byte) && !matches!(byte, b'\'' | b'\\') {
                    char::from(*byte).to_string()
                } else {
                    format!("\\x{byte:02x}")
                };
                Some(("UnpicklingError", format!("invalid load key, '{shown}'.")))
            }
            Self::Constructor { class, message } => Some((class, message.clone())),
            Self::Native(exception) => exception.message().scalar_string().map(|message| (exception.class(), message)),
            Self::Malformed(_) => None,
        }
    }

    /// The error value before an output encoding boundary. This retains
    /// lone surrogates, Unicode error attributes, notes and error context.
    pub fn native(&self) -> Option<NativeException> {
        match self {
            Self::Native(exception) => Some(exception.clone()),
            _ => self
                .native_exception()
                .map(|(class, message)| NativeException::new(class, PythonString::from(message.as_str()))),
        }
    }
}

/// A callable the restricted unpickler may reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllowedGlobal {
    Dict,
    List,
    Str,
    Int,
    Float,
    Bool,
    Set,
    Tuple,
    DateTime,
}

impl AllowedGlobal {
    fn resolve_native(module: &PythonString, name: &PythonString) -> Result<Self, PickleError> {
        if let (Some(module), Some(name)) = (module.scalar_string(), name.scalar_string()) {
            return Self::resolve(&module, &name);
        }
        let mut message = PythonString::from("forbidden global: ").codepoints().to_vec();
        message.extend_from_slice(module.codepoints());
        message.push(u32::from('.'));
        message.extend_from_slice(name.codepoints());
        Err(PickleError::Native(NativeException::new(
            "UnpicklingError",
            PythonString::from_codepoints(message).expect("code points"),
        )))
    }
    fn native(self) -> NativeGlobal {
        let name = match self {
            Self::Dict => "dict",
            Self::List => "list",
            Self::Str => "str",
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
            Self::Set => "set",
            Self::Tuple => "tuple",
            Self::DateTime => "datetime",
        };
        NativeGlobal { module: if self == Self::DateTime { "datetime" } else { "builtins" }.into(), name: name.into() }
    }
    /// The allowed globals, by module and name.
    fn resolve(module: &str, name: &str) -> Result<Self, PickleError> {
        let builtin = module == "builtins";
        let found = match (builtin, module, name) {
            (true, _, "dict") => Some(AllowedGlobal::Dict),
            (true, _, "list") => Some(AllowedGlobal::List),
            (true, _, "str") => Some(AllowedGlobal::Str),
            (true, _, "int") => Some(AllowedGlobal::Int),
            (true, _, "float") => Some(AllowedGlobal::Float),
            (true, _, "bool") => Some(AllowedGlobal::Bool),
            (true, _, "set") => Some(AllowedGlobal::Set),
            (true, _, "tuple") => Some(AllowedGlobal::Tuple),
            (false, "datetime", "datetime") => Some(AllowedGlobal::DateTime),
            _ => None,
        };
        found.ok_or_else(|| PickleError::ForbiddenGlobal(module.to_owned(), name.to_owned()))
    }
}

#[derive(Debug, Clone)]
enum Item {
    Value(LegacyValue),
    Global(AllowedGlobal),
    Mark,
}

#[derive(Default)]
struct Memo {
    entries: Vec<Option<Item>>,
    used: usize,
}

struct Reader<'a> {
    data: &'a [u8],
    /// The input is an in-memory byte stream without look-ahead. A FRAME fills
    /// this window and advances the file cursor before its opcodes execute.
    window_start: usize,
    window_end: usize,
    pos: usize,
    file_pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PickleError> {
        if n <= self.window_end - self.pos {
            let end = self.pos + n;
            let slice = &self.data[self.pos..end];
            self.pos = end;
            return Ok(slice);
        }
        // The buffered index is checked before the window is replaced. A crossing fixed-width read discards its buffered remainder.
        if n > i64::MAX as usize || self.pos - self.window_start > i64::MAX as usize - n {
            return Err(PickleError::ReadOverflow);
        }
        let start = self.file_pos;
        let end = start.checked_add(n).ok_or(PickleError::Truncated)?;
        let slice = self.data.get(start..end).ok_or(PickleError::Truncated)?;
        self.window_start = start;
        self.window_end = end;
        self.file_pos = end;
        self.pos = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, PickleError> {
        Ok(self.take(1)?[0])
    }

    fn uint(&mut self, n: usize) -> Result<u64, PickleError> {
        let bytes = self.take(n)?;
        Ok(bytes.iter().rev().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
    }

    fn line(&mut self) -> Result<&'a str, PickleError> {
        std::str::from_utf8(self.line_bytes()?).map_err(|_| PickleError::Malformed("non-UTF-8 text line".into()))
    }

    fn line_bytes(&mut self) -> Result<&'a [u8], PickleError> {
        let buffered = &self.data[self.pos..self.window_end];
        let line = if let Some(end) = buffered.iter().position(|b| *b == b'\n') {
            self.pos += end + 1;
            &buffered[..end]
        } else {
            // A line read, like a crossing fixed-width read, replaces rather
            // than joins the window when no newline is buffered.
            let start = self.file_pos;
            let rest = &self.data[start..];
            let end = rest.iter().position(|b| *b == b'\n').ok_or(PickleError::Truncated)?;
            self.window_start = start;
            self.window_end = start + end + 1;
            self.file_pos = self.window_end;
            self.pos = self.window_end;
            &rest[..end]
        };
        Ok(line)
    }

    fn sized(&mut self, len: u64) -> Result<&'a [u8], PickleError> {
        let len = usize::try_from(len).map_err(|_| PickleError::Truncated)?;
        self.take(len)
    }

    fn bytes(&mut self, len: u64) -> Result<Vec<u8>, PickleError> {
        let len = usize::try_from(len).map_err(|_| PickleError::Truncated)?;
        let buffered = (self.window_end - self.pos).min(len);
        let missing = len - buffered;
        let end = self.file_pos.checked_add(missing).ok_or(PickleError::Truncated)?;
        let suffix = self.data.get(self.file_pos..end).ok_or(PickleError::Truncated)?;
        // BINBYTES reads into its own buffer: consume buffered bytes first,
        // then read from the stream without replacing the existing window.
        let mut value = Vec::with_capacity(len);
        value.extend_from_slice(&self.data[self.pos..self.pos + buffered]);
        value.extend_from_slice(suffix);
        self.pos += buffered;
        self.file_pos = end;
        Ok(value)
    }

    fn frame(&mut self) -> Result<(), PickleError> {
        let size = self.uint(8)?;
        if size > i64::MAX as u64 {
            return Err(PickleError::FrameTooLarge);
        }
        let size = size as usize;
        self.take(size)?;
        self.pos -= size;
        Ok(())
    }
}

/// Decodes one pickle (protocols 0–5, data opcodes only).
pub fn decode(data: &[u8]) -> Result<LegacyValue, PickleError> {
    let _operation = openrigcue_model::native_object::native_operation();
    let mut r = Reader { data, window_start: 0, window_end: 0, pos: 0, file_pos: 0 };
    let mut stack: Vec<Item> = Vec::new();
    let mut memo = Memo::default();
    loop {
        let at = r.pos;
        let op = r.byte().map_err(|_| PickleError::Exhausted)?;
        match op {
            0x80 => {
                let version = r.byte()?;
                if version > 5 {
                    return Err(PickleError::UnsupportedProtocol(version));
                }
            }
            0x95 => r.frame()?,
            b'.' => {
                let value = pop_value(&mut stack)?;
                drop(stack);
                drop(memo);
                return Ok(NativeObject::materialize(value));
            }
            b'(' => stack.push(Item::Mark),
            b'}' => stack.push(Item::Value(LegacyValue::Dict(Dict::new()))),
            b']' => stack.push(Item::Value(LegacyValue::List(Vec::new()))),
            b')' => stack.push(Item::Value(LegacyValue::Tuple(Vec::new()))),
            0x8f => stack.push(Item::Value(LegacyValue::Set(Vec::new()))),
            b'N' => stack.push(Item::Value(LegacyValue::None)),
            0x88 => stack.push(Item::Value(LegacyValue::Bool(true))),
            0x89 => stack.push(Item::Value(LegacyValue::Bool(false))),
            b'J' => {
                let v = r.uint(4)? as u32 as i32;
                stack.push(Item::Value(LegacyValue::Int(i64::from(v))));
            }
            b'K' => stack.push(Item::Value(LegacyValue::Int(i64::from(r.byte()?)))),
            b'M' => stack.push(Item::Value(LegacyValue::Int(r.uint(2)? as i64))),
            0x8a => {
                let n = r.byte()? as u64;
                let bytes = r.sized(n)?;
                stack.push(Item::Value(decode_long(bytes)?));
            }
            0x8b => {
                let n = r.uint(4)?;
                if n > i32::MAX as u64 {
                    return Err(PickleError::Constructor {
                        class: "UnpicklingError",
                        message: "LONG pickle has negative byte count".into(),
                    });
                }
                let bytes = r.sized(n)?;
                stack.push(Item::Value(decode_long(bytes)?));
            }
            b'I' => {
                stack.push(Item::Value(text_number::integer(r.line()?, false)?));
            }
            b'L' => {
                stack.push(Item::Value(text_number::integer(r.line()?, true)?));
            }
            b'G' => {
                let bytes: [u8; 8] = r.take(8)?.try_into().expect("took 8 bytes");
                let value = f64::from_be_bytes(bytes);
                stack.push(Item::Value(LegacyValue::from_float(value)));
            }
            b'F' => {
                let value = text_number::float(r.line()?)?;
                stack.push(Item::Value(match value {
                    LegacyValue::Float(value) => LegacyValue::from_float(value),
                    value => value,
                }));
            }
            b'X' => {
                let n = r.uint(4)?;
                stack.push(Item::Value(unicode_value(r.sized(n)?)?));
            }
            0x8c => {
                let n = r.byte()? as u64;
                stack.push(Item::Value(unicode_value(r.sized(n)?)?));
            }
            0x8d => {
                let n = r.uint(8)?;
                check_binary_size(n, "BINUNICODE")?;
                stack.push(Item::Value(unicode_value(r.sized(n)?)?));
            }
            b'V' => {
                stack.push(Item::Value(text_string::raw_unicode_value(r.line_bytes()?)?));
            }
            b'S' => {
                stack.push(Item::Value(LegacyValue::Str(text_string::quoted_ascii(r.line_bytes()?)?)));
            }
            b'T' => {
                let n = r.uint(4)?;
                stack.push(Item::Value(LegacyValue::Str(ascii(r.sized(n)?)?)));
            }
            b'U' => {
                let n = r.byte()? as u64;
                stack.push(Item::Value(LegacyValue::Str(ascii(r.sized(n)?)?)));
            }
            b'C' => {
                let n = r.byte()? as u64;
                stack.push(Item::Value(LegacyValue::Bytes(r.bytes(n)?)));
            }
            b'B' => {
                let n = r.uint(4)?;
                stack.push(Item::Value(LegacyValue::Bytes(r.bytes(n)?)));
            }
            0x8e => {
                let n = r.uint(8)?;
                check_binary_size(n, "BINBYTES")?;
                stack.push(Item::Value(LegacyValue::Bytes(r.bytes(n)?)));
            }
            0x96 => {
                let n = r.uint(8)?;
                check_binary_size(n, "BYTEARRAY8")?;
                stack.push(Item::Value(LegacyValue::ByteArray(r.bytes(n)?)));
            }
            0x82..=0x84 => {
                let code = match op {
                    0x82 => i32::from(r.byte()?),
                    0x83 => r.uint(2)? as i32,
                    _ => r.uint(4)? as i32,
                };
                return Err(PickleError::Constructor {
                    class: if code <= 0 { "UnpicklingError" } else { "ValueError" },
                    message: if code <= 0 {
                        "EXT specifies code <= 0".into()
                    } else {
                        format!("unregistered extension code {code}")
                    },
                });
            }
            b'P' => {
                if !r.line_bytes()?.is_ascii() {
                    return Err(PickleError::Constructor {
                        class: "UnpicklingError",
                        message: "persistent IDs in protocol 0 must be ASCII strings".into(),
                    });
                }
                return Err(missing_persistent_loader());
            }
            b'Q' => {
                top_item(&stack)?;
                return Err(missing_persistent_loader());
            }
            0x97 => {
                return Err(PickleError::Constructor {
                    class: "UnpicklingError",
                    message: "pickle stream refers to out-of-band data but no *buffers* argument was given".into(),
                });
            }
            0x98 => {
                top_item(&stack)?;
                if let Some(Item::Value(value)) = stack.last_mut() {
                    let resolved = value.resolved();
                    if matches!(resolved, LegacyValue::ByteArray(_)) {
                        *value = LegacyValue::MemoryView(NativeBufferView::new(
                            std::mem::replace(value, LegacyValue::None),
                            true,
                        ));
                        continue;
                    }
                    if let LegacyValue::MemoryView(view) = resolved {
                        if !view.readonly {
                            *value = LegacyValue::MemoryView(NativeBufferView::new(view.buffer.clone(), true));
                        }
                        continue;
                    }
                }
                // Read-only bytes keep their identity;
                // other represented values have no buffer interface.
                let kind = match top_item(&stack)? {
                    Item::Value(value) if matches!(value.resolved(), LegacyValue::Bytes(_)) => None,
                    Item::Value(value) => Some(value.type_name()),
                    Item::Global(_) => Some("type"),
                    Item::Mark => unreachable!("top_item rejects MARK"),
                };
                if let Some(kind) = kind {
                    return Err(PickleError::Constructor {
                        class: "TypeError",
                        message: format!("memoryview: a bytes-like object is required, not '{kind}'"),
                    });
                }
            }
            0x94 => {
                let index = memo.used;
                memo_put(&mut memo, index, &mut stack)?;
            }
            b'q' => {
                let idx = r.byte()? as usize;
                memo_put(&mut memo, idx, &mut stack)?;
            }
            b'r' => {
                let idx = r.uint(4)? as usize;
                memo_put(&mut memo, idx, &mut stack)?;
            }
            b'p' => {
                let line = r.line()?;
                if line.is_empty() {
                    return Err(PickleError::Truncated);
                }
                top_item(&stack)?;
                let idx = text_number::memo_index(line)?;
                if idx < 0 {
                    return Err(PickleError::Constructor {
                        class: "ValueError",
                        message: "negative PUT argument".into(),
                    });
                }
                memo_put(&mut memo, idx as usize, &mut stack)?;
            }
            b'h' => {
                let idx = r.byte()? as usize;
                stack.push(memo_get(&memo, idx)?);
            }
            b'j' => {
                let idx = r.uint(4)? as usize;
                stack.push(memo_get(&memo, idx)?);
            }
            b'g' => {
                let idx = text_number::memo_index(r.line()?)?;
                if idx < 0 {
                    return Err(PickleError::Constructor {
                        class: "UnpicklingError",
                        message: missing_memo_message(idx),
                    });
                }
                stack.push(memo_get(&memo, idx as usize)?);
            }
            b'c' => {
                let module = global_line(r.line_bytes()?)?;
                let name = global_line(r.line_bytes()?)?;
                stack.push(Item::Global(AllowedGlobal::resolve(&module, &name)?));
            }
            b'i' => {
                // INST consumes the MARK before reading its ASCII names,
                // unlike GLOBAL's UTF-8 names. Resolution still uses the
                // same allowlist.
                let args = take_mark_items(&mut stack)?;
                let module = instance_line(r.line_bytes()?)?;
                let name = instance_line(r.line_bytes()?)?;
                let callable = AllowedGlobal::resolve(&module, &name)?;
                stack.push(Item::Value(construct(callable, LegacyValue::Tuple(mark_values(args)?))?));
            }
            b'o' => {
                let items = take_mark_items(&mut stack)?;
                let mut items = items.into_iter();
                let callable = match items.next() {
                    Some(Item::Global(callable)) => callable,
                    Some(Item::Value(value)) => {
                        return Err(PickleError::Constructor {
                            class: "TypeError",
                            message: format!("'{}' object is not callable", value.type_name()),
                        });
                    }
                    None => return Err(stack_underflow(&stack)),
                    Some(Item::Mark) => unreachable!("take_mark_items consumes the last MARK"),
                };
                let args = mark_values(items.collect())?;
                stack.push(Item::Value(construct(callable, LegacyValue::Tuple(args))?));
            }
            0x81 | 0x92 => {
                let kwargs = if op == 0x92 { Some(pop_value(&mut stack)?.resolved()) } else { None };
                let args = pop_value(&mut stack)?.resolved();
                let class = top_item(&stack)?.clone();
                stack.pop();
                let opcode = if op == 0x92 { "NEWOBJ_EX" } else { "NEWOBJ" };
                let callable = match class {
                    Item::Global(callable) => callable,
                    Item::Value(value) => {
                        return Err(PickleError::Constructor {
                            class: "UnpicklingError",
                            message: format!("{opcode} class argument must be a type, not {}", value.type_name()),
                        });
                    }
                    Item::Mark => unreachable!("top_item rejects MARK"),
                };
                if !matches!(args, LegacyValue::Tuple(_)) {
                    return Err(PickleError::Constructor {
                        class: "UnpicklingError",
                        message: format!("{opcode} args argument must be a tuple, not {}", args.type_name()),
                    });
                }
                if let Some(kwargs) = &kwargs {
                    if !matches!(kwargs, LegacyValue::Dict(_)) {
                        return Err(PickleError::Constructor {
                            class: "UnpicklingError",
                            message: format!("{opcode} kwargs argument must be a dict, not {}", kwargs.type_name()),
                        });
                    }
                }
                // NEWOBJ allocates without initializing: these containers
                // take their contents only from a normal constructor call.
                let value = match callable {
                    AllowedGlobal::List => LegacyValue::List(Vec::new()),
                    AllowedGlobal::Dict => LegacyValue::Dict(Dict::new()),
                    AllowedGlobal::Set => LegacyValue::Set(Vec::new()),
                    _ => newobj::construct(callable, args, kwargs.as_ref().and_then(LegacyValue::as_dict))?,
                };
                stack.push(Item::Value(value));
            }
            0x93 => {
                let name = pop_value(&mut stack)?.resolved();
                let module = pop_value(&mut stack)?.resolved();
                let (Some(module), Some(name)) = (module.python_string(), name.python_string()) else {
                    return Err(PickleError::Constructor {
                        class: "UnpicklingError",
                        message: "STACK_GLOBAL requires str".into(),
                    });
                };
                stack.push(Item::Global(AllowedGlobal::resolve_native(&module, &name)?));
            }
            b'R' => {
                let args = pop_value(&mut stack)?.resolved();
                top_item(&stack)?;
                let callable = match stack.pop() {
                    Some(Item::Global(g)) => g,
                    Some(Item::Value(value)) => {
                        return Err(PickleError::Constructor {
                            class: "TypeError",
                            message: if matches!(args, LegacyValue::Tuple(_)) {
                                format!("'{}' object is not callable", value.type_name())
                            } else {
                                "argument list must be a tuple".into()
                            },
                        });
                    }
                    _ => return Err(PickleError::Malformed("REDUCE without an allowed callable".into())),
                };
                stack.push(Item::Value(construct(callable, args)?));
            }
            b'b' => {
                // No represented type has a custom state setter. The default
                // BUILD path checks the stack before consuming state, splits
                // a two-tuple, then handles dictionary state before slot state.
                if stack.iter().rev().take(2).take_while(|item| !matches!(item, Item::Mark)).count() < 2 {
                    return Err(stack_underflow(&stack));
                }
                let state = pop_value(&mut stack)?.resolved();
                let kind = match top_item(&stack)? {
                    Item::Value(value) => value.qualified_type_name(),
                    Item::Global(_) => "type",
                    Item::Mark => unreachable!("top_item rejects MARK"),
                };
                let (state, slots) = match state {
                    LegacyValue::Tuple(mut items) if items.len() == 2 => {
                        let slots = items.pop().expect("two-tuple");
                        (items.pop().expect("two-tuple"), Some(slots))
                    }
                    value => (value, None),
                };
                let state = state.resolved();
                let slots = slots.map(|value| value.resolved());
                if !matches!(state, LegacyValue::None) {
                    if let LegacyValue::Dict(fields) = &state {
                        if matches!(top_item(&stack)?, Item::Global(_)) {
                            if fields.is_empty() {
                                continue_build_slots(&stack, slots.as_ref())?;
                                continue;
                            }
                            return Err(PickleError::Constructor {
                                class: "TypeError",
                                message: "'mappingproxy' object does not support item assignment".into(),
                            });
                        }
                    }
                    let (class, message) = if matches!(state, LegacyValue::Dict(_)) {
                        ("AttributeError", format!("'{kind}' object has no attribute '__dict__'"))
                    } else {
                        ("UnpicklingError", "state is not a dictionary".into())
                    };
                    return Err(PickleError::Constructor { class, message });
                }
                if let Some(slots) = slots {
                    match slots {
                        LegacyValue::Dict(slots) if slots.is_empty() => {}
                        LegacyValue::Dict(slots) => build_slots(top_item(&stack)?, &slots)?,
                        _ => {
                            return Err(PickleError::Constructor {
                                class: "UnpicklingError",
                                message: "slot state is not a dictionary".into(),
                            });
                        }
                    }
                }
            }
            b'0' => {
                // Unlike other stack operations, POP may remove the current
                // MARK itself (marks are kept on a separate stack).
                if stack.pop().is_none() {
                    return Err(stack_underflow(&stack));
                }
            }
            b'1' => {
                pop_mark(&mut stack)?;
            }
            b'2' => {
                root_top(&mut stack)?;
                let top = top_item(&stack)?.clone();
                stack.push(top);
            }
            b'd' => {
                let items = pop_mark(&mut stack)?;
                require_pairs(&items, "DICT")?;
                let mut dict = Dict::new();
                insert_pairs(&mut dict, items)?;
                stack.push(Item::Value(LegacyValue::Dict(dict)));
            }
            b'l' => {
                let items = pop_mark(&mut stack)?;
                stack.push(Item::Value(LegacyValue::List(items)));
            }
            b't' => {
                let items = pop_mark(&mut stack)?;
                stack.push(Item::Value(LegacyValue::Tuple(items)));
            }
            0x85..=0x87 => {
                let n = usize::from(op - 0x84);
                if stack.iter().rev().take(n).take_while(|item| !matches!(item, Item::Mark)).count() < n {
                    return Err(stack_underflow(&stack));
                }
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(pop_value(&mut stack)?);
                }
                items.reverse();
                stack.push(Item::Value(LegacyValue::Tuple(items)));
            }
            0x91 => {
                let items = pop_mark(&mut stack)?;
                let mut set = Vec::new();
                extend_set(&mut set, items)?;
                stack.push(Item::Value(LegacyValue::FrozenSet(set)));
            }
            b's' => {
                let value = pop_value(&mut stack)?;
                let key = pop_value(&mut stack)?;
                assign_items(&mut stack, vec![key, value])?;
            }
            b'u' => {
                let items = pop_mark(&mut stack)?;
                top_item(&stack)?;
                if !items.is_empty() {
                    require_pairs(&items, "SETITEMS")?;
                    assign_items(&mut stack, items)?;
                }
            }
            b'a' => {
                let value = pop_value(&mut stack)?;
                append_items(&mut stack, vec![value])?;
            }
            b'e' => {
                let items = pop_mark(&mut stack)?;
                top_item(&stack)?;
                if !items.is_empty() {
                    append_items(&mut stack, items)?;
                }
            }
            0x90 => {
                let items = pop_mark(&mut stack)?;
                top_item(&stack)?;
                if items.is_empty() {
                    continue;
                }
                if let Some(Item::Value(LegacyValue::Object(object))) = stack.last() {
                    let mut value = object.resolved();
                    let result = if let LegacyValue::Set(set) = &mut value {
                        extend_set(set, items)
                    } else {
                        Err(PickleError::Constructor {
                            class: "AttributeError",
                            message: format!("'{}' object has no attribute 'add'", value.type_name()),
                        })
                    };
                    object.replace(value);
                    result?;
                    continue;
                }
                match stack.last_mut() {
                    Some(Item::Value(LegacyValue::Set(set))) => {
                        extend_set(set, items)?;
                    }
                    Some(Item::Value(value)) => {
                        return Err(PickleError::Constructor {
                            class: "AttributeError",
                            message: format!("'{}' object has no attribute 'add'", value.type_name()),
                        })
                    }
                    Some(Item::Global(global)) => {
                        return Err(PickleError::Constructor {
                            class: if *global == AllowedGlobal::Set { "TypeError" } else { "AttributeError" },
                            message: if *global == AllowedGlobal::Set {
                                let item = items.first().expect("nonempty ADDITEMS");
                                if item.type_name() == "set" {
                                    "set.add() takes exactly one argument (0 given)".into()
                                } else {
                                    format!(
                                        "descriptor 'add' for 'set' objects doesn't apply to a '{}' object",
                                        item.qualified_type_name()
                                    )
                                }
                            } else {
                                format!("type object '{}' has no attribute 'add'", global.native().name)
                            },
                        })
                    }
                    _ => return Err(PickleError::Malformed("ADDITEMS target is not a set".into())),
                }
            }
            other => return Err(PickleError::UnsupportedOpcode(other, at)),
        }
    }
}

fn unicode_value(bytes: &[u8]) -> Result<LegacyValue, PickleError> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(LegacyValue::Str(text.to_owned()));
    }
    if let Ok(text) = PythonString::from_wtf8(bytes) {
        return Ok(LegacyValue::from_python_string(text));
    }
    // BINUNICODE uses UTF-8/surrogatepass, including the precise later error
    // span when an earlier lone surrogate was valid under that handler.
    codecs::construct_str(&[LegacyValue::Bytes(bytes.to_vec()), "utf-8".into(), "surrogatepass".into()], None)
}

fn continue_build_slots(target: &[Item], slots: Option<&LegacyValue>) -> Result<(), PickleError> {
    if let Some(slots) = slots {
        let slots = slots.resolved();
        let LegacyValue::Dict(slots) = slots else {
            return Err(PickleError::Constructor {
                class: "UnpicklingError",
                message: "slot state is not a dictionary".into(),
            });
        };
        build_slots(top_item(target)?, &slots)?;
    }
    Ok(())
}
fn named_constructor_error(class: &'static str, prefix: &str, name: &PythonString, suffix: &str) -> PickleError {
    let mut message = prefix.chars().map(u32::from).collect::<Vec<_>>();
    message.extend_from_slice(name.codepoints());
    message.extend(suffix.chars().map(u32::from));
    PickleError::Native(NativeException::new(class, PythonString::from_codepoints(message).expect("code points")))
}
fn build_slots(target: &Item, slots: &Dict) -> Result<(), PickleError> {
    let Some((key, value)) = slots.iter().next() else {
        return Ok(());
    };
    let key_value = key.to_value();
    let name = key_value.python_string().ok_or_else(|| PickleError::Constructor {
        class: "TypeError",
        message: format!("attribute name must be string, not '{}'", key.qualified_type_name()),
    })?;
    if let Item::Global(global) = target {
        let global = global.native();
        let class_name =
            if global.module == "builtins" { global.name } else { format!("{}.{}", global.module, global.name) };
        return Err(named_constructor_error(
            "TypeError",
            "cannot set '",
            &name,
            &format!("' attribute of immutable type '{class_name}'"),
        ));
    }
    let Item::Value(target) = target else { unreachable!("BUILD target was validated") };
    let kind = target.type_name();
    let scalar = name.scalar_string();
    let attr = scalar.as_deref().unwrap_or("");
    if attr == "__class__" {
        return Err(PickleError::Constructor {
            class: "TypeError",
            message: if matches!(value.resolved(), LegacyValue::Global(_)) {
                "__class__ assignment only supported for mutable types or ModuleType subclasses".into()
            } else {
                format!("__class__ must be set to a class, not '{}' object", value.qualified_type_name())
            },
        });
    }
    let calendar = kind == "datetime"
        && ["year", "month", "day", "hour", "minute", "second", "microsecond", "tzinfo", "fold"].contains(&attr);
    let numeric = matches!(kind, "int" | "bool") && ["real", "imag", "numerator", "denominator"].contains(&attr)
        || kind == "float" && ["real", "imag"].contains(&attr);
    let buffer = kind == "memoryview"
        && [
            "obj",
            "nbytes",
            "readonly",
            "itemsize",
            "format",
            "ndim",
            "shape",
            "strides",
            "suboffsets",
            "c_contiguous",
            "f_contiguous",
            "contiguous",
        ]
        .contains(&attr);
    if calendar || numeric || buffer {
        return Err(named_constructor_error(
            "AttributeError",
            "attribute '",
            &name,
            &format!("' of '{}' objects is not writable", target.qualified_type_name()),
        ));
    }
    if kind == "timedelta" && ["days", "seconds", "microseconds"].contains(&attr) {
        return Err(PickleError::Constructor { class: "AttributeError", message: "readonly attribute".into() });
    }
    if builtin_attribute(kind, attr) {
        return Err(named_constructor_error(
            "AttributeError",
            &format!("'{}' object attribute '", target.qualified_type_name()),
            &name,
            "' is read-only",
        ));
    }
    Err(named_constructor_error(
        "AttributeError",
        &format!("'{}' object has no attribute '", target.qualified_type_name()),
        &name,
        "'",
    ))
}

fn builtin_attribute(kind: &str, name: &str) -> bool {
    // The method names each builtin type defines. BUILD state naming one of them
    // invokes that method; it does not add
    // attributes to immutable builtin instances.
    if matches!(kind, "int" | "bool" | "float" | "tuple" | "str" | "bytes") && name == "__getnewargs__" {
        return true;
    }
    if matches!(kind, "int" | "bool" | "float" | "timedelta")
        && [
            "__radd__",
            "__rsub__",
            "__rmul__",
            "__rtruediv__",
            "__rfloordiv__",
            "__rmod__",
            "__divmod__",
            "__rdivmod__",
        ]
        .contains(&name)
    {
        return true;
    }
    if matches!(kind, "int" | "bool")
        && ["__rpow__", "__rand__", "__ror__", "__rxor__", "__rlshift__", "__rrshift__"].contains(&name)
        || kind == "float" && ["__rpow__", "__getformat__"].contains(&name)
        || kind == "list" && ["__iadd__", "__imul__"].contains(&name)
        || kind == "NoneType" && name == "__bool__"
        || kind == "bytes" && name == "__bytes__"
        || kind == "memoryview" && ["_from_flags", "__delitem__"].contains(&name)
        || kind == "datetime" && ["__replace__", "__radd__", "__rsub__"].contains(&name)
        || matches!(kind, "set" | "frozenset")
            && ["__rand__", "__ror__", "__rxor__", "__rsub__", "__class_getitem__"].contains(&name)
    {
        return true;
    }
    const OBJECT: &[&str] = &[
        "__str__",
        "__repr__",
        "__format__",
        "__getattribute__",
        "__setattr__",
        "__delattr__",
        "__dir__",
        "__reduce__",
        "__reduce_ex__",
        "__sizeof__",
        "__init__",
        "__new__",
        "__eq__",
        "__lt__",
        "__le__",
        "__ge__",
        "__gt__",
        "__ne__",
        "__hash__",
        "__doc__",
        "__getstate__",
        "__init_subclass__",
        "__subclasshook__",
    ];
    if OBJECT.contains(&name) {
        return true;
    }
    if kind == "bytearray"
        && [
            "append",
            "clear",
            "copy",
            "extend",
            "insert",
            "pop",
            "remove",
            "reverse",
            "__alloc__",
            "__setitem__",
            "__delitem__",
            "__iadd__",
            "__imul__",
        ]
        .contains(&name)
    {
        return true;
    }
    let names: &[&str] = match kind {
        "list" => &[
            "append",
            "clear",
            "copy",
            "count",
            "extend",
            "index",
            "insert",
            "pop",
            "remove",
            "reverse",
            "sort",
            "__add__",
            "__mul__",
            "__rmul__",
            "__contains__",
            "__getitem__",
            "__setitem__",
            "__delitem__",
            "__iter__",
            "__len__",
            "__reversed__",
            "__class_getitem__",
        ],
        "tuple" => &[
            "count",
            "index",
            "__add__",
            "__mul__",
            "__rmul__",
            "__contains__",
            "__getitem__",
            "__iter__",
            "__len__",
            "__class_getitem__",
        ],
        "dict" => &[
            "clear",
            "copy",
            "fromkeys",
            "get",
            "items",
            "keys",
            "pop",
            "popitem",
            "setdefault",
            "update",
            "values",
            "__or__",
            "__ror__",
            "__ior__",
            "__contains__",
            "__getitem__",
            "__setitem__",
            "__delitem__",
            "__iter__",
            "__len__",
            "__reversed__",
            "__class_getitem__",
        ],
        "set" => &[
            "add",
            "clear",
            "copy",
            "difference",
            "difference_update",
            "discard",
            "intersection",
            "intersection_update",
            "isdisjoint",
            "issubset",
            "issuperset",
            "pop",
            "remove",
            "symmetric_difference",
            "symmetric_difference_update",
            "union",
            "update",
            "__and__",
            "__iand__",
            "__or__",
            "__ior__",
            "__xor__",
            "__ixor__",
            "__sub__",
            "__isub__",
            "__contains__",
            "__iter__",
            "__len__",
        ],
        "frozenset" => &[
            "copy",
            "difference",
            "intersection",
            "isdisjoint",
            "issubset",
            "issuperset",
            "symmetric_difference",
            "union",
            "__and__",
            "__or__",
            "__xor__",
            "__sub__",
            "__contains__",
            "__iter__",
            "__len__",
        ],
        "int" | "bool" => &[
            "as_integer_ratio",
            "bit_count",
            "bit_length",
            "conjugate",
            "from_bytes",
            "to_bytes",
            "is_integer",
            "__int__",
            "__index__",
            "__float__",
            "__bool__",
            "__ceil__",
            "__floor__",
            "__trunc__",
            "__round__",
            "__add__",
            "__sub__",
            "__mul__",
            "__truediv__",
            "__floordiv__",
            "__mod__",
            "__pow__",
            "__neg__",
            "__pos__",
            "__abs__",
            "__invert__",
            "__and__",
            "__or__",
            "__xor__",
            "__lshift__",
            "__rshift__",
        ],
        "float" => &[
            "as_integer_ratio",
            "conjugate",
            "fromhex",
            "hex",
            "is_integer",
            "__int__",
            "__float__",
            "__bool__",
            "__ceil__",
            "__floor__",
            "__trunc__",
            "__round__",
            "__add__",
            "__sub__",
            "__mul__",
            "__truediv__",
            "__floordiv__",
            "__mod__",
            "__pow__",
            "__neg__",
            "__pos__",
            "__abs__",
        ],
        "str" => &[
            "capitalize",
            "casefold",
            "center",
            "count",
            "encode",
            "endswith",
            "expandtabs",
            "find",
            "format",
            "format_map",
            "index",
            "isalnum",
            "isalpha",
            "isascii",
            "isdecimal",
            "isdigit",
            "isidentifier",
            "islower",
            "isnumeric",
            "isprintable",
            "isspace",
            "istitle",
            "isupper",
            "join",
            "ljust",
            "lower",
            "lstrip",
            "maketrans",
            "partition",
            "removeprefix",
            "removesuffix",
            "replace",
            "rfind",
            "rindex",
            "rjust",
            "rpartition",
            "rsplit",
            "rstrip",
            "split",
            "splitlines",
            "startswith",
            "strip",
            "swapcase",
            "title",
            "translate",
            "upper",
            "zfill",
            "__add__",
            "__mul__",
            "__rmul__",
            "__contains__",
            "__getitem__",
            "__iter__",
            "__len__",
            "__mod__",
            "__rmod__",
        ],
        "bytes" | "bytearray" => &[
            "capitalize",
            "center",
            "count",
            "decode",
            "endswith",
            "expandtabs",
            "find",
            "fromhex",
            "hex",
            "index",
            "isalnum",
            "isalpha",
            "isascii",
            "isdigit",
            "islower",
            "isspace",
            "istitle",
            "isupper",
            "join",
            "ljust",
            "lower",
            "lstrip",
            "maketrans",
            "partition",
            "removeprefix",
            "removesuffix",
            "replace",
            "rfind",
            "rindex",
            "rjust",
            "rpartition",
            "rsplit",
            "rstrip",
            "split",
            "splitlines",
            "startswith",
            "strip",
            "swapcase",
            "title",
            "translate",
            "upper",
            "zfill",
            "__add__",
            "__mul__",
            "__rmul__",
            "__contains__",
            "__getitem__",
            "__iter__",
            "__len__",
            "__mod__",
            "__rmod__",
        ],
        "memoryview" => &[
            "cast",
            "hex",
            "release",
            "tobytes",
            "tolist",
            "toreadonly",
            "__enter__",
            "__exit__",
            "__getitem__",
            "__setitem__",
            "__len__",
            "__iter__",
        ],
        "datetime" => &[
            "astimezone",
            "combine",
            "ctime",
            "date",
            "dst",
            "fromisocalendar",
            "fromisoformat",
            "fromordinal",
            "fromtimestamp",
            "isocalendar",
            "isoformat",
            "isoweekday",
            "now",
            "replace",
            "strftime",
            "strptime",
            "time",
            "timestamp",
            "timetuple",
            "timetz",
            "toordinal",
            "today",
            "tzname",
            "utcfromtimestamp",
            "utcnow",
            "utcoffset",
            "utctimetuple",
            "weekday",
            "max",
            "min",
            "resolution",
            "__add__",
            "__sub__",
        ],
        "timedelta" => &[
            "total_seconds",
            "max",
            "min",
            "resolution",
            "__add__",
            "__sub__",
            "__mul__",
            "__truediv__",
            "__floordiv__",
            "__mod__",
            "__neg__",
            "__pos__",
            "__abs__",
            "__bool__",
        ],
        _ => &[],
    };
    names.contains(&name)
}

fn global_line(bytes: &[u8]) -> Result<String, PickleError> {
    if bytes.is_empty() {
        return Err(PickleError::Truncated);
    }
    decode_utf8(bytes)
}

fn instance_line(bytes: &[u8]) -> Result<String, PickleError> {
    if bytes.is_empty() {
        return Err(PickleError::Truncated);
    }
    ascii(bytes)
}

fn decode_utf8(bytes: &[u8]) -> Result<String, PickleError> {
    String::from_utf8(bytes.to_vec()).map_err(|error| {
        let utf8 = error.utf8_error();
        let start = utf8.valid_up_to();
        let (end, reason) = match utf8.error_len() {
            None => (bytes.len(), "unexpected end of data"),
            Some(count) => (
                start + count,
                if matches!(bytes[start], 0xc2..=0xf4) { "invalid continuation byte" } else { "invalid start byte" },
            ),
        };
        let location = if end == start + 1 {
            format!("byte 0x{:02x} in position {start}", bytes[start])
        } else {
            format!("bytes in position {start}-{}", end - 1)
        };
        PickleError::Constructor {
            class: "UnicodeDecodeError",
            message: format!("'utf-8' codec can't decode {location}: {reason}"),
        }
    })
}

fn check_binary_size(size: u64, name: &str) -> Result<(), PickleError> {
    if size > i64::MAX as u64 {
        return Err(PickleError::Constructor {
            class: "OverflowError",
            message: format!("{name} exceeds system's maximum size of 9223372036854775807 bytes"),
        });
    }
    Ok(())
}

fn missing_persistent_loader() -> PickleError {
    PickleError::Constructor {
        class: "UnpicklingError",
        message: "A load persistent id instruction was encountered, but no persistent_load function was specified."
            .into(),
    }
}

/// Byte-string values of old-protocol pickles are decoded as ASCII.
fn ascii(bytes: &[u8]) -> Result<String, PickleError> {
    if let Some(index) = bytes.iter().position(|byte| !byte.is_ascii()) {
        return Err(PickleError::Constructor {
            class: "UnicodeDecodeError",
            message: format!(
                "'ascii' codec can't decode byte 0x{:02x} in position {index}: ordinal not in range(128)",
                bytes[index]
            ),
        });
    }
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn decode_long(mut bytes: &[u8]) -> Result<LegacyValue, PickleError> {
    if bytes.is_empty() {
        return Ok(LegacyValue::Int(0));
    }
    // LONG1/LONG4 need not use the shortest signed encoding. Ignore redundant
    // sign bytes before applying the represented integer width limit.
    while bytes.len() > 1 {
        let end = bytes.len();
        if (bytes[end - 1] == 0 && bytes[end - 2] & 0x80 == 0) || (bytes[end - 1] == 0xff && bytes[end - 2] & 0x80 != 0)
        {
            bytes = &bytes[..end - 1];
        } else {
            break;
        }
    }
    Ok(LegacyValue::from_integer(BigInt::from_signed_bytes_le(bytes)))
}

fn memo_put(memo: &mut Memo, idx: usize, stack: &mut [Item]) -> Result<(), PickleError> {
    root_top(stack)?;
    let top = top_item(stack)?.clone();
    if memo.entries.len() <= idx {
        memo.entries.resize(idx + 1, None);
    }
    if memo.entries[idx].is_none() {
        memo.used += 1;
    }
    memo.entries[idx] = Some(top);
    Ok(())
}
fn root_top(stack: &mut [Item]) -> Result<(), PickleError> {
    top_item(stack)?;
    if let Some(Item::Value(value)) = stack.last_mut() {
        if !matches!(
            value,
            LegacyValue::Object(_)
                | LegacyValue::None
                | LegacyValue::Bool(_)
                | LegacyValue::Int(_)
                | LegacyValue::BigInt(_)
                | LegacyValue::Float(_)
        ) {
            *value = LegacyValue::Object(NativeObject::new(std::mem::replace(value, LegacyValue::None)));
        }
    }
    Ok(())
}

fn memo_get(memo: &Memo, idx: usize) -> Result<Item, PickleError> {
    memo.entries
        .get(idx)
        .cloned()
        .flatten()
        .ok_or_else(|| PickleError::Constructor { class: "UnpicklingError", message: missing_memo_message(idx as i64) })
}

fn missing_memo_message(index: i64) -> String {
    // The size in this message is a signed 32-bit number: only the low word
    // of a larger size is shown.
    format!("Memo value not found at index {}", index as i32)
}

fn stack_underflow(stack: &[Item]) -> PickleError {
    PickleError::Constructor {
        class: "UnpicklingError",
        message: if stack.iter().any(|item| matches!(item, Item::Mark)) {
            "unexpected MARK found"
        } else {
            "unpickling stack underflow"
        }
        .into(),
    }
}

fn top_item(stack: &[Item]) -> Result<&Item, PickleError> {
    match stack.last() {
        None | Some(Item::Mark) => Err(stack_underflow(stack)),
        Some(item) => Ok(item),
    }
}

fn pop_value(stack: &mut Vec<Item>) -> Result<LegacyValue, PickleError> {
    top_item(stack)?;
    match stack.pop() {
        Some(Item::Value(v)) => Ok(v),
        Some(Item::Global(value)) => Ok(LegacyValue::Global(value.native())),
        _ => Err(PickleError::Malformed("expected a value on the stack".into())),
    }
}

fn pop_mark(stack: &mut Vec<Item>) -> Result<Vec<LegacyValue>, PickleError> {
    mark_values(take_mark_items(stack)?)
}

fn take_mark_items(stack: &mut Vec<Item>) -> Result<Vec<Item>, PickleError> {
    let mark = stack
        .iter()
        .rposition(|i| matches!(i, Item::Mark))
        .ok_or_else(|| PickleError::Constructor { class: "UnpicklingError", message: "could not find MARK".into() })?;
    let items = stack.split_off(mark + 1);
    stack.pop();
    Ok(items)
}

fn mark_values(items: Vec<Item>) -> Result<Vec<LegacyValue>, PickleError> {
    items
        .into_iter()
        .map(|i| match i {
            Item::Value(v) => Ok(v),
            Item::Global(value) => Ok(LegacyValue::Global(value.native())),
            _ => Err(PickleError::Malformed("unexpected marker inside a container".into())),
        })
        .collect()
}

fn extend_set(set: &mut Vec<LegacyValue>, items: impl IntoIterator<Item = LegacyValue>) -> Result<(), PickleError> {
    for item in items {
        require_hashable(&item)?;
        if !set.iter().any(|existing| existing.same_object(&item) || native_equal(existing, &item)) {
            set.push(item);
        }
    }
    Ok(())
}

fn require_hashable(value: &LegacyValue) -> Result<(), PickleError> {
    match value {
        LegacyValue::Object(value) => require_hashable(&value.resolved()),
        LegacyValue::List(_) | LegacyValue::Dict(_) | LegacyValue::Set(_) | LegacyValue::ByteArray(_) => {
            Err(PickleError::Constructor {
                class: "TypeError",
                message: format!("unhashable type: '{}'", value.type_name()),
            })
        }
        LegacyValue::Tuple(values) | LegacyValue::FrozenSet(values) => values.iter().try_for_each(require_hashable),
        LegacyValue::MemoryView(_) => Key::from_value(value).map(|_| ()).map_err(|message| PickleError::Constructor {
            class: if message == "cannot hash writable memoryview object" { "ValueError" } else { "TypeError" },
            message,
        }),
        _ => Ok(()),
    }
}

fn insert_pairs(dict: &mut Dict, items: Vec<LegacyValue>) -> Result<(), PickleError> {
    if !items.len().is_multiple_of(2) {
        return Err(PickleError::Malformed("odd number of dict items".into()));
    }
    let mut it = items.into_iter();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        dict.insert_native(k, v).map_err(|message| PickleError::Constructor { class: "TypeError", message })?;
    }
    Ok(())
}

fn require_pairs(items: &[LegacyValue], opcode: &str) -> Result<(), PickleError> {
    if !items.len().is_multiple_of(2) {
        return Err(PickleError::Constructor {
            class: "UnpicklingError",
            message: format!("odd number of items for {opcode}"),
        });
    }
    Ok(())
}

fn assign_items(stack: &mut [Item], items: Vec<LegacyValue>) -> Result<(), PickleError> {
    top_item(stack)?;
    if let Some(Item::Value(LegacyValue::Object(object))) = stack.last() {
        let mut temporary = [Item::Value(object.resolved())];
        let result = assign_items(&mut temporary, items);
        let Item::Value(value) = std::mem::replace(&mut temporary[0], Item::Mark) else { unreachable!() };
        object.replace(value);
        return result;
    }
    match stack.last_mut() {
        Some(Item::Value(LegacyValue::Dict(d))) => insert_pairs(d, items),
        Some(Item::Value(LegacyValue::List(list))) => {
            let mut items = items.into_iter();
            while let (Some(key), Some(value)) = (items.next(), items.next()) {
                // SETITEMS assigns by subscript: a list target
                // replaces existing indices, including bool/negative indices.
                let index = match key.integer() {
                    Some(index) => index.to_i64().ok_or_else(|| PickleError::Constructor {
                        class: "IndexError",
                        message: "cannot fit 'int' into an index-sized integer".into(),
                    })?,
                    None => {
                        return Err(PickleError::Constructor {
                            class: "TypeError",
                            message: format!("list indices must be integers or slices, not {}", key.type_name()),
                        })
                    }
                };
                let adjusted = if index < 0 { i128::from(index) + list.len() as i128 } else { i128::from(index) };
                let slot = usize::try_from(adjusted).ok().and_then(|index| list.get_mut(index)).ok_or_else(|| {
                    PickleError::Constructor {
                        class: "IndexError",
                        message: "list assignment index out of range".into(),
                    }
                })?;
                *slot = value;
            }
            Ok(())
        }
        Some(Item::Value(value)) if matches!(value, LegacyValue::ByteArray(_)) => {
            let mut items = items.into_iter();
            while let (Some(key), Some(cell)) = (items.next(), items.next()) {
                let raw = key.integer().ok_or_else(|| PickleError::Constructor {
                    class: "TypeError",
                    message: format!("bytearray indices must be integers or slices, not {}", key.qualified_type_name()),
                })?;
                let raw = raw.to_i64().ok_or_else(|| PickleError::Constructor {
                    class: "IndexError",
                    message: "cannot fit 'int' into an index-sized integer".into(),
                })?;
                let byte = cell
                    .integer()
                    .ok_or_else(|| PickleError::Constructor {
                        class: "TypeError",
                        message: format!("'{}' object cannot be interpreted as an integer", cell.qualified_type_name()),
                    })?
                    .to_u8()
                    .ok_or_else(|| PickleError::Constructor {
                        class: "ValueError",
                        message: "byte must be in range(0, 256)".into(),
                    })?;
                let length = value.py_len().expect("bytearray");
                let adjusted = if raw < 0 { i128::from(raw) + length as i128 } else { i128::from(raw) };
                let index = usize::try_from(adjusted).map_err(|_| PickleError::Constructor {
                    class: "IndexError",
                    message: "bytearray index out of range".into(),
                })?;
                value.assign_index(index, LegacyValue::Int(i64::from(byte))).map_err(native_failure)?;
            }
            Ok(())
        }
        Some(Item::Value(LegacyValue::MemoryView(_))) => {
            Err(PickleError::Constructor { class: "TypeError", message: "cannot modify read-only memory".into() })
        }
        Some(Item::Value(value)) => Err(PickleError::Constructor {
            class: "TypeError",
            message: format!("'{}' object does not support item assignment", value.qualified_type_name()),
        }),
        Some(Item::Global(_)) => Err(PickleError::Constructor {
            class: "TypeError",
            message: "'type' object does not support item assignment".into(),
        }),
        _ => unreachable!("SETITEM validates target stack"),
    }
}

fn with_top_list(stack: &mut [Item], f: impl FnOnce(&mut Vec<LegacyValue>)) -> Result<(), PickleError> {
    top_item(stack)?;
    if let Some(Item::Value(LegacyValue::Object(object))) = stack.last() {
        return object.edit(|value| match value {
            LegacyValue::List(list) => {
                f(list);
                Ok(())
            }
            value => Err(PickleError::Constructor {
                class: "AttributeError",
                message: format!("'{}' object has no attribute 'append'", value.type_name()),
            }),
        });
    }
    match stack.last_mut() {
        Some(Item::Value(LegacyValue::List(l))) => {
            f(l);
            Ok(())
        }
        Some(Item::Value(value)) => Err(PickleError::Constructor {
            class: "AttributeError",
            message: format!("'{}' object has no attribute 'append'", value.type_name()),
        }),
        _ => Err(PickleError::Malformed("APPEND target is not a list".into())),
    }
}

fn native_failure(detail: String) -> PickleError {
    let Some((class, message)) = detail.split_once(": ") else {
        return PickleError::Malformed(detail);
    };
    let class = match class {
        "TypeError" => "TypeError",
        "ValueError" => "ValueError",
        "IndexError" => "IndexError",
        "BufferError" => "BufferError",
        "OverflowError" => "OverflowError",
        "AttributeError" => "AttributeError",
        "RecursionError" => "RecursionError",
        _ => return PickleError::Malformed(detail),
    };
    PickleError::Constructor { class, message: message.into() }
}
fn append_items(stack: &mut [Item], items: Vec<LegacyValue>) -> Result<(), PickleError> {
    if let Some(Item::Value(value)) = stack.last_mut() {
        if value.type_name() == "bytearray" {
            return value.extend_bytearray(&items).map_err(native_failure);
        }
    }
    if let Some(Item::Global(global)) = stack.last() {
        let global = global.native();
        return Err(PickleError::Constructor {
            class: if global.name == "list" { "TypeError" } else { "AttributeError" },
            message: if global.name == "list" {
                "list.extend() takes exactly one argument (0 given)".into()
            } else {
                format!("type object '{}' has no attribute 'append'", global.name)
            },
        });
    }
    with_top_list(stack, |values| values.extend(items))
}

fn items_of(args: &LegacyValue) -> Option<&[LegacyValue]> {
    match args {
        LegacyValue::Tuple(v) => Some(v),
        _ => None,
    }
}

fn iterable_values(arg: &LegacyValue) -> Result<Vec<LegacyValue>, PickleError> {
    match arg {
        LegacyValue::Object(object) => iterable_values(&object.resolved()),
        LegacyValue::List(v) | LegacyValue::Tuple(v) | LegacyValue::Set(v) | LegacyValue::FrozenSet(v) => Ok(v.clone()),
        LegacyValue::Str(v) => Ok(v.chars().map(|c| LegacyValue::Str(c.to_string())).collect()),
        LegacyValue::Unicode(v) => Ok(v
            .codepoints()
            .iter()
            .map(|&c| LegacyValue::from_python_string(PythonString::from_codepoints(vec![c]).expect("codepoint")))
            .collect()),
        LegacyValue::Bytes(v) | LegacyValue::ByteArray(v) => {
            Ok(v.iter().map(|&b| LegacyValue::Int(i64::from(b))).collect())
        }
        LegacyValue::MemoryView(v) => {
            Ok(v.buffer.buffer_bytes().expect("live buffer").iter().map(|&b| LegacyValue::Int(i64::from(b))).collect())
        }
        LegacyValue::Dict(v) => Ok(v.iter_native().map(|(key, _)| key).collect()),
        value => Err(PickleError::Constructor {
            class: "TypeError",
            message: format!("'{}' object is not iterable", value.qualified_type_name()),
        }),
    }
}

/// Applies one of the allowed constructors the way `REDUCE` does (`callable(*args)`).
fn construct(callable: AllowedGlobal, args: LegacyValue) -> Result<LegacyValue, PickleError> {
    let args = args.resolved();
    let args = items_of(&args).ok_or_else(|| PickleError::Constructor {
        class: "TypeError",
        message: "argument list must be a tuple".into(),
    })?;
    if callable == AllowedGlobal::Str {
        return codecs::construct_str(args, None);
    }
    if matches!(callable, AllowedGlobal::Str | AllowedGlobal::Int | AllowedGlobal::Float | AllowedGlobal::Bool) {
        return scalar_constructor(callable, args);
    }
    if !matches!(callable, AllowedGlobal::DateTime) && args.len() > 1 {
        let name = match callable {
            AllowedGlobal::Dict => "dict",
            AllowedGlobal::List => "list",
            AllowedGlobal::Tuple => "tuple",
            AllowedGlobal::Set => "set",
            _ => unreachable!(),
        };
        return Err(PickleError::Constructor {
            class: "TypeError",
            message: format!("{name} expected at most 1 argument, got {}", args.len()),
        });
    }
    let iterable = |arg: Option<&LegacyValue>| -> Result<Vec<LegacyValue>, PickleError> {
        arg.map_or_else(|| Ok(Vec::new()), iterable_values)
    };
    match callable {
        AllowedGlobal::Set => {
            let mut out: Vec<LegacyValue> = Vec::new();
            extend_set(&mut out, iterable(args.first())?)?;
            Ok(LegacyValue::Set(out))
        }
        AllowedGlobal::List => Ok(LegacyValue::List(iterable(args.first())?)),
        AllowedGlobal::Tuple => {
            if args.first().is_some_and(|v| matches!(v.resolved(), LegacyValue::Tuple(_))) {
                Ok(args[0].clone())
            } else {
                Ok(LegacyValue::Tuple(iterable(args.first())?))
            }
        }
        AllowedGlobal::Dict => match args.first() {
            None => Ok(LegacyValue::Dict(Dict::new())),
            Some(LegacyValue::Dict(d)) => Ok(LegacyValue::Dict(d.clone())),
            Some(value) if matches!(value.resolved(), LegacyValue::Dict(_)) => Ok(value.resolved()),
            Some(value) => {
                let mut out = Dict::new();
                for (index, item) in iterable(Some(value))?.iter().enumerate() {
                    let pair = iterable(Some(item)).map_err(|_| PickleError::Constructor {
                        class: "TypeError",
                        message: format!("cannot convert dictionary update sequence element #{index} to a sequence"),
                    })?;
                    if pair.len() != 2 {
                        return Err(PickleError::Constructor {
                            class: "ValueError",
                            message: format!(
                                "dictionary update sequence element #{index} has length {}; 2 is required",
                                pair.len()
                            ),
                        });
                    }
                    out.insert_native(pair[0].clone(), pair[1].clone()).map_err(|message| {
                        PickleError::Constructor {
                            class: if message == "cannot hash writable memoryview object" {
                                "ValueError"
                            } else {
                                "TypeError"
                            },
                            message,
                        }
                    })?;
                }
                Ok(LegacyValue::Dict(out))
            }
        },
        AllowedGlobal::DateTime => newobj::datetime(args, None),
        AllowedGlobal::Str | AllowedGlobal::Int | AllowedGlobal::Float | AllowedGlobal::Bool => unreachable!(),
    }
}

fn scalar_constructor(callable: AllowedGlobal, args: &[LegacyValue]) -> Result<LegacyValue, PickleError> {
    use LegacyValue as V;
    let name = match callable {
        AllowedGlobal::Str => "str",
        AllowedGlobal::Int => "int",
        AllowedGlobal::Float => "float",
        AllowedGlobal::Bool => "bool",
        _ => unreachable!(),
    };
    if args.len() > 1 {
        if matches!(callable, AllowedGlobal::Float | AllowedGlobal::Bool) {
            return Err(PickleError::Constructor {
                class: "TypeError",
                message: format!("{name} expected at most 1 argument, got {}", args.len()),
            });
        }
        if matches!(callable, AllowedGlobal::Int) {
            return based_integer_constructor(args);
        }
        // Explicit-base int and encoded str forms are not silently evaluated
        // as their first argument. They need separate conversion support.
        return Err(PickleError::Malformed(format!("unsupported {name} constructor argument count")));
    }
    let Some(value) = args.first() else {
        return Ok(match callable {
            AllowedGlobal::Str => V::Str(String::new()),
            AllowedGlobal::Int => V::Int(0),
            AllowedGlobal::Float => V::Float(0.0),
            AllowedGlobal::Bool => V::Bool(false),
            _ => unreachable!(),
        });
    };
    if matches!(value, LegacyValue::Object(_))
        && match callable {
            AllowedGlobal::Int => matches!(value.resolved(), V::Int(_) | V::BigInt(_)),
            AllowedGlobal::Float => matches!(value.resolved(), V::Float(_)),
            _ => false,
        }
    {
        return Ok(value.clone());
    }
    let resolved;
    let value = if matches!(callable, AllowedGlobal::Str | AllowedGlobal::Bool) {
        value
    } else {
        resolved = value.resolved();
        &resolved
    };
    match callable {
        AllowedGlobal::Str => Ok(V::Str(openrigcue_model::python_repr::str_value(value))),
        AllowedGlobal::Bool => Ok(V::Bool(value.is_truthy())),
        AllowedGlobal::Int => value.py_integer().map(V::from_integer).map_err(native_failure),
        AllowedGlobal::Float => value.py_float_native().map(V::from_float).map_err(native_failure),
        _ => unreachable!(),
    }
}

fn based_integer_constructor(args: &[LegacyValue]) -> Result<LegacyValue, PickleError> {
    let error = |class, message| PickleError::Constructor { class, message };
    if args.len() > 2 {
        return Err(error("TypeError", format!("int expected at most 2 arguments, got {}", args.len())));
    }
    let base = args[1].integer().ok_or_else(|| {
        error("TypeError", format!("'{}' object cannot be interpreted as an integer", args[1].qualified_type_name()))
    })?;
    // An overflowing base is clipped first; the base
    // range check then reports a value error rather than an overflow.
    let base = base.to_i64().unwrap_or_else(|| if base < BigInt::from(0) { i64::MIN } else { i64::MAX });
    if base != 0 && !(2..=36).contains(&base) {
        return Err(error("ValueError", "int() base must be >= 2 and <= 36, or 0".into()));
    }
    let value = args[0].resolved();
    let text = if let Some(text) = value.python_string() {
        text.scalar_string()
    } else if matches!(value, LegacyValue::Bytes(_) | LegacyValue::ByteArray(_)) {
        value
            .buffer_bytes()
            .filter(|bytes| bytes.is_ascii())
            .map(|bytes| String::from_utf8(bytes).expect("ASCII bytes"))
    } else {
        return Err(error("TypeError", "int() can't convert non-string with explicit base".into()));
    };
    let text = text.ok_or_else(|| {
        error(
            "ValueError",
            format!("invalid literal for int() with base {base}: {}", openrigcue_model::python_repr::repr(&value)),
        )
    })?;
    text_number::based_integer(&text, &value, base as u32)
}

fn datetime_from_state(args: &[LegacyValue]) -> Result<LegacyValue, PickleError> {
    let args = args.iter().map(LegacyValue::resolved).collect::<Vec<_>>();
    let args = args.as_slice();
    let state = if (1..=2).contains(&args.len()) {
        match &args[0] {
            LegacyValue::Bytes(bytes) if bytes.len() == 10 && (1..=12).contains(&(bytes[2] & 0x7f)) => Some(bytes.clone()),
            LegacyValue::Str(text) if text.chars().count() == 10 && text.chars().nth(2).is_some_and(|c| (1..=12).contains(&((c as u32) & 0x7f))) => {
                Some(text.chars().map(|c| u8::try_from(u32::from(c)).map_err(|_| PickleError::Constructor {
                    class: "ValueError",
                    message: "Failed to encode latin1 string when unpickling a datetime object. pickle.load(data, encoding='latin1') is assumed.".into(),
                })).collect::<Result<Vec<_>, _>>()?)
            }
            LegacyValue::Unicode(text) if text.len()==10&&(1..=12).contains(&(text.codepoints()[2]&127))=>{
                Some(text.codepoints().iter().map(|&cp|u8::try_from(cp).map_err(|_|PickleError::Constructor{class:"ValueError",message:"Failed to encode latin1 string when unpickling a datetime object. pickle.load(data, encoding='latin1') is assumed.".into()})).collect::<Result<Vec<_>,_>>()?)
            },
            _ => None,
        }
    } else {
        None
    };
    if let Some(bytes) = state {
        if args.get(1).is_some_and(|value| !matches!(value, LegacyValue::None)) {
            return Err(PickleError::Constructor { class: "TypeError", message: "bad tzinfo state arg".into() });
        }
        return Ok(datetime_parts(&bytes));
    }
    datetime_from_calendar(args)
}

fn datetime_parts(bytes: &[u8]) -> LegacyValue {
    // The high bit of the month byte is the PEP 495 `fold` flag.
    LegacyValue::DateTime(DateTimeParts {
        year: u16::from(bytes[0]) << 8 | u16::from(bytes[1]),
        month: bytes[2] & 0x7f,
        fold: bytes[2] & 0x80 != 0,
        day: bytes[3],
        hour: bytes[4],
        minute: bytes[5],
        second: bytes[6],
        microsecond: u32::from(bytes[7]) << 16 | u32::from(bytes[8]) << 8 | u32::from(bytes[9]),
    })
}

fn datetime_from_calendar(args: &[LegacyValue]) -> Result<LegacyValue, PickleError> {
    if args.len() > 8 {
        return Err(PickleError::Constructor {
            class: "TypeError",
            message: format!("function takes at most 8 positional arguments ({} given)", args.len()),
        });
    }
    let mut values = [0_i32; 7];
    for (index, field) in ["year", "month", "day", "hour", "minute", "second", "microsecond"].iter().enumerate() {
        let Some(value) = args.get(index) else {
            if index < 3 {
                return Err(PickleError::Constructor {
                    class: "TypeError",
                    message: format!("function missing required argument '{field}' (pos {})", index + 1),
                });
            }
            break;
        };
        values[index] = newobj::c_int(value)?;
    }
    let [year, month, day, hour, minute, second, microsecond] = values;
    let error = |message| PickleError::Constructor { class: "ValueError", message };
    if !(1..=9999).contains(&year) {
        return Err(error(format!("year {year} is out of range")));
    }
    if !(1..=12).contains(&month) {
        return Err(error("month must be in 1..12".into()));
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month as usize - 1];
    if !(1..=days).contains(&day) {
        return Err(error("day is out of range for month".into()));
    }
    for (value, max, message) in [
        (hour, 23, "hour must be in 0..23"),
        (minute, 59, "minute must be in 0..59"),
        (second, 59, "second must be in 0..59"),
        (microsecond, 999999, "microsecond must be in 0..999999"),
    ] {
        if !(0..=max).contains(&value) {
            return Err(error(message.into()));
        }
    }
    if let Some(value) = args.get(7).filter(|value| !matches!(value, LegacyValue::None)) {
        return Err(PickleError::Constructor {
            class: "TypeError",
            message: format!("tzinfo argument must be None or of a tzinfo subclass, not type '{}'", value.type_name()),
        });
    }
    Ok(LegacyValue::DateTime(DateTimeParts {
        year: year as u16,
        month: month as u8,
        day: day as u8,
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
        microsecond: microsecond as u32,
        fold: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_fold_survives_the_configuration_data_roundtrip() {
        let encoded = b"\x80\x04cdatetime\ndatetime\nC\n\x07\xe8\x8b\x03\x01\x1e\x00\x00\x00\x00\x85R.";
        let value = decode(encoded).unwrap();
        assert_eq!(decode(&encode(&value)).unwrap(), value);
        assert_eq!(openrigcue_model::python_repr::repr(&value), "datetime.datetime(2024, 11, 3, 1, 30, fold=1)");
        assert_eq!(openrigcue_model::python_repr::str_value(&value), "2024-11-03 01:30:00");
        let LegacyValue::DateTime(parts) = &value else { panic!("datetime fixture") };
        let without_fold = LegacyValue::DateTime(DateTimeParts { fold: false, ..*parts });
        assert_ne!(value, without_fold, "the stored fold bit remains distinct");
        assert!(native_equal(&value, &without_fold), "naive datetime equality ignores fold");
        let aliases = LegacyValue::Set(vec![value.clone(), without_fold]);
        assert_eq!(decode(&encode(&aliases)).unwrap(), LegacyValue::Set(vec![value.clone()]));
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("datetime.db");
        crate::import::sqlitedict::set_item(&database, "unnamed", "datetime", &value).unwrap();
        assert_eq!(crate::import::sqlitedict::read_item(&database, "unnamed", "datetime").unwrap(), Some(value));
    }

    #[test]
    fn numeric_set_aliases_keep_the_first_represented_value() {
        for encoded in [
            &b"\x80\x04\x8f(\x88K\x01G?\xf0\x00\x00\x00\x00\x00\x00\x90."[..],
            &b"\x80\x02cbuiltins\nset\n](\x88K\x01G?\xf0\x00\x00\x00\x00\x00\x00e\x85R."[..],
            &b"\x80\x04(\x88K\x01G?\xf0\x00\x00\x00\x00\x00\x00\x91."[..],
        ] {
            assert_eq!(
                decode(encoded).unwrap(),
                if encoded[encoded.len() - 2] == 0x91 {
                    LegacyValue::FrozenSet(vec![LegacyValue::Bool(true)])
                } else {
                    LegacyValue::Set(vec![LegacyValue::Bool(true)])
                }
            );
        }
        let int_first = b"\x80\x04\x8f(K\x01\x88G?\xf0\x00\x00\x00\x00\x00\x00\x90.";
        assert_eq!(decode(int_first).unwrap(), LegacyValue::Set(vec![LegacyValue::Int(1)]));
        let tuples = b"\x80\x04\x8f(\x88K\x02\x86K\x01G@\x00\x00\x00\x00\x00\x00\x00\x86\x90.";
        let value = decode(tuples).unwrap();
        assert_eq!(
            value,
            LegacyValue::Set(vec![LegacyValue::Tuple(vec![LegacyValue::Bool(true), LegacyValue::Int(2)])])
        );
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("sets.db");
        crate::import::sqlitedict::set_item(&database, "unnamed", "set", &value).unwrap();
        assert_eq!(crate::import::sqlitedict::read_item(&database, "unnamed", "set").unwrap(), Some(value));
    }

    #[test]
    fn float_dictionary_keys_retain_numeric_kind_and_first_zero_sign() {
        let encoded = b"\x80\x04}(G?\xf8\x00\x00\x00\x00\x00\x00K*\x8c\x031.5K+u.";
        let value = decode(encoded).unwrap();
        assert_eq!(openrigcue_model::python_repr::repr(&value), "{1.5: 42, '1.5': 43}");
        assert_eq!(serde_json::to_string(&value).unwrap(), r#"{"1.5":42,"1.5":43}"#);
        assert_eq!(decode(&encode(&value)).unwrap(), value);
        for (encoded, expected) in [
            (&b"\x80\x04}(G?\xf0\x00\x00\x00\x00\x00\x00K*\x88K+K\x01K,u."[..], "{1.0: 44}"),
            (&b"\x80\x04}(G\x80\x00\x00\x00\x00\x00\x00\x00K*\x89K+K\x00K,u."[..], "{-0.0: 44}"),
        ] {
            let value = decode(encoded).unwrap();
            assert_eq!(openrigcue_model::python_repr::repr(&value), expected);
            assert_eq!(decode(&encode(&value)).unwrap(), value);
        }
    }

    #[test]
    fn boolean_dictionary_keys_keep_the_first_numeric_alias_kind() {
        // Protocol-4 data only: true, text True and false are distinct keys.
        let encoded = b"\x80\x04}(\x88K*\x8c\x04TrueK+\x89K,u.";
        let value = decode(encoded).unwrap();
        assert_eq!(openrigcue_model::python_repr::repr(&value), "{True: 42, 'True': 43, False: 44}");
        assert_eq!(serde_json::to_string(&value).unwrap(), r#"{"true":42,"True":43,"false":44}"#);
        assert_eq!(decode(&encode(&value)).unwrap(), value);

        let mut aliases = Vec::new();
        for (encoded, expected, roundtrip) in [
            (&b"\x80\x04}(\x88K*K\x01K+\x89K,K\x00K-u."[..], "{True: 43, False: 45}", &b"\x80\x04}(\x88K+\x89K-u."[..]),
            (&b"\x80\x04}(K\x01K*\x88K+K\x00K,\x89K-u."[..], "{1: 43, 0: 45}", &b"\x80\x04}(K\x01K+K\x00K-u."[..]),
        ] {
            let value = decode(encoded).unwrap();
            assert_eq!(openrigcue_model::python_repr::repr(&value), expected);
            assert_eq!(decode(&encode(&value)).unwrap(), decode(roundtrip).unwrap());
            let dict = value.as_dict().unwrap();
            assert_eq!(dict.get_key(&Key::Bool(true)), Some(&LegacyValue::Int(43)));
            assert_eq!(dict.get_key(&Key::Int(1)), Some(&LegacyValue::Int(43)));
            assert_eq!(dict.get_key(&Key::Bool(false)), Some(&LegacyValue::Int(45)));
            assert_eq!(dict.get_key(&Key::Int(0)), Some(&LegacyValue::Int(45)));
            aliases.push(value);
        }
        assert_ne!(aliases[0], aliases[1], "structural equality retains the actual key kinds");
        assert!(openrigcue_model::python_compare::native_equal(&aliases[0], &aliases[1]));
    }

    #[test]
    fn null_dictionary_key_remains_distinct_from_text_none() {
        let encoded = b"\x80\x04}(NK*\x8c\x04NoneK+u.";
        let value = decode(encoded).unwrap();
        assert_eq!(openrigcue_model::python_repr::repr(&value), "{None: 42, 'None': 43}");
        assert_eq!(serde_json::to_string(&value).unwrap(), r#"{"null":42,"None":43}"#);
        assert_eq!(decode(&encode(&value)).unwrap(), value);
    }

    /// Protocol-5 pickle of `{'gear_shift_perc': 94, 'wind_type': 'F', 'publish': False, 'f': 1.5,
    /// 'vehicles': {}}`.
    fn protocol5_profile() -> Vec<u8> {
        b"\x80\x05\x95N\x00\x00\x00\x00\x00\x00\x00}\x94(\x8c\x0fgear_shift_perc\x94K^\x8c\twind_type\x94\x8c\x01F\x94\x8c\x07publish\x94\x89\x8c\x01f\x94G?\xf8\x00\x00\x00\x00\x00\x00\x8c\x08vehicles\x94}\x94u."
            .to_vec()
    }

    /// Decodes a profile-shaped dict.
    #[test]
    fn decodes_protocol5_dict() {
        let v = decode(&protocol5_profile()).unwrap();
        let d = v.as_dict().unwrap();
        assert_eq!(d.str_keys(), ["gear_shift_perc", "wind_type", "publish", "f", "vehicles"]);
        assert_eq!(d.get("gear_shift_perc"), Some(&LegacyValue::Int(94)));
        assert_eq!(d.get("wind_type"), Some(&LegacyValue::Str("F".into())));
        assert_eq!(d.get("publish"), Some(&LegacyValue::Bool(false)));
        assert_eq!(d.get("f"), Some(&LegacyValue::Float(1.5)));
        assert_eq!(d.get("vehicles"), Some(&LegacyValue::Dict(Dict::new())));
    }

    /// Any global outside the allowlist is rejected.
    #[test]
    fn rejects_forbidden_globals() {
        let data = b"\x80\x02cos\nsystem\nq\x00X\x02\x00\x00\x00idq\x01\x85q\x02Rq\x03.";
        assert_eq!(decode(data), Err(PickleError::ForbiddenGlobal("os".into(), "system".into())));
        let data = b"\x80\x04\x8c\x08builtins\x8c\x04eval\x93.";
        assert_eq!(decode(data), Err(PickleError::ForbiddenGlobal("builtins".into(), "eval".into())));
    }

    /// Protocol-4 pickle of the datetime 2024-03-05 14:07:09.123456.
    #[test]
    fn decodes_datetime() {
        let data = b"\x80\x04\x95*\x00\x00\x00\x00\x00\x00\x00\x8c\x08datetime\x94\x8c\x08datetime\x94\x93\x94C\n\x07\xe8\x03\x05\x0e\x07\t\x01\xe2@\x94\x85\x94R\x94.";
        let v = decode(data).unwrap();
        assert_eq!(
            v,
            LegacyValue::DateTime(DateTimeParts {
                year: 2024,
                month: 3,
                day: 5,
                hour: 14,
                minute: 7,
                second: 9,
                microsecond: 123456,
                fold: false,
            })
        );
    }

    /// Protocol 2 plain containers do not require a forbidden global lookup.
    #[test]
    fn decodes_protocol2_lists_and_ints() {
        let data = b"\x80\x02}q\x00X\x01\x00\x00\x00aq\x01]q\x02(K\x01J\xfe\xff\xff\xffM,\x01Jp\x11\x01\x00es.";
        let v = decode(data).unwrap();
        let d = v.as_dict().unwrap();
        assert_eq!(
            d.get("a"),
            Some(&LegacyValue::List(vec![
                LegacyValue::Int(1),
                LegacyValue::Int(-2),
                LegacyValue::Int(300),
                LegacyValue::Int(70000)
            ]))
        );
    }

    #[test]
    fn long_integers_and_truncation() {
        assert_eq!(decode(b"\x80\x02\x8a\x02\x00\x80."), Ok(LegacyValue::Int(-32768)));
        assert_eq!(decode(b"\x80\x02\x8a\x00."), Ok(LegacyValue::Int(0)));
        assert_eq!(decode(b"\x80\x02}"), Err(PickleError::Exhausted));
    }
}

#[cfg(test)]
mod scalar_constructor_tests {
    use super::*;

    fn reduce(name: &str, args: Vec<LegacyValue>) -> Vec<u8> {
        let mut bytes = vec![0x80, 0x04];
        bytes.extend_from_slice(format!("cbuiltins\n{name}\n").as_bytes());
        let arguments = encode(&LegacyValue::Tuple(args));
        bytes.extend_from_slice(&arguments[2..arguments.len() - 1]);
        bytes.extend_from_slice(b"R.");
        bytes
    }

    #[test]
    fn allowed_scalar_reductions_construct_the_requested_type() {
        let cases = [
            ("str", vec![], LegacyValue::Str(String::new())),
            ("int", vec![], LegacyValue::Int(0)),
            ("float", vec![], LegacyValue::Float(0.0)),
            ("bool", vec![], LegacyValue::Bool(false)),
            ("int", vec!["１２_٣".into()], LegacyValue::Int(123)),
            ("int", vec![LegacyValue::Float(-2.9)], LegacyValue::Int(-2)),
            ("int", vec![LegacyValue::Bool(true)], LegacyValue::Int(1)),
            ("int", vec![LegacyValue::Bytes(b" -42 ".to_vec())], LegacyValue::Int(-42)),
            ("float", vec!["１_２.５".into()], LegacyValue::Float(12.5)),
            ("float", vec![LegacyValue::Int(3)], LegacyValue::Float(3.0)),
            ("float", vec![LegacyValue::Bytes(b"1e2".to_vec())], LegacyValue::Float(100.0)),
            ("bool", vec![LegacyValue::Str("False".into())], LegacyValue::Bool(true)),
            ("bool", vec![LegacyValue::List(vec![])], LegacyValue::Bool(false)),
            ("bool", vec![LegacyValue::List(vec![0.into()])], LegacyValue::Bool(true)),
            ("str", vec![LegacyValue::Bool(true)], LegacyValue::Str("True".into())),
            ("str", vec![LegacyValue::List(vec![1.into(), "x".into()])], LegacyValue::Str("[1, 'x']".into())),
        ];
        for (name, args, expected) in cases {
            assert_eq!(decode(&reduce(name, args)).unwrap(), expected, "{name}");
        }
        let LegacyValue::Float(zero) = decode(&reduce("float", vec!["-0".into()])).unwrap() else {
            panic!("float type")
        };
        assert_eq!(zero.to_bits(), (-0.0f64).to_bits());
        assert_eq!(decode(&reduce("int", vec![LegacyValue::Float(-0.0)])).unwrap(), LegacyValue::Int(0));
    }

    #[test]
    fn explicit_base_integer_reductions_preserve_native_grammar() {
        for (text, base, expected) in [
            ("ff", 16, 255),
            ("0x_ff", 16, 255),
            ("010", 10, 10),
            ("0o17", 0, 15),
            ("0b1_01", 2, 5),
            ("z", 36, 35),
            ("  -１２ ", 10, -12),
            ("\u{a0}٧\u{a0}", 10, 7),
        ] {
            assert_eq!(decode(&reduce("int", vec![text.into(), base.into()])), Ok(LegacyValue::Int(expected)));
        }
        assert_eq!(
            decode(&reduce("int", vec![LegacyValue::Bytes(b"ff".to_vec()), 16.into()])),
            Ok(LegacyValue::Int(255))
        );
    }

    #[test]
    fn explicit_base_integer_reductions_keep_error_priority() {
        for (args, class, message) in [
            (vec!["7".into(), 1.into()], "ValueError", "int() base must be >= 2 and <= 36, or 0"),
            (vec![7.into(), 1.into()], "ValueError", "int() base must be >= 2 and <= 36, or 0"),
            (vec![7.into(), 10.into()], "TypeError", "int() can't convert non-string with explicit base"),
            (
                vec!["7".into(), LegacyValue::Float(10.0)],
                "TypeError",
                "'float' object cannot be interpreted as an integer",
            ),
            (vec!["7".into(), 10.into(), 2.into()], "TypeError", "int expected at most 2 arguments, got 3"),
            (vec!["09".into(), 0.into()], "ValueError", "invalid literal for int() with base 0: '09'"),
            (vec!["ff\0tail".into(), 16.into()], "ValueError", "invalid literal for int() with base 16: 'ff\\x00tail'"),
        ] {
            assert_eq!(decode(&reduce("int", args)).unwrap_err().native_exception(), Some((class, message.into())));
        }
    }

    #[test]
    fn invalid_scalar_reductions_do_not_return_the_argument() {
        let cases = [
            ("int", "abc".into(), "ValueError", "invalid literal for int() with base 10: 'abc'"),
            ("int", LegacyValue::Float(f64::NAN), "ValueError", "cannot convert float NaN to integer"),
            ("int", LegacyValue::Float(f64::INFINITY), "OverflowError", "cannot convert float infinity to integer"),
            ("float", "abc".into(), "ValueError", "could not convert string to float: 'abc'"),
            (
                "float",
                LegacyValue::None,
                "TypeError",
                "float() argument must be a string or a real number, not 'NoneType'",
            ),
        ];
        for (name, value, class, message) in cases {
            let error = decode(&reduce(name, vec![value])).unwrap_err();
            assert_eq!(error.native_exception(), Some((class, message.into())), "{name}");
        }
        for name in ["bool", "float"] {
            let error = decode(&reduce(name, vec![1.into(), 2.into()])).unwrap_err();
            assert_eq!(
                error.native_exception(),
                Some(("TypeError", format!("{name} expected at most 1 argument, got 2")))
            );
        }
        let wide = decode(&reduce("int", vec!["9223372036854775808".into()])).unwrap();
        assert_eq!(wide.integer(), Some(BigInt::from(1) << 63));
        let digits = "1".repeat(4301);
        let error = decode(&reduce("int", vec![digits.into()])).unwrap_err();
        assert_eq!(error.native_exception().unwrap().0, "ValueError");
    }

    #[test]
    fn scalar_reductions_survive_the_real_dictionary_roundtrip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE fixture (key TEXT PRIMARY KEY, value BLOB)").unwrap();
        connection
            .execute(
                "INSERT INTO fixture VALUES (?, ?)",
                rusqlite::params!["converted", reduce("int", vec!["42".into()])],
            )
            .unwrap();
        drop(connection);
        assert_eq!(
            crate::import::sqlitedict::read_item(&path, "fixture", "converted").unwrap(),
            Some(LegacyValue::Int(42))
        );
    }
    #[test]
    fn allowed_containers_iterate_text_bytes_and_dictionary_keys() {
        let mut fields = Dict::new();
        fields.insert("first", 9.into());
        fields.insert("second", 8.into());
        for name in ["list", "tuple", "set"] {
            for (input, expected) in [
                (LegacyValue::Str("Aé".into()), vec!["A".into(), "é".into()]),
                (LegacyValue::Bytes(vec![0, 255]), vec![0.into(), 255.into()]),
                (LegacyValue::Dict(fields.clone()), vec!["first".into(), "second".into()]),
            ] {
                let actual = decode(&reduce(name, vec![input])).unwrap();
                let values = match actual {
                    LegacyValue::List(values) | LegacyValue::Tuple(values) | LegacyValue::Set(values) => values,
                    other => panic!("{name}: {other:?}"),
                };
                assert_eq!(values, expected, "{name}");
            }
        }
        let pairs = LegacyValue::List(vec![
            LegacyValue::Tuple(vec!["key".into(), 1.into()]),
            LegacyValue::Tuple(vec!["key".into(), 2.into()]),
            LegacyValue::List(vec!["other".into(), 3.into()]),
        ]);
        let LegacyValue::Dict(actual) = decode(&reduce("dict", vec![pairs])).unwrap() else { panic!("dict type") };
        assert_eq!(actual.str_keys(), ["key", "other"]);
        assert_eq!(actual.get("key"), Some(&2.into()));
        assert_eq!(actual.get("other"), Some(&3.into()));
    }

    #[test]
    fn container_reductions_reject_invalid_iteration_pair_lengths_and_keys() {
        let cases = [
            ("list", LegacyValue::Int(1), "TypeError", "'int' object is not iterable"),
            (
                "dict",
                LegacyValue::List(vec![1.into()]),
                "TypeError",
                "cannot convert dictionary update sequence element #0 to a sequence",
            ),
            (
                "dict",
                LegacyValue::List(vec![LegacyValue::List(vec![1.into()])]),
                "ValueError",
                "dictionary update sequence element #0 has length 1; 2 is required",
            ),
            ("set", LegacyValue::List(vec![LegacyValue::List(vec![])]), "TypeError", "unhashable type: 'list'"),
            (
                "set",
                LegacyValue::List(vec![LegacyValue::Tuple(vec![LegacyValue::Dict(Dict::new())])]),
                "TypeError",
                "unhashable type: 'dict'",
            ),
        ];
        for (name, input, class, message) in cases {
            let error = decode(&reduce(name, vec![input])).unwrap_err();
            assert_eq!(error.native_exception(), Some((class, message.into())), "{name}");
        }
        for name in ["dict", "list", "tuple", "set"] {
            let error = decode(&reduce(name, vec![LegacyValue::List(vec![]), LegacyValue::List(vec![])])).unwrap_err();
            assert_eq!(
                error.native_exception(),
                Some(("TypeError", format!("{name} expected at most 1 argument, got 2")))
            );
        }
        // Direct SETITEM and ADDITEMS use the same hashability checks.
        for bytes in [b"\x80\x04}(]K\x01u.".as_slice(), b"\x80\x04\x8f(]\x90.".as_slice()] {
            assert_eq!(
                decode(bytes).unwrap_err().native_exception(),
                Some(("TypeError", "unhashable type: 'list'".into()))
            );
        }
    }
    #[test]
    fn padded_binary_integers_keep_values_within_the_represented_width() {
        for value in [0_i64, 1, -1, i64::MIN, i64::MAX] {
            for extra in [1_usize, 9, 100] {
                let mut payload = value.to_le_bytes().to_vec();
                payload.extend(std::iter::repeat_n(if value < 0 { 0xff } else { 0 }, extra));
                let mut bytes = vec![0x80, 4, 0x8a, payload.len() as u8];
                bytes.extend_from_slice(&payload);
                bytes.push(b'.');
                assert_eq!(decode(&bytes).unwrap(), LegacyValue::Int(value), "value={value}, extra={extra}");
            }
        }
        let mut outside = vec![0x80, 4, 0x8a, 9];
        outside.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0x80, 0]);
        outside.push(b'.');
        assert_eq!(decode(&outside).unwrap().integer(), Some(BigInt::from(1) << 63));
    }
}
