//! SMC3 setup/monitor protocol, sharing OpenRigCue's serial transport and frame encoder.
use openrigcue_devices::motion::smc3::{wire, BAUD_RATE};
use openrigcue_devices::serial::{Link, OpenOptions, SerialLink};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Device(String),
    #[error("Timed out reading controller parameter {0}")]
    Timeout(char),
    #[error("{0}")]
    Invalid(String),
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MotorSettings {
    pub kp: u16,
    pub ki: u16,
    pub kd: u16,
    pub ks: u16,
    pub pwm_min: u8,
    pub pwm_max: u8,
    pub pwm_reverse: u8,
    pub limit: u8,
    pub clip: u8,
    pub deadzone: u8,
}
impl MotorSettings {
    pub fn validate(&self) -> Result<()> {
        if [self.kp, self.ki, self.kd].into_iter().any(|v| v > 1000) || self.ks == 0 || self.ks > 1000 {
            return Err(Error::Invalid("PID gains must be 0–1000 and Ks must be 1–1000".into()));
        }
        if self.pwm_min > self.pwm_max {
            return Err(Error::Invalid("PWM minimum exceeds maximum".into()));
        }
        if self.limit > self.clip {
            return Err(Error::Invalid("Motor limit exceeds input clip".into()));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub version: u16,
    pub resolution: u8,
    pub motors: Vec<MotorSettings>,
    pub pid_divider: u8,
    pub pwm_frequency_12: u8,
    pub pwm_frequency_3: u8,
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.motors.len() != 3 {
            return Err(Error::Invalid("Select three motor configurations".into()));
        }
        if !(1..=10).contains(&self.pid_divider) {
            return Err(Error::Invalid("PID divider must be 1–10".into()));
        }
        if !(1..=35).contains(&self.pwm_frequency_12) || ![4, 31].contains(&self.pwm_frequency_3) {
            return Err(Error::Invalid(
                "PWM frequencies must be 1–35 kHz for motors 1/2 and 4 or 31 kHz for motor 3".into(),
            ));
        }
        for motor in &self.motors {
            motor.validate()?;
        }
        Ok(())
    }
    pub fn to_legacy(&self) -> String {
        let mut text = format!("SMC3_Version={}\n", self.version);
        for (i, m) in self.motors.iter().enumerate() {
            for (key, value) in [
                ("Kp", m.kp),
                ("Ki", m.ki),
                ("Kd", m.kd),
                ("Ks", m.ks),
                ("PWMmin", m.pwm_min.into()),
                ("PWMmax", m.pwm_max.into()),
                ("PWMrev", m.pwm_reverse.into()),
                ("Limit", m.limit.into()),
                ("Clip", m.clip.into()),
                ("Deadzone", m.deadzone.into()),
            ] {
                text.push_str(&format!("Motor{}_{key}={value}\n", i + 1));
            }
        }
        text.push_str(&format!(
            "PID_Divider={}\nMotor1and2PWMFreqkHz={}\nMotor3PWMFreqkHz={}\n",
            self.pid_divider, self.pwm_frequency_12, self.pwm_frequency_3
        ));
        text
    }
    pub fn from_legacy(text: &str) -> Result<Self> {
        let mut values = std::collections::BTreeMap::new();
        for line in text.lines().map(str::trim).filter(|s| !s.is_empty() && !s.starts_with('#')) {
            let (k, v) =
                line.split_once('=').ok_or_else(|| Error::Invalid(format!("Invalid settings line: {line}")))?;
            values
                .insert(k.trim(), v.trim().parse::<u16>().map_err(|_| Error::Invalid(format!("Invalid setting {k}")))?);
        }
        let get = |k: &str| values.get(k).copied().ok_or_else(|| Error::Invalid(format!("Missing setting {k}")));
        let small = |k: &str| u8::try_from(get(k)?).map_err(|_| Error::Invalid(format!("Setting exceeds 255: {k}")));
        let mut motors = Vec::new();
        for i in 1..=3 {
            let k = |s: &str| format!("Motor{i}_{s}");
            motors.push(MotorSettings {
                kp: get(&k("Kp"))?,
                ki: get(&k("Ki"))?,
                kd: get(&k("Kd"))?,
                ks: get(&k("Ks"))?,
                pwm_min: small(&k("PWMmin"))?,
                pwm_max: small(&k("PWMmax"))?,
                pwm_reverse: small(&k("PWMrev"))?,
                limit: small(&k("Limit"))?,
                clip: small(&k("Clip"))?,
                deadzone: small(&k("Deadzone"))?,
            });
        }
        let result = Self {
            version: get("SMC3_Version")?,
            resolution: 10,
            motors,
            pid_divider: small("PID_Divider")?,
            pwm_frequency_12: small("Motor1and2PWMFreqkHz")?,
            pwm_frequency_3: small("Motor3PWMFreqkHz")?,
        };
        result.validate()?;
        Ok(result)
    }
}

#[derive(Default)]
pub struct Decoder {
    bytes: VecDeque<u8>,
}
impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<(u8, [u8; 2])> {
        self.bytes.extend(bytes);
        let mut frames = Vec::new();
        while self.bytes.len() >= 5 {
            if self.bytes[0] != b'[' || self.bytes[4] != b']' {
                self.bytes.pop_front();
                continue;
            }
            frames.push((self.bytes[1], [self.bytes[2], self.bytes[3]]));
            self.bytes.drain(..5);
        }
        frames
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Monitor,
    Manual,
    Sine,
    Triangle,
    Square,
    Motion,
    Udp,
}
#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub seconds: f64,
    pub motor: u8,
    pub target: u16,
    pub feedback: u16,
    pub pwm: u8,
    pub enabled: bool,
    pub calculations: u16,
}

