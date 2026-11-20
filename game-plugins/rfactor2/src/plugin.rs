//! `SharedMemoryPlugin` (object of 0x108c8 bytes from
//! CreatePluginObject): the InternalsPluginV07 implementation
//! that copies what rFactor 2 hands it into the five mapped buffers.

use crate::cfmt::{sprintf, Arg, TRACE_CAP};

/// Bytes from each trace function's `char[0x200]` sprintf buffer to its
/// stack cookie.
const GS_DIST: usize = 0x200;
use crate::crt::{self, stricmp_eq};
use crate::dump;
use crate::host::PluginHost;
use crate::layout::*;
use crate::log::{level, Ctx, DEBUG_OUTPUT_FILE};
use crate::mapped::*;
use crate::sys::OpenMode;
use crate::vtbl;
use core::ffi::c_void;
use core::ptr::{addr_of_mut, copy_nonoverlapping as memcpy, write_bytes as memset};

/// The plugin's rF2Extended copy (+0x38) and its damage tracking table.
#[repr(C, align(8))]
pub struct ExtState(pub [u8; ext::STATE_SIZE]);

impl ExtState {
    #[inline]
    pub fn p(&mut self) -> *mut u8 {
        self.0.as_mut_ptr()
    }
}

#[repr(C)]
pub struct SharedMemoryPlugin {
    /// +0x00 vfptr (the game calls through it).
    pub vtable: *const VTable,
    /// +0x08 PluginObject::mInfo, written by the game (CreatePluginObject
    /// returns this + 8 as the PluginObject *).
    pub plugin_info: *mut c_void,
    /// +0x10..+0x30 trace timestamps (µs): telemetry begin, telemetry
    /// vehicle added, scoring, rules, multi rules.
    pub last_telemetry_update_us: f64,
    pub last_telemetry_vehicle_added_us: f64,
    pub last_scoring_update_us: f64,
    pub last_rules_update_us: f64,
    pub last_multi_rules_update_us: f64,
    /// +0x38 rF2Extended + damage tracking.
    pub ext: ExtState,
    /// +0x4518 mLastTelemetryUpdateET, +0x4520 mLastScoringUpdateET.
    pub last_telemetry_update_et: f64,
    pub last_scoring_update_et: f64,
    /// +0x4528 telemetry frame completed, +0x4529 skip already traced.
    pub telemetry_frame_completed: bool,
    pub telemetry_skip_reported: bool,
    /// +0x452c vehicles copied into the current telemetry frame.
    pub cur_telemetry_vehicle_index: i32,
    /// +0x4530 per-mID "telemetry already copied this frame".
    pub participant_telemetry_updated: [bool; MAX_MAPPED_IDS],
    /// +0x4730.. the five MappedBuffer<T>.
    pub telemetry: MappedBuffer,
    pub scoring: MappedBuffer,
    pub rules: MappedBuffer,
    pub multi_rules: MappedBuffer,
    pub extended: MappedBuffer,
    /// +0x4820 mIsMapped.
    pub is_mapped: bool,
    /// +0x4828 PluginHost.
    pub host: PluginHost,
    /// +0x4860 SCR plugin messages captured.
    pub scr_messages_captured: bool,
    /// +0x4861 last TrackRulesV01::mMessage the hosted SCR plugin produced.
    pub last_track_rules_message: [u8; track_rules::MESSAGE_LEN],
    /// +0x48c1 last TrackRulesParticipantV01::mMessage per mID.
    pub participant_track_rules_messages: [[u8; track_rules::MESSAGE_LEN]; MAX_MAPPED_IDS],
    pub cx: Ctx,
}

const FN_STARTUP: &[u8] = b"SharedMemoryPlugin::Startup";

impl SharedMemoryPlugin {
    /// operator new + constructor.
    pub unsafe fn create(cx: Ctx) -> *mut SharedMemoryPlugin {
        let p = crate::allocation::operator_new(cx.sys, core::mem::size_of::<SharedMemoryPlugin>()) as *mut SharedMemoryPlugin;
        memset(p as *mut u8, 0, core::mem::size_of::<SharedMemoryPlugin>());
        addr_of_mut!((*p).vtable).write(&VTABLE);
        // ExtendedStateTracker(): mVersion, mIs64bit, StockCarRules_DoubleFileType = -1.
        let e = (*p).ext.p();
        memcpy(PLUGIN_VERSION.as_ptr(), e.add(ext::VERSION), PLUGIN_VERSION.len());
        wr_u8(e, ext::IS_64BIT, 1);
        wr_i32(e, ext::SCR_DOUBLE_FILE_TYPE, -1);
        addr_of_mut!((*p).telemetry_frame_completed).write(true);
        addr_of_mut!((*p).last_telemetry_update_et).write(-1.0);
        addr_of_mut!((*p).last_scoring_update_et).write(-1.0);
        addr_of_mut!((*p).telemetry).write(MappedBuffer::new(TELEMETRY_NAME, TELEMETRY_MAP_SIZE, &TELEMETRY_NAMES));
        addr_of_mut!((*p).scoring).write(MappedBuffer::new(SCORING_NAME, SCORING_MAP_SIZE, &SCORING_NAMES));
        addr_of_mut!((*p).rules).write(MappedBuffer::new(RULES_NAME, RULES_MAP_SIZE, &RULES_NAMES));
        addr_of_mut!((*p).multi_rules).write(MappedBuffer::new(MULTI_RULES_NAME, MULTI_RULES_MAP_SIZE, &MULTI_RULES_NAMES));
        addr_of_mut!((*p).extended).write(MappedBuffer::new(EXTENDED_NAME, EXTENDED_MAP_SIZE, &EXTENDED_NAMES));
        addr_of_mut!((*p).host).write(PluginHost::new());
        addr_of_mut!((*p).cx).write(cx);
        p
    }

    /// Destructor.
    pub unsafe fn destruct(&mut self) {
        let cx = self.cx;
        self.host.cleanup(cx);
        self.extended.release_resources(cx);
        self.multi_rules.release_resources(cx);
        self.rules.release_resources(cx);
        self.scoring.release_resources(cx);
        self.telemetry.release_resources(cx);
    }

    // ---- helpers ------------------------------------------------------------

    /// ext.BeginUpdate(); memcpy(ext.mpBuff, &mExtended, sizeof(rF2Extended)); ext.EndUpdate();
    /// (inlined in most callbacks; no mapped check besides BeginUpdate's trace).
    unsafe fn flush_extended(&mut self) {
        let cx = self.cx;
        self.extended.begin_update(cx);
        if !self.extended.buff.is_null() {
            memcpy(self.ext.p(), self.extended.buff, EXTENDED_PAYLOAD_SIZE);
        }
        self.extended.end_update(cx);
    }

    /// MappedBuffer<rF2Extended>::ClearState(pInitialContents).
    unsafe fn clear_extended(&mut self, contents: Option<*const u8>) {
        let cx = self.cx;
        if !self.extended.mapped {
            return;
        }
        self.extended.begin_update(cx);
        match contents {
            Some(c) => memcpy(c, self.extended.buff, EXTENDED_PAYLOAD_SIZE),
            None => memset(self.extended.buff, 0, EXTENDED_PAYLOAD_SIZE),
        }
        self.extended.end_update(cx);
    }

