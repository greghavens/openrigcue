//! The logging configuration, in the easylogging++ configuration file format:
//! the configurations (a vector of (level, type, value) in registration order), their parser, and the
//! typed values the driver uses (integer parsing with its assertions, the
//! per-level `LOG_FLUSH_THRESHOLD`).
//!
//! Strings are byte strings; comparisons of C strings stop at the first NUL.
//! Assertion messages are appended to `err`, which goes to standard error.

/// A logging level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Level {
    Global = 1,
    Trace = 2,
    Debug = 4,
    Fatal = 8,
    Error = 16,
    Warning = 32,
    Verbose = 64,
    Info = 128,
    Unknown = 1010,
}

/// The levels in ascending bit order, from Trace to Info.
pub const LEVELS: [Level; 7] =
    [Level::Trace, Level::Debug, Level::Fatal, Level::Error, Level::Warning, Level::Verbose, Level::Info];

/// A configuration type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum ConfType {
    Enabled = 1,
    ToFile = 2,
    ToStandardOutput = 4,
    Format = 8,
    Filename = 16,
    MillisecondsWidth = 32,
    PerformanceTracking = 64,
    MaxLogFileSize = 128,
    LogFlushThreshold = 256,
    Unknown = 1010,
}

/// The built-in configuration text, parsed after the configuration file.
pub const DEFAULT_CONFIG_TEXT: &[u8] = b"* GLOBAL:\n\tFORMAT = \"[%level] %datetime{%Y-%M-%d %H:%m:%s}: %msg\"\n\tFILENAME = \"driver_motionpose.log\"\n\tENABLED = true\n\tTO_FILE = true\n\tTO_STANDARD_OUTPUT = true\n\tMAX_LOG_FILE_SIZE = 2097152 ## 2MB\n* TRACE:\n\tENABLED = true\n* DEBUG:\n\tENABLED = true\n";

/// The name of the configuration file, relative to the
/// working directory.
pub const CONFIG_FILE_NAME: &[u8] = b"logging.conf";

fn c_str(s: &[u8]) -> &[u8] {
    match s.iter().position(|&b| b == 0) {
        Some(i) => &s[..i],
        None => s,
    }
}

/// The level of a name (compared against the upper and lower case names).
pub fn level_from_string(s: &[u8]) -> Level {
    let s = c_str(s);
    let table: [(&[u8], &[u8], Level); 8] = [
        (b"GLOBAL", b"global", Level::Global),
        (b"DEBUG", b"debug", Level::Debug),
        (b"INFO", b"info", Level::Info),
        (b"WARNING", b"warning", Level::Warning),
        (b"ERROR", b"error", Level::Error),
        (b"FATAL", b"fatal", Level::Fatal),
        (b"VERBOSE", b"verbose", Level::Verbose),
        (b"TRACE", b"trace", Level::Trace),
    ];
    for (upper, lower, level) in table {
        if s == upper || s == lower {
            return level;
        }
    }
    Level::Unknown
}

/// The configuration type of a name.
pub fn conf_type_from_string(s: &[u8]) -> ConfType {
    let s = c_str(s);
    let table: [(&[u8], &[u8], ConfType); 9] = [
        (b"ENABLED", b"enabled", ConfType::Enabled),
        (b"TO_FILE", b"to_file", ConfType::ToFile),
        (b"TO_STANDARD_OUTPUT", b"to_standard_output", ConfType::ToStandardOutput),
        (b"FORMAT", b"format", ConfType::Format),
        (b"FILENAME", b"filename", ConfType::Filename),
        (b"MILLISECONDS_WIDTH", b"milliseconds_width", ConfType::MillisecondsWidth),
        (b"PERFORMANCE_TRACKING", b"performance_tracking", ConfType::PerformanceTracking),
        (b"MAX_LOG_FILE_SIZE", b"max_log_file_size", ConfType::MaxLogFileSize),
        (b"LOG_FLUSH_THRESHOLD", b"log_flush_threshold", ConfType::LogFlushThreshold),
    ];
    for (upper, lower, ty) in table {
        if s == upper || s == lower {
            return ty;
        }
    }
    ConfType::Unknown
}

