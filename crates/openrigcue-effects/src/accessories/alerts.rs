//! Shift-light beep and the IntelliTune voice prompts.

/// What the beep needs each shaker cycle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeepInputs {
    /// A telemetry listener is running.
    pub listener_running: bool,
    /// The `APP.shift_sound` switch.
    pub shift_sound: bool,
    /// The shift-light indicator value.
    pub shift_light: f64,
}

/// What to do with the looping beep sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeepCommand {
    /// Start playing `sounds/transmission/2.wav` in a loop.
    StartLoop,
    Stop,
}

/// Plays the beep on the default audio device.
pub trait BeepPlayer {
    fn apply(&mut self, command: BeepCommand);
}

/// Beep state: the previous shift-light value.
#[derive(Debug, Default, Clone)]
pub struct ShiftBeep {
    previous_shift_light: f64,
}

impl ShiftBeep {
    pub fn new() -> Self {
        Self::default()
    }

    /// The beep command of this cycle, if any.
    ///
    /// With a game running and the shift sound
    /// enabled, the loop starts on the rising edge of the shift light and stops while it is off; in
    /// every other case the sound is stopped.
    pub fn update(&mut self, inputs: &BeepInputs) -> Option<BeepCommand> {
        if !(inputs.listener_running && inputs.shift_sound) {
            return Some(BeepCommand::Stop);
        }
        let command = if inputs.shift_light == 1.0 && self.previous_shift_light == 0.0 {
            Some(BeepCommand::StartLoop)
        } else if inputs.shift_light == 0.0 {
            Some(BeepCommand::Stop)
        } else {
            None
        };
        self.previous_shift_light = inputs.shift_light;
        command
    }
}

/// A pre-recorded prompt from the user's SRS sound files (`sounds/voices/<name>.mp3`), played by the
/// IntelliTune wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VoiceClip {
    /// `0.mp3` … `10.mp3`; the wizard's start countdown plays 10 down to 1.
    Number(u8),
    /// `intellitune_welcome.mp3`, played when the wizard opens.
    IntellituneWelcome,
    /// `intellitune_starting.mp3`, played to the end before the countdown.
    IntellituneStarting,
    /// `intellitune_captured.mp3`, played after the capture.
    IntellituneCaptured,
}

impl VoiceClip {
    /// Every clip expected in `sounds/voices`.
    pub const ALL: [VoiceClip; 14] = [
        VoiceClip::Number(0),
        VoiceClip::Number(1),
        VoiceClip::Number(2),
        VoiceClip::Number(3),
        VoiceClip::Number(4),
        VoiceClip::Number(5),
        VoiceClip::Number(6),
        VoiceClip::Number(7),
        VoiceClip::Number(8),
        VoiceClip::Number(9),
        VoiceClip::Number(10),
        VoiceClip::IntellituneWelcome,
        VoiceClip::IntellituneStarting,
        VoiceClip::IntellituneCaptured,
    ];

    /// The clip's name without extension (the number or `intellitune_*`).
    pub fn name(self) -> String {
        match self {
            VoiceClip::Number(n) => n.to_string(),
            VoiceClip::IntellituneWelcome => "intellitune_welcome".into(),
            VoiceClip::IntellituneStarting => "intellitune_starting".into(),
            VoiceClip::IntellituneCaptured => "intellitune_captured".into(),
        }
    }

    /// The file under `sounds/voices` (`<number>.mp3`, `intellitune_*.mp3`).
    pub fn file_name(self) -> String {
        format!("{}.mp3", self.name())
    }

    /// The clip named `name` (as [`VoiceClip::name`] gives it), if there is one.
    pub fn from_name(name: &str) -> Option<VoiceClip> {
        VoiceClip::ALL.into_iter().find(|clip| clip.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(shift_light: f64) -> BeepInputs {
        BeepInputs { listener_running: true, shift_sound: true, shift_light }
    }

    /// Rising edge starts the loop, off stops it, staying on does nothing.
    #[test]
    fn beep_edges() {
        let mut beep = ShiftBeep::new();
        assert_eq!(beep.update(&inputs(0.0)), Some(BeepCommand::Stop));
        assert_eq!(beep.update(&inputs(1.0)), Some(BeepCommand::StartLoop));
        assert_eq!(beep.update(&inputs(1.0)), None);
        assert_eq!(beep.update(&inputs(0.0)), Some(BeepCommand::Stop));
        let mut off = inputs(1.0);
        off.shift_sound = false;
        assert_eq!(beep.update(&off), Some(BeepCommand::Stop));
    }

    /// There is no cycle limit: the beep still starts after 9000 game
    /// cycles; no game running stops it.
    #[test]
    fn beep_has_no_cycle_limit() {
        let mut beep = ShiftBeep::new();
        let mut i = inputs(0.0);
        for _ in 0..9000 {
            beep.update(&i);
        }
        i.shift_light = 1.0;
        assert_eq!(beep.update(&i), Some(BeepCommand::StartLoop));
        i.listener_running = false;
        assert_eq!(beep.update(&i), Some(BeepCommand::Stop));
    }

    /// The wizard clips: `sounds/voices/<number>.mp3` and the
    /// three `intellitune_*.mp3` prompts.
    #[test]
    fn voice_clip_names() {
        assert_eq!(VoiceClip::Number(10).file_name(), "10.mp3");
        assert_eq!(VoiceClip::Number(0).file_name(), "0.mp3");
        assert_eq!(VoiceClip::IntellituneStarting.file_name(), "intellitune_starting.mp3");
        assert_eq!(VoiceClip::from_name("intellitune_captured"), Some(VoiceClip::IntellituneCaptured));
        assert_eq!(VoiceClip::from_name("7"), Some(VoiceClip::Number(7)));
        assert_eq!(VoiceClip::from_name("11"), None);
        assert_eq!(VoiceClip::from_name("intellitune_welcome.mp3"), None);
    }
}