    /// TraceBeginUpdate(buffer, lastUpdateMillis, msgPrefix).
    fn trace_begin_update(cx: Ctx, buf: &MappedBuffer, last: &mut f64, prefix: &[u8]) {
        if cx.level() > level::TIMING - 1 {
            let now = cx.ticks_now_us();
            let (b, e) = buf.versions();
            let msg = sprintf::<TRACE_CAP>(
                b"%s - Begin Update:  Delta since last update:%f  Version Begin:%d  End:%d",
                &[Arg::Str(prefix), Arg::Dbl((now - *last) * 1e-6), Arg::Int(b as i32), Arg::Int(e as i32)],
            );
            cx.dbg(level::TIMING, b"SharedMemoryPlugin::TraceBeginUpdate", 0x2c3, msg.as_bytes());
            *last = now;
            cx.gs_check(msg.written() > GS_DIST);
        }
    }

    /// ClearState: clears the mapped buffers and the tracked state.
    unsafe fn clear_state(&mut self) {
        let cx = self.cx;
        if !self.is_mapped {
            return;
        }
        self.telemetry.clear_payload(cx);
        self.scoring.clear_payload(cx);
        self.rules.clear_payload(cx);
        let e = self.ext.p();
        memset(e.add(ext::TRACKED_DAMAGES), 0, MAX_MAPPED_IDS * ext::TRACKED_DAMAGE_SIZE);
        memset(e.add(ext::DAMAGE_TRACKING), 0, MAX_MAPPED_IDS * ext::DAMAGE_TRACKING_SIZE);
        let e = self.ext.p() as *const u8;
        self.clear_extended(Some(e));
        self.clear_timings_and_counters();
    }

    /// ClearTimingsAndCounters.
    unsafe fn clear_timings_and_counters(&mut self) {
        self.telemetry_end_update();
        self.last_telemetry_update_us = 0.0;
        self.last_telemetry_vehicle_added_us = 0.0;
        self.last_scoring_update_us = 0.0;
        self.last_rules_update_us = 0.0;
        self.last_multi_rules_update_us = 0.0;
        self.cur_telemetry_vehicle_index = 0;
        self.last_telemetry_update_et = -1.0;
        self.last_scoring_update_et = -1.0;
        memset(self.participant_telemetry_updated.as_mut_ptr(), 0, MAX_MAPPED_IDS);
        memset(self.last_track_rules_message.as_mut_ptr(), 0, track_rules::MESSAGE_LEN);
        memset(self.participant_track_rules_messages.as_mut_ptr() as *mut u8, 0, MAX_MAPPED_IDS * track_rules::MESSAGE_LEN);
        self.scr_messages_captured = false;
    }

    /// TelemetryEndUpdate.
    unsafe fn telemetry_end_update(&mut self) {
        if self.telemetry_frame_completed {
            return;
        }
        let b = self.telemetry.buff;
        wr_i32(b, TEL_NUM_VEHICLES, self.cur_telemetry_vehicle_index);
        let n = rd_i32(b, TEL_NUM_VEHICLES);
        wr_i32(b, TEL_BYTES_HINT, n.wrapping_mul(telem::SIZE as i32).wrapping_add(8));
        self.telemetry.end_update(self.cx);
        self.telemetry_trace_end_update(rd_i32(b, TEL_NUM_VEHICLES));
        self.telemetry_frame_completed = true;
    }

    /// TelemetryBeginNewFrame.
    unsafe fn telemetry_begin_new_frame(&mut self, info: *const u8, delta_et: f64) {
        self.telemetry_trace_begin_update(rd_f64(info, telem::ELAPSED_TIME), delta_et);
        memset(self.participant_telemetry_updated.as_mut_ptr(), 0, MAX_MAPPED_IDS);
        self.telemetry_frame_completed = false;
        self.telemetry_skip_reported = false;
        self.cur_telemetry_vehicle_index = 0;
        self.last_telemetry_update_et = rd_f64(info, telem::ELAPSED_TIME);
        self.telemetry.begin_update(self.cx);
    }

    /// TelemetryTraceBeginUpdate.
    fn telemetry_trace_begin_update(&mut self, et: f64, delta_et: f64) {
        let cx = self.cx;
        if cx.level() < level::TIMING {
            self.last_telemetry_update_us = 0.0;
            return;
        }
        let now = cx.ticks_now_us();
        let (b, e) = self.telemetry.versions();
        let msg = sprintf::<TRACE_CAP>(
            b"TELEMETRY - Begin Update:  ET:%f  ET delta:%f  Time delta since last update:%f  Version Begin:%d  End:%d",
            &[
                Arg::Dbl(et),
                Arg::Dbl(delta_et),
                Arg::Dbl((now - self.last_telemetry_update_us) * 1e-6),
                Arg::Int(b as i32),
                Arg::Int(e as i32),
            ],
        );
        cx.dbg(level::TIMING, b"SharedMemoryPlugin::TelemetryTraceBeginUpdate", 0x201, msg.as_bytes());
        self.last_telemetry_update_us = now;
        // The buffer ends where its stack cookie begins.
        cx.gs_check(msg.written() > GS_DIST);
    }

    /// TelemetryTraceEndUpdate.
    fn telemetry_trace_end_update(&self, num_vehicles: i32) {
        let cx = self.cx;
        if cx.level() > level::TIMING - 1 {
            let (b, e) = self.telemetry.versions();
            let msg = sprintf::<TRACE_CAP>(
                b"TELEMETRY - End Update.  Telemetry chain update took %f:  Vehicles in chain: %d  Version Begin:%d  End:%d",
                &[
                    Arg::Dbl((self.last_telemetry_vehicle_added_us - self.last_telemetry_update_us) * 1e-6),
                    Arg::Int(num_vehicles),
                    Arg::Int(b as i32),
                    Arg::Int(e as i32),
                ],
            );
            cx.dbg(level::TIMING, b"SharedMemoryPlugin::TelemetryTraceEndUpdate", 0x223, msg.as_bytes());
            cx.gs_check(msg.written() > GS_DIST);
        }
    }

