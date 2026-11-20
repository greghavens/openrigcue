//! The one ADC read in set-up: GPIO27 attachment and ADC2 read.

pub const TOUCH_ENABLE: usize = 0x3ff4_888c;
pub const RTC_PAD27: usize = 0x3ff4_84b0;
pub const RTC_OUTPUT_CLEAR: usize = 0x3ff4_8414;
pub const IO_MUX27: usize = 0x3ff4_902c;

pub trait Io {
    fn read(&mut self, address: usize) -> u32;
    fn write(&mut self, address: usize, value: u32);
    fn attenuation(&mut self, channel: u8, value: u8);
    fn clock_divider(&mut self, value: u8);
    fn width(&mut self, value: u8);
    fn adc2_raw(&mut self, channel: u8, width: u8, value: &mut i32) -> i32;
    fn read_error(&mut self, error: i32);
}

/// The first and only analog reading of GPIO27, with clock divider 1, attenuation 3 and width 3.
/// A driver error leaves the raw value at its initial 0.
pub fn read_gpio27(io: &mut impl Io) -> u32 {
    let touch = io.read(TOUCH_ENABLE);
    if touch & (1 << 7) != 0 {
        io.write(TOUCH_ENABLE, touch & !((1 << 7) | (1 << 17) | (1 << 27)));
    }
    let pad = io.read(RTC_PAD27);
    if pad & 0x0008_0000 == 0 {
        let clear = 0x0000_2000 | 0x0800_0000 | 0x1000_0000 | (3 << 17);
        io.write(RTC_OUTPUT_CLEAR, 1 << 31);
        io.write(RTC_PAD27, (pad & !clear) | 0x0008_0000);
        io.write(IO_MUX27, 0x2a00);
    }
    io.attenuation(7, 3);
    io.clock_divider(1);
    io.width(3);
    let mut raw = 0;
    let error = io.adc2_raw(7, 3, &mut raw);
    if error != 0 {
        io.read_error(error);
    }
    u32::from(raw as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    #[derive(Debug, PartialEq)]
    enum Op {
        Read(usize),
        Write(usize, u32),
        Attenuation(u8, u8),
        Clock(u8),
        Width(u8),
        Raw(u8, u8),
        Error(i32),
    }
    struct Fake {
        touch: u32,
        pad: u32,
        value: i32,
        error: i32,
        ops: Vec<Op>,
    }
    impl Io for Fake {
        fn read(&mut self, address: usize) -> u32 {
            self.ops.push(Op::Read(address));
            match address {
                TOUCH_ENABLE => self.touch,
                RTC_PAD27 => self.pad,
                _ => panic!("unexpected read"),
            }
        }
        fn write(&mut self, address: usize, value: u32) {
            self.ops.push(Op::Write(address, value));
        }
        fn attenuation(&mut self, channel: u8, value: u8) {
            self.ops.push(Op::Attenuation(channel, value));
        }
        fn clock_divider(&mut self, value: u8) {
            self.ops.push(Op::Clock(value));
        }
        fn width(&mut self, value: u8) {
            self.ops.push(Op::Width(value));
        }
        fn adc2_raw(&mut self, channel: u8, width: u8, value: &mut i32) -> i32 {
            self.ops.push(Op::Raw(channel, width));
            if self.error == 0 {
                *value = self.value;
            }
            self.error
        }
        fn read_error(&mut self, error: i32) {
            self.ops.push(Op::Error(error));
        }
    }
    #[test]
    fn gpio27_attachment_and_adc_driver_call_order() {
        for touch in [0, 0x0802_0080, u32::MAX] {
            for pad in [0, 0x1806_2000, 0x0008_0000, u32::MAX] {
                let mut io = Fake { touch, pad, value: 0x1234_0abc, error: 0, ops: Vec::new() };
                assert_eq!(read_gpio27(&mut io), 0xabc);
                let mut expected = vec![Op::Read(TOUCH_ENABLE)];
                if touch & 0x80 != 0 {
                    expected.push(Op::Write(TOUCH_ENABLE, touch & !0x0802_0080));
                }
                expected.push(Op::Read(RTC_PAD27));
                if pad & 0x80000 == 0 {
                    expected.extend([
                        Op::Write(RTC_OUTPUT_CLEAR, 0x8000_0000),
                        Op::Write(RTC_PAD27, (pad & !0x1806_2000) | 0x80000),
                        Op::Write(IO_MUX27, 0x2a00),
                    ]);
                }
                expected.extend([Op::Attenuation(7, 3), Op::Clock(1), Op::Width(3), Op::Raw(7, 3)]);
                assert_eq!(io.ops, expected);
            }
        }
    }
    #[test]
    fn adc_failure_logs_and_returns_initialized_zero() {
        for error in [0x103, 0x107, -1] {
            let mut io = Fake { touch: 0, pad: 0x80000, value: 123, error, ops: Vec::new() };
            assert_eq!(read_gpio27(&mut io), 0);
            assert_eq!(io.ops.last(), Some(&Op::Error(error)));
        }
    }
}
