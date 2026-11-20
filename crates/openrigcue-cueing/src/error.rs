//! The errors that abort one motion frame, with their log text and call trace.
//!
//! Every error carries its kind, its message and the frames it passed on its way out; a frame
//! names a stage and a line label and carries no source text. A failing stream frame is logged by
//! the motion loop as one line holding the message and the formatted trace
//! ([`CueError::refresh_motionbox_log`]); a failing test frame ends the test thread.

use std::fmt;

/// The kind of an error the motion code can fail with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exception {
    KeyError,
    IndexError,
    TypeError,
    ValueError,
    ZeroDivisionError,
    OverflowError,
    /// A general failure with only a message (an unreadable boolean text).
    Exception,
    /// Another kind, by the name the trace prints (for example a serial driver's error).
    Named(&'static str),
}

impl Exception {
    /// The name the trace prints for it.
    pub fn name(self) -> &'static str {
        match self {
            Exception::KeyError => "KeyError",
            Exception::IndexError => "IndexError",
            Exception::TypeError => "TypeError",
            Exception::ValueError => "ValueError",
            Exception::ZeroDivisionError => "ZeroDivisionError",
            Exception::OverflowError => "OverflowError",
            Exception::Exception => "Exception",
            Exception::Named(name) => name,
        }
    }
}

/// One call trace entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// The file label printed for the entry.
    pub file: &'static str,
    /// The start of the qualified name printed for the entry.
    pub prefix: &'static str,
    /// The rest of the name: the stage the entry belongs to.
    pub function: &'static str,
    pub line: u32,
}

const MOTION_HELPERS_FILE: &str = "backend/controllers/motion_helpers.py";
const FUNCTIONS_FILE: &str = "backend/functions.py";

/// A frame of a motion cueing stage.
pub(crate) const fn mh(function: &'static str, line: u32) -> Frame {
    Frame { file: MOTION_HELPERS_FILE, prefix: "backend.controllers.motion_helpers.", function, line }
}

/// A frame of the one-time motion cueing initialisation.
pub(crate) const fn mh_module(line: u32) -> Frame {
    Frame { file: MOTION_HELPERS_FILE, prefix: "init backend.controllers.motion_helpers", function: "", line }
}

/// A frame of a shared conversion helper.
pub(crate) const fn functions(function: &'static str, line: u32) -> Frame {
    Frame { file: FUNCTIONS_FILE, prefix: "backend.functions.", function, line }
}

/// The motion loop's call of the stream frame computation.
pub const REFRESH_MOTIONBOX_FRAME: Frame = Frame {
    file: "backend/controllers/controllers.py",
    prefix: "backend.controllers.controllers.",
    function: "refresh_motionbox",
    line: 418,
};

/// An error of the motion code, with its frames.
#[derive(Debug, Clone, PartialEq)]
pub struct CueError {
    exception: Exception,
    message: String,
    /// Innermost first, in the order the error passes them on its way out.
    frames: Vec<Frame>,
}

impl CueError {
    pub fn new(exception: Exception, message: impl Into<String>) -> CueError {
        CueError { exception, message: message.into(), frames: Vec::new() }
    }

    /// A missing text key: the message is the key in quotes.
    pub fn key(key: &str) -> CueError {
        CueError::new(Exception::KeyError, crate::pyvalue::repr_str(key))
    }

    pub fn type_error(message: impl Into<String>) -> CueError {
        CueError::new(Exception::TypeError, message)
    }

    pub fn value_error(message: impl Into<String>) -> CueError {
        CueError::new(Exception::ValueError, message)
    }

    pub fn overflow(message: impl Into<String>) -> CueError {
        CueError::new(Exception::OverflowError, message)
    }

    /// An index past the end of a list.
    pub fn index_out_of_range() -> CueError {
        CueError::new(Exception::IndexError, "list index out of range")
    }

    /// A division by zero: `float division by zero` when an operand is a float,
    /// `division by zero` for two integers, `complex division by zero` for a complex operand.
    pub fn zero_division(kind: DivisionKind) -> CueError {
        let message = match kind {
            DivisionKind::Int => "division by zero",
            DivisionKind::Float => "float division by zero",
            DivisionKind::Complex => "complex division by zero",
        };
        CueError::new(Exception::ZeroDivisionError, message)
    }

    /// The error passed through (or started in) `frame`.
    pub fn at(mut self, frame: Frame) -> CueError {
        self.frames.push(frame);
        self
    }

    pub fn exception(&self) -> Exception {
        self.exception
    }

    /// The message text.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The frames, outermost first (the order the trace prints them).
    pub fn frames(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter().rev()
    }

    /// The formatted call trace: a header line, one line per frame, then the kind and message.
    ///
    /// Only motion-code frames are listed; the random number source an empty effect range fails in
    /// contributes no frame.
    pub fn traceback(&self) -> String {
        let mut out = String::from("Traceback (most recent call last):\n");
        for f in self.frames() {
            out.push_str(&format!("  File \"{}\", line {}, in {}{}\n", f.file, f.line, f.prefix, f.function));
        }
        out.push_str(self.exception.name());
        if !self.message.is_empty() {
            out.push_str(": ");
            out.push_str(&self.message);
        }
        out.push('\n');
        out
    }

    /// The error line the motion loop logs for a failed stream frame.
    pub fn refresh_motionbox_log(&self) -> String {
        // Preparing the frame's arguments can fail in the motion loop itself, before
        // the stream computation is called. That frame already identifies the actual
        // failing argument line; there is no second caller frame.
        let traced = if self.frames().any(|frame| {
            frame.file == REFRESH_MOTIONBOX_FRAME.file && frame.function == REFRESH_MOTIONBOX_FRAME.function
        }) {
            self.clone()
        } else {
            self.clone().at(REFRESH_MOTIONBOX_FRAME)
        };
        format!("error sending data to motionbox: {}, traceback: {}", self.message, traced.traceback())
    }
}

/// The operand types of a failing division.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivisionKind {
    Int,
    Float,
    Complex,
}

impl fmt::Display for CueError {
    /// Writes the message text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CueError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceback_lists_the_frames_outermost_first() {
        let e = CueError::key("pitch_rate").at(mh("calculate_motor_power", 259)).at(mh("send_stream", 839));
        assert_eq!(e.to_string(), "'pitch_rate'");
        assert_eq!(
            e.refresh_motionbox_log(),
            "error sending data to motionbox: 'pitch_rate', traceback: Traceback (most recent call last):\n  \
             File \"backend/controllers/controllers.py\", line 418, in backend.controllers.controllers.refresh_motionbox\n  \
             File \"backend/controllers/motion_helpers.py\", line 839, in backend.controllers.motion_helpers.send_stream\n  \
             File \"backend/controllers/motion_helpers.py\", line 259, in \
             backend.controllers.motion_helpers.calculate_motor_power\nKeyError: 'pitch_rate'\n"
        );
    }

    #[test]
    fn zero_division_texts() {
        assert_eq!(CueError::zero_division(DivisionKind::Float).to_string(), "float division by zero");
        assert_eq!(CueError::zero_division(DivisionKind::Int).to_string(), "division by zero");
        assert_eq!(CueError::zero_division(DivisionKind::Complex).to_string(), "complex division by zero");
    }
}
