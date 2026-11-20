//! `DebugISIInternals` output (dumps in the format of ISI's example
//! plugin): WriteToAllExampleOutputFiles (used by the game-flow
//! callbacks), WriteScoringInternals, WriteTelemetryInternals.

use crate::cfmt::Arg::{self, Dbl, Int, Str};
use crate::crt::{CrtFile, StreamSlot};
use crate::layout::*;
use crate::log::{Ctx, INTERNALS_SCORING_FILE, INTERNALS_TELEMETRY_FILE};
use crate::sys::OpenMode;

/// Square root as one `sqrtsd`.
#[inline]
pub fn sqrt(x: f64) -> f64 {
    let r: f64;
    unsafe {
        core::arch::asm!("sqrtsd {0}, {0}", inlateout(xmm_reg) x => r, options(pure, nomem, nostack));
    }
    r
}

/// `fopen(file, mode)` / `fprintf(fo, "%s\n", msg)` / `fclose` on both
/// internals files.
pub fn write_to_all_output_files(cx: Ctx, mode: OpenMode, msg: &[u8]) {
    for file in [INTERNALS_TELEMETRY_FILE, INTERNALS_SCORING_FILE] {
        unsafe {
            if let Some(mut f) = CrtFile::open(cx.sys, &mut (*cx.g).errno, file, mode) {
                f.printf(cx.sys, b"%s\n", &[Str(msg)]);
                f.close(cx.sys);
            }
        }
    }
}

/// The cached `static FILE *` of a dump: opened with `_fsopen(.., "a",
/// _SH_DENYNO)` + `setvbuf(.., 0x800)` the first time, `fclose`d after every
/// dump without being reset.
struct Dump {
    cx: Ctx,
    slot: *mut StreamSlot,
}

impl Dump {
    unsafe fn open(cx: Ctx, slot: *mut StreamSlot, file: &[u8]) -> Dump {
        if (*slot).is_null() {
            match CrtFile::open(cx.sys, &mut (*cx.g).errno, file, OpenMode::Append) {
                Some(mut f) => {
                    f.setvbuf(0x800);
                    *slot = StreamSlot::Open(f);
                }
                // setvbuf(NULL, ...) precedes the NULL check.
                None => cx.sys.invalid_parameter(),
            }
        }
        Dump { cx, slot }
    }
    fn p(&mut self, fmt: &[u8], args: &[Arg]) {
        if let StreamSlot::Open(f) = unsafe { &mut *self.slot } {
            unsafe { f.printf(self.cx.sys, fmt, args) }
        }
    }
    fn putc(&mut self, c: u8) {
        if let StreamSlot::Open(f) = unsafe { &mut *self.slot } {
            unsafe { f.write(self.cx.sys, &[c]) }
        }
    }
    fn close(self) {
        if let StreamSlot::Open(f) = core::mem::replace(unsafe { &mut *self.slot }, StreamSlot::Stale) {
            unsafe { f.close(self.cx.sys) };
        }
    }
}