    /// TelemetryTraceSkipUpdate.
    unsafe fn telemetry_trace_skip_update(&mut self, info: *const u8, delta_et: f64) {
        let cx = self.cx;
        if cx.level() > level::TIMING - 1 && !self.telemetry_skip_reported {
            self.telemetry_skip_reported = true;
            // Two adjacent char[0x200] buffers, then
            // the stack cookie: the first line reaches the cookie only
            // past 0x400 bytes (beyond 0x200 it runs into the second buffer,
            // which the mismatch path memsets after the line was traced).
            let msg = sprintf::<TRACE_CAP>(
                b"TELEMETRY - Skipping update due to no changes in the input data.  Delta ET: %f  New ET: %f  Prev ET:%f  mID(new):%d",
                &[
                    Arg::Dbl(delta_et),
                    Arg::Dbl(rd_f64(info, telem::ELAPSED_TIME)),
                    Arg::Dbl(self.last_telemetry_update_et),
                    Arg::Int(rd_i32(info, telem::ID)),
                ],
            );
            cx.dbg(level::TIMING, b"SharedMemoryPlugin::TelemetryTraceSkipUpdate", 0x1df, msg.as_bytes());
            let mut smashed = msg.written() > 2 * GS_DIST;
            // Vehicle 0 of the mapped buffer against the new data (NaN compares equal).
            let v0 = self.telemetry.buff.add(TEL_VEHICLES);
            let ip = |k: usize| rd_f64(info, telem::POS + 8 * k);
            let bp = |k: usize| rd_f64(v0, telem::POS + 8 * k);
            if !(ucom_eq(ip(0), bp(0)) && ucom_eq(bp(1), ip(1)) && ucom_eq(bp(2), ip(2))) {
                let msg = sprintf::<TRACE_CAP>(
                    b"WARNING - Pos Mismatch on skip update!!!  New ET: %f  Prev ET:%f  mID(old):%d  Prev Pos: %f %f %f  New Pos %f %f %f",
                    &[
                        Arg::Dbl(rd_f64(info, telem::ELAPSED_TIME)),
                        Arg::Dbl(self.last_telemetry_update_et),
                        Arg::Int(rd_i32(v0, telem::ID)),
                        Arg::Dbl(ip(0)),
                        Arg::Dbl(ip(1)),
                        Arg::Dbl(ip(2)),
                        Arg::Dbl(bp(0)),
                        Arg::Dbl(bp(1)),
                        Arg::Dbl(bp(2)),
                    ],
                );
                cx.dbg(level::TIMING, b"SharedMemoryPlugin::TelemetryTraceSkipUpdate", 0x1f0, msg.as_bytes());
                smashed |= msg.written() > GS_DIST;
            }
            cx.gs_check(smashed);
        }
    }

    /// TelemetryTraceVehicleAdded.
    unsafe fn telemetry_trace_vehicle_added(&mut self, info: *const u8) {
        let cx = self.cx;
        let mut smashed = false;
        if cx.level() == level::ALL {
            let slot = self.telemetry.buff.add(TEL_VEHICLES + self.cur_telemetry_vehicle_index as usize * telem::SIZE);
            let same = ucom_eq(rd_f64(slot, telem::POS), rd_f64(info, telem::POS))
                && ucom_eq(rd_f64(slot, telem::POS + 8), rd_f64(info, telem::POS + 8))
                && ucom_eq(rd_f64(slot, telem::POS + 16), rd_f64(info, telem::POS + 16));
            let msg = sprintf::<TRACE_CAP>(
                b"Telemetry added - mID:%d  ET:%f  Pos Changed:%s",
                &[
                    Arg::Int(rd_i32(info, telem::ID)),
                    Arg::Dbl(rd_f64(info, telem::ELAPSED_TIME)),
                    Arg::Str(if same { b"Same" } else { b"Changed" }),
                ],
            );
            cx.dbg(level::ALL, b"SharedMemoryPlugin::TelemetryTraceVehicleAdded", 0x212, msg.as_bytes());
            smashed = msg.written() > GS_DIST;
        }
        if cx.level() > level::TIMING - 1 {
            self.last_telemetry_vehicle_added_us = cx.ticks_now_us();
        }
        cx.gs_check(smashed);
    }

    /// ScoringTraceBeginUpdate.
    fn scoring_trace_begin_update(&mut self) {
        let cx = self.cx;
        if cx.level() > level::TIMING - 1 {
            let mut last = self.last_scoring_update_us;
            Self::trace_begin_update(cx, &self.scoring, &mut last, b"SCORING");
            self.last_scoring_update_us = last;
            let msg = sprintf::<TRACE_CAP>(
                b"SCORING - Scoring ET:%f  Telemetry ET:%f",
                &[Arg::Dbl(self.last_scoring_update_et), Arg::Dbl(self.last_telemetry_update_et)],
            );
            cx.dbg(level::TIMING, b"SharedMemoryPlugin::ScoringTraceBeginUpdate", 0x2b2, msg.as_bytes());
            cx.gs_check(msg.written() > GS_DIST);
        }
    }

    /// UpdateInRealtimeFC.
    unsafe fn update_in_realtime_fc(&mut self, in_realtime: bool) {
        if !self.is_mapped {
            return;
        }
        self.cx.dbg(
            level::SYNCHRONIZATION,
            b"SharedMemoryPlugin::UpdateInRealtimeFC",
            0x19a,
            if in_realtime { b"Entering Realtime" } else { b"Exiting Realtime" },
        );
        wr_u8(self.ext.p(), ext::IN_REALTIME_FC, in_realtime as u8);
        self.flush_extended();
    }

    /// UpdateThreadState.
    unsafe fn update_thread_state(&mut self, ty: i32, started: bool) {
        let off = if ty != 0 { ext::SIMULATION_THREAD_STARTED } else { ext::MULTIMEDIA_THREAD_STARTED };
        wr_u8(self.ext.p(), off, started as u8);
        self.flush_extended();
    }

    /// ExtendedStateTracker::ProcessTelemetryUpdate.
    unsafe fn ext_process_telemetry(&mut self, info: *const u8) {
        let e = self.ext.p();
        let id = mapped_id(rd_i32(info, telem::ID));
        let dt = ext::DAMAGE_TRACKING + id * ext::DAMAGE_TRACKING_SIZE;
        let impact_et = rd_f64(info, telem::LAST_IMPACT_ET);
        // Strictly greater; NaN fails.
        if impact_et > rd_f64(e, dt + 8) && impact_et > rd_f64(e, dt) {
            let td = ext::TRACKED_DAMAGES + id * ext::TRACKED_DAMAGE_SIZE;
            let mag = rd_f64(info, telem::LAST_IMPACT_MAGNITUDE);
            let cur = rd_f64(e, td);
            wr_f64(e, td, if mag > cur { mag } else { cur }); // the larger; cur when unordered
            wr_f64(e, td + 8, mag + rd_f64(e, td + 8));
            wr_f64(e, dt, impact_et);
        }
    }

    /// ExtendedStateTracker::ProcessScoringUpdate: reset the
    /// damage of cars stopped in their pit stall (the game's ScoringInfoV01).
    unsafe fn ext_process_scoring(&mut self, info: *const u8) {
        let e = self.ext.p();
        let mut i = 0;
        while i < rd_i32(info, scoring_info::NUM_VEHICLES) {
            let v = rd_ptr(info, scoring_info::VEHICLE).add(i as usize * vehicle_scoring::SIZE);
            if rd_u8(v, vehicle_scoring::PIT_STATE) == vehicle_scoring::PIT_STATE_STOPPED {
                let id = mapped_id(rd_i32(v, vehicle_scoring::ID));
                let td = ext::TRACKED_DAMAGES + id * ext::TRACKED_DAMAGE_SIZE;
                let dt = ext::DAMAGE_TRACKING + id * ext::DAMAGE_TRACKING_SIZE;
                wr_u64(e, td, 0);
                wr_u64(e, td + 8, 0);
                wr_u64(e, dt, 0);
                wr_u64(e, dt + 8, rd_u64(info, scoring_info::CURRENT_ET));
            }
            i += 1;
        }
    }

