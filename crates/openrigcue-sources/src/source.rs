use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;

/// Errors a source can report. A source that cannot reach its game simply reports "not running".
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed data: {0}")]
    Malformed(String),
}

/// A typed getter distinguishes a failed read from a source that still uses the numeric adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeRead {
    Value(LegacyValue),
    Raised,
    Unsupported,
}

/// One game's telemetry listener.
///
/// The dispatcher calls [`Source::is_running`] before every [`Source::receive_native`] (data is received only from a live source,
/// both while it looks for a game and once per tick while the game is active), and [`Source::stop`] when the
/// game goes away.
pub trait Source: Send {
    /// Stable identifier, equal to the listener key of the game catalog (for example `"dirt"` or `"xplane"`).
    fn id(&self) -> &'static str;

    /// Liveness test: whether the game is currently sending data.
    fn is_running(&mut self) -> Result<bool, SourceError>;

    /// Read the newest data into `telemetry`. Returns `Ok(false)` when nothing new arrived this tick;
    /// channels not provided by the game are left unchanged.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError>;

    /// The dispatcher's receive call. Sources with typed getters can update their input state here without
    /// replaying getters into the older numeric compatibility frame.
    fn receive_native(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        self.poll(telemetry)
    }

    /// The typed value of a channel, for sources that deliver exact value kinds. `None` uses the
    /// numeric frame adapter; `Some(LegacyValue::None)` is a present none value.
    fn native_channel(&self, _name: &str) -> Option<LegacyValue> {
        None
    }

    /// A typed channel read failed. Its numeric compatibility frame may still hold an older
    /// answer; the dispatcher must handle the failed read instead of consuming that value.
    fn channel_read_raised(&self, _name: &str) -> bool {
        false
    }

    /// Read one typed getter once. Stateful sources can override this instead of separate value/error probes.
    fn read_native(&self, name: &str) -> NativeRead {
        if self.channel_read_raised(name) {
            NativeRead::Raised
        } else {
            self.native_channel(name).map_or(NativeRead::Unsupported, NativeRead::Value)
        }
    }

    /// Release sockets or mappings.
    fn stop(&mut self) {}
}
