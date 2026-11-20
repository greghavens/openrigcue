//! The motion driver table and the settings every driver reads on each call.

use openrigcue_config::settings::Settings;
use openrigcue_config::tunes::Tune;
use openrigcue_devices::motion::driver_support::{ConfigSource, HostActivity, RuntimeReader, RuntimeView};
use openrigcue_devices::motion::prosimu::{ProsimuSettings, Prs200Driver};
use openrigcue_devices::motion::pt_actuator::ptamotion::{PtaMotionDriver, PtaMotionSettings};
use openrigcue_devices::motion::pt_actuator::ptcan::{PtCanDriver, PtCanSettings};
use openrigcue_devices::motion::pt_actuator::MotionWarning;
use openrigcue_devices::motion::scn6::{DyadicDriver, DyadicSettings, DyadicVariant};
use openrigcue_devices::motion::simtech::{SimTechDriver, SimTechSettings, SimTechShared};
use openrigcue_devices::motion::smc3::{Smc3Driver, Smc3Settings};
use openrigcue_devices::motion::thanos::{ThanosDriver, ThanosSettings, ThanosShared};
use openrigcue_devices::motion::vnm::{VnmDriver, VnmSettings};
use openrigcue_devices::motion::yaw_vr::{Translate, YawShared, YawVrDriver, YawVrSettings};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use openrigcue_devices::motion::driver_support::CornerTravel;
use openrigcue_devices::motion::{
    ActuatorPositions, AxisAssignment, ControllerIdentity, DriverEnvironment, DriverKind, LiveSettings, ModelSpec,
    MotionContext, MotionDriver, MotionError, MotionHost, MotionRegistry,
};

use super::convert::{self, DriverRuntime};

/// Settings and shared state of every motion driver, kept current by the motion loops (the drivers
/// read the configuration, the tune and the runtime state on every call).
#[derive(Clone, Default)]
pub struct DriverSettings {
    pub smc3: LiveSettings<Smc3Settings>,
    pub prosimu: LiveSettings<ProsimuSettings>,
    /// The PRS200 traction-loss serial port, retained across selected-model changes.
    pub prs200_tl_port: LiveSettings<String>,
    pub activity: LiveSettings<HostActivity>,
    pub dyadic: LiveSettings<DyadicSettings>,
    pub thanos: LiveSettings<ThanosSettings>,
    pub thanos_shared: LiveSettings<ThanosShared>,
    pub yaw_vr: LiveSettings<YawVrSettings>,
    pub yaw_shared: LiveSettings<YawShared>,
    pub pt_can: LiveSettings<PtCanSettings>,
    pub pta_motion: LiveSettings<PtaMotionSettings>,
    pub vnm: LiveSettings<VnmSettings>,
    pub simtech: LiveSettings<SimTechSettings>,
    pub simtech_shared: LiveSettings<SimTechShared>,
    /// The motion warning as the PT and VNM drivers write it.
    pub warning: MotionWarning,
    /// Where the drivers read the runtime state and telemetry at the point of use (see
    /// [`read_runtime_from`](Self::read_runtime_from)); until a source is installed, the refreshed values.
    pub runtime: RuntimeReader,
}

impl DriverSettings {
    /// Brings every driver's settings up to date with the configuration, the active tune and the runtime state.
    pub fn refresh(
        &self,
        settings: &Settings,
        manufacturer: &str,
        tune: &Tune,
        runtime: &DriverRuntime<'_>,
        native_thanos: &openrigcue_model::native_value::LegacyValue,
    ) {
        let native = settings.native_snapshot();
        let mut smc3 = convert::smc3_settings_with_native(settings, &native);
        smc3.runtime = self.runtime.clone();
        self.smc3.set(smc3);
        let mut prosimu = convert::prosimu_settings_with_native(settings, &native);
        prosimu.runtime = self.runtime.clone();
        self.prosimu.set(prosimu);
        self.activity.set(HostActivity {
            game_running: runtime.game_running.to_owned(),
            listener_running: runtime.listener_running.to_owned(),
            motion_test_mode: runtime.motion_test,
            runtime: self.runtime.clone(),
        });
        let mut dyadic = convert::dyadic_settings_with_native(settings, &native);
        dyadic.runtime = self.runtime.clone();
        self.dyadic.set(dyadic);
        let mut thanos =
            convert::thanos_settings_with_native_profile(settings, manufacturer, tune, runtime, native_thanos);
        thanos.runtime = self.runtime.clone();
        self.thanos.set(thanos);
        let mut yaw_vr = convert::yaw_vr_settings_with_native(settings, &native, tune, runtime);
        yaw_vr.runtime = self.runtime.clone();
        self.yaw_vr.set(yaw_vr);
        let mut pt_can = convert::pt_can_settings(settings, runtime);
        pt_can.runtime = self.runtime.clone();
        self.pt_can.set(pt_can);
        let mut pta_motion = convert::pta_motion_settings(settings);
        pta_motion.runtime = self.runtime.clone();
        self.pta_motion.set(pta_motion);
        let mut vnm = convert::vnm_settings(settings);
        vnm.runtime = self.runtime.clone();
        self.vnm.set(vnm);
        let mut simtech = convert::simtech_settings_with_native(settings, &native, runtime);
        simtech.runtime = self.runtime.clone();
        self.simtech.set(simtech);
    }