    /// ExtendedStateTracker::CaptureSessionTransition, from
    /// the mapped scoring buffer.
    unsafe fn ext_capture_session_transition(&mut self) {
        let e = self.ext.p();
        let sc = self.scoring.buff;
        let info = sc.add(SC_INFO);
        wr_u8(e, ext::ST_GAME_PHASE, rd_u8(info, scoring_info::GAME_PHASE));
        wr_i32(e, ext::ST_SESSION, rd_i32(info, scoring_info::SESSION));
        let n = rd_i32(info, scoring_info::NUM_VEHICLES).min(MAX_MAPPED_VEHICLES as i32);
        wr_i32(e, ext::ST_NUM_SCORING_VEHICLES, n);
        for i in 0..n.max(0) as usize {
            let v = sc.add(SC_VEHICLES + i * vehicle_scoring::SIZE);
            let o = ext::ST_VEHICLES + i * ext::ST_VEHICLE_SIZE;
            wr_i32(e, o, rd_i32(v, vehicle_scoring::ID));
            wr_u8(e, o + 6, rd_u8(v, vehicle_scoring::FINISH_STATUS));
            wr_u8(e, o + 5, rd_u8(v, vehicle_scoring::IS_PLAYER));
            wr_u8(e, o + 4, rd_u8(v, vehicle_scoring::PLACE));
        }
    }

    /// CaptureSCRPluginMessages.
    unsafe fn capture_scr_plugin_messages(&mut self, info: *const u8) {
        use track_rules as tr;
        const FUNC: &[u8] = b"SharedMemoryPlugin::CaptureSCRPluginMessages";
        let cx = self.cx;
        if !self.host.loaded {
            return;
        }
        if rd_i8(info, tr::YELLOW_FLAG_STATE) != 0 {
            if rd_u8(info, tr::MESSAGE) != 0 {
                cx.dbg_s(level::CRITICAL_INFO, FUNC, 0x36f, b"Non empty TR message: ", cstr(info.add(tr::MESSAGE)));
                crt::strcpy_s(cx.sys, self.last_track_rules_message.as_mut_ptr(), tr::MESSAGE_LEN, info.add(tr::MESSAGE));
            }
            let mut i = 0;
            while i < rd_i32(info, tr::NUM_PARTICIPANTS) {
                let p = rd_ptr(info, tr::PARTICIPANT).add(i as usize * tr::PARTICIPANT_SIZE);
                if rd_u8(p, tr::PARTICIPANT_MESSAGE) != 0 {
                    cx.dbg_s(
                        level::CRITICAL_INFO,
                        FUNC,
                        0x375,
                        b"Non empty TR Participant message: ",
                        cstr(p.add(tr::PARTICIPANT_MESSAGE)),
                    );
                    cx.dbg_d(level::CRITICAL_INFO, FUNC, 0x376, b"Participant mID: ", rd_i32(p, tr::PARTICIPANT_ID));
                    let id = mapped_id(rd_i32(p, tr::PARTICIPANT_ID));
                    crt::strcpy_s(
                        cx.sys,
                        self.participant_track_rules_messages[id].as_mut_ptr(),
                        tr::MESSAGE_LEN,
                        p.add(tr::PARTICIPANT_MESSAGE),
                    );
                }
                i += 1;
            }
            self.scr_messages_captured = true;
            return;
        }
        if self.scr_messages_captured {
            // memset(mLastTrackRulesMessage, 0, 0x60 + 512 * 0x60)
            memset(self.last_track_rules_message.as_mut_ptr(), 0, tr::MESSAGE_LEN);
            memset(self.participant_track_rules_messages.as_mut_ptr() as *mut u8, 0, MAX_MAPPED_IDS * tr::MESSAGE_LEN);
            self.scr_messages_captured = false;
        }
    }

    // ---- InternalsPluginV07 implementation -----------------------------------

    /// +0x08 Startup(long version).
    pub unsafe fn startup(&mut self, version: i32) {
        let cx = self.cx;
        cx.dbg_s(
            level::CRITICAL_INFO,
            FN_STARTUP,
            0xae,
            b"Starting rFactor 2 Shared Memory Map Plugin 64bit Version:",
            &PLUGIN_VERSION[..PLUGIN_VERSION.len() - 1],
        );
        cx.dbg(level::CRITICAL_INFO, FN_STARTUP, 0xaf, b"Configuration:");
        cx.dbg_d(level::CRITICAL_INFO, FN_STARTUP, 0xb0, b"EnableStockCarRulesPlugin:", cx.g().enable_stock_car_rules as i32);
        cx.dbg_d(level::CRITICAL_INFO, FN_STARTUP, 0xb1, b"DebugOutputLevel:", cx.g().debug_level);
        cx.dbg_d(level::CRITICAL_INFO, FN_STARTUP, 0xb2, b"DebugISIInternals:", cx.g().debug_isi_internals as i32);
        cx.dbg_d(
            level::CRITICAL_INFO,
            FN_STARTUP,
            0xb3,
            b"DedicatedServerMapGlobally:",
            cx.g().dedicated_server_map_globally as i32,
        );
        let line = sprintf::<0x50>(b"-STARTUP- (version %.3f)", &[Arg::Dbl(((version as f32) * 0.001f32) as f64)]);
        if cx.g().debug_isi_internals {
            dump::write_to_all_output_files(cx, OpenMode::Write, line.as_bytes());
        }
        let global = cx.g().dedicated_server_map_globally;
        if !self.telemetry.initialize(cx, global) {
            cx.dbg(level::ERRORS, FN_STARTUP, 0xba, b"Failed to initialize telemetry mapping");
            return;
        }
        if !self.scoring.initialize(cx, global) {
            cx.dbg(level::ERRORS, FN_STARTUP, 0xbf, b"Failed to initialize scoring mapping");
            return;
        }
        if !self.rules.initialize(cx, global) {
            cx.dbg(level::ERRORS, FN_STARTUP, 0xc4, b"Failed to initialize rules mapping");
            return;
        }
        if !self.multi_rules.initialize(cx, global) {
            cx.dbg(level::ERRORS, FN_STARTUP, 0xc9, b"Failed to initialize multi rules mapping");
            return;
        }
        if !self.extended.initialize(cx, global) {
            cx.dbg(level::ERRORS, FN_STARTUP, 0xce, b"Failed to initialize extended mapping");
            return;
        }
        self.is_mapped = true;
        self.clear_state();
        // MappedBuffer<rF2MultiRules>::ClearState(nullptr).
        if self.multi_rules.mapped {
            self.multi_rules.begin_update(cx);
            memset(self.multi_rules.buff, 0, MULTI_RULES_PAYLOAD_SIZE);
            self.multi_rules.end_update(cx);
        }
        cx.dbg(level::CRITICAL_INFO, FN_STARTUP, 0xd9, b"Files mapped successfully");
        if cx.g().debug_level != 0 {
            let sizes: [(&[u8], i32, i32); 5] = [
                (b"Size of telemetry buffer:", TELEMETRY_MAP_SIZE as i32, 0xde),
                (b"Size of scoring buffer:", SCORING_MAP_SIZE as i32, 0xe5),
                (b"Size of rules buffer:", RULES_MAP_SIZE as i32, 0xec),
                (b"Size of multi rules buffer:", MULTI_RULES_MAP_SIZE as i32, 0xf3),
                (b"Size of extended buffer:", EXTENDED_MAP_SIZE as i32, 0xfa),
            ];
            for (msg, size, line) in sizes {
                let mut num = [0u8; 20];
                let n = crt::itoa10(size, &mut num);
                cx.dbg_ss(level::CRITICAL_INFO, FN_STARTUP, line, msg, &num[..n], b"bytes.");
            }
        }
        let enable_scr = cx.g().enable_stock_car_rules;
        self.host.initialize(cx, enable_scr);
        self.host.startup(version);
        if !cx.g().enable_stock_car_rules {
            return;
        }
        let e = self.ext.p();
        wr_u8(e, ext::SCR_IS_HOSTED, self.host.loaded as u8);
        if self.host.loaded {
            wr_i32(e, ext::SCR_DOUBLE_FILE_TYPE, self.host.double_file_type);
        }
        self.flush_extended();
    }

