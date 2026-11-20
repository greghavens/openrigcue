//! One typed getter reads each decoded aircraft value at the point where it is used.
//!
//! The receive thread may publish a new data block at only one point inside a getter: the configuration
//! lookup of the MSFS menu branch (`MOTIONBOX.model`). The `interpreted` callback is invoked there so the
//! caller can take a fresh view of the decoded aircraft state for the reads that follow.

use super::aircraft::Datum;
use super::*;

impl<S: Read + Write + Send + 'static> FlightSimulator<S> {
    #[cfg(test)]
    pub(super) fn native_answer_with(
        &self,
        request: &str,
        read: impl FnMut(Datum) -> Option<f64>,
        title: impl FnMut() -> Option<Vec<u8>>,
    ) -> Answer {
        self.native_answer_at(request, read, title, || {})
    }

    pub(super) fn native_answer_at(
        &self,
        request: &str,
        mut read: impl FnMut(Datum) -> Option<f64>,
        mut title: impl FnMut() -> Option<Vec<u8>>,
        mut interpreted: impl FnMut(),
    ) -> Answer {
        self.try_native_answer(request, &mut read, &mut title, &mut interpreted).unwrap_or(Answer::Integer(0))
    }

    fn try_native_answer(
        &self,
        request: &str,
        read: &mut impl FnMut(Datum) -> Option<f64>,
        title: &mut impl FnMut() -> Option<Vec<u8>>,
        interpreted: &mut impl FnMut(),
    ) -> Option<Answer> {
        match self.settings.simulator {
            Simulator::Fsx | Simulator::Prepar3d => {
                let sum = read(Datum::BankDegrees)?
                    + read(Datum::PitchDegrees)?
                    + read(Datum::HeadingDegreesTrue)?
                    + read(Datum::AccelerationY)?
                    + read(Datum::AccelerationZ)?
                    + read(Datum::AccelerationX)?;
                if lock(&self.freeze).observe(sum) {
                    return Some(Answer::Integer(0));
                }
            }
            Simulator::Msfs => {
                let camera = read(Datum::CameraState)?;
                if !camera.is_finite() {
                    return None;
                }
                if camera.trunc() > LAST_FLIGHT_CAMERA_STATE {
                    let yaw_vr = match &self.model_reader {
                        Some(reader) => {
                            reader()
                                .and_then(|v| v.as_str().map(str::to_owned))
                                .and_then(|name| openrigcue_model::motion_models::motion_model(&name))
                                .and_then(|model| model.fields.get("manufacturer"))
                                .and_then(|value| value.as_str())
                                == Some("Yaw VR")
                        }
                        None => self.settings.yaw_vr,
                    };
                    interpreted();
                    return Some(match request {
                        "yaw" if yaw_vr => {
                            Answer::Number(read(Datum::HeadingDegreesTrue)? * Self::factor(self.settings.inversion.yaw))
                        }
                        "yaw_rate" if yaw_vr => {
                            Answer::Number(read(Datum::RotationVelocityY)? * Self::factor(self.settings.inversion.yaw))
                        }
                        _ => Answer::Integer(0),
                    });
                }
            }
        }
        let factor = self.settings.inversion;
        let number = match request {
            "roll" => fold_roll(-read(Datum::BankDegrees)? * Self::factor(factor.roll)),
            "pitch" => fold_roll(-read(Datum::PitchDegrees)? * Self::factor(factor.pitch)),
            "yaw" => read(Datum::HeadingDegreesTrue)? * Self::factor(factor.yaw),
            "pitch_rate" => -read(Datum::RotationVelocityX)? * Self::factor(factor.pitch),
            "roll_rate" => -read(Datum::RotationVelocityZ)? * Self::factor(factor.roll),
            "yaw_rate" => read(Datum::RotationVelocityY)? * Self::factor(factor.yaw),
            "turbulence" => {
                let mut delta = [0.0; 3];
                for (i, datum) in [Datum::WindX, Datum::WindY, Datum::WindZ].into_iter().enumerate() {
                    delta[i] = read(datum)? - lock(&self.previous_wind)[i];
                }
                let magnitude = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
                for (i, datum) in [Datum::WindX, Datum::WindY, Datum::WindZ].into_iter().enumerate() {
                    lock(&self.previous_wind)[i] = read(datum)?;
                }
                return Some(if magnitude > 1.0 { Answer::Number(magnitude) } else { Answer::Integer(0) });
            }
            "touchdown_nose" | "touchdown_left" | "touchdown_right" => {
                let index = match request {
                    "touchdown_nose" => 0,
                    "touchdown_left" => 1,
                    _ => 2,
                };
                return Some(if read(Datum::GearAnimation(index))? > 50.0 && read(Datum::OnGround)? == 1.0 {
                    Answer::Number(read(Datum::AccelerationY)?)
                } else {
                    Answer::Integer(0)
                });
            }
            "rpm" => {
                let second = read(Datum::EngineRpm(1))?;
                let third = read(Datum::EngineRpm(2))?;
                let fourth = read(Datum::EngineRpm(3))?;
                let first = read(Datum::EngineRpm(0))?;
                return Some(Answer::maximum([first, second, third, fourth].map(Answer::Number)));
            }
            "max_rpm" => {
                for i in 0..4 {
                    let value = if read(Datum::EngineMaxRpmPercent(i))? == 0.0 {
                        Answer::Integer(0)
                    } else {
                        let rpm = read(Datum::EngineRpm(i))?;
                        let percent = read(Datum::EngineMaxRpmPercent(i))?;
                        if percent == 0.0 {
                            return None;
                        }
                        Answer::Number(rpm * 10.0 / percent)
                    };
                    lock(&self.estimated_rpm)[i] = Some(value);
                }
                let values = lock(&self.estimated_rpm);
                return Some(Answer::maximum(std::array::from_fn(|i| values[i].clone().unwrap())));
            }
            "lateral_acceleration" => read(Datum::AccelerationX).unwrap_or(0.0) / 10.0,
            "vertical_acceleration" => read(Datum::AccelerationY).unwrap_or(0.0) / 3.0,
            "longitudinal_acceleration" => read(Datum::AccelerationZ).unwrap_or(0.0) / 3.0,
            "canopy" => read(Datum::CanopyOpen)?,
            "afterburner" => return Some(Answer::Integer(0)),
            "landing_gear_nose" => read(Datum::GearCenterPosition)?,
            "landing_gear_left" => read(Datum::GearLeftPosition)?,
            "landing_gear_right" => read(Datum::GearRightPosition)?,
            "car_name" => return Some(self.vehicle_name(&title()?)),
            "speed" => read(Datum::AirspeedIndicated)?,
            "engine_rpm1" => read(Datum::EngineRpm(0))?,
            "engine_rpm2" => read(Datum::EngineRpm(1))?,
            "engine_rpm3" => read(Datum::EngineRpm(2))?,
            "engine_rpm4" => read(Datum::EngineRpm(3))?,
            "engine_max_perc_rpm1" => read(Datum::EngineMaxRpmPercent(0))?,
            "engine_max_perc_rpm2" => read(Datum::EngineMaxRpmPercent(1))?,
            "engine_max_perc_rpm3" => read(Datum::EngineMaxRpmPercent(2))?,
            "engine_max_perc_rpm4" => read(Datum::EngineMaxRpmPercent(3))?,
            "lateral_wind" => read(Datum::WindX)?,
            "vertical_wind" => read(Datum::WindY)?,
            "longitudinal_wind" => read(Datum::WindZ)?,
            "afterburner_1" => read(Datum::Afterburner1)?,
            "landing_gear_center" => read(Datum::GearCenterPosition)?,
            "gear_animation_center" => read(Datum::GearAnimation(0))?,
            "gear_animation_left" => read(Datum::GearAnimation(1))?,
            "gear_animation_right" => read(Datum::GearAnimation(2))?,
            "gear_animation_aux" => read(Datum::GearAnimationAux)?,
            "is_onground" => read(Datum::OnGround)?,
            "camera_state" if self.settings.simulator == Simulator::Msfs => read(Datum::CameraState)?,
            "max_rpm1" | "max_rpm2" | "max_rpm3" | "max_rpm4" => {
                return lock(&self.estimated_rpm)[usize::from(request.as_bytes()[7] - b'1')].clone();
            }
            _ => return None,
        };
        Some(Answer::Number(number))
    }
}