    /// Makes every driver read the running game and listener, the motion test mode, the motion switch,
    /// the motion error and the telemetry values it uses from `state` at the point of use,
    /// so that changes other threads make are seen at once (the values refreshed
    /// before cueing are then only the fallback of a reader without a source).
    pub fn read_runtime_from(&self, state: &crate::state::ServiceState) {
        let live = state.clone();
        let state = state.clone();
        self.runtime.install(std::sync::Arc::new(move || {
            let (game_running, listener_running) = {
                let session = state.session.read();
                (session.game_key().to_owned(), session.listener_key().to_owned())
            };
            let (motion_test_mode, motion_switch) = {
                let controls = state.controls.read();
                (controls.tests.motion_test, controls.switches.motion)
            };
            let motion_error = state.motion.read().error;
            let telemetry = state.telemetry.read();
            RuntimeView {
                game_running,
                listener_running,
                motion_test_mode,
                motion_switch,
                motion_error,
                speed: telemetry.speed,
                max_speed: telemetry.max_speed,
                rpm: telemetry.rpm,
                max_rpm: telemetry.max_rpm,
                yaw_angle: telemetry.yaw_angle,
            }
        }));
        let state = live.clone();
        self.runtime.install_game_config(std::sync::Arc::new(move || state.profile.read().native_tune()));
        self.runtime.install_config(std::sync::Arc::new(LiveConfig(live.clone())));
    }

    /// The warning the drivers publish, the one holder the PT, VNM and Yaw VR
    /// drivers write.
    pub fn warning(&self) -> String {
        self.warning.get()
    }
}

/// The configuration as the drivers read it at the point of use: the service's live settings tree
/// (`ConfigStore::native_settings_tree`), looked up and converted with the same rules as the refreshed values.
struct LiveConfig(crate::state::ServiceState);

impl LiveConfig {
    fn tree(&self) -> openrigcue_model::native_value::LegacyValue {
        self.0.config.read().native_settings_tree()
    }
}

impl ConfigSource for LiveConfig {
    fn value(&self, section: &str, key: &str) -> Result<openrigcue_model::native_value::LegacyValue, String> {
        convert::live_setting_value(&self.tree(), section, key)
    }
    fn switch(&self, section: &str, key: &str) -> Result<bool, String> {
        convert::live_setting_boolean(&self.tree(), section, key)
    }
    fn filter(&self, section: &str) -> Result<bool, String> {
        convert::live_thanos_filter(&self.tree(), section)
    }
}

/// Which drivers are available.
///
/// Every controller under the Windows rules, with the legacy SCN6 driver when `[PROSIMU] use_scn6_legacy`.
pub fn driver_environment(settings: &Settings) -> DriverEnvironment {
    DriverEnvironment {
        company_id: "srs".into(),
        windows_drivers: true,
        use_scn6_legacy: settings.prosimu.scn6_legacy,
        simracingtech_enabled: false,
    }
}

/// The shared SMC3 driver: one driver instance that both the driver table and
/// the stream path's debug call reach.
#[derive(Clone)]
pub struct Smc3Module(Arc<Mutex<Smc3Driver>>);

impl Smc3Module {
    /// A driver whose start-up state comes from the settings as they are now.
    pub fn new(settings: LiveSettings<Smc3Settings>) -> Smc3Module {
        Smc3Module(Arc::new(Mutex::new(Smc3Driver::new(settings))))
    }

    fn lock(&self) -> MutexGuard<'_, Smc3Driver> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Passes a frame to the driver's debug hook.
    pub fn debug(&self, motors: &[i64], host: &mut dyn MotionHost) -> Result<(), MotionError> {
        self.lock().debug(motors, host)
    }
}