    /// +0x10 Shutdown().
    pub unsafe fn shutdown(&mut self) {
        let cx = self.cx;
        if cx.g().debug_isi_internals {
            dump::write_to_all_output_files(cx, OpenMode::Append, b"-SHUTDOWN-");
        }
        if self.is_mapped {
            self.telemetry_end_update();
        }
        self.host.shutdown();
        self.host.cleanup(cx);
        cx.dbg(level::CRITICAL_INFO, b"SharedMemoryPlugin::Shutdown", 0x117, b"Shutting down");
        let g = cx.g();
        for slot in [&mut g.debug_file, &mut g.telemetry_dump, &mut g.scoring_dump] {
            match core::mem::replace(slot, crate::crt::StreamSlot::None) {
                crate::crt::StreamSlot::Open(f) => {
                    f.close(cx.sys);
                }
                // fclose of an already closed stream: EOF, nothing else.
                crate::crt::StreamSlot::Stale | crate::crt::StreamSlot::None => {}
            }
        }
        self.is_mapped = false;
        self.telemetry.clear_payload(cx);
        self.telemetry.release_resources(cx);
        self.scoring.clear_payload(cx);
        self.scoring.release_resources(cx);
        self.rules.clear_payload(cx);
        self.rules.release_resources(cx);
        self.multi_rules.clear_payload(cx);
        self.multi_rules.release_resources(cx);
        self.extended.clear_payload(cx);
        self.extended.release_resources(cx);
    }

    /// +0x28 StartSession().
    pub unsafe fn start_session(&mut self) {
        let cx = self.cx;
        if cx.g().debug_isi_internals {
            dump::write_to_all_output_files(cx, OpenMode::Append, b"--STARTSESSION--");
        }
        if !self.is_mapped {
            return;
        }
        self.host.start_session();
        cx.dbg(level::TIMING, b"SharedMemoryPlugin::StartSession", 0x16c, b"SESSION - Started.");
        let e = self.ext.p();
        wr_u8(e, ext::SESSION_STARTED, 1);
        wr_u64(e, ext::TICKS_SESSION_STARTED, cx.sys.tick_count64());
        self.ext_capture_session_transition();
        self.clear_state();
    }

    /// +0x30 EndSession().
    pub unsafe fn end_session(&mut self) {
        let cx = self.cx;
        if cx.g().debug_isi_internals {
            dump::write_to_all_output_files(cx, OpenMode::Append, b"--ENDSESSION--");
        }
        if !self.is_mapped {
            return;
        }
        self.telemetry_end_update();
        self.host.end_session();
        cx.dbg(level::TIMING, b"SharedMemoryPlugin::EndSession", 0x187, b"SESSION - Ended.");
        let e = self.ext.p();
        wr_u8(e, ext::SESSION_STARTED, 0);
        wr_u64(e, ext::TICKS_SESSION_ENDED, cx.sys.tick_count64());
        self.ext_capture_session_transition();
        self.flush_extended();
    }

    /// +0x38 EnterRealtime().
    pub unsafe fn enter_realtime(&mut self) {
        let cx = self.cx;
        if !self.is_mapped {
            return;
        }
        if cx.g().debug_isi_internals {
            dump::write_to_all_output_files(cx, OpenMode::Append, b"---ENTERREALTIME---");
        }
        self.host.enter_realtime();
        self.update_in_realtime_fc(true);
    }

    /// +0x40 ExitRealtime().
    pub unsafe fn exit_realtime(&mut self) {
        let cx = self.cx;
        if !self.is_mapped {
            return;
        }
        self.telemetry_end_update();
        if cx.g().debug_isi_internals {
            dump::write_to_all_output_files(cx, OpenMode::Append, b"---EXITREALTIME---");
        }
        self.host.exit_realtime();
        self.update_in_realtime_fc(false);
    }

    /// +0x50 UpdateScoring(const ScoringInfoV01 &).
    pub unsafe fn update_scoring(&mut self, info: *const u8) {
        let cx = self.cx;
        dump::write_scoring_internals(cx, info);
        if !self.is_mapped {
            return;
        }
        self.host.update_scoring(info);
        self.last_scoring_update_et = rd_f64(info, scoring_info::CURRENT_ET);
        self.scoring_trace_begin_update();
        // Warn unless telemetry ET >= scoring ET (NaN warns).
        if !(self.last_telemetry_update_et >= self.last_scoring_update_et) {
            cx.dbg(
                level::WARNINGS,
                b"SharedMemoryPlugin::UpdateScoring",
                0x2da,
                b"WARNING: Scoring update is ahead of telemetry.",
            );
        }
        self.scoring.begin_update(cx);
        let b = self.scoring.buff;
        memcpy(info, b.add(SC_INFO), scoring_info::SIZE);
        let num = rd_i32(info, scoring_info::NUM_VEHICLES);
        if num > 0x7f {
            cx.dbg(
                level::ERRORS,
                b"SharedMemoryPlugin::UpdateScoring",
                0x2e1,
                b"ERROR: Scoring exceeded maximum of allowed mapped vehicles.",
            );
        }
        let n = num.min(MAX_MAPPED_VEHICLES as i32);
        let veh = rd_ptr(info, scoring_info::VEHICLE);
        for i in 0..n.max(0) as usize {
            memcpy(veh.add(i * vehicle_scoring::SIZE), b.add(SC_VEHICLES + i * vehicle_scoring::SIZE), vehicle_scoring::SIZE);
        }
        wr_i32(b, SC_BYTES_HINT, n.wrapping_mul(vehicle_scoring::SIZE as i32).wrapping_add(SC_VEHICLES as i32));
        self.scoring.end_update(cx);
        self.ext_process_scoring(info);
        self.flush_extended();
    }