/// White space of the "C" locale: HT, LF, VT, FF, CR, space.
fn is_space(c: u8) -> bool {
    matches!(c, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ')
}

/// Trims white space at the end, then at the start.
pub fn trim(s: &[u8]) -> Vec<u8> {
    let end = s.iter().rposition(|&c| !is_space(c)).map_or(0, |i| i + 1);
    let s = &s[..end];
    let start = s.iter().position(|&c| !is_space(c)).unwrap_or(s.len());
    s[start..].to_vec()
}

/// Upper case in the "C" locale: ASCII letters only.
fn to_upper(s: &[u8]) -> Vec<u8> {
    s.iter().map(|c| c.to_ascii_uppercase()).collect()
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// One configuration: level, type and value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    pub level: Level,
    pub ty: ConfType,
    pub value: Vec<u8>,
}

/// The configurations, in registration order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Configurations {
    pub list: Vec<Configuration>,
}

/// The output of a failed assertion: `ASSERTION FAILURE FROM EASYLOGGING++ (LINE: n) [expr] WITH MESSAGE "msg"` and a newline.
pub fn assert_failure(err: &mut Vec<u8>, line: u32, expr: &str, msg: &[u8]) {
    err.extend_from_slice(b"ASSERTION FAILURE FROM EASYLOGGING++ (LINE: ");
    err.extend_from_slice(line.to_string().as_bytes());
    err.extend_from_slice(b") [");
    err.extend_from_slice(expr.as_bytes());
    err.extend_from_slice(b"] WITH MESSAGE \"");
    err.extend_from_slice(msg);
    err.extend_from_slice(b"\"\n");
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

impl Configurations {
    pub fn new() -> Self {
        Self::default()
    }

    fn index(&self, level: Level, ty: ConfType) -> Option<usize> {
        self.list.iter().position(|c| c.level == level && c.ty == ty)
    }

    pub fn get(&self, level: Level, ty: ConfType) -> Option<&[u8]> {
        self.index(level, ty).map(|i| self.list[i].value.as_slice())
    }

    /// Sets one value for a level, without the global handling.
    fn unsafe_set(&mut self, level: Level, ty: ConfType, value: &[u8]) {
        match self.index(level, ty) {
            None => self.list.push(Configuration { level, ty, value: value.to_vec() }),
            Some(i) => self.list[i].value = value.to_vec(),
        }
        if level == Level::Global {
            self.unsafe_set_globally(ty, value, false);
        }
    }

    /// Sets one value for every level, optionally including the global level.
    fn unsafe_set_globally(&mut self, ty: ConfType, value: &[u8], include_global: bool) {
        if include_global {
            self.unsafe_set(Level::Global, ty, value);
        }
        for level in LEVELS {
            self.unsafe_set(level, ty, value);
        }
    }

    /// Sets a value for a level; the global level sets it for every level.
    pub fn set(&mut self, level: Level, ty: ConfType, value: &[u8]) {
        self.unsafe_set(level, ty, value);
        if level == Level::Global {
            self.unsafe_set_globally(ty, value, false);
        }
    }

    /// Sets a value for every level, optionally including the global level.
    pub fn set_globally(&mut self, ty: ConfType, value: &[u8], include_global: bool) {
        if include_global {
            self.set(Level::Global, ty, value);
        }
        for level in LEVELS {
            self.set(level, ty, value);
        }
    }

    fn unsafe_set_if_not_exist(&mut self, level: Level, ty: ConfType, value: &[u8]) {
        if self.index(level, ty).is_none() {
            self.unsafe_set(level, ty, value);
        }
    }

    /// Sets every value to its default (there is no default FILENAME).
    pub fn set_to_default(&mut self) {
        self.set_globally(ConfType::Enabled, b"true", true);
        self.set_globally(ConfType::ToFile, b"true", true);
        self.set_globally(ConfType::ToStandardOutput, b"true", true);
        self.set_globally(ConfType::MillisecondsWidth, b"3", true);
        self.set_globally(ConfType::PerformanceTracking, b"true", true);
        self.set_globally(ConfType::MaxLogFileSize, b"0", true);
        self.set_globally(ConfType::LogFlushThreshold, b"0", true);
        self.set_globally(ConfType::Format, b"%datetime %level [%logger] %msg", true);
        self.set(Level::Debug, ConfType::Format, b"%datetime %level [%logger] [%user@%host] [%func] [%loc] %msg");
        self.set(Level::Error, ConfType::Format, b"%datetime %level [%logger] %msg");
        self.set(Level::Fatal, ConfType::Format, b"%datetime %level [%logger] %msg");
        self.set(Level::Verbose, ConfType::Format, b"%datetime %level-%vlevel [%logger] %msg");
        self.set(Level::Trace, ConfType::Format, b"%datetime %level [%logger] [%func] [%loc] %msg");
    }

    /// Sets the values that are not yet set to their defaults.
    pub fn set_remaining_to_default(&mut self) {
        self.unsafe_set_if_not_exist(Level::Global, ConfType::Enabled, b"true");
        self.unsafe_set_if_not_exist(Level::Global, ConfType::ToFile, b"true");
        self.unsafe_set_if_not_exist(Level::Global, ConfType::ToStandardOutput, b"true");
        self.unsafe_set_if_not_exist(Level::Global, ConfType::MillisecondsWidth, b"3");
        self.unsafe_set_if_not_exist(Level::Global, ConfType::PerformanceTracking, b"true");
        self.unsafe_set_if_not_exist(Level::Global, ConfType::MaxLogFileSize, b"0");
        self.unsafe_set_if_not_exist(Level::Global, ConfType::Format, b"%datetime %level [%logger] %msg");
        self.unsafe_set_if_not_exist(
            Level::Debug,
            ConfType::Format,
            b"%datetime %level [%logger] [%user@%host] [%func] [%loc] %msg",
        );
        self.unsafe_set_if_not_exist(Level::Error, ConfType::Format, b"%datetime %level [%logger] %msg");
        self.unsafe_set_if_not_exist(Level::Fatal, ConfType::Format, b"%datetime %level [%logger] %msg");
        self.unsafe_set_if_not_exist(Level::Verbose, ConfType::Format, b"%datetime %level-%vlevel [%logger] %msg");
        self.unsafe_set_if_not_exist(Level::Trace, ConfType::Format, b"%datetime %level [%logger] [%func] [%loc] %msg");
    }

    /// Sets every value of `base`, in its order.
    pub fn set_from_base(&mut self, base: &Configurations) {
        for c in base.list.clone() {
            self.set(c.level, c.ty, &c.value);
        }
    }

    /// The configurations of a configuration file, with the remaining values at their defaults; the file is parsed only when it exists.
    /// `read_file` returns `None` when the path does not exist, `Some(None)` when it exists but cannot be opened
    /// for reading as a file, `Some(Some(bytes))` with the bytes otherwise.
    pub fn from_file(path: &[u8], read_file: Option<Option<Vec<u8>>>, err: &mut Vec<u8>) -> Self {
        let mut conf = Configurations::new();
        match read_file {
            None => assert_failure(
                err,
                2517,
                "(assertionPassed = base::utils::File::pathExists(configurationFile.c_str(), true))",
                &cat(&[b"Configuration file [", path, b"] does not exist!"]),
            ),
            Some(contents) => {
                parse_from_file(path, contents, &mut conf, err);
            }
        }
        conf.set_remaining_to_default();
        conf
    }
}

/// The bytes of a file read in text mode: reading stops at Ctrl-Z, CR LF becomes LF.
pub fn crt_text_read(raw: &[u8]) -> Vec<u8> {
    let raw = match raw.iter().position(|&b| b == 0x1a) {
        Some(i) => &raw[..i],
        None => raw,
    };
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\r' && i + 1 < raw.len() && raw[i + 1] == b'\n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    out
}

/// Parses a configuration file: `contents` is `None` when the file does not open.
pub fn parse_from_file(path: &[u8], contents: Option<Vec<u8>>, sender: &mut Configurations, err: &mut Vec<u8>) -> bool {
    let Some(raw) = contents else {
        assert_failure(
            err,
            2707,
            "fileStream_.is_open()",
            &cat(&[b"Unable to open configuration file [", path, b"] for parsing."]),
        );
        return false;
    };
    let text = crt_text_read(&raw);
    let mut parsed = false;
    let mut curr_level = Level::Unknown;
    let mut curr_config_str: Vec<u8> = Vec::new();
    let mut curr_level_str: Vec<u8> = Vec::new();
    // Lines are read while the stream is good.
    let mut pos = 0usize;
    let mut good = true;
    while good {
        let mut line = match text[pos..].iter().position(|&b| b == b'\n') {
            Some(i) => {
                let l = text[pos..pos + i].to_vec();
                pos += i + 1;
                l
            }
            None => {
                // End of file reached while reading this line: eofbit (and failbit when nothing was read).
                let l = text[pos..].to_vec();
                pos = text.len();
                good = false;
                l
            }
        };
        let addr = &curr_config_str as *const Vec<u8> as usize;
        parsed = parse_line(&mut line, &mut curr_config_str, &mut curr_level_str, &mut curr_level, sender, err, addr);
        if !parsed {
            assert_failure(err, 2716, "parsedSuccessfully", &cat(&[b"Unable to parse configuration line: ", &line]));
        }
    }
    parsed
}

/// Parses configuration text.
pub fn parse_from_text(text: &[u8], sender: &mut Configurations, err: &mut Vec<u8>) -> bool {
    let mut parsed = false;
    let mut curr_level = Level::Unknown;
    let mut curr_config_str: Vec<u8> = Vec::new();
    let mut curr_level_str: Vec<u8> = Vec::new();
    // Line by line: a final segment without '\n' is still delivered; an empty tail is not.
    let mut pos = 0usize;
    while pos < text.len() {
        let mut line = match text[pos..].iter().position(|&b| b == b'\n') {
            Some(i) => {
                let l = text[pos..pos + i].to_vec();
                pos += i + 1;
                l
            }
            None => {
                let l = text[pos..].to_vec();
                pos = text.len();
                l
            }
        };
        let addr = &curr_config_str as *const Vec<u8> as usize;
        parsed = parse_line(&mut line, &mut curr_config_str, &mut curr_level_str, &mut curr_level, sender, err, addr);
        if !parsed {
            assert_failure(err, 2741, "parsedSuccessfully", &cat(&[b"Unable to parse configuration line: ", &line]));
        }
    }
    parsed
}

/// Removes the comment of a line.
fn ignore_comments(line: &mut Vec<u8>) {
    let quotes_start = find(line, b"\"", 0);
    let mut quotes_end: Option<usize> = None;
    if let Some(qs) = quotes_start {
        quotes_end = find(line, b"\"", qs + 1);
        while let Some(qe) = quotes_end {
            if line[qe - 1] != b'\\' {
                break;
            }
            quotes_end = find(line, b"\"", qe + 2);
        }
    }
    if let Some(first) = find(line, b"##", 0) {
        let mut found_at = Some(first);
        // The comment lies before the closing quote; "not found" counts as the largest position.
        let qe = quotes_end.unwrap_or(usize::MAX);
        if first < qe {
            // The search goes on after the closing quote; "not found" plus one wraps to 0.
            found_at = find(line, b"##", qe.wrapping_add(1));
        }
        // The line is cut at the comment; "not found" keeps the whole line.
        if let Some(f) = found_at {
            line.truncate(f);
        }
    }
}

/// Parses one line.
fn parse_line(
    line: &mut Vec<u8>,
    curr_config_str: &mut Vec<u8>,
    curr_level_str: &mut Vec<u8>,
    curr_level: &mut Level,
    conf: &mut Configurations,
    err: &mut Vec<u8>,
    curr_config_str_addr: usize,
) -> bool {
    let mut curr_config = ConfType::Unknown;
    let mut curr_value: Vec<u8> = Vec::new();
    *line = trim(line);
    if line.starts_with(b"##") {
        return true;
    }
    ignore_comments(line);
    *line = trim(line);
    if line.is_empty() {
        return true;
    }
    if line.starts_with(b"*") {
        if line.len() <= 2 {
            return true;
        }
        *curr_level_str = line[1..line.len() - 1].to_vec();
        *curr_level_str = to_upper(curr_level_str);
        *curr_level_str = trim(curr_level_str);
        *curr_level = level_from_string(curr_level_str);
        return true;
    }
    let assignment = line.iter().position(|&b| b == b'=');
    let is_config = !line.is_empty() && line[0].is_ascii_alphabetic() && assignment.is_some();
    if is_config {
        let assignment = assignment.unwrap_or(0);
        *curr_config_str = line[..assignment].to_vec();
        *curr_config_str = to_upper(curr_config_str);
        *curr_config_str = trim(curr_config_str);
        curr_config = conf_type_from_string(curr_config_str);
        curr_value = trim(&line[assignment + 1..]);
        let quotes_start = find(&curr_value, b"\"", 0);
        let mut quotes_end: Option<usize> = None;
        if let Some(qs) = quotes_start {
            quotes_end = find(&curr_value, b"\"", qs + 1);
            while let Some(qe) = quotes_end {
                if curr_value[qe - 1] != b'\\' {
                    break;
                }
                curr_value.remove(qe - 1);
                quotes_end = find(&curr_value, b"\"", qe + 2);
            }
        }
        if let (Some(qs), Some(qe)) = (quotes_start, quotes_end) {
            if qs + 1 == qe {
                let addr = format!("{curr_config_str_addr:016X}");
                assert_failure(
                    err,
                    2824,
                    "(quotesStart + 1 != quotesEnd)",
                    &cat(&[b"Empty configuration value for [", addr.as_bytes(), b"]"]),
                );
            }
            if qs != qe && qs + 1 != qe {
                // The value from after the opening quote, with a length of the closing quote's position minus one.
                let start = qs + 1;
                let len = (qe - 1).min(curr_value.len() - start);
                curr_value = curr_value[start..start + len].to_vec();
            }
        }
    }
    if *curr_level == Level::Unknown {
        assert_failure(
            err,
            2831,
            "*currLevel != Level::Unknown",
            &cat(&[b"Unrecognized severity level [", curr_level_str, b"]"]),
        );
    }
    if curr_config == ConfType::Unknown {
        assert_failure(
            err,
            2832,
            "currConfig != ConfigurationType::Unknown",
            &cat(&[b"Unrecognized configuration [", curr_config_str, b"]"]),
        );
    }
    if *curr_level == Level::Unknown || curr_config == ConfType::Unknown {
        return false;
    }
    conf.set(*curr_level, curr_config, &curr_value);
    true
}

/// An unsigned integer value: the saturating 32-bit signed value of an all-digit
/// text, else the assertion and 0.
pub fn get_ulong(value: &[u8], err: &mut Vec<u8>) -> u64 {
    let v = trim(value);
    let valid = !v.is_empty() && v.iter().all(|c| c.is_ascii_digit());
    if !valid {
        assert_failure(err, 3115, "valid", &cat(&[b"Configuration value not a valid integer [", &v, b"]"]));
        return 0;
    }
    let mut n: i64 = 0;
    for &c in c_str(&v) {
        n = n * 10 + i64::from(c - b'0');
        if n > i64::from(i32::MAX) {
            n = i64::from(i32::MAX);
            break;
        }
    }
    // The signed 32-bit value converted to unsigned 32 bits and then widened.
    n as u64
}

/// The typed values of one logger that the driver uses: the flush threshold per level.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Typed {
    /// The flush threshold of each level.
    pub log_flush_threshold: Vec<(Level, u64)>,
}