/// WriteScoringInternals (called first thing in UpdateScoring).
pub unsafe fn write_scoring_internals(cx: Ctx, info: *const u8) {
    use scoring_info as s;
    use vehicle_scoring as v;
    if !(*cx.g).debug_isi_internals {
        return;
    }
    let mut d = Dump::open(cx, core::ptr::addr_of_mut!((*cx.g).scoring_dump), INTERNALS_SCORING_FILE);
    d.p(b"TrackName=%s\n", &[Str(cstr(info.add(s::TRACK_NAME)))]);
    d.p(
        b"Session=%d NumVehicles=%d CurET=%.3f\n",
        &[Int(rd_i32(info, s::SESSION)), Int(rd_i32(info, s::NUM_VEHICLES)), Dbl(rd_f64(info, s::CURRENT_ET))],
    );
    d.p(
        b"EndET=%.3f MaxLaps=%d LapDist=%.1f\n",
        &[Dbl(rd_f64(info, s::END_ET)), Int(rd_i32(info, s::MAX_LAPS)), Dbl(rd_f64(info, s::LAP_DIST))],
    );
    d.p(b"START STREAM\n", &[]);
    let mut ptr = rd_ptr(info, s::RESULTS_STREAM);
    while *ptr != 0 {
        d.putc(*ptr);
        ptr = ptr.add(1);
    }
    d.p(b"END STREAM\n", &[]);
    d.p(
        b"GamePhase=%d YellowFlagState=%d SectorFlags=(%d,%d,%d)\n",
        &[
            Int(rd_u8(info, s::GAME_PHASE) as i32),
            Int(rd_i8(info, s::YELLOW_FLAG_STATE) as i32),
            Int(rd_i8(info, s::SECTOR_FLAG) as i32),
            Int(rd_i8(info, s::SECTOR_FLAG + 1) as i32),
            Int(rd_i8(info, s::SECTOR_FLAG + 2) as i32),
        ],
    );
    d.p(
        b"InRealtime=%d StartLight=%d NumRedLights=%d\n",
        &[
            Int(rd_u8(info, s::IN_REALTIME) as i32),
            Int(rd_u8(info, s::START_LIGHT) as i32),
            Int(rd_u8(info, s::NUM_RED_LIGHTS) as i32),
        ],
    );
    d.p(
        b"PlayerName=%s PlrFileName=%s\n",
        &[Str(cstr(info.add(s::PLAYER_NAME))), Str(cstr(info.add(s::PLR_FILE_NAME)))],
    );
    d.p(
        b"DarkCloud=%.2f Raining=%.2f AmbientTemp=%.1f TrackTemp=%.1f\n",
        &[
            Dbl(rd_f64(info, s::DARK_CLOUD)),
            Dbl(rd_f64(info, s::RAINING)),
            Dbl(rd_f64(info, s::AMBIENT_TEMP)),
            Dbl(rd_f64(info, s::TRACK_TEMP)),
        ],
    );
    d.p(
        b"Wind=(%.1f,%.1f,%.1f) MinPathWetness=%.2f MaxPathWetness=%.2f\n",
        &[
            Dbl(rd_f64(info, s::WIND)),
            Dbl(rd_f64(info, s::WIND + 8)),
            Dbl(rd_f64(info, s::WIND + 16)),
            Dbl(rd_f64(info, s::MIN_PATH_WETNESS)),
            Dbl(rd_f64(info, s::MAX_PATH_WETNESS)),
        ],
    );
    let mut i = 0;
    while i < rd_i32(info, s::NUM_VEHICLES) {
        let vi = rd_ptr(info, s::VEHICLE).add(i as usize * v::SIZE);
        let f = |o| Dbl(rd_f64(vi, o));
        d.p(b"Driver %d: %s\n", &[Int(i), Str(cstr(vi.add(v::DRIVER_NAME)))]);
        d.p(b" ID=%d Vehicle=%s\n", &[Int(rd_i32(vi, v::ID)), Str(cstr(vi.add(v::VEHICLE_NAME)))]);
        d.p(
            b" Laps=%d Sector=%d FinishStatus=%d\n",
            &[
                Int(rd_i16(vi, v::TOTAL_LAPS) as i32),
                Int(rd_i8(vi, v::SECTOR) as i32),
                Int(rd_i8(vi, v::FINISH_STATUS) as i32),
            ],
        );
        d.p(
            b" LapDist=%.1f PathLat=%.2f RelevantTrackEdge=%.2f\n",
            &[f(v::LAP_DIST), f(v::PATH_LATERAL), f(v::TRACK_EDGE)],
        );
        d.p(b" Best=(%.3f, %.3f, %.3f)\n", &[f(v::BEST_SECTOR1), f(v::BEST_SECTOR2), f(v::BEST_LAP_TIME)]);
        d.p(b" Last=(%.3f, %.3f, %.3f)\n", &[f(v::LAST_SECTOR1), f(v::LAST_SECTOR2), f(v::LAST_LAP_TIME)]);
        d.p(
            b" Current Sector 1 = %.3f, Current Sector 2 = %.3f\n",
            &[f(v::CUR_SECTOR1), f(v::CUR_SECTOR2)],
        );
        d.p(
            b" Pitstops=%d, Penalties=%d\n",
            &[Int(rd_i16(vi, v::NUM_PITSTOPS) as i32), Int(rd_i16(vi, v::NUM_PENALTIES) as i32)],
        );
        d.p(
            b" IsPlayer=%d Control=%d InPits=%d LapStartET=%.3f\n",
            &[
                Int(rd_u8(vi, v::IS_PLAYER) as i32),
                Int(rd_i8(vi, v::CONTROL) as i32),
                Int(rd_u8(vi, v::IN_PITS) as i32),
                f(v::LAP_START_ET),
            ],
        );
        d.p(b" Place=%d VehicleClass=%s\n", &[Int(rd_u8(vi, v::PLACE) as i32), Str(cstr(vi.add(v::VEHICLE_CLASS)))]);
        d.p(
            b" TimeBehindNext=%.3f LapsBehindNext=%d\n",
            &[f(v::TIME_BEHIND_NEXT), Int(rd_i32(vi, v::LAPS_BEHIND_NEXT))],
        );
        d.p(
            b" TimeBehindLeader=%.3f LapsBehindLeader=%d\n",
            &[f(v::TIME_BEHIND_LEADER), Int(rd_i32(vi, v::LAPS_BEHIND_LEADER))],
        );
        d.p(b" Pos=(%.3f,%.3f,%.3f)\n", &[f(v::POS), f(v::POS + 8), f(v::POS + 16)]);
        d.p(b" LocalVel=(%.2f,%.2f,%.2f)\n", &[f(v::LOCAL_VEL), f(v::LOCAL_VEL + 8), f(v::LOCAL_VEL + 16)]);
        d.p(b" LocalAccel=(%.1f,%.1f,%.1f)\n", &[f(v::LOCAL_ACCEL), f(v::LOCAL_ACCEL + 8), f(v::LOCAL_ACCEL + 16)]);
        for r in 0..3 {
            let o = v::ORI + r * 24;
            d.p(b" [%6.3f,%6.3f,%6.3f]\n", &[f(o), f(o + 8), f(o + 16)]);
        }
        d.p(b" LocalRot=(%.3f,%.3f,%.3f)\n", &[f(v::LOCAL_ROT), f(v::LOCAL_ROT + 8), f(v::LOCAL_ROT + 16)]);
        d.p(
            b" LocalRotAccel=(%.2f,%.2f,%.2f)\n",
            &[f(v::LOCAL_ROT_ACCEL), f(v::LOCAL_ROT_ACCEL + 8), f(v::LOCAL_ROT_ACCEL + 16)],
        );
        i += 1;
    }
    d.p(b"\n", &[]);
    d.close();
}