    /// +0x60 UpdateTelemetry(const TelemInfoV01 &).
    pub unsafe fn update_telemetry(&mut self, info: *const u8) {
        let cx = self.cx;
        dump::write_telemetry_internals(cx, info);
        if !self.is_mapped {
            return;
        }
        self.host.update_telemetry(info);
        let et = rd_f64(info, telem::ELAPSED_TIME);
        let last = self.last_telemetry_update_et;
        let delta = et - last;
        let mut new_frame = true;
        // |delta| against 0.0199: NaN takes the "same frame" path.
        if !(delta.abs() >= 0.0199) {
            self.last_telemetry_update_et = if last < et { last } else { et }; // the smaller; et when unordered
            if self.cur_telemetry_vehicle_index < MAX_MAPPED_VEHICLES as i32 {
                new_frame = false;
            }
        }
        if new_frame {
            self.telemetry_end_update();
            self.telemetry_begin_new_frame(info, delta);
        }
        if self.telemetry_frame_completed {
            self.telemetry_trace_skip_update(info, delta);
            return;
        }
        let id = mapped_id(rd_i32(info, telem::ID));
        if !self.participant_telemetry_updated[id] {
            if self.cur_telemetry_vehicle_index > 0x7f {
                cx.dbg(
                    level::ERRORS,
                    b"SharedMemoryPlugin::UpdateTelemetry",
                    0x285,
                    b"TELEMETRY - Exceeded maximum of allowed mapped vehicles.",
                );
                return;
            }
            self.ext_process_telemetry(info);
            self.participant_telemetry_updated[id] = true;
            self.telemetry_trace_vehicle_added(info);
            let b = self.telemetry.buff;
            memcpy(info, b.add(TEL_VEHICLES + self.cur_telemetry_vehicle_index as usize * telem::SIZE), telem::SIZE);
            self.cur_telemetry_vehicle_index += 1;
            let sc_num = rd_i32(self.scoring.buff, SC_INFO + scoring_info::NUM_VEHICLES);
            if sc_num != self.cur_telemetry_vehicle_index {
                return;
            }
        } else {
            self.telemetry_trace_skip_update(info, delta);
        }
        self.telemetry_end_update();
    }

    /// +0xb8 SetPhysicsOptions(PhysicsOptionsV01 &).
    pub unsafe fn set_physics_options(&mut self, opts: *mut u8) {
        if !self.is_mapped {
            return;
        }
        self.host.set_physics_options(opts);
        self.cx.dbg(level::TIMING, b"SharedMemoryPlugin::SetPhysicsOptions", 0x39d, b"PHYSICS - Updated.");
        memcpy(opts, self.ext.p().add(ext::PHYSICS), PHYSICS_OPTIONS_SIZE);
        self.flush_extended();
    }

    /// +0xd0 WantsToDisplayMessage(MessageInfoV01 &).
    pub unsafe fn wants_to_display_message(&mut self, msg: *mut u8) -> u8 {
        const FUNC: &[u8] = b"SharedMemoryPlugin::WantsToDisplayMessage";
        let cx = self.cx;
        if !self.is_mapped {
            return 0;
        }
        cx.dbg(level::TIMING, FUNC, 0x2fb, b"WantsToDisplayMessage - Invoked.");
        let r = self.host.wants_to_display_message(msg);
        if r != 0 && *msg != 0 {
            cx.dbg_s(level::CRITICAL_INFO, FUNC, 0x302, b"New displayed messsage captured: ", cstr(msg));
            crt::strcpy_s(cx.sys, self.ext.p().add(ext::DISPLAYED_MESSAGE), MESSAGE_TEXT_LEN, msg);
            self.flush_extended();
        }
        r
    }

    /// +0xd8 SetEnvironment(const EnvironmentInfoV01 &).
    pub unsafe fn set_environment(&mut self, info: *const u8) {
        if self.is_mapped {
            self.cx.dbg(level::TIMING, b"SharedMemoryPlugin::SetEnvironment", 0x429, b"SetEnvironment - Invoked.");
            self.host.set_environment(info);
        }
    }

    /// +0x138 ThreadStarted(long type).
    pub unsafe fn thread_started(&mut self, ty: i32) {
        if self.is_mapped {
            self.host.thread_started(ty);
            self.cx.dbg(
                level::SYNCHRONIZATION,
                b"SharedMemoryPlugin::ThreadStarted",
                800,
                if ty == 0 { b"Multimedia thread started" } else { b"Simulation thread started" },
            );
            self.update_thread_state(ty, true);
        }
    }

    /// +0x140 ThreadStopping(long type).
    pub unsafe fn thread_stopping(&mut self, ty: i32) {
        if self.is_mapped {
            self.host.thread_stopping(ty);
            self.cx.dbg(
                level::SYNCHRONIZATION,
                b"SharedMemoryPlugin::ThreadStopping",
                0x32b,
                if ty == 0 { b"Multimedia thread stopped" } else { b"Simulation thread stopped" },
            );
            self.update_thread_state(ty, false);
        }
    }

    /// +0x148 GetCustomVariable(long i, CustomVariableV01 &var).
    pub unsafe fn get_custom_variable(&mut self, i: i32, var: *mut u8) -> u8 {
        let cx = self.cx;
        // The caption is only read when the trace is written.
        if cx.level() >= level::TIMING {
            cx.dbg_s(
                level::TIMING,
                b"SharedMemoryPlugin::GetCustomVariable",
                0x3c6,
                b"GetCustomVariable - Invoked:  mCaption - ",
                cstr(var),
            );
        }
        let (name, settings): (&[u8], u8) = match i {
            0 => (b" Enabled\0", 2),
            1 => (b"EnableStockCarRulesPlugin\0", 2),
            2 => (b"DebugOutputLevel\0", 8),
            3 => (b"DebugISIInternals\0", 2),
            4 => (b"DedicatedServerMapGlobally\0", 2),
            _ => return 0,
        };
        crt::strcpy_s(cx.sys, var.add(custom_var::CAPTION), custom_var::CAPTION_LEN, name.as_ptr());
        // mNumSettings = settings, mCurrentSetting = 0 (one 8-byte store).
        memset(var.add(custom_var::NUM_SETTINGS), 0, 8);
        *var.add(custom_var::NUM_SETTINGS) = settings;
        1
    }