impl Typed {
    /// The flush threshold of a level: the level's entry, else Level::Global's.
    pub fn log_flush_threshold(&self, level: Level) -> u64 {
        let get = |l: Level| self.log_flush_threshold.iter().find(|(k, _)| *k == l).map(|(_, v)| *v);
        get(level).or_else(|| get(Level::Global)).unwrap_or(0)
    }
}

/// Sets the typed value of a level.
fn set_value(map: &mut Vec<(Level, u64)>, level: Level, value: u64) {
    if map.is_empty() {
        map.push((Level::Global, value));
        return;
    }
    if map.iter().any(|(k, v)| *k == Level::Global && *v == value) {
        return;
    }
    match map.iter_mut().find(|(k, _)| *k == level) {
        Some(entry) => entry.1 = value,
        None => map.push((level, value)),
    }
}

/// Builds the typed values of the integer-valued types (MILLISECONDS_WIDTH, MAX_LOG_FILE_SIZE,
/// LOG_FLUSH_THRESHOLD, parsed in vector order); the other types reach the log only
/// through values the default text forces for every level (see `effective_constants_are_forced`).
pub fn build_typed(confs: &Configurations, err: &mut Vec<u8>) -> Typed {
    let mut typed = Typed::default();
    for c in &confs.list {
        match c.ty {
            ConfType::MillisecondsWidth | ConfType::MaxLogFileSize => {
                get_ulong(&c.value, err);
            }
            ConfType::LogFlushThreshold => {
                let v = get_ulong(&c.value, err);
                set_value(&mut typed.log_flush_threshold, c.level, v);
            }
            _ => {}
        }
    }
    typed
}