pub struct Session {
    link: Box<dyn Link>,
    decoder: Decoder,
    pub settings: Settings,
    pub mode: Mode,
    pub motor: u8,
    pub center: u16,
    pub amplitude: u16,
    pub period: f64,
    started: Instant,
    last_emit: Instant,
    pwm: [u8; 3],
    disabled: u8,
    calculations: u16,
    pub samples: VecDeque<Sample>,
    last_poll: Instant,
    udp: Option<std::net::UdpSocket>,
    udp_decoder: Decoder,
    pub udp_packets: u64,
    pub udp_port: Option<u16>,
}
impl Session {
    pub fn connect(port: &str) -> Result<Self> {
        let link =
            SerialLink::open(port, &OpenOptions { baud_rate: BAUD_RATE, write_timeout: Some(Duration::from_secs(2)) })
                .map_err(|e| Error::Device(e.to_string()))?;
        std::thread::sleep(Duration::from_millis(1500));
        Self::with_link(Box::new(link))
    }
    pub fn with_link(mut link: Box<dyn Link>) -> Result<Self> {
        link.clear_input().map_err(|e| Error::Device(e.to_string()))?;
        link.write_all(b"[ver]").map_err(|e| Error::Device(e.to_string()))?;
        let start = Instant::now();
        let mut response = Vec::new();
        while start.elapsed() < Duration::from_secs(2) {
            response.extend(link.read_available().map_err(|e| Error::Device(e.to_string()))?);
            if response.iter().filter(|&&b| b == b']').count() > 0 {
                std::thread::sleep(Duration::from_millis(50));
                response.extend(link.read_available().map_err(|e| Error::Device(e.to_string()))?);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let frame = wire::find_reply(&response, Some(b'v')).ok_or(Error::Timeout('v'))?;
        let resolution = wire::reply_resolution(frame).0 as u8;
        let version = wire::decode_version(frame).version;
        let version =
            if version < 0 && frame.len() == 5 { u16::from_be_bytes([frame[2], frame[3]]) as i64 } else { version };
        if version < 0 {
            return Err(Error::Invalid("Controller returned an unsupported SMC3 version".into()));
        }
        let blank = MotorSettings {
            kp: 0,
            ki: 0,
            kd: 0,
            ks: 1,
            pwm_min: 0,
            pwm_max: 0,
            pwm_reverse: 0,
            limit: 0,
            clip: 0,
            deadzone: 0,
        };
        let now = Instant::now();
        let mut session = Self {
            link,
            decoder: Decoder::default(),
            settings: Settings {
                version: version as u16,
                resolution,
                motors: vec![blank; 3],
                pid_divider: 1,
                pwm_frequency_12: 25,
                pwm_frequency_3: 31,
            },
            mode: Mode::Monitor,
            motor: 1,
            center: 512,
            amplitude: 100,
            period: 4.,
            started: now,
            last_emit: now,
            pwm: [0; 3],
            disabled: 7,
            calculations: 0,
            samples: VecDeque::new(),
            last_poll: now,
            udp: None,
            udp_decoder: Decoder::default(),
            udp_packets: 0,
            udp_port: None,
        };
        session.refresh_settings()?;
        session.send(b"[mo1]")?;
        Ok(session)
    }
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.link.write_all(bytes).map_err(|e| Error::Device(e.to_string()))
    }
    fn read(&mut self, key: u8) -> Result<[u8; 2]> {
        self.send(&[b'[', b'r', b'd', key, b']'])?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(700) {
            let data = self.link.read_available().map_err(|e| Error::Device(e.to_string()))?;
            for (letter, bytes) in self.decoder.feed(&data) {
                if letter == key {
                    return Ok(bytes);
                }
                self.capture(letter, bytes);
            }
            std::thread::sleep(Duration::from_millis(3));
        }
        Err(Error::Timeout(key as char))
    }
    pub fn refresh_settings(&mut self) -> Result<()> {
        for i in 0..3 {
            let kp = u16::from_be_bytes(self.read(b'D' + i)?);
            let ki = u16::from_be_bytes(self.read(b'G' + i)?);
            let kd = u16::from_be_bytes(self.read(b'J' + i)?);
            let ks = u16::from_be_bytes(self.read(b'M' + i)?);
            let pwm = self.read(b'P' + i)?;
            let limits = self.read(b'S' + i)?;
            let reverse = self.read(b'V' + i)?;
            self.settings.motors[i as usize] = MotorSettings {
                kp,
                ki,
                kd,
                ks,
                pwm_min: pwm[0],
                pwm_max: pwm[1],
                limit: limits[0],
                clip: limits[1],
                deadzone: reverse[0],
                pwm_reverse: reverse[1],
            };
        }
        let state = self.read(b'Y')?;
        self.disabled = state[0] & 7;
        self.settings.pid_divider = state[0] >> 4;
        let freq = self.read(b'~')?;
        self.settings.pwm_frequency_12 = freq[0];
        self.settings.pwm_frequency_3 = freq[1];
        Ok(())
    }
    pub fn apply_settings(&mut self, settings: Settings) -> Result<()> {
        settings.validate()?;
        self.mode = Mode::Monitor;
        self.udp = None;
        for (i, m) in settings.motors.iter().enumerate() {
            for (letter, value) in [
                (b'D', m.kp),
                (b'G', m.ki),
                (b'J', m.kd),
                (b'M', m.ks),
                (b'P', u16::from_be_bytes([m.pwm_min, m.pwm_max])),
                (b'S', u16::from_be_bytes([m.limit, m.clip])),
                (b'V', u16::from_be_bytes([m.deadzone, m.pwm_reverse])),
            ] {
                self.send(&wire::frame(letter + i as u8, value.into()).map_err(|e| Error::Invalid(e.to_string()))?)?;
            }
        }
        self.send(&[b'[', b'Z', settings.pid_divider, 0, b']'])?;
        self.send(&[b'[', b'~', settings.pwm_frequency_12, settings.pwm_frequency_3, b']'])?;
        self.refresh_settings()?;
        if self.settings.motors != settings.motors
            || self.settings.pid_divider != settings.pid_divider
            || self.settings.pwm_frequency_12 != settings.pwm_frequency_12
            || self.settings.pwm_frequency_3 != settings.pwm_frequency_3
        {
            return Err(Error::Device("Controller did not retain all requested settings; readback differs".into()));
        }
        Ok(())
    }
    pub fn save_controller(&mut self) -> Result<()> {
        self.send(b"[sav]")
    }
    pub fn select_motor(&mut self, motor: u8) -> Result<()> {
        if !(1..=3).contains(&motor) {
            return Err(Error::Invalid("Select motor 1, 2 or 3".into()));
        }
        self.mode = Mode::Monitor;
        self.udp = None;
        self.udp_port = None;
        self.motor = motor;
        self.send(&[b'[', b'm', b'o', b'0' + motor, b']'])
    }
    pub fn set_mode(&mut self, mode: Mode, center: u16, amplitude: u16, period: f64) -> Result<()> {
        if center > 1023 || amplitude > 511 || !period.is_finite() || period < 0.25 {
            return Err(Error::Invalid("Invalid waveform range or period".into()));
        }
        if mode == Mode::Udp && self.udp.is_none() {
            return Err(Error::Invalid("Start UDP pass-through with a local UDP port".into()));
        }
        if mode != Mode::Udp {
            self.udp = None;
            self.udp_port = None;
        }
        self.mode = mode;
        self.center = center;
        self.amplitude = amplitude;
        self.period = period;
        self.started = Instant::now();
        Ok(())
    }
    pub fn start_udp(&mut self, port: u16) -> Result<()> {
        let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .map_err(|e| Error::Device(e.to_string()))?;
        socket.set_nonblocking(true).map_err(|e| Error::Device(e.to_string()))?;
        self.udp_port = Some(socket.local_addr().map_err(|e| Error::Device(e.to_string()))?.port());
        self.udp = Some(socket);
        self.udp_packets = 0;
        self.udp_decoder = Decoder::default();
        self.mode = Mode::Udp;
        Ok(())
    }
    pub fn position(&mut self, motor: u8, position: u16) -> Result<()> {
        if !(1..=3).contains(&motor) || position > 1023 {
            return Err(Error::Invalid("Motor or target is outside its range".into()));
        }
        let position =
            if self.settings.resolution == 12 { u32::from(position) * 4095 / 1023 } else { u32::from(position) };
        self.send(&wire::frame(b'A' + motor - 1, position.into()).map_err(|e| Error::Invalid(e.to_string()))?)
    }
    fn capture(&mut self, letter: u8, bytes: [u8; 2]) {
        if (b'a'..=b'c').contains(&letter) {
            self.pwm[(letter - b'a') as usize] = bytes[1];
            self.disabled = bytes[0] & 7;
        }
        if letter == b'Z' {
            self.calculations = u16::from_be_bytes(bytes);
        }
        if (b'A'..=b'C').contains(&letter) {
            let motor = letter - b'A' + 1;
            self.samples.push_back(Sample {
                seconds: self.started.elapsed().as_secs_f64(),
                motor,
                target: u16::from(bytes[1]) * 4,
                feedback: u16::from(bytes[0]) * 4,
                pwm: self.pwm[(motor - 1) as usize],
                enabled: self.disabled & (1 << (motor - 1)) == 0,
                calculations: self.calculations,
            });
            while self.samples.len() > 1500 {
                self.samples.pop_front();
            }
        }
    }
    pub fn tick(&mut self) -> Result<()> {
        if self.mode == Mode::Udp {
            let mut packet = [0u8; 4096];
            for _ in 0..32 {
                let received =
                    self.udp.as_ref().ok_or_else(|| Error::Device("UDP socket is closed".into()))?.recv(&mut packet);
                match received {
                    Ok(n) => {
                        self.udp_packets += 1;
                        for (command, bytes) in self.udp_decoder.feed(&packet[..n]) {
                            if (b'A'..=b'C').contains(&command) {
                                let value = u16::from_be_bytes(bytes);
                                let max = if self.settings.resolution == 12 { 4095 } else { 1023 };
                                if value <= max {
                                    self.send(&[b'[', command, bytes[0], bytes[1], b']'])?;
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(Error::Device(e.to_string())),
                }
            }
        }
        if self.mode != Mode::Monitor && self.mode != Mode::Udp && self.last_emit.elapsed() >= Duration::from_millis(10)
        {
            let value =
                waveform(self.mode, self.started.elapsed().as_secs_f64(), self.center, self.amplitude, self.period);
            self.position(self.motor, value)?;
            self.last_emit = Instant::now();
        }
        if self.last_poll.elapsed() >= Duration::from_secs(1) {
            self.send(&[b'[', b'r', b'd', b'a' + self.motor - 1, b']'])?;
            self.send(b"[rdZ]")?;
            self.last_poll = Instant::now();
        }
        let bytes = self.link.read_available().map_err(|e| Error::Device(e.to_string()))?;
        for (letter, payload) in self.decoder.feed(&bytes) {
            self.capture(letter, payload);
        }
        Ok(())
    }
    pub fn stop(&mut self) -> Result<()> {
        self.mode = Mode::Monitor;
        self.udp = None;
        self.udp_port = None;
        self.send(b"[mo0]")
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
pub fn waveform(mode: Mode, seconds: f64, center: u16, amplitude: u16, period: f64) -> u16 {
    let t = (seconds / period).rem_euclid(1.);
    let wave = match mode {
        Mode::Sine => (t * std::f64::consts::TAU).sin(),
        Mode::Motion => {
            // Two beating sines plus a faster one, normalised to ±1, over n=0..628.
            let n = t * 628.;
            (4. * (n / 200.).sin() * (n / 15.5).sin() + (n / 6.3).sin()) / 5.
        }
        Mode::Square => {
            if t < 0.5 {
                1.
            } else {
                -1.
            }
        }
        Mode::Triangle => 1. - 4. * (t - 0.5).abs(),
        _ => 0.,
    };
    (f64::from(center) + f64::from(amplitude) * wave).round().clamp(0., 1023.) as u16
}

impl Session {
    pub fn port(&self) -> &str {
        self.link.name()
    }
    pub fn enable_motor(&mut self) -> Result<()> {
        self.send(&[b'[', b'e', b'n', b'0' + self.motor, b']'])
    }
    pub fn controller_command(&mut self, command: &str) -> Result<()> {
        let bytes: &[u8] = match command {
            "bootloader" => b"[lWW]",
            "10bit" => b"[lW1]",
            "12bit" => b"[lW4]",
            // Rollback sends both frames as one operation.
            "rollback" => b"[rta][rt0]",
            "park_off" => b"[sp0]",
            "park_bottom" => b"[spD]",
            "park_center" => b"[spC]",
            "park_top" => b"[spU]",
            _ => return Err(Error::Invalid("Unknown controller operation".into())),
        };
        self.mode = Mode::Monitor;
        self.udp = None;
        self.udp_port = None;
        self.send(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_frames_keep_delimiter_in_payload() {
        let mut d = Decoder::default();
        assert!(d.feed(b"noise[A]").is_empty());
        assert_eq!(d.feed(&[9, b']', b'[', b'B', 1, 2, b']']), vec![(b'A', [b']', 9]), (b'B', [1, 2])]);
    }
    #[test]
    fn settings_roundtrip_and_invalid_limits() {
        let m = MotorSettings {
            kp: 400,
            ki: 0,
            kd: 30,
            ks: 3,
            pwm_min: 20,
            pwm_max: 200,
            pwm_reverse: 100,
            limit: 20,
            clip: 30,
            deadzone: 1,
        };
        let s = Settings {
            version: 102,
            resolution: 10,
            motors: vec![m; 3],
            pid_divider: 1,
            pwm_frequency_12: 25,
            pwm_frequency_3: 31,
        };
        assert_eq!(Settings::from_legacy(&s.to_legacy()).unwrap().motors, s.motors);
        let mut bad = s;
        bad.motors[0].clip = 5;
        assert!(bad.validate().is_err());
    }
    #[test]
    fn waveform_limits_and_phase() {
        assert_eq!(waveform(Mode::Sine, 1., 512, 100, 4.), 612);
        assert_eq!(waveform(Mode::Square, 3., 512, 100, 4.), 412);
        assert_eq!(waveform(Mode::Triangle, 2., 512, 100, 4.), 612);
        assert_eq!(waveform(Mode::Manual, 9., 600, 100, 4.), 600);
        let n = 157.0f64;
        let expected =
            (512. + 0.5 * (400. * (n / 200.).sin() * (n / 15.5).sin() + 100. * (n / 6.3).sin())).round() as u16;
        assert_eq!(waveform(Mode::Motion, 1., 512, 250, 4.), expected);
    }

    #[derive(Default)]
    struct FakeState {
        pending: Vec<u8>,
        writes: Vec<Vec<u8>>,
        values: std::collections::BTreeMap<u8, [u8; 2]>,
    }
    struct Fake(std::sync::Arc<std::sync::Mutex<FakeState>>);
    impl Link for Fake {
        fn write_all(&mut self, bytes: &[u8]) -> std::result::Result<(), openrigcue_devices::serial::LinkError> {
            let mut s = self.0.lock().unwrap();
            s.writes.push(bytes.to_vec());
            if bytes == b"[ver]" {
                s.pending.extend(b"[v\0\x46]");
            } else if bytes.starts_with(b"[rd") {
                let key = bytes[3];
                let value = s.values.get(&key).copied().unwrap_or([0, 0]);
                s.pending.extend([b'[', key, value[0], value[1], b']']);
            } else if bytes.len() == 5 && (b'D'..=b'Z').contains(&bytes[1]) || bytes.get(1) == Some(&b'~') {
                s.values.insert(bytes[1], [bytes[2], bytes[3]]);
            }
            Ok(())
        }
        fn read_available(&mut self) -> std::result::Result<Vec<u8>, openrigcue_devices::serial::LinkError> {
            Ok(std::mem::take(&mut self.0.lock().unwrap().pending))
        }
        fn clear_input(&mut self) -> std::result::Result<(), openrigcue_devices::serial::LinkError> {
            Ok(())
        }
        fn name(&self) -> &str {
            "simulated-controller"
        }
    }
    fn fake_session() -> (Session, std::sync::Arc<std::sync::Mutex<FakeState>>) {
        let mut s = FakeState::default();
        for i in 0..3 {
            for (key, value) in [
                (b'D', [1, 144]),
                (b'G', [0, 0]),
                (b'J', [0, 30]),
                (b'M', [0, 3]),
                (b'P', [20, 200]),
                (b'S', [20, 30]),
                (b'V', [1, 100]),
            ] {
                s.values.insert(key + i, value);
            }
        }
        s.values.insert(b'Y', [0x10, 0]);
        s.values.insert(b'~', [25, 31]);
        let s = std::sync::Arc::new(std::sync::Mutex::new(s));
        (Session::with_link(Box::new(Fake(s.clone()))).unwrap(), s)
    }
    #[test]
    fn setup_reads_without_movement_and_roundtrips_settings() {
        let (mut session, state) = fake_session();
        assert_eq!(session.settings.version, 70);
        assert!(!state.lock().unwrap().writes.iter().any(|w| (b'A'..=b'C').contains(&w[1])));
        let mut settings = session.settings.clone();
        settings.motors[1].kp = 500;
        session.apply_settings(settings).unwrap();
        assert_eq!(session.settings.motors[1].kp, 500);
        session.set_mode(Mode::Manual, 600, 0, 1.).unwrap();
        session.last_emit = Instant::now() - Duration::from_secs(1);
        session.tick().unwrap();
        assert!(state.lock().unwrap().writes.iter().any(|w| w == b"[A\x02\x58]"));
        session.stop().unwrap();
        let n = state.lock().unwrap().writes.len();
        session.tick().unwrap();
        assert_eq!(state.lock().unwrap().writes.len(), n);
    }
    #[test]
    fn udp_passes_only_position_frames() {
        let (mut session, state) = fake_session();
        session.start_udp(0).unwrap();
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"[A\x02\0][sav][B\x03\0]", ("127.0.0.1", session.udp_port.unwrap())).unwrap();
        let n = state.lock().unwrap().writes.len();
        session.tick().unwrap();
        assert_eq!(&state.lock().unwrap().writes[n..], &[b"[A\x02\0]".to_vec(), b"[B\x03\0]".to_vec()]);
        assert_eq!(session.udp_packets, 1);
    }
}