    /// +0x150 AccessCustomVariable(CustomVariableV01 &var).
    pub unsafe fn access_custom_variable(&mut self, var: *mut u8) {
        let cx = self.cx;
        cx.dbg(level::TIMING, b"SharedMemoryPlugin::AccessCustomVariable", 0x3ee, b"AccessCustomVariable - Invoked.");
        let caption = cstr(var);
        let cur = rd_i32(var, custom_var::CURRENT_SETTING);
        let g = cx.g();
        if stricmp_eq(caption, b" Enabled") {
        } else if stricmp_eq(caption, b"EnableStockCarRulesPlugin") {
            g.enable_stock_car_rules = cur != 0;
        } else if stricmp_eq(caption, b"DebugOutputLevel") {
            g.debug_level = if cur < 7 { cur.max(0) } else { 7 };
            if g.debug_level != 0 {
                if let Err(e) = cx.sys.file_delete(DEBUG_OUTPUT_FILE) {
                    g.errno = crt::errno_from_win32(e);
                    cx.sys.set_crt_errno(g.errno);
                }
            }
        } else if stricmp_eq(caption, b"DebugISIInternals") {
            g.debug_isi_internals = cur != 0;
        } else if stricmp_eq(caption, b"DedicatedServerMapGlobally") {
            g.dedicated_server_map_globally = cur != 0;
        }
    }

    /// +0x158 GetCustomVariableSetting(CustomVariableV01 &var, long i,
    /// CustomSettingV01 &setting).
    pub unsafe fn get_custom_variable_setting(&mut self, var: *mut u8, i: i32, setting: *mut u8) {
        let cx = self.cx;
        cx.dbg(
            level::TIMING,
            b"SharedMemoryPlugin::GetCustomVariableSetting",
            0x405,
            b"GetCustomVariableSetting - Invoked.",
        );
        let caption = cstr(var);
        if !stricmp_eq(caption, b" Enabled") && !stricmp_eq(caption, b"EnableStockCarRulesPlugin") {
            if stricmp_eq(caption, b"DebugOutputLevel") {
                // sprintf_s(setting.mName, 0x80, "%d%%", i)
                let s = sprintf::<0x80>(b"%d%%", &[Arg::Int(i)]);
                let b = s.as_bytes();
                memcpy(b.as_ptr(), setting, b.len());
                *setting.add(b.len()) = 0;
                return;
            }
            if !stricmp_eq(caption, b"DebugISIInternals") && !stricmp_eq(caption, b"DedicatedServerMapGlobally") {
                return;
            }
        }
        let v: &[u8] = if i != 0 { b"True\0" } else { b"False\0" };
        crt::strcpy_s(cx.sys, setting, CUSTOM_SETTING_NAME_LEN, v.as_ptr());
    }

    /// +0x168 AccessMultiSessionRules(MultiSessionRulesV01 &).
    pub unsafe fn access_multi_session_rules(&mut self, info: *mut u8) -> u8 {
        use multi_rules as m;
        let cx = self.cx;
        if !self.is_mapped {
            return 0;
        }
        let r = self.host.access_multi_session_rules(info);
        let mut last = self.last_multi_rules_update_us;
        Self::trace_begin_update(cx, &self.multi_rules, &mut last, b"MULTI RULES");
        self.last_multi_rules_update_us = last;
        self.multi_rules.begin_update(cx);
        let b = self.multi_rules.buff;
        memcpy(info, b.add(MR_RULES), m::SIZE);
        let num = rd_i32(info, m::NUM_PARTICIPANTS);
        if num > 0x7f {
            cx.dbg(
                level::ERRORS,
                b"SharedMemoryPlugin::AccessMultiSessionRules",
                0x3b6,
                b"ERROR: Multi rules exceeded maximum of allowed mapped vehicles.",
            );
        }
        let n = num.min(MAX_MAPPED_VEHICLES as i32);
        let parts = rd_ptr(info, m::PARTICIPANT);
        for i in 0..n.max(0) as usize {
            memcpy(parts.add(i * m::PARTICIPANT_SIZE), b.add(MR_PARTICIPANTS + i * m::PARTICIPANT_SIZE), m::PARTICIPANT_SIZE);
        }
        wr_i32(b, MR_BYTES_HINT, n.wrapping_mul(m::PARTICIPANT_SIZE as i32).wrapping_add(MR_PARTICIPANTS as i32));
        self.multi_rules.end_update(cx);
        r
    }

    /// +0x178 AccessTrackRules(TrackRulesV01 &).
    pub unsafe fn access_track_rules(&mut self, info: *mut u8) -> u8 {
        use track_rules as tr;
        const FUNC: &[u8] = b"SharedMemoryPlugin::AccessTrackRules";
        let cx = self.cx;
        if !self.is_mapped {
            return 0;
        }
        let r = self.host.access_track_rules(info);
        let mut last = self.last_rules_update_us;
        Self::trace_begin_update(cx, &self.rules, &mut last, b"RULES");
        self.last_rules_update_us = last;
        self.capture_scr_plugin_messages(info);
        self.rules.begin_update(cx);
        let b = self.rules.buff;
        memcpy(info, b.add(RU_RULES), tr::SIZE);
        let inject = self.host.loaded && rd_i8(info, tr::YELLOW_FLAG_STATE) != 0;
        if inject && self.last_track_rules_message[0] != 0 {
            crt::strcpy_s(cx.sys, b.add(RU_RULES + tr::MESSAGE), tr::MESSAGE_LEN, self.last_track_rules_message.as_ptr());
        }
        let num_actions = rd_i32(info, tr::NUM_ACTIONS);
        if num_actions > 0x7f {
            cx.dbg(level::ERRORS, FUNC, 0x348, b"ERROR: Rules exceeded maximum of allowed actions.");
        }
        let na = num_actions.min(MAX_MAPPED_VEHICLES as i32);
        let actions = rd_ptr(info, tr::ACTION);
        for i in 0..na.max(0) as usize {
            memcpy(actions.add(i * tr::ACTION_SIZE), b.add(RU_ACTIONS + i * tr::ACTION_SIZE), tr::ACTION_SIZE);
        }
        let num_parts = rd_i32(info, tr::NUM_PARTICIPANTS);
        if num_parts > 0x7f {
            cx.dbg(level::ERRORS, FUNC, 0x350, b"ERROR: Rules exceeded maximum of allowed mapped vehicles.");
        }
        let np = num_parts.min(MAX_MAPPED_VEHICLES as i32);
        let parts = rd_ptr(info, tr::PARTICIPANT);
        for i in 0..np.max(0) as usize {
            let src = parts.add(i * tr::PARTICIPANT_SIZE);
            let dst = b.add(RU_PARTICIPANTS + i * tr::PARTICIPANT_SIZE);
            memcpy(src, dst, tr::PARTICIPANT_SIZE);
            if inject {
                let id = mapped_id(rd_i32(src, tr::PARTICIPANT_ID));
                crt::strcpy_s(
                    cx.sys,
                    dst.add(tr::PARTICIPANT_MESSAGE),
                    tr::MESSAGE_LEN,
                    self.participant_track_rules_messages[id].as_ptr(),
                );
            }
        }
        wr_i32(b, RU_BYTES_HINT, np.wrapping_mul(tr::PARTICIPANT_SIZE as i32).wrapping_add(RU_PARTICIPANTS as i32));
        self.rules.end_update(cx);
        r
    }

    /// +0x188 AccessPitMenu(PitMenuV01 &).
    pub unsafe fn access_pit_menu(&mut self, info: *mut u8) -> u8 {
        if !self.is_mapped {
            return 0;
        }
        self.cx.dbg(level::TIMING, b"SharedMemoryPlugin::AccessPitMenu", 0x390, b"AccessPitMenu - Invoked.");
        self.host.access_pit_menu(info)
    }
}