impl MotionDriver for Smc3Module {
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.lock().connect(ctx)
    }
    fn disconnect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.lock().disconnect(ctx)
    }
    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.lock().send_stream(motors, ctx)
    }
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.lock().send_test(motors, ctx)
    }
    fn send_stream_native(
        &mut self,
        motors: &mut openrigcue_model::native_value::LegacyValue,
        ctx: &mut MotionContext<'_>,
    ) -> Result<(), MotionError> {
        self.lock().send_stream_native(motors, ctx)
    }
    fn send_test_native(
        &mut self,
        motors: &mut openrigcue_model::native_value::LegacyValue,
        ctx: &mut MotionContext<'_>,
    ) -> Result<(), MotionError> {
        self.lock().send_test_native(motors, ctx)
    }
    /// The stream path's debug call, on the driver instance the driver table also holds.
    fn debug_native(
        &mut self,
        motors: &openrigcue_model::native_value::LegacyValue,
        host: &mut dyn MotionHost,
    ) -> Result<(), MotionError> {
        self.lock().debug_native(motors, host)
    }
    fn take_edited_frame(&mut self) -> Option<Vec<i64>> {
        self.lock().take_edited_frame()
    }
    fn actuators_position(&self) -> Option<ActuatorPositions> {
        self.lock().actuators_position()
    }
    fn actuators_travel(&self) -> Option<CornerTravel> {
        self.lock().actuators_travel()
    }
    fn identity(&self) -> Option<ControllerIdentity> {
        self.lock().identity()
    }
    fn expected_controllers(&self, model: &ModelSpec) -> Option<i64> {
        self.lock().expected_controllers(model)
    }
    fn assignment(&self) -> Option<AxisAssignment> {
        self.lock().assignment()
    }
    fn describe(&self) -> String {
        self.lock().describe()
    }
}