/// WriteTelemetryInternals (called first thing in UpdateTelemetry).
pub unsafe fn write_telemetry_internals(cx: Ctx, info: *const u8) {
    use telem as t;
    use telem::wheel as w;
    if !(*cx.g).debug_isi_internals {
        return;
    }
    let mut d = Dump::open(cx, core::ptr::addr_of_mut!((*cx.g).telemetry_dump), INTERNALS_TELEMETRY_FILE);
    let f = |o| Dbl(rd_f64(info, o));
    let pct = |o| Dbl(rd_f64(info, o) * 100.0);
    d.p(b"DT=%.4f  ET=%.4f\n", &[f(t::DELTA_TIME), f(t::ELAPSED_TIME)]);
    d.p(b"Lap=%d StartET=%.3f\n", &[Int(rd_i32(info, t::LAP_NUMBER)), f(t::LAP_START_ET)]);
    d.p(b"Vehicle=%s\n", &[Str(cstr(info.add(t::VEHICLE_NAME)))]);
    d.p(b"Track=%s\n", &[Str(cstr(info.add(t::TRACK_NAME)))]);
    d.p(b"Pos=(%.3f,%.3f,%.3f)\n", &[f(t::POS), f(t::POS + 8), f(t::POS + 16)]);
    d.p(b"LocalVel=(%.2f,%.2f,%.2f)\n", &[f(t::LOCAL_VEL), f(t::LOCAL_VEL + 8), f(t::LOCAL_VEL + 16)]);
    d.p(b"LocalAccel=(%.1f,%.1f,%.1f)\n", &[f(t::LOCAL_ACCEL), f(t::LOCAL_ACCEL + 8), f(t::LOCAL_ACCEL + 16)]);
    for r in 0..3 {
        let o = t::ORI + r * 24;
        d.p(b"[%6.3f,%6.3f,%6.3f]\n", &[f(o), f(o + 8), f(o + 16)]);
    }
    d.p(b"LocalRot=(%.3f,%.3f,%.3f)\n", &[f(t::LOCAL_ROT), f(t::LOCAL_ROT + 8), f(t::LOCAL_ROT + 16)]);
    d.p(
        b"LocalRotAccel=(%.2f,%.2f,%.2f)\n",
        &[f(t::LOCAL_ROT_ACCEL), f(t::LOCAL_ROT_ACCEL + 8), f(t::LOCAL_ROT_ACCEL + 16)],
    );
    d.p(b"Gear=%d RPM=%.1f RevLimit=%.1f\n", &[Int(rd_i32(info, t::GEAR)), f(t::ENGINE_RPM), f(t::ENGINE_MAX_RPM)]);
    d.p(b"Water=%.1f Oil=%.1f\n", &[f(t::ENGINE_WATER_TEMP), f(t::ENGINE_OIL_TEMP)]);
    d.p(b"ClutchRPM=%.1f\n", &[f(t::CLUTCH_RPM)]);
    d.p(b"UnfilteredThrottle=%.1f%%\n", &[pct(t::UNFILTERED_THROTTLE)]);
    d.p(b"UnfilteredBrake=%.1f%%\n", &[pct(t::UNFILTERED_BRAKE)]);
    d.p(b"UnfilteredSteering=%.1f%%\n", &[pct(t::UNFILTERED_STEERING)]);
    d.p(b"UnfilteredClutch=%.1f%%\n", &[pct(t::UNFILTERED_CLUTCH)]);
    d.p(b"FilteredThrottle=%.1f%%\n", &[pct(t::FILTERED_THROTTLE)]);
    d.p(b"FilteredBrake=%.1f%%\n", &[pct(t::FILTERED_BRAKE)]);
    d.p(b"FilteredSteering=%.1f%%\n", &[pct(t::FILTERED_STEERING)]);
    d.p(b"FilteredClutch=%.1f%%\n", &[pct(t::FILTERED_CLUTCH)]);
    d.p(b"SteeringShaftTorque=%.1f\n", &[f(t::STEERING_SHAFT_TORQUE)]);
    d.p(
        b"Front3rdDeflection=%.3f Rear3rdDeflection=%.3f\n",
        &[f(t::FRONT_3RD_DEFLECTION), f(t::REAR_3RD_DEFLECTION)],
    );
    d.p(
        b"FrontWingHeight=%.3f FrontRideHeight=%.3f RearRideHeight=%.3f\n",
        &[f(t::FRONT_WING_HEIGHT), f(t::FRONT_RIDE_HEIGHT), f(t::REAR_RIDE_HEIGHT)],
    );
    d.p(
        b"Drag=%.1f FrontDownforce=%.1f RearDownforce=%.1f\n",
        &[f(t::DRAG), f(t::FRONT_DOWNFORCE), f(t::REAR_DOWNFORCE)],
    );
    d.p(
        b"Fuel=%.1f ScheduledStops=%d Overheating=%d Detached=%d\n",
        &[
            f(t::FUEL),
            Int(rd_u8(info, t::SCHEDULED_STOPS) as i32),
            Int(rd_u8(info, t::OVERHEATING) as i32),
            Int(rd_u8(info, t::DETACHED) as i32),
        ],
    );
    let dent = |k| Int(rd_u8(info, t::DENT_SEVERITY + k) as i32);
    d.p(
        b"Dents=(%d,%d,%d,%d,%d,%d,%d,%d)\n",
        &[dent(0), dent(1), dent(2), dent(3), dent(4), dent(5), dent(6), dent(7)],
    );
    d.p(
        b"LastImpactET=%.1f Mag=%.1f, Pos=(%.1f,%.1f,%.1f)\n",
        &[
            f(t::LAST_IMPACT_ET),
            f(t::LAST_IMPACT_MAGNITUDE),
            f(t::LAST_IMPACT_POS),
            f(t::LAST_IMPACT_POS + 8),
            f(t::LAST_IMPACT_POS + 16),
        ],
    );
    for i in 0..4usize {
        let wh = t::WHEELS + i * t::WHEEL_SIZE;
        let fw = |o| Dbl(rd_f64(info, wh + o));
        let name: &[u8] = match i {
            0 => b"FrontLeft",
            1 => b"FrontRight",
            2 => b"RearLeft",
            _ => b"RearRight",
        };
        d.p(b"Wheel=%s\n", &[Str(name)]);
        d.p(b" SuspensionDeflection=%.3f RideHeight=%.3f\n", &[fw(w::SUSPENSION_DEFLECTION), fw(w::RIDE_HEIGHT)]);
        d.p(
            b" SuspForce=%.1f BrakeTemp=%.1f BrakePressure=%.3f\n",
            &[fw(w::SUSP_FORCE), fw(w::BRAKE_TEMP), fw(w::BRAKE_PRESSURE)],
        );
        d.p(b" ForwardRotation=%.1f Camber=%.3f\n", &[Dbl(-rd_f64(info, wh + w::ROTATION)), fw(w::CAMBER)]);
        d.p(
            b" LateralPatchVel=%.2f LongitudinalPatchVel=%.2f\n",
            &[fw(w::LATERAL_PATCH_VEL), fw(w::LONGITUDINAL_PATCH_VEL)],
        );
        d.p(
            b" LateralGroundVel=%.2f LongitudinalGroundVel=%.2f\n",
            &[fw(w::LATERAL_GROUND_VEL), fw(w::LONGITUDINAL_GROUND_VEL)],
        );
        d.p(b" LateralForce=%.1f LongitudinalForce=%.1f\n", &[fw(w::LATERAL_FORCE), fw(w::LONGITUDINAL_FORCE)]);
        d.p(
            b" TireLoad=%.1f GripFract=%.3f TirePressure=%.1f\n",
            &[fw(w::TIRE_LOAD), fw(w::GRIP_FRACT), fw(w::PRESSURE)],
        );
        d.p(
            b" TireTemp(l/c/r)=%.1f/%.1f/%.1f\n",
            &[fw(w::TEMPERATURE), fw(w::TEMPERATURE + 8), fw(w::TEMPERATURE + 16)],
        );
        d.p(
            b" Wear=%.3f TerrainName=%s SurfaceType=%d\n",
            &[fw(w::WEAR), Str(cstr(info.add(wh + w::TERRAIN_NAME))), Int(rd_u8(info, wh + w::SURFACE_TYPE) as i32)],
        );
        d.p(
            b" Flat=%d Detached=%d\n",
            &[Int(rd_u8(info, wh + w::FLAT) as i32), Int(rd_u8(info, wh + w::DETACHED) as i32)],
        );
    }
    // forward = -ori[*].z, left = ori[*].x
    let ori = |r: usize, c: usize| rd_f64(info, t::ORI + r * 24 + c * 8);
    let roll = cx.sys.atan2(ori(1, 0), sqrt(ori(2, 0) * ori(2, 0) + ori(0, 0) * ori(0, 0)));
    let (fx, fy, fz) = (-ori(0, 2), -ori(1, 2), -ori(2, 2));
    let pitch = cx.sys.atan2(fy, sqrt(fz * fz + fx * fx));
    d.p(b"Pitch = %.1f deg, Roll = %.1f deg\n", &[Dbl(pitch * 57.296), Dbl(roll * 57.296)]);
    let (vx, vy, vz) = (rd_f64(info, t::LOCAL_VEL), rd_f64(info, t::LOCAL_VEL + 8), rd_f64(info, t::LOCAL_VEL + 16));
    let mps = sqrt(vx * vx + vy * vy + vz * vz);
    d.p(b"Speed = %.1f KPH, %.1f MPH\n\n", &[Dbl(mps * 3.6), Dbl(mps * 2.237)]);
    d.close();
}