/// Inequality in which unordered compares as equal.
#[inline]
fn ucom_eq(a: f64, b: f64) -> bool {
    a == b || a.is_nan() || b.is_nan()
}

// ---- the vtable the game calls ----------------------------------------------

#[repr(C)]
pub struct VTable(pub [*const (); vtbl::SLOTS]);
unsafe impl Sync for VTable {}

type This = *mut SharedMemoryPlugin;

unsafe extern "C" fn v_destructor(this: This, flags: u32) -> This {
    (*this).destruct();
    if flags & 1 != 0 {
        let sys = (*this).cx.sys;
        sys.free(this as *mut u8);
    }
    this
}
unsafe extern "C" fn v_startup(this: This, version: i32) {
    (*this).startup(version)
}
unsafe extern "C" fn v_shutdown(this: This) {
    (*this).shutdown()
}
unsafe extern "C" fn v_start_session(this: This) {
    (*this).start_session()
}
unsafe extern "C" fn v_end_session(this: This) {
    (*this).end_session()
}
unsafe extern "C" fn v_enter_realtime(this: This) {
    (*this).enter_realtime()
}
unsafe extern "C" fn v_exit_realtime(this: This) {
    (*this).exit_realtime()
}
unsafe extern "C" fn v_update_scoring(this: This, info: *const u8) {
    (*this).update_scoring(info)
}
unsafe extern "C" fn v_wants_telemetry_updates(_: This) -> i32 {
    2
}
unsafe extern "C" fn v_update_telemetry(this: This, info: *const u8) {
    (*this).update_telemetry(info)
}
unsafe extern "C" fn v_set_physics_options(this: This, opts: *mut u8) {
    (*this).set_physics_options(opts)
}
unsafe extern "C" fn v_wants_to_display_message(this: This, msg: *mut u8) -> u8 {
    (*this).wants_to_display_message(msg)
}
unsafe extern "C" fn v_set_environment(this: This, info: *const u8) {
    (*this).set_environment(info)
}
unsafe extern "C" fn v_thread_started(this: This, ty: i32) {
    (*this).thread_started(ty)
}
unsafe extern "C" fn v_thread_stopping(this: This, ty: i32) {
    (*this).thread_stopping(ty)
}
unsafe extern "C" fn v_get_custom_variable(this: This, i: i32, var: *mut u8) -> u8 {
    (*this).get_custom_variable(i, var)
}
unsafe extern "C" fn v_access_custom_variable(this: This, var: *mut u8) {
    (*this).access_custom_variable(var)
}
unsafe extern "C" fn v_get_custom_variable_setting(this: This, var: *mut u8, i: i32, setting: *mut u8) {
    (*this).get_custom_variable_setting(var, i, setting)
}
unsafe extern "C" fn v_access_multi_session_rules(this: This, info: *mut u8) -> u8 {
    (*this).access_multi_session_rules(info)
}
unsafe extern "C" fn v_access_track_rules(this: This, info: *mut u8) -> u8 {
    (*this).access_track_rules(info)
}
unsafe extern "C" fn v_access_pit_menu(this: This, info: *mut u8) -> u8 {
    (*this).access_pit_menu(info)
}
/// An empty virtual.
unsafe extern "C" fn v_nop(_: This) {}
/// A virtual that returns bool false.
unsafe extern "C" fn v_false(_: This) -> u8 {
    0
}
/// A virtual that returns bool true.
unsafe extern "C" fn v_true(_: This) -> u8 {
    1
}

const NOP: *const () = v_nop as *const ();
const FALSE: *const () = v_false as *const ();
const TRUE: *const () = v_true as *const ();

/// SharedMemoryPlugin::vftable, slot by slot.
pub static VTABLE: VTable = VTable([
    v_destructor as *const (),          // 0x000 scalar deleting destructor
    v_startup as *const (),             // 0x008 Startup
    v_shutdown as *const (),            // 0x010 Shutdown
    NOP,                                // 0x018 Load
    NOP,                                // 0x020 Unload
    v_start_session as *const (),       // 0x028 StartSession
    v_end_session as *const (),         // 0x030 EndSession
    v_enter_realtime as *const (),      // 0x038 EnterRealtime
    v_exit_realtime as *const (),       // 0x040 ExitRealtime
    TRUE,                               // 0x048 WantsScoringUpdates
    v_update_scoring as *const (),      // 0x050 UpdateScoring
    v_wants_telemetry_updates as *const (), // 0x058 WantsTelemetryUpdates (2 = all vehicles)
    v_update_telemetry as *const (),    // 0x060 UpdateTelemetry
    FALSE,                              // 0x068 WantsGraphicsUpdates
    NOP,                                // 0x070 UpdateGraphics(V01)
    FALSE,                              // 0x078 RequestCommentary
    FALSE,                              // 0x080 HasHardwareInputs
    NOP,                                // 0x088 UpdateHardware
    NOP,                                // 0x090 EnableHardware
    NOP,                                // 0x098 DisableHardware
    FALSE,                              // 0x0a0 CheckHWControl
    FALSE,                              // 0x0a8 ForceFeedback
    NOP,                                // 0x0b0 Error
    v_set_physics_options as *const (), // 0x0b8 SetPhysicsOptions
    FALSE,                              // 0x0c0 WantsToViewVehicle
    NOP,                                // 0x0c8 UpdateGraphics(V02)
    v_wants_to_display_message as *const (), // 0x0d0 WantsToDisplayMessage
    v_set_environment as *const (),     // 0x0d8 SetEnvironment
    NOP,                                // 0x0e0 InitScreen
    NOP,                                // 0x0e8 UninitScreen
    NOP,                                // 0x0f0 DeactivateScreen
    NOP,                                // 0x0f8 ReactivateScreen
    NOP,                                // 0x100 RenderScreenBeforeOverlays
    NOP,                                // 0x108 RenderScreenAfterOverlays
    NOP,                                // 0x110 PreReset
    NOP,                                // 0x118 PostReset
    FALSE,                              // 0x120 InitCustomControl
    FALSE,                              // 0x128 WantsWeatherAccess
    FALSE,                              // 0x130 AccessWeather
    v_thread_started as *const (),      // 0x138 ThreadStarted
    v_thread_stopping as *const (),     // 0x140 ThreadStopping
    v_get_custom_variable as *const (), // 0x148 GetCustomVariable
    v_access_custom_variable as *const (), // 0x150 AccessCustomVariable
    v_get_custom_variable_setting as *const (), // 0x158 GetCustomVariableSetting
    TRUE,                               // 0x160 WantsMultiSessionRulesAccess
    v_access_multi_session_rules as *const (), // 0x168 AccessMultiSessionRules
    TRUE,                               // 0x170 WantsTrackRulesAccess
    v_access_track_rules as *const (),  // 0x178 AccessTrackRules
    TRUE,                               // 0x180 WantsPitMenuAccess
    v_access_pit_menu as *const (),     // 0x188 AccessPitMenu
]);