/// A logger's configurations before any configuration is applied: the defaults
/// (the performance logger only changes FORMAT values in place).
pub fn logger_initial_configurations() -> Configurations {
    let mut c = Configurations::new();
    c.set_to_default();
    c
}

/// The logging set-up up to the reconfiguration: returns the configurations the
/// loggers are then reconfigured with.
pub fn init_logging_configurations(read_file: Option<Option<Vec<u8>>>, err: &mut Vec<u8>) -> Configurations {
    let mut conf = Configurations::from_file(CONFIG_FILE_NAME, read_file, err);
    parse_from_text(DEFAULT_CONFIG_TEXT, &mut conf, err);
    conf.set_remaining_to_default();
    conf
}

/// Reconfigures the registered loggers ("default", "performance", in name
/// order): each one copies `conf` into its own configurations and rebuilds its
/// typed values. Returns the "default" logger's typed values.
pub fn reconfigure_all_loggers(conf: &Configurations, err: &mut Vec<u8>) -> Typed {
    let mut default_typed = Typed::default();
    for (i, _id) in ["default", "performance"].iter().enumerate() {
        let mut own = logger_initial_configurations();
        own.set_from_base(conf);
        let typed = build_typed(&own, err);
        if i == 0 {
            default_typed = typed;
        }
    }
    default_typed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(file: Option<Option<&[u8]>>) -> (Vec<u8>, Typed, Configurations) {
        let mut err = Vec::new();
        let conf = init_logging_configurations(file.map(|f| f.map(|b| b.to_vec())), &mut err);
        let typed = reconfigure_all_loggers(&conf, &mut err);
        (err, typed, conf)
    }

    fn harness_file(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tools/harness/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn flush_thresholds_follow_the_file() {
        assert_eq!(run(Some(Some(b"* GLOBAL:\n\tLOG_FLUSH_THRESHOLD = 2\n"))).1.log_flush_threshold(Level::Trace), 2);
        let probe = run(Some(Some(&harness_file("conf-probe.conf")))).1;
        assert_eq!(probe.log_flush_threshold(Level::Info), 8);
        let bad = run(Some(Some(&harness_file("conf-bad.conf")))).1;
        assert_eq!(bad.log_flush_threshold(Level::Debug), 0);
        assert_eq!(bad.log_flush_threshold(Level::Warning), 3);
        assert_eq!(bad.log_flush_threshold(Level::Info), 0);
        let only_debug = run(Some(Some(b"* DEBUG:\n LOG_FLUSH_THRESHOLD = 5\n"))).1;
        assert_eq!(only_debug.log_flush_threshold(Level::Debug), 5);
        assert_eq!(only_debug.log_flush_threshold(Level::Info), 0);
    }

    #[test]
    fn set_from_base_quirk_global_after_level() {
        // Debug registered before Global in the base, then changed again: the logger copy ends with Global's value.
        let (_, typed, conf) =
            run(Some(Some(b"* DEBUG:\nLOG_FLUSH_THRESHOLD = 3\n* GLOBAL:\nLOG_FLUSH_THRESHOLD = 5\n* DEBUG:\nLOG_FLUSH_THRESHOLD = 4\n")));
        assert_eq!(conf.get(Level::Debug, ConfType::LogFlushThreshold), Some(&b"4"[..]));
        // The logger copies the base in its order: Debug = 4 first, then Global = 5 sets every level again.
        assert_eq!(typed.log_flush_threshold(Level::Debug), 5);
    }

    #[test]
    fn effective_constants_are_forced() {
        // Whatever the file says, the default text sets these globally after it, for every level.
        let (_, _, conf) = run(Some(Some(&harness_file("conf-bad.conf"))));
        for level in std::iter::once(Level::Global).chain(LEVELS) {
            assert_eq!(conf.get(level, ConfType::Enabled), Some(&b"true"[..]));
            assert_eq!(conf.get(level, ConfType::ToFile), Some(&b"true"[..]));
            assert_eq!(conf.get(level, ConfType::ToStandardOutput), Some(&b"true"[..]));
            assert_eq!(conf.get(level, ConfType::Filename), Some(&b"driver_motionpose.log"[..]));
            assert_eq!(conf.get(level, ConfType::MaxLogFileSize), Some(&b"2097152"[..]));
            assert_eq!(conf.get(level, ConfType::Format), Some(&b"[%level] %datetime{%Y-%M-%d %H:%m:%s}: %msg"[..]));
        }
    }

    #[test]
    fn get_ulong_matches_msvc_atol() {
        let mut err = Vec::new();
        assert_eq!(get_ulong(b" 42 ", &mut err), 42);
        assert_eq!(get_ulong(b"99999999999", &mut err), 2_147_483_647);
        assert!(err.is_empty());
        assert_eq!(get_ulong(b"", &mut err), 0);
        assert_eq!(err, b"ASSERTION FAILURE FROM EASYLOGGING++ (LINE: 3115) [valid] WITH MESSAGE \"Configuration value not a valid integer []\"\n");
    }
}