/// A registry with a factory for every driver kind; the environment decides which ones are reachable. The SMC3
/// entry is `smc3`, the driver instance the debug call also reaches. `translate` is the UI language's
/// translation, which the Yaw VR driver applies to the warning it writes.
pub fn motion_registry(
    environment: DriverEnvironment,
    drivers: &DriverSettings,
    smc3: &Smc3Module,
    translate: Translate,
) -> MotionRegistry {
    let mut registry = MotionRegistry::new(environment);
    let d = drivers.clone();
    let smc3 = smc3.clone();
    registry.register(DriverKind::Smc3, Box::new(move || Box::new(smc3.clone())));
    registry.register(DriverKind::Thanos, ThanosDriver::factory(d.thanos.clone(), d.thanos_shared.clone()));
    registry.register(
        DriverKind::YawVr,
        YawVrDriver::factory_with_motion_warning(d.yaw_vr.clone(), d.yaw_shared.clone(), d.warning.clone(), translate),
    );
    registry.register(DriverKind::Dyadic, DyadicDriver::factory(DyadicVariant::Current, d.dyadic.clone()));
    registry.register(DriverKind::DyadicPrevious, DyadicDriver::factory(DyadicVariant::Previous, d.dyadic.clone()));
    registry.register(DriverKind::PtaMotion, PtaMotionDriver::factory(d.pta_motion.clone(), d.warning.clone()));
    registry.register(DriverKind::PtCan, PtCanDriver::factory(d.pt_can.clone(), d.warning.clone()));
    registry.register(DriverKind::Vnm, VnmDriver::factory(d.vnm.clone(), d.warning.clone()));
    registry
        .register(DriverKind::SimTechRevolution, SimTechDriver::factory(d.simtech.clone(), d.simtech_shared.clone()));
    for kind in
        [DriverKind::Prs200Dyadic, DriverKind::Prs200DyadicSv, DriverKind::Prs200Prs50, DriverKind::Prs200DyadicTlSurge]
    {
        registry.register(
            kind,
            Prs200Driver::factory_with_tl_port(d.prosimu.clone(), d.activity.clone(), d.prs200_tl_port.clone()),
        );
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrigcue_devices::motion::Controller;

    /// The driver table under the Windows rules has every controller except the
    /// Actuon white-label one.
    #[test]
    fn controllers_table() {
        let settings = Settings::default();
        let windows = driver_environment(&settings);
        assert_eq!(windows.driver_for(Controller::Smc3), Some(DriverKind::Smc3));
        assert_eq!(windows.driver_for(Controller::Thanos), Some(DriverKind::Thanos));
        assert_eq!(windows.driver_for(Controller::Scn6), Some(DriverKind::Dyadic));
        assert_eq!(windows.driver_for(Controller::YawVr), Some(DriverKind::YawVr));
        let mut legacy = settings.clone();
        legacy.prosimu.scn6_legacy = true;
        assert_eq!(driver_environment(&legacy).driver_for(Controller::Scn6), Some(DriverKind::DyadicPrevious));
    }

    /// The motion warning: one holder, whichever driver wrote it last.
    #[test]
    fn warning_sources() {
        let drivers = DriverSettings::default();
        drivers.warning.set("PT offline".into());
        assert_eq!(drivers.warning(), "PT offline");
    }

    /// The drivers read the configuration, the tune, the runtime state and telemetry at the point of use: an
    /// in-place mutation of the live settings tree (no revision, no refresh) and later control and telemetry
    /// changes are what the next read sees, with the refreshed values only as the fallback without a source.
    #[test]
    fn installed_sources_follow_live_mutations_between_refreshes() {
        use crate::state::{OriginalPlatform, ServiceState};
        use crate::store::ConfigStore;
        use openrigcue_config::settings::Configuration;
        use openrigcue_devices::motion::vnm::VnmModel;
        use openrigcue_model::native_value::LegacyValue;

        let drivers = DriverSettings::default();
        let refreshed: Result<LegacyValue, String> = Err("refreshed".into());
        assert_eq!(drivers.runtime.value("MOTION_COMPENSATION", "vertical_actuators_stroke", &refreshed), refreshed);
        let state = ServiceState::new(ConfigStore::in_memory(Configuration::first_run()), OriginalPlatform::Windows);
        drivers.refresh(
            state.config.read().settings(),
            "VNM SIMULATION",
            &state.profile.read().tune,
            &DriverRuntime {
                game_running: "none",
                listener_running: "none",
                motion_test: false,
                motion_switch: false,
                motion_error: false,
                telemetry: &openrigcue_model::Telemetry::default(),
                simtech_tune: LegacyValue::None,
            },
            &LegacyValue::None,
        );
        drivers.read_runtime_from(&state);
        let tree = state.config.read().native_settings_tree();
        let revision = state.config.read().revision();
        let mut section = tree.item_owned("MOTION_COMPENSATION").unwrap();
        section.assign_item("vertical_actuators_stroke", LegacyValue::from("275")).unwrap();
        assert_eq!(state.config.read().revision(), revision, "an alias mutation has no revision");
        assert_eq!(
            drivers.runtime.value("MOTION_COMPENSATION", "vertical_actuators_stroke", &refreshed),
            Ok(LegacyValue::from("275"))
        );
        let mut vnm = tree.item_owned("VNM SIMULATION").unwrap();
        vnm.assign_item("reverse_controller_port_5", LegacyValue::from("maybe")).unwrap();
        assert_eq!(
            drivers.runtime.switch("VNM SIMULATION", "reverse_controller_port_5", &Ok(false)),
            Err("Exception: Boolean value expected. Got maybe".into())
        );
        assert_eq!(
            drivers.runtime.value("VNM SIMULATION", "no_such_key", &refreshed),
            Err("KeyError: 'no_such_key'".into())
        );
        let mut thanos = tree.item_owned("THANOS").unwrap();
        thanos.assign_item("filter", LegacyValue::from("no")).unwrap();
        assert_eq!(drivers.runtime.filter("THANOS", &Ok(true)), Ok(false));
        // A driver read: VNM's reversal reads the switch now, between two frames without a refresh.
        vnm.assign_item("reverse_controller_port_5", LegacyValue::from("yes")).unwrap();
        let settings = drivers.vnm.get();
        let mut driver = openrigcue_devices::motion::vnm::VnmDriver::new(
            openrigcue_devices::motion::LiveSettings::new(settings),
            drivers.warning.clone(),
        );
        let mut list = LegacyValue::List((0..12).map(|_| LegacyValue::Int(1000)).collect());
        let spec = openrigcue_devices::motion::motion_model(VnmModel::Corners4Tl.name(), false).unwrap();
        let mut link = openrigcue_devices::motion::LinkState {
            status: openrigcue_devices::motion::LinkStatus::Connected,
            ..Default::default()
        };
        let mut host = openrigcue_devices::motion::SystemHost::default();
        driver
            .send_stream_native(&mut list, &mut MotionContext { model: spec, link: &mut link, host: &mut host })
            .unwrap();
        assert_eq!(list.get_index(4).unwrap(), LegacyValue::Int(64_536), "reversed by the live switch");
        // The runtime state and telemetry the same way.
        state.controls.write().tests.motion_test = true;
        assert!(drivers.runtime.read().unwrap().motion_test_mode);
        assert!(drivers.activity.get().live().motion_test_mode);
    }
}
