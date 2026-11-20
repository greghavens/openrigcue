//! Construction of every registered listener with the configured ports and options.
//! Each factory reads its listener's settings either when the listener is loaded or when its worker runs;
//! PMR reads on each retry, while War Thunder reads once at worker entry.

use std::path::Path;

use openrigcue_config::patch::{SteamLibraries, SteamPlugin};
use openrigcue_config::settings::{JoystickProfiles, Settings};
use openrigcue_sources::api::joystick::JoystickAssignment;
use openrigcue_sources::api::{ApiDecoder, JoystickInputs, JoystickOptions, JoystickProfile, JoystickSource};
use openrigcue_sources::derive::Listener;
use openrigcue_sources::process_memory::elite_dangerous::{EliteDangerous, EliteSettings};
use openrigcue_sources::process_memory::simconnect::aircraft::Simulator;
use openrigcue_sources::process_memory::simconnect::{AttitudeInversion, FlightSimSettings, FlightSimulator};
use openrigcue_sources::process_memory::{
    bus_sim::BusSim, dynamic_cab::DynamicCab, farming_simulator::FarmingSimulator, no_limits2::NoLimits2,
    vracer::VRacer, war_thunder::WarThunder,
};
use openrigcue_sources::shared_memory::assetto_corsa::{AcGame, AssettoCorsa};
use openrigcue_sources::shared_memory::iracing::{Iracing, IracingOptions};
use openrigcue_sources::shared_memory::piboso::{self, Piboso};
use openrigcue_sources::shared_memory::raceroom::{RaceRoom, R3E_DATA_FILE};
use openrigcue_sources::shared_memory::rfactor1::{RfactorOne, RfactorOneVariant};
use openrigcue_sources::shared_memory::simbin::{SimBin, SimBinTitle};
use openrigcue_sources::shared_memory::{
    assetto_corsa_evo::AssettoCorsaEvo, automobilista2::Automobilista2, falcon_bms::FalconBms, il2_cliffs::Il2Cliffs,
    project_cars::ProjectCars, project_cars2::ProjectCars2, rfactor2::Rfactor2, space_monkey::SpaceMonkey,
    trackmania::TrackMania, trucksim::TruckSim, warplanes::Warplanes,
};
use openrigcue_sources::udp_flight::{
    aces_high::AcesHighDecoder, aerofly::AeroflyDecoder, condor::CondorDecoder, dcs::DcsDecoder,
    fly_inside::FlyInsideDecoder, il2::Il2Source, silent_wings::SilentWingsDecoder, xplane::XPlaneOptions,
    xplane::XPlaneSource, Decoder, PolledUdpSource,
};
use openrigcue_sources::udp_racing::codemasters::{dirt::DirtSource, f1_classic::F1ClassicSource, grid::GridSource};
use openrigcue_sources::udp_racing::codemasters::{tdusc::TduscSource, wrc::WrcSource};
use openrigcue_sources::udp_racing::ea_f1::season2018::F1Season2018Source;
use openrigcue_sources::udp_racing::ea_f1::seasons::{F1SeasonSource, Season};
use openrigcue_sources::udp_racing::forward::Forwarder;
use openrigcue_sources::udp_racing::gran_turismo::{GranTurismo, GranTurismoSettings, GranTurismoSource};
use openrigcue_sources::udp_racing::project_motor_racing::{NetworkPortsReader, PmRacingSettings, PmRacingSource};
use openrigcue_sources::udp_racing::socket::ImportedPort;
use openrigcue_sources::udp_racing::{
    assetto_corsa_udp::AssettoCorsaUdpSource, beamng::BeamNgSource, drift_physics::DriftPhysicsSource,
    epic_roller_coasters::EpicRollerCoastersSource, forza::ForzaSource, gtav::GtavSource, hyperstorm::HyperStormSource,
    iron_rebellion::IronRebellionSource, kartkraft::KartKraftSource, live_for_speed::LiveForSpeedSource,
    motogp::MotoGpSource, project_cars::ProjectCarsSource, project_cars2::ProjectCars2Source, rbr::RbrSource,
    red_rover::RedRoverSource, the_crew::TheCrewSource, touring_karts::TouringKartsSource, vector36::Vector36Source,
    wrc2023::Wrc2023Source, wreckfest2::Wreckfest2Source,
};

use super::sources::ForwardingVRacer;
use super::{GameListener, GameSource, SourceContext, SourceOutputs};
use crate::runtime::Shared;

/// What building the listeners reads besides the settings.
pub struct ListenerInputs<'a> {
    pub settings: &'a Settings,
    /// The documents folder: Elite Dangerous keeps its offsets archive there.
    pub documents: &'a Path,
    /// The runtime directory the pipe listeners (DynamicCab, Farming Simulator) bind their relay sockets below
    /// ([`ServiceOptions::runtime_dir`](crate::ServiceOptions::runtime_dir)).
    pub runtime_dir: &'a Path,
    /// Steam libraries, where RaceRoom's `r3e-data.json` is looked for.
    pub steam: Option<&'a SteamLibraries>,
    /// Controller states the joystick listener maps.
    pub joystick_inputs: JoystickInputs,
    pub joystick_runtime: Option<openrigcue_sources::api::joystick::JoystickRuntimeReader>,
    /// Live settings read by game workers at their own session boundaries.
    pub network_ports_reader: Option<NetworkPortsReader>,
    pub pause_reader: Option<openrigcue_sources::shared_memory::NativePauseReader>,
    pub game_reader: Option<openrigcue_sources::shared_memory::NativeGameReader>,
    pub iracing_dams_reader: Option<openrigcue_sources::shared_memory::iracing::NativeDamsReader>,
    pub xplane_lateral_reader: Option<openrigcue_sources::udp_flight::xplane::NativeLateralReader>,
    pub console_reader: Option<openrigcue_sources::udp_racing::gran_turismo::NativeConsoleReader>,
    pub windows_reader: Option<openrigcue_sources::shared_memory::NativeWindowsReader>,
    pub context: Shared<SourceContext>,
    pub outputs: Shared<SourceOutputs>,
}

pub fn network_ports_reader(config: Shared<crate::store::ConfigStore>) -> NetworkPortsReader {
    std::sync::Arc::new(move || {
        config
            .read()
            .settings()
            .native_snapshot()
            .get("NETWORK_PORT")
            .and_then(|value| value.as_dict())
            .cloned()
            .unwrap_or_default()
    })
}

/// The required APP pause value at a channel read, after releasing the config lock.
pub fn pause_reader(config: Shared<crate::store::ConfigStore>) -> openrigcue_sources::shared_memory::NativePauseReader {
    std::sync::Arc::new(move || {
        config
            .read()
            .settings()
            .native_snapshot()
            .get("APP")?
            .as_dict()?
            .get("pause_telemetry_when_not_playing")
            .cloned()
    })
}

pub fn iracing_dams_reader(
    config: Shared<crate::store::ConfigStore>,
) -> openrigcue_sources::shared_memory::iracing::NativeDamsReader {
    std::sync::Arc::new(move || {
        config.read().settings().native_snapshot().get("CUSTOM_FEATURES")?.as_dict()?.get("dams_front_tl_mode").cloned()
    })
}

pub fn xplane_lateral_reader(
    config: Shared<crate::store::ConfigStore>,
) -> openrigcue_sources::udp_flight::xplane::NativeLateralReader {
    std::sync::Arc::new(move || {
        config
            .read()
            .settings()
            .native_snapshot()
            .get("CUSTOM_FEATURES")?
            .as_dict()?
            .get("rollback_xplane_latg")
            .cloned()
    })
}

pub fn console_reader(
    config: Shared<crate::store::ConfigStore>,
) -> openrigcue_sources::udp_racing::gran_turismo::NativeConsoleReader {
    std::sync::Arc::new(move || {
        config.read().settings().native_snapshot().get("APP")?.as_dict()?.get("playstation_ip").cloned()
    })
}

pub fn windows_reader(
    snapshot: Shared<openrigcue_config::detect::SystemSnapshot>,
) -> openrigcue_sources::shared_memory::NativeWindowsReader {
    std::sync::Arc::new(move || snapshot.read().windows.clone())
}

pub fn motion_model_reader(
    config: Shared<crate::store::ConfigStore>,
) -> openrigcue_sources::process_memory::simconnect::NativeModelReader {
    std::sync::Arc::new(move || {
        config.read().settings().native_snapshot().get("MOTIONBOX")?.as_dict()?.get("model").cloned()
    })
}

pub fn elite_pid_reader(
    snapshot: Shared<openrigcue_config::detect::SystemSnapshot>,
) -> openrigcue_sources::process_memory::elite_dangerous::NativePidReader {
    std::sync::Arc::new(move || {
        snapshot.read().processes.get(openrigcue_sources::process_memory::elite_dangerous::PROCESS_NAME).copied()
    })
}

/// Ordinary forwarding string lists are read from current settings independently
/// of the main dispatcher, including after that thread has ended.
pub fn forwarding_reader(
    config: Shared<crate::store::ConfigStore>,
) -> openrigcue_sources::udp_racing::forward::ForwardingEntriesReader {
    std::sync::Arc::new(move |key| config.read().settings().port_forwarding.get(key).cloned())
}

impl ListenerInputs<'_> {
    fn pmracing(&self) -> PmRacingSource {
        match &self.network_ports_reader {
            Some(reader) => PmRacingSource::with_live_network_ports(reader.clone()),
            None => PmRacingSource::new(PmRacingSettings {
                network_ports: self
                    .settings
                    .native_snapshot()
                    .get("NETWORK_PORT")
                    .and_then(|value| value.as_dict())
                    .cloned()
                    .unwrap_or_default(),
            }),
        }
    }

    fn acudp_listener_with<T>(&self, build: impl FnOnce(NetworkPortsReader) -> T) -> Option<T> {
        let reader = self.network_ports_reader.clone().unwrap_or_else(|| {
            let ports = self
                .settings
                .native_snapshot()
                .get("NETWORK_PORT")
                .and_then(|value| value.as_dict())
                .cloned()
                .unwrap_or_default();
            std::sync::Arc::new(move || ports.clone())
        });
        Some(build(reader))
    }
    /// Read the stored settings value when the listener is loaded. Typed-view defaults must not
    /// supply a missing or malformed key.
    #[cfg(test)]
    fn configured_port(&self, key: &str) -> Option<u16> {
        self.settings.network_port(key)
    }

    /// A missing port makes loading the listener fail before its constructor can bind a fallback socket.
    #[cfg(test)]
    fn configured<S: GameSource + 'static>(&self, key: &str, build: impl FnOnce() -> S) -> Option<Box<dyn Listener>> {
        if self.configured_port(key).is_none() {
            tracing::warn!(key, "listener not imported: configured network port missing or invalid");
            return None;
        }
        Some(self.wrap(build()))
    }

    fn probe_listener(&self, key: &str) -> Option<Box<dyn Listener>> {
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw
            .get("NETWORK_PORT")
            .and_then(|section| section.as_dict())
            .and_then(|section| section.get(key))
            .and_then(ImportedPort::from_native);
        let Some(port) = port else {
            tracing::warn!(key, "listener not imported: configured network port missing or invalid");
            return None;
        };
        Some(match key {
            "grid" => self.wrap(GridSource::from_imported_port(port)),
            "f1" => self.wrap(F1ClassicSource::from_imported_port(port)),
            _ => return None,
        })
    }

    /// Construct a season from its load-time port, before its receiver starts.
    fn season_listener_with<T>(&self, season: Season, build: impl FnOnce(Season, ImportedPort) -> T) -> Option<T> {
        let key = season.id();
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw
            .get("NETWORK_PORT")
            .and_then(|section| section.as_dict())
            .and_then(|section| section.get(key))
            .and_then(ImportedPort::from_native);
        let Some(port) = port else {
            tracing::warn!(key, "listener not imported: configured network port missing or invalid");
            return None;
        };
        Some(build(season, port))
    }

    fn season_2018_listener_with<T>(&self, build: impl FnOnce(ImportedPort) -> T) -> Option<T> {
        #[cfg(test)]
        tests::note_port_key("f12018");
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get("f12018").and_then(ImportedPort::from_native)?;
        Some(build(port))
    }

    fn il2_listener_with<T>(&self, build: impl FnOnce(ImportedPort) -> T) -> Option<T> {
        #[cfg(test)]
        tests::note_port_key("il2");
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get("il2").and_then(ImportedPort::from_native)?;
        Some(build(port))
    }

    fn xplane_listener_with<T>(&self, build: impl FnOnce(ImportedPort, XPlaneOptions) -> T) -> Option<T> {
        #[cfg(test)]
        tests::note_port_key("xplane");
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get("xplane").and_then(ImportedPort::from_native)?;
        Some(build(port, self.xplane_import_options()?))
    }

    fn vracer_listener_with<T>(&self, build: impl FnOnce(ImportedPort) -> T) -> Option<T> {
        #[cfg(test)]
        tests::note_port_key("vracer");
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get("vracer").and_then(ImportedPort::from_native)?;
        Some(build(port))
    }

    fn nl2_listener_with<T>(&self, build: impl FnOnce(ImportedPort) -> T) -> Option<T> {
        #[cfg(test)]
        tests::note_port_key("nl2");
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get("nl2").and_then(ImportedPort::from_native)?;
        Some(build(port))
    }

    fn bussim_listener_with<T>(&self, build: impl FnOnce(String) -> T) -> Option<T> {
        #[cfg(test)]
        tests::note_port_key("bussim");
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get("bussim")?.py_str()?;
        Some(build(port))
    }

    fn warthunder_listener_with<T>(&self, build: impl FnOnce(NetworkPortsReader) -> T) -> T {
        #[cfg(test)]
        tests::note_port_key("warthunder");
        let reader = self.network_ports_reader.clone().unwrap_or_else(|| {
            let ports = self
                .settings
                .native_snapshot()
                .get("NETWORK_PORT")
                .and_then(|value| value.as_dict())
                .cloned()
                .unwrap_or_default();
            std::sync::Arc::new(move || ports.clone())
        });
        build(reader)
    }

    fn dashboard_listener_with<T>(&self, key: &str, build: impl FnOnce(ImportedPort) -> T) -> Option<T> {
        debug_assert!(matches!(key, "forza" | "beamng"));
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(key).and_then(ImportedPort::from_native)?;
        Some(build(port))
    }

    fn gran_turismo_listener_with<T>(&self, key: &str, build: impl FnOnce(ImportedPort) -> T) -> Option<T> {
        debug_assert!(matches!(key, "gt7" | "gtsport"));
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(key).and_then(ImportedPort::from_native)?;
        Some(build(port))
    }

    fn project_cars_listener_with<T>(&self, key: &str, build: impl FnOnce(ImportedPort) -> T) -> Option<T> {
        debug_assert!(matches!(key, "projectcarsudp" | "projectcars2udp"));
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(key).and_then(ImportedPort::from_native)?;
        Some(build(port))
    }

    fn error_set_listener_with<T>(&self, key: &str, build: impl FnOnce(ImportedPort) -> T) -> Option<T> {
        debug_assert!(matches!(key, "wrc2023" | "kartkraft"));
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(key).and_then(ImportedPort::from_native)?;
        Some(build(port))
    }

    fn simple_listener(&self, key: &str) -> Option<Box<dyn Listener>> {
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(key).and_then(ImportedPort::from_native)?;
        match key {
            "ironrebellion" => Some(self.wrap(IronRebellionSource::from_imported_port(port))),
            "thecrew" => Some(self.wrap(TheCrewSource::from_imported_port(port))),
            "gtav" => Some(self.wrap(GtavSource::from_imported_port(port))),
            _ => None,
        }
    }

    fn compact_listener(&self, key: &str) -> Option<Box<dyn Listener>> {
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(key).and_then(ImportedPort::from_native)?;
        match key {
            "driftphysicscrew" => Some(self.wrap(DriftPhysicsSource::from_imported_port(port))),
            "hyperstorm" => Some(self.wrap(HyperStormSource::from_imported_port(port))),
            "vector36" => Some(self.wrap(Vector36Source::from_imported_port(port))),
            _ => None,
        }
    }

    fn text_stream_listener(&self, key: &str) -> Option<Box<dyn Listener>> {
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(key).and_then(ImportedPort::from_native)?;
        match key {
            "redrover" => Some(self.wrap(RedRoverSource::from_imported_port(port))),
            "epic" => Some(self.wrap(EpicRollerCoastersSource::from_imported_port(port))),
            _ => None,
        }
    }

    fn rbr_listener(&self) -> Option<Box<dyn Listener>> {
        #[cfg(test)]
        tests::note_port_key("rbr");
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get("rbr").and_then(ImportedPort::from_native)?;
        Some(self.wrap(RbrSource::from_imported_port(port)))
    }

    fn final_poll_listener(&self, key: &str) -> Option<Box<dyn Listener>> {
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(key).and_then(ImportedPort::from_native)?;
        match key {
            "motogp" => Some(self.wrap(MotoGpSource::from_imported_port(port))),
            "touringkarts" => Some(self.wrap(TouringKartsSource::from_imported_port(port))),
            _ => None,
        }
    }

    fn wreckfest_listener(&self) -> Option<Box<dyn Listener>> {
        #[cfg(test)]
        tests::note_port_key("wreckfest2");
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get("wreckfest2").and_then(ImportedPort::from_native)?;
        Some(self.wrap(self.wreckfest(port)))
    }

    fn wreckfest(&self, port: ImportedPort) -> Wreckfest2Source {
        let mut source = Wreckfest2Source::from_imported_port(port, self.pause());
        source.set_pause_reader(self.native_pause_reader());
        source
    }

    fn lfs_listener(&self) -> Option<Box<dyn Listener>> {
        #[cfg(test)]
        tests::note_port_key("lfs");
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get("lfs").and_then(ImportedPort::from_native)?;
        Some(self.wrap(LiveForSpeedSource::from_imported_port(port)))
    }

    fn remaining_codemasters_listener(&self, key: &str) -> Option<Box<dyn Listener>> {
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(key).and_then(ImportedPort::from_native)?;
        match key {
            "dirt" => Some(self.wrap(DirtSource::from_imported_port(port))),
            "wrc" => Some(self.wrap(WrcSource::from_imported_port(port))),
            "tdusc" => Some(self.wrap(TduscSource::from_imported_port(port))),
            _ => None,
        }
    }

    fn quiet_flight_listener<D: Decoder + Default>(&self) -> Option<Box<dyn Listener>>
    where
        PolledUdpSource<D>: GameSource + 'static,
    {
        debug_assert!(matches!(D::ID, "condor" | "silentwings" | "aeroflyfs2"));
        #[cfg(test)]
        tests::note_port_key(D::ID);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(D::ID).and_then(ImportedPort::from_native)?;
        Some(self.wrap(PolledUdpSource::from_imported_port(D::default(), port)))
    }

    fn text_flight_listener<D: Decoder + Default>(&self) -> Option<Box<dyn Listener>>
    where
        PolledUdpSource<D>: GameSource + 'static,
    {
        debug_assert!(matches!(D::ID, "aceshigh" | "flyinside"));
        #[cfg(test)]
        tests::note_port_key(D::ID);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(D::ID).and_then(ImportedPort::from_native)?;
        Some(self.wrap(PolledUdpSource::from_imported_port(D::default(), port)))
    }

    fn diagnostic_flight_listener<D: Decoder + Default>(&self) -> Option<Box<dyn Listener>>
    where
        PolledUdpSource<D>: GameSource + 'static,
    {
        debug_assert!(matches!(D::ID, "dcs" | "api"));
        #[cfg(test)]
        tests::note_port_key(D::ID);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(D::ID).and_then(ImportedPort::from_native)?;
        Some(self.wrap(PolledUdpSource::from_imported_port(D::default(), port)))
    }

    fn pause(&self) -> bool {
        self.settings.app.pause_telemetry_when_not_playing
    }

    fn native_pause_reader(&self) -> openrigcue_sources::shared_memory::NativePauseReader {
        self.pause_reader.clone().unwrap_or_else(|| {
            let value = self
                .settings
                .native_snapshot()
                .get("APP")
                .and_then(|app| app.as_dict())
                .and_then(|app| app.get("pause_telemetry_when_not_playing"))
                .cloned();
            std::sync::Arc::new(move || value.clone())
        })
    }

    fn rfactor2(&self) -> Rfactor2 {
        let mut source = Rfactor2::new(self.pause());
        source.set_pause_reader(self.native_pause_reader());
        source
    }

    fn iracing(&self) -> Iracing {
        let mut source = Iracing::new(IracingOptions::default());
        source.set_pause_reader(self.native_pause_reader());
        source.set_dams_reader(self.iracing_dams_reader.clone().unwrap_or_else(|| {
            let value = self
                .settings
                .native_snapshot()
                .get("CUSTOM_FEATURES")
                .and_then(|section| section.as_dict())
                .and_then(|section| section.get("dams_front_tl_mode"))
                .cloned();
            std::sync::Arc::new(move || value.clone())
        }));
        source
    }

    fn xplane_import_options(&self) -> Option<XPlaneOptions> {
        let raw = self.settings.native_snapshot();
        let features = raw.get("CUSTOM_FEATURES")?.as_dict()?;
        // These load-time comparisons are exact equality with 'yes', not the lenient boolean parse.
        let invert_yaw = features.get("xplane_invert_yaw")?.as_str() == Some("yes");
        let invert_roll = features.get("xplane_invert_roll")?.as_str() == Some("yes");
        let invert_pitch = features.get("xplane_invert_pitch")?.as_str() == Some("yes");
        Some(XPlaneOptions {
            invert_pitch,
            invert_roll,
            invert_yaw,
            lateral_g_from_local_ax: self.settings.features.rollback_xplane_lateral_g,
        })
    }

    fn xplane(&self, port: ImportedPort, options: XPlaneOptions) -> XPlaneSource {
        let mut source = XPlaneSource::from_imported_port(port, options);
        source.set_lateral_reader(self.xplane_lateral_reader.clone().unwrap_or_else(|| {
            let value = self
                .settings
                .native_snapshot()
                .get("CUSTOM_FEATURES")
                .and_then(|section| section.as_dict())
                .and_then(|section| section.get("rollback_xplane_latg"))
                .cloned();
            std::sync::Arc::new(move || value.clone())
        }));
        source
    }

    fn native_game_reader(&self) -> openrigcue_sources::shared_memory::NativeGameReader {
        self.game_reader.clone().unwrap_or_else(|| {
            let context = self.context.clone();
            std::sync::Arc::new(move || context.read().game_running.clone().unwrap_or_else(|| "none".to_owned()))
        })
    }

    fn falcon(&self) -> FalconBms {
        let context = self.context.clone();
        let mut source = FalconBms::new(Box::new(move || context.read().window_titles.clone()));
        if let Some(reader) = &self.windows_reader {
            source.set_windows_reader(reader.clone());
        }
        source
    }

    fn space_monkey(&self) -> SpaceMonkey {
        let mut source = SpaceMonkey::new();
        if let Some(reader) = &self.windows_reader {
            source.set_windows_reader(reader.clone());
        }
        source
    }

    fn assetto_corsa(&self) -> AssettoCorsa {
        let mut source = AssettoCorsa::new(AcGame::AssettoCorsa, self.pause());
        source.set_native_readers(self.native_game_reader(), self.native_pause_reader());
        source
    }

    fn assetto_corsa_evo(&self) -> AssettoCorsaEvo {
        let mut source = AssettoCorsaEvo::new(self.pause());
        source.set_native_readers(self.native_game_reader(), self.native_pause_reader());
        source
    }

    fn project_cars(&self) -> ProjectCars {
        let mut source = ProjectCars::new(self.pause());
        source.set_pause_reader(self.native_pause_reader());
        source
    }

    fn project_cars2(&self) -> ProjectCars2 {
        let mut source = ProjectCars2::new(self.pause());
        source.set_pause_reader(self.native_pause_reader());
        source
    }

    fn automobilista2(&self) -> Automobilista2 {
        let mut source = Automobilista2::new(self.pause());
        source.set_pause_reader(self.native_pause_reader());
        source
    }

    /// Whether the configured motion platform is a Yaw VR one (Elite Dangerous and MSFS let their yaw through).
    fn yaw_vr_platform(&self) -> bool {
        openrigcue_devices::motion::motion_model(&self.settings.motion.model, false)
            .is_some_and(|model| model.manufacturer == "Yaw VR")
    }

    fn elite_listener_with<T>(&self, build: impl FnOnce(EliteSettings) -> T) -> Option<T> {
        let raw = self.settings.native_snapshot();
        // Only the manufacturer's lookup has a local fallback in this module.
        let yaw_vr = raw
            .get("MOTIONBOX")
            .and_then(|section| section.as_dict())
            .and_then(|section| section.get("model"))
            .and_then(|value| value.as_str())
            .and_then(|name| openrigcue_devices::motion::motion_model(name, false))
            .is_some_and(|model| model.manufacturer == "Yaw VR");
        let features = raw.get("CUSTOM_FEATURES")?.as_dict()?;
        let invert_yaw = features.get("ed_invert_yaw")?.as_str() == Some("yes");
        let invert_roll = features.get("ed_invert_roll")?.as_str() == Some("yes");
        let invert_pitch = features.get("ed_invert_pitch")?.as_str() == Some("yes");
        let check_updates = openrigcue_model::native_value::str2bool(&features.get("check_ed_updates")?.py_str()?)?;
        Some(build(EliteSettings {
            check_updates,
            invert_yaw,
            invert_roll,
            invert_pitch,
            yaw_vr,
            documents_dir: self.documents.to_path_buf(),
        }))
    }

    fn flight_simulator_with<T>(
        &self,
        simulator: Simulator,
        build: impl FnOnce(FlightSimSettings, ImportedPort) -> T,
    ) -> Option<T> {
        let key = if simulator == Simulator::Prepar3d { "p3d" } else { "fsx" };
        #[cfg(test)]
        tests::note_port_key(key);
        let raw = self.settings.native_snapshot();
        let port = raw.get("NETWORK_PORT")?.as_dict()?.get(key).and_then(ImportedPort::from_native)?;
        // All three listeners use p3d_invert_* and require each key when loaded.
        // The value is compared directly with 'yes'; other accepted bool strings do not invert.
        let features = raw.get("CUSTOM_FEATURES")?.as_dict()?;
        let yaw = features.get("p3d_invert_yaw")?.as_str() == Some("yes");
        let roll = features.get("p3d_invert_roll")?.as_str() == Some("yes");
        let pitch = features.get("p3d_invert_pitch")?.as_str() == Some("yes");
        let settings = FlightSimSettings {
            simulator,
            port: match port {
                ImportedPort::Valid(port) => port,
                ImportedPort::OutOfRange => 0,
            },
            inversion: AttitudeInversion { yaw, roll, pitch },
            yaw_vr: self.yaw_vr_platform(),
        };
        Some(build(settings, port))
    }

    fn wrap(&self, source: impl GameSource + 'static) -> Box<dyn Listener> {
        Box::new(GameListener::new(source, self.context.clone(), self.outputs.clone()))
    }
}

/// Every registered listener, in registration order (the first 19 registered on every platform, then
/// the Windows ones), each with its load-time configuration reads.
pub fn build_listeners(inputs: &ListenerInputs<'_>) -> Vec<Box<dyn Listener>> {
    build_listeners_with_model_reader(inputs, None)
}

/// The service variant: MSFS resolves the current motion model at each menu-camera getter.
pub fn build_listeners_with_model_reader(
    inputs: &ListenerInputs<'_>,
    model_reader: Option<openrigcue_sources::process_memory::simconnect::NativeModelReader>,
) -> Vec<Box<dyn Listener>> {
    build_listeners_with_native_readers(inputs, model_reader, None)
}

/// The listeners in load order (the common ones, then the
/// Windows ones), by listener id. The second entry is the `NETWORK_PORT` key the listener converts
/// to an integer when loaded (the bus simulator only reads it as text), `None` for a listener
/// that reads no port while it is loaded.
const IMPORT_ORDER: &[(&str, Option<&str>)] = &[
    ("f1", Some("f1")),
    ("forza", Some("forza")),
    ("f12018", Some("f12018")),
    ("api", Some("api")),
    ("f12019", Some("f12019")),
    ("f12020", Some("f12020")),
    ("f12021", Some("f12021")),
    ("f12022", Some("f12022")),
    ("f12023", Some("f12023")),
    ("f12024", Some("f12024")),
    ("f12025", Some("f12025")),
    ("grid", Some("grid")),
    ("joy", None),
    ("acudp", None),
    ("projectcarsudp", Some("projectcarsudp")),
    ("projectcars2udp", Some("projectcars2udp")),
    ("gt7", Some("gt7")),
    ("gtsport", Some("gtsport")),
    ("ironrebellion", Some("ironrebellion")),
    ("ac", None),
    ("acevo", None),
    ("projectcars2", None),
    ("projectcars", None),
    ("iracing", None),
    ("dirt", Some("dirt")),
    ("rfpro", None),
    ("rf1", None),
    ("rf2", None),
    ("rre", None),
    ("xplane", Some("xplane")),
    ("nl2", Some("nl2")),
    ("epic", Some("epic")),
    ("trucksim", None),
    ("gpbikes", None),
    ("mxbikes", None),
    ("kart", None),
    ("wrs", None),
    ("warthunder", None),
    ("dcs", Some("dcs")),
    ("rbr", Some("rbr")),
    ("motogp", Some("motogp")),
    ("lfs", Some("lfs")),
    ("beamng", Some("beamng")),
    ("ccd", None),
    ("gtr", None),
    ("gtr2", None),
    ("gtl", None),
    ("race07", None),
    ("kartkraft", Some("kartkraft")),
    ("ed", None),
    ("p3d", Some("p3d")),
    ("condor", Some("condor")),
    ("touringkarts", Some("touringkarts")),
    ("il2", Some("il2")),
    ("il2cliffs", None),
    ("fsx", Some("fsx")),
    ("fs2k", Some("fsx")),
    ("flyinside", Some("flyinside")),
    ("silentwings", Some("silentwings")),
    ("automobilista2", None),
    ("thecrew", Some("thecrew")),
    ("gtav", Some("gtav")),
    ("vracer", Some("vracer")),
    ("aeroflyfs2", Some("aeroflyfs2")),
    ("bussim", Some("bussim")),
    ("driftphysicscrew", Some("driftphysicscrew")),
    ("hyperstorm", Some("hyperstorm")),
    ("vector36", Some("vector36")),
    ("trackmania", None),
    ("spacemonkey", None),
    ("falconbms", None),
    ("redrover", Some("redrover")),
    ("aceshigh", Some("aceshigh")),
    ("fs", None),
    ("warplanes", None),
    ("wrc", Some("wrc")),
    ("wrc2023", Some("wrc2023")),
    ("wreckfest2", Some("wreckfest2")),
    ("pmracing", None),
    ("tdusc", Some("tdusc")),
];

/// Why loading the listeners failed: the first listener, in load order, that
/// could not take its port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportFailure {
    pub listener: &'static str,
    pub key: &'static str,
    /// The error's text: a missing key, a value that is no integer, or a value of the wrong kind.
    pub text: String,
}

/// The outcome of loading the listeners at start-up.
pub struct ImportedListeners {
    /// The registered listeners. Empty when loading failed: nothing is
    /// registered and the telemetry main loop is never started.
    pub registered: Vec<Box<dyn Listener>>,
    /// The listeners loaded before the failing one. Their load-time threads run (the F1 receivers
    /// keep their ports and forward), but nothing dispatches them.
    pub detached: Vec<Box<dyn Listener>>,
    pub failure: Option<ImportFailure>,
}

/// The error text when a port value is not an integer.
fn int_error(value: &openrigcue_model::native_value::LegacyValue) -> Option<String> {
    use openrigcue_model::native_value::LegacyValue;
    if ImportedPort::from_native(value).is_some() {
        return None;
    }
    Some(match value {
        LegacyValue::Str(_) | LegacyValue::Bytes(_) => {
            let shown: String = openrigcue_model::python_repr::repr(value).chars().take(200).collect();
            format!("invalid literal for int() with base 10: {shown}")
        }
        LegacyValue::Float(number) if number.is_nan() => "cannot convert float NaN to integer".to_owned(),
        LegacyValue::Float(_) => "cannot convert float infinity to integer".to_owned(),
        other => format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            other.type_name()
        ),
    })
}

impl ListenerInputs<'_> {
    /// The first listener, in load order, that fails on its port.
    pub fn import_failure(&self) -> Option<ImportFailure> {
        let raw = self.settings.native_snapshot();
        let ports = raw.get("NETWORK_PORT").and_then(|section| section.as_dict());
        IMPORT_ORDER.iter().find_map(|&(listener, key)| {
            let key = key?;
            let text = match ports.map(|ports| ports.get(key)) {
                None => Some("'NETWORK_PORT'".to_owned()),
                Some(None) => Some(format!("'{key}'")),
                Some(Some(_)) if key == "bussim" => None,
                Some(Some(value)) => int_error(value),
            };
            text.map(|text| ImportFailure { listener, key, text })
        })
    }
}

/// Loads the listeners at start-up: when a
/// listener fails on its port, the whole load fails with it. The caller
/// only logs; the application goes on without registered listeners and without the telemetry main loop,
/// while the listeners loaded before the failing one stay loaded.
///
/// The later listeners are built and released again here: their sockets are
/// bound for that moment.
pub fn import_listeners(
    inputs: &ListenerInputs<'_>,
    model_reader: Option<openrigcue_sources::process_memory::simconnect::NativeModelReader>,
    pid_reader: Option<openrigcue_sources::process_memory::elite_dangerous::NativePidReader>,
) -> ImportedListeners {
    let failure = inputs.import_failure();
    let listeners = build_listeners_with_native_readers(inputs, model_reader, pid_reader);
    let Some(failure) = failure else {
        return ImportedListeners { registered: listeners, detached: Vec::new(), failure: None };
    };
    let position = |id: &str| IMPORT_ORDER.iter().position(|(listener, _)| *listener == id);
    let failed_at = position(failure.listener).unwrap_or(0);
    // The input device loops start after the listeners and so never start here: the joystick listener's mapping
    // thread would run without any input pass, which leaves nothing to observe. It is not kept.
    let detached = listeners
        .into_iter()
        .filter(|listener| listener.id() != "joy" && position(listener.id()).is_some_and(|at| at < failed_at))
        .collect();
    ImportedListeners { registered: Vec::new(), detached, failure: Some(failure) }
}

/// The process scanner and current settings lookups, used at the point each consumer needs them.
pub fn build_listeners_with_native_readers(
    inputs: &ListenerInputs<'_>,
    model_reader: Option<openrigcue_sources::process_memory::simconnect::NativeModelReader>,
    pid_reader: Option<openrigcue_sources::process_memory::elite_dangerous::NativePidReader>,
) -> Vec<Box<dyn Listener>> {
    let i = inputs;
    let mut listeners = vec![
        i.probe_listener("grid"),
        i.probe_listener("f1"),
        i.season_2018_listener_with(|port| i.wrap(F1Season2018Source::from_imported_port(port))),
    ];
    for (season, _, _) in SEASONS {
        listeners.push(
            i.season_listener_with(season, |season, port| i.wrap(F1SeasonSource::from_imported_port(season, port))),
        );
    }
    listeners.extend([
        i.dashboard_listener_with("forza", |port| i.wrap(ForzaSource::from_imported_port(port))),
        i.gran_turismo_listener_with("gt7", |port| i.wrap(gran_turismo(i, GranTurismo::Gt7, port))),
        i.gran_turismo_listener_with("gtsport", |port| i.wrap(gran_turismo(i, GranTurismo::Sport, port))),
        i.diagnostic_flight_listener::<ApiDecoder>(),
        Some(i.wrap({
            let mut source =
                JoystickSource::new(JoystickProfile::default(), JoystickOptions::default(), i.joystick_inputs.clone());
            if let Some(reader) = &i.joystick_runtime {
                source.set_runtime_reader(reader.clone());
            }
            source
        })),
        i.acudp_listener_with(|reader| i.wrap(AssettoCorsaUdpSource::with_live_network_ports(reader))),
        i.project_cars_listener_with("projectcarsudp", |port| i.wrap(ProjectCarsSource::from_imported_port(port))),
        i.project_cars_listener_with("projectcars2udp", |port| i.wrap(ProjectCars2Source::from_imported_port(port))),
        i.simple_listener("ironrebellion"),
        Some(i.wrap(i.iracing())),
        Some(i.wrap(i.project_cars())),
        Some(i.wrap(i.project_cars2())),
        Some(i.wrap(i.automobilista2())),
        i.simple_listener("thecrew"),
        i.simple_listener("gtav"),
        i.vracer_listener_with(|port| i.wrap(vracer(port))),
        i.bussim_listener_with(|port| i.wrap(BusSim::from_port_text(port))),
        i.compact_listener("driftphysicscrew"),
        i.compact_listener("hyperstorm"),
        i.compact_listener("vector36"),
        Some(i.wrap(TrackMania::new())),
        Some(i.wrap(i.space_monkey())),
        Some(i.wrap(i.falcon())),
        i.text_stream_listener("redrover"),
        i.quiet_flight_listener::<AeroflyDecoder>(),
        i.remaining_codemasters_listener("dirt"),
        i.remaining_codemasters_listener("tdusc"),
        Some(i.wrap(RfactorOne::new(RfactorOneVariant::Rf1))),
        Some(i.wrap(RfactorOne::new(RfactorOneVariant::Rfpro))),
        Some(i.wrap(i.rfactor2())),
        i.xplane_listener_with(|port, options| i.wrap(i.xplane(port, options))),
        i.diagnostic_flight_listener::<DcsDecoder>(),
        i.il2_listener_with(|port| i.wrap(Il2Source::from_imported_port(port))),
        Some(i.wrap(Il2Cliffs::new())),
        i.nl2_listener_with(|port| i.wrap(NoLimits2::from_imported_port(port))),
        i.text_stream_listener("epic"),
        Some(i.warthunder_listener_with(|reader| i.wrap(WarThunder::with_live_network_ports(reader)))),
        Some(i.wrap(TruckSim::new())),
        Some(i.wrap(Piboso::new(&piboso::KART_RACING_PRO))),
        Some(i.wrap(Piboso::new(&piboso::GP_BIKES))),
        Some(i.wrap(Piboso::new(&piboso::MX_BIKES))),
        Some(i.wrap(raceroom(i))),
        Some(i.wrap(i.assetto_corsa())),
        Some(i.wrap(i.assetto_corsa_evo())),
        Some(i.wrap(Piboso::new(&piboso::WORLD_RACING_SERIES))),
        i.remaining_codemasters_listener("wrc"),
        i.error_set_listener_with("wrc2023", |port| i.wrap(Wrc2023Source::from_imported_port(port))),
        i.wreckfest_listener(),
        Some(i.wrap(i.pmracing())),
        i.rbr_listener(),
        i.final_poll_listener("motogp"),
        i.lfs_listener(),
        i.dashboard_listener_with("beamng", |port| i.wrap(BeamNgSource::from_imported_port(port))),
        Some(i.wrap(DynamicCab::with_runtime_dir(i.runtime_dir))),
        Some(i.wrap(SimBin::new(SimBinTitle::Gtr))),
        Some(i.wrap(SimBin::new(SimBinTitle::Gtr2))),
        Some(i.wrap(SimBin::new(SimBinTitle::GtLegends))),
        Some(i.wrap(SimBin::new(SimBinTitle::Race07))),
        i.error_set_listener_with("kartkraft", |port| i.wrap(KartKraftSource::from_imported_port(port))),
        i.elite_listener_with(|settings| {
            let mut source = EliteDangerous::new(settings);
            if let Some(reader) = pid_reader {
                source.set_pid_reader(reader);
            }
            i.wrap(source)
        }),
        i.flight_simulator_with(Simulator::Prepar3d, |settings, port| {
            i.wrap(FlightSimulator::from_imported_port(settings, port))
        }),
        i.flight_simulator_with(Simulator::Fsx, |settings, port| {
            i.wrap(FlightSimulator::from_imported_port(settings, port))
        }),
        i.flight_simulator_with(Simulator::Msfs, |settings, port| {
            let mut source = FlightSimulator::from_imported_port(settings, port);
            if let Some(reader) = model_reader {
                source.set_model_reader(reader);
            }
            i.wrap(source)
        }),
        i.quiet_flight_listener::<CondorDecoder>(),
        i.text_flight_listener::<FlyInsideDecoder>(),
        i.quiet_flight_listener::<SilentWingsDecoder>(),
        i.text_flight_listener::<AcesHighDecoder>(),
        Some(i.wrap(FarmingSimulator::with_runtime_dir(i.runtime_dir))),
        i.final_poll_listener("touringkarts"),
        Some(i.wrap(Warplanes::new())),
    ]);
    listeners.into_iter().flatten().collect()
}

/// The EA F1 seasons 2019–2025: listener id (also the `NETWORK_PORT` key) and default port.
const SEASONS: [(Season, &str, u16); 7] = [
    (Season::Y2019, "f12019", 20780),
    (Season::Y2020, "f12020", 20782),
    (Season::Y2021, "f12021", 20783),
    (Season::Y2022, "f12022", 20784),
    (Season::Y2023, "f12023", 20785),
    (Season::Y2024, "f12024", 20786),
    (Season::Y2025, "f12025", 20787),
];

fn gran_turismo(i: &ListenerInputs<'_>, game: GranTurismo, port: ImportedPort) -> GranTurismoSource {
    let mut source = GranTurismoSource::from_imported_port(
        game,
        GranTurismoSettings {
            bind_port: game.default_bind_port(),
            playstation_ip: i.settings.app.playstation_address.clone(),
        },
        port,
    );
    source.set_console_reader(i.console_reader.clone().unwrap_or_else(|| {
        let value = i
            .settings
            .native_snapshot()
            .get("APP")
            .and_then(|app| app.as_dict())
            .and_then(|app| app.get("playstation_ip"))
            .cloned();
        std::sync::Arc::new(move || value.clone())
    }));
    source
}

/// V-Racer hands every datagram to UDP forwarding (the running listener's `PORT_FORWARDING` targets).
fn vracer(port: ImportedPort) -> ForwardingVRacer {
    let forwarder = Forwarder::default();
    let mut source = VRacer::from_imported_port(port);
    source.set_forwarder(Box::new(move |datagram| forwarder.forward(datagram)));
    ForwardingVRacer { source }
}

/// The RaceRoom listener reads the car table on its own thread from the plugin folder of the first
/// Steam library holding the game, even when that folder lacks the file.
fn raceroom(i: &ListenerInputs<'_>) -> RaceRoom {
    let steam = i.steam.cloned();
    let mut source = RaceRoom::with_car_loader(i.pause(), move || {
        steam.as_ref().and_then(|libraries| SteamPlugin { location: R3E_DATA_FILE }.plugin_dir(libraries))
    });
    source.set_pause_reader(i.native_pause_reader());
    source
}

/// The joystick profile as the joystick listener reads it. Entries that can never act
/// are left out; an entry with a non-integer trigger is reported and left out.
pub fn joystick_profile(profiles: &JoystickProfiles, name: &str) -> JoystickProfile {
    let mut assignments = Vec::new();
    for (key, entry) in profiles.profile(name).into_iter().flatten() {
        match JoystickAssignment::from_stored(
            key,
            &entry.joystick,
            &entry.kind,
            &entry.trigger,
            &entry.mode,
            entry.invert_axis,
        ) {
            Ok(Some(assignment)) => assignments.push(assignment),
            Ok(None) => {}
            Err(error) => tracing::warn!(profile = name, ?error, "joystick profile entry ignored"),
        }
    }
    JoystickProfile { name: name.to_owned(), assignments }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        /// The `NETWORK_PORT` keys looked up on this thread, so a test can check it configured every one.
        static PORT_KEYS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn note_port_key(key: &str) {
        PORT_KEYS.with(|keys| keys.borrow_mut().push(key.to_owned()));
    }

    /// Settings that keep the listeners off the real game ports: several listeners bind their socket when
    /// constructed (`il2`, the F1 seasons), and a running service or game may own
    /// those ports. Every `NETWORK_PORT` entry is 0 (an ephemeral loopback or wildcard port), and the Elite
    /// Dangerous update thread stays off.
    fn isolated_settings() -> Settings {
        let mut settings = Settings::default();
        settings.features.check_elite_dangerous_updates = false;
        let keys: Vec<String> = settings.network_ports.iter().map(|(key, _)| key.to_owned()).collect();
        for key in keys {
            settings.network_ports.set(&key, 0);
        }
        settings
    }

    #[test]
    fn xplane_native_import_flags_compare_exact_yes() {
        use openrigcue_model::native_value::LegacyValue;
        for (value, expected) in [
            (LegacyValue::from("yes"), true),
            (LegacyValue::from("Y"), false),
            (LegacyValue::from("true"), false),
            (LegacyValue::from("1"), false),
            (LegacyValue::from("Yes"), false),
            (LegacyValue::from(" yes "), false),
            (LegacyValue::Bool(true), false),
            (LegacyValue::Int(1), false),
            (LegacyValue::None, false),
            (LegacyValue::List(vec!["yes".into()]), false),
        ] {
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                let fields = raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap();
                for key in ["xplane_invert_yaw", "xplane_invert_roll", "xplane_invert_pitch"] {
                    fields.insert(key, value.clone());
                }
            });
            let inputs = ListenerInputs {
                settings: &settings,
                documents: Path::new("/unused"),
                runtime_dir: Path::new("/unused"),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            let (port, options) = inputs.xplane_listener_with(|port, options| (port, options)).unwrap();
            assert_eq!(port, ImportedPort::Valid(49000));
            assert_eq!([options.invert_yaw, options.invert_roll, options.invert_pitch], [expected; 3], "{value:?}");
        }
    }

    #[test]
    fn xplane_native_import_requires_all_flags_before_constructing() {
        for missing in ["CUSTOM_FEATURES", "xplane_invert_yaw", "xplane_invert_roll", "xplane_invert_pitch"] {
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                if missing == "CUSTOM_FEATURES" {
                    raw.remove(missing);
                } else {
                    raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap().remove(missing);
                }
            });
            let inputs = ListenerInputs {
                settings: &settings,
                documents: Path::new("/unused"),
                runtime_dir: Path::new("/unused"),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            assert!(inputs.xplane_listener_with(|_, _| panic!("constructor ran with missing {missing}")).is_none());
        }
    }

    /// The listeners of [`build_listeners`] on [`isolated_settings`], with the documents folder and the pipe
    /// relay's runtime directory in `tmp` (never the real `$XDG_RUNTIME_DIR`). Fails when a listener looked up a
    /// port that is not configured, since it would have bound its default game port.
    fn isolated_listeners(tmp: &Path) -> Vec<Box<dyn Listener>> {
        let settings = isolated_settings();
        PORT_KEYS.with(|keys| keys.borrow_mut().clear());
        let listeners = build_listeners(&ListenerInputs {
            settings: &settings,
            documents: tmp,
            runtime_dir: tmp,
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: None,
            pause_reader: None,
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: None,
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        });
        let unset: Vec<String> = PORT_KEYS
            .with(|keys| keys.take())
            .into_iter()
            .filter(|key| settings.network_ports.get(key) != Some(0))
            .collect();
        assert!(unset.is_empty(), "listeners on their default ports: {unset:?}");
        listeners
    }

    #[test]
    fn native_missing_port_does_not_construct_a_fallback_listener() {
        use openrigcue_config::import::value::LegacyValue;
        let mut raw = isolated_settings().native_snapshot();
        raw.get_mut("NETWORK_PORT").and_then(LegacyValue::as_dict_mut).unwrap().remove("api");
        let (settings, _) = Settings::from_legacy(&raw);
        // No listener constructor may substitute a default for this missing key.
        assert!(settings.network_ports.get("api").is_none());
        let tmp = tempfile::tempdir().unwrap();
        let inputs = ListenerInputs {
            settings: &settings,
            documents: tmp.path(),
            runtime_dir: tmp.path(),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: None,
            pause_reader: None,
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: None,
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        };
        let source = inputs.configured("api", || -> PolledUdpSource<ApiDecoder> {
            panic!("missing native key must prevent source construction")
        });
        assert!(source.is_none());
        assert_eq!(inputs.configured_port("api"), None);
    }

    #[test]
    fn native_dashboard_import_keeps_successful_integer_ports() {
        for key in ["forza", "beamng"] {
            for (text, expected) in [
                ("70000", Some(ImportedPort::OutOfRange)),
                ("-1", Some(ImportedPort::OutOfRange)),
                ("999999999999999999999999999999999999999999", Some(ImportedPort::OutOfRange)),
                ("+0065535", Some(ImportedPort::Valid(65535))),
                ("0", Some(ImportedPort::Valid(0))),
                ("bad", None),
            ] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let mut constructions = 0;
                assert_eq!(
                    inputs.dashboard_listener_with(key, |port| {
                        constructions += 1;
                        port
                    }),
                    expected,
                    "{key} {text}"
                );
                assert_eq!(constructions, usize::from(expected.is_some()));
            }
        }
    }

    #[test]
    fn native_gran_turismo_import_keeps_successful_integer_ports() {
        for key in ["gt7", "gtsport"] {
            for (text, expected) in [
                ("70000", Some(ImportedPort::OutOfRange)),
                ("-1", Some(ImportedPort::OutOfRange)),
                ("999999999999999999999999999999999999999999", Some(ImportedPort::OutOfRange)),
                ("+0065535", Some(ImportedPort::Valid(65535))),
                ("0", Some(ImportedPort::Valid(0))),
                ("bad", None),
            ] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let mut constructions = 0;
                assert_eq!(
                    inputs.gran_turismo_listener_with(key, |port| {
                        constructions += 1;
                        port
                    }),
                    expected,
                    "{key} {text}"
                );
                assert_eq!(constructions, usize::from(expected.is_some()));
            }
        }
    }

    #[test]
    fn native_project_cars_import_keeps_successful_integer_ports() {
        for key in ["projectcarsudp", "projectcars2udp"] {
            for (text, expected) in [
                ("70000", Some(ImportedPort::OutOfRange)),
                ("-1", Some(ImportedPort::OutOfRange)),
                ("999999999999999999999999999999999999999999", Some(ImportedPort::OutOfRange)),
                ("+0065535", Some(ImportedPort::Valid(65535))),
                ("0", Some(ImportedPort::Valid(0))),
                ("bad", None),
            ] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let mut constructions = 0;
                assert_eq!(
                    inputs.project_cars_listener_with(key, |port| {
                        constructions += 1;
                        port
                    }),
                    expected,
                    "{key} {text}"
                );
                assert_eq!(constructions, usize::from(expected.is_some()));
            }
        }
    }

    #[test]
    fn native_acudp_registration_defers_required_port_lookup_until_selected_session() {
        for raw in [None, Some("70000"), Some("bad")] {
            let mut settings = Settings::default();
            settings.edit_native(|native| {
                let ports = native.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap();
                ports.remove("acudp");
                if let Some(raw) = raw {
                    ports.insert("acudp", raw.into());
                }
            });
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            let result = inputs.acudp_listener_with(|reader| reader().get("acudp").cloned());
            assert_eq!(result, Some(raw.map(openrigcue_model::native_value::LegacyValue::from)), "{raw:?}");
        }
    }

    #[test]
    fn native_acudp_factory_keeps_the_live_reader_after_construction() {
        use openrigcue_model::native_value::{Dict, LegacyValue};
        let current =
            std::sync::Arc::new(std::sync::Mutex::new(Dict::from_iter([("acudp".into(), LegacyValue::from("6001"))])));
        let reader: NetworkPortsReader = {
            let current = current.clone();
            std::sync::Arc::new(move || current.lock().unwrap().clone())
        };
        let settings = Settings::default();
        let tmp = tempfile::tempdir().unwrap();
        let inputs = ListenerInputs {
            settings: &settings,
            documents: tmp.path(),
            runtime_dir: tmp.path(),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: Some(reader),
            pause_reader: None,
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: None,
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        };
        let saved = inputs.acudp_listener_with(|reader| reader).unwrap();
        assert_eq!(saved().get("acudp"), Some(&LegacyValue::from("6001")));
        current.lock().unwrap().insert("acudp", "6002".into());
        assert_eq!(saved().get("acudp"), Some(&LegacyValue::from("6002")));
    }

    #[test]
    fn native_moto_touring_import_keeps_integer_ports_until_bind() {
        for key in ["motogp", "touringkarts"] {
            for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let source = inputs.final_poll_listener(key);
                assert_eq!(source.is_some(), imported, "{key}: {text}");
                if let Some(source) = source {
                    assert_eq!(source.id(), key);
                }
            }
        }
    }

    #[test]
    fn native_wreckfest_import_keeps_integer_port_until_bind() {
        for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
            let parsed =
                openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\nwreckfest2 = {text}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("wreckfest2").unwrap().clone();
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("wreckfest2", value);
            });
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            let source = inputs.wreckfest_listener();
            assert_eq!(source.is_some(), imported, "wreckfest2: {text}");
            if let Some(source) = source {
                assert_eq!(source.id(), "wreckfest2");
            }
        }
    }

    #[test]
    fn native_wreckfest_factory_keeps_the_pause_reader_lazy() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let settings = Settings::default();
        let tmp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let inputs = ListenerInputs {
            settings: &settings,
            documents: tmp.path(),
            runtime_dir: tmp.path(),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: None,
            pause_reader: Some(Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                None
            })),
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: None,
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        };
        let source = inputs.wreckfest(ImportedPort::Valid(6001));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            openrigcue_sources::Source::read_native(&source, "rpm"),
            openrigcue_sources::NativeRead::Value(openrigcue_model::native_value::LegacyValue::Int(0))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no packet short-circuits before the setting read");
    }

    #[test]
    fn native_gran_turismo_factory_reads_current_console_setting() {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_sources::{NativeRead, Source};
        let config =
            Shared::new(crate::store::ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()));
        let settings = config.read().settings().clone();
        let read = console_reader(config.clone());
        let tmp = tempfile::tempdir().unwrap();
        let inputs = ListenerInputs {
            settings: &settings,
            documents: tmp.path(),
            runtime_dir: tmp.path(),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: None,
            pause_reader: None,
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: Some(read.clone()),
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        };
        let sources =
            [GranTurismo::Gt7, GranTurismo::Sport].map(|game| gran_turismo(&inputs, game, ImportedPort::Valid(6001)));
        for (value, expected) in [
            (LegacyValue::from(""), NativeRead::Value(LegacyValue::Int(0))),
            (LegacyValue::from("192.168.1.50"), NativeRead::Raised),
            (LegacyValue::None, NativeRead::Raised),
            (LegacyValue::from(""), NativeRead::Value(LegacyValue::Int(0))),
        ] {
            config.write().update(|configuration| {
                configuration.settings.edit_native(|native| {
                    native.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("playstation_ip", value.clone());
                })
            });
            assert_eq!(read(), Some(value));
            for source in &sources {
                assert_eq!(source.read_native("speed"), expected);
            }
        }
        config.write().update(|configuration| {
            configuration.settings.edit_native(|native| {
                native.get_mut("APP").unwrap().as_dict_mut().unwrap().remove("playstation_ip");
            })
        });
        assert_eq!(read(), None);
        for source in &sources {
            assert_eq!(source.read_native("speed"), NativeRead::Raised);
        }
    }

    #[test]
    fn native_windows_factory_observes_scanner_publication_without_a_tick() {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_sources::{NativeRead, Source};
        let settings = Settings::default();
        let tmp = tempfile::tempdir().unwrap();
        let snapshot = Shared::new(openrigcue_config::detect::SystemSnapshot::default());
        let read = windows_reader(snapshot.clone());
        let context: Shared<SourceContext> = Shared::default();
        context.write().window_titles = Some(vec!["Notepad".into()]);
        let inputs = ListenerInputs {
            settings: &settings,
            documents: tmp.path(),
            runtime_dir: tmp.path(),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: None,
            pause_reader: None,
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: None,
            windows_reader: Some(read.clone()),
            context: context.clone(),
            outputs: Shared::default(),
        };
        let source = inputs.space_monkey();
        assert_eq!(source.read_native("car_name"), NativeRead::Raised);
        snapshot.write().windows = Some(vec!["DiRT 5".into()]);
        assert_eq!(source.read_native("car_name"), NativeRead::Value(LegacyValue::from("dirt5")));
        snapshot.write().windows = Some(vec!["WRC7".into()]);
        assert_eq!(source.read_native("car_name"), NativeRead::Value(LegacyValue::from("wrc7")));
        snapshot.write().windows = Some(Vec::new());
        assert_eq!(source.read_native("car_name"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(context.read().window_titles, Some(vec!["Notepad".into()]));
        snapshot.write().windows = Some(vec!["Falcon BMS".into()]);
        let falcon = inputs.falcon();
        assert_eq!(read(), Some(vec!["Falcon BMS".into()]));
        assert_eq!(falcon.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(10300)));
    }

    #[test]
    fn native_remaining_codemasters_import_keeps_integer_ports_until_bind() {
        for key in ["dirt", "wrc", "tdusc"] {
            for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let source = inputs.remaining_codemasters_listener(key);
                assert_eq!(source.is_some(), imported, "{key}: {text}");
                if let Some(source) = source {
                    assert_eq!(source.id(), key);
                }
            }
        }
    }

    #[test]
    fn native_elite_pid_reader_tracks_published_scanner_map() {
        let snapshot = Shared::new(openrigcue_config::detect::SystemSnapshot::default());
        let reader = elite_pid_reader(snapshot.clone());
        assert_eq!(reader(), None);
        snapshot.write().processes.insert("EliteDangerous64.exe".into(), 4242);
        snapshot.write().processes.insert("unrelated.exe".into(), 9999);
        assert_eq!(reader(), Some(4242));
        snapshot.write().processes.insert("EliteDangerous64.exe".into(), 5151);
        assert_eq!(reader(), Some(5151));
        snapshot.write().processes.remove("EliteDangerous64.exe");
        assert_eq!(reader(), None);
    }

    #[test]
    fn native_msfs_model_reader_tracks_current_settings_without_retaining_lock() {
        use openrigcue_model::native_value::LegacyValue;
        let config =
            Shared::new(crate::store::ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()));
        let reader = motion_model_reader(config.clone());
        for value in [LegacyValue::from("Yaw2 PRO"), LegacyValue::from("None"), LegacyValue::Bool(true)] {
            config.write().update(|configuration| {
                configuration.settings.edit_native(|raw| {
                    raw.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap().insert("model", value.clone());
                });
            });
            assert_eq!(reader(), Some(value));
        }
        config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.remove("MOTIONBOX");
            });
        });
        assert_eq!(reader(), None);
    }

    #[test]
    fn elite_native_import_flags_compare_exact_yes() {
        use openrigcue_model::native_value::LegacyValue;

        for (value, expected) in [
            (LegacyValue::from("yes"), true),
            (LegacyValue::from("Y"), false),
            (LegacyValue::from("true"), false),
            (LegacyValue::from("1"), false),
            (LegacyValue::from("Yes"), false),
            (LegacyValue::Bool(true), false),
            (LegacyValue::Int(1), false),
            (LegacyValue::None, false),
        ] {
            let value = if let LegacyValue::Str(text) = value {
                let parsed = openrigcue_config::import::configobj::parse(&format!(
                    "[CUSTOM_FEATURES]\ned_invert_yaw = {text}\n"
                ))
                .unwrap();
                parsed.get("CUSTOM_FEATURES").unwrap().as_dict().unwrap().get("ed_invert_yaw").unwrap().clone()
            } else {
                value
            };
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                let features = raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap();
                for key in ["ed_invert_yaw", "ed_invert_roll", "ed_invert_pitch"] {
                    features.insert(key, value.clone());
                }
                features.insert("check_ed_updates", "no".into());
            });
            let configured = simconnect_metadata_inputs(&settings).elite_listener_with(|settings| settings).unwrap();
            assert_eq!(
                [configured.invert_yaw, configured.invert_roll, configured.invert_pitch],
                [expected; 3],
                "{value:?}"
            );
            assert!(!configured.check_updates);
        }
        for active in 0..3 {
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                let features = raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap();
                for (axis, key) in ["ed_invert_yaw", "ed_invert_roll", "ed_invert_pitch"].into_iter().enumerate() {
                    features.insert(key, if axis == active { "yes" } else { "no" }.into());
                }
            });
            let configured = simconnect_metadata_inputs(&settings).elite_listener_with(|settings| settings).unwrap();
            let mut expected = [false; 3];
            expected[active] = true;
            assert_eq!([configured.invert_yaw, configured.invert_roll, configured.invert_pitch], expected);
        }
    }

    #[test]
    fn elite_native_import_requires_features_before_updater_construction() {
        for missing in ["CUSTOM_FEATURES", "ed_invert_yaw", "ed_invert_roll", "ed_invert_pitch", "check_ed_updates"] {
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                if missing == "CUSTOM_FEATURES" {
                    raw.remove(missing);
                } else {
                    raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap().remove(missing);
                }
            });
            assert!(simconnect_metadata_inputs(&settings)
                .elite_listener_with(|_| panic!("constructor ran with missing {missing}"))
                .is_none());
        }
    }

    #[test]
    fn elite_native_import_update_gate_and_manufacturer_fallback() {
        use openrigcue_model::native_value::LegacyValue;

        for (value, expected) in [
            (LegacyValue::from("yes"), Some(true)),
            (LegacyValue::from("Y"), Some(true)),
            (LegacyValue::from("True"), Some(true)),
            (LegacyValue::from("no"), Some(false)),
            (LegacyValue::from("0"), Some(false)),
            (LegacyValue::Bool(true), Some(true)),
            (LegacyValue::Bool(false), Some(false)),
            (LegacyValue::Int(1), Some(true)),
            (LegacyValue::Int(0), Some(false)),
            (LegacyValue::from(" yes "), None),
            (LegacyValue::from("bad"), None),
            (LegacyValue::None, None),
            (LegacyValue::Int(2), None),
            (LegacyValue::List(vec!["yes".into()]), None),
        ] {
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("CUSTOM_FEATURES")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("check_ed_updates", value.clone());
            });
            let configured = simconnect_metadata_inputs(&settings).elite_listener_with(|settings| settings);
            assert_eq!(configured.map(|settings| settings.check_updates), expected, "{value:?}");
        }
        for model in [Some("Yaw2 PRO"), Some("None"), Some("unknown"), None] {
            let mut settings = Settings::default();
            settings.edit_native(|raw| match model {
                Some(model) => {
                    raw.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap().insert("model", model.into());
                }
                None => {
                    raw.remove("MOTIONBOX");
                }
            });
            let configured = simconnect_metadata_inputs(&settings).elite_listener_with(|settings| settings).unwrap();
            assert_eq!(configured.yaw_vr, model == Some("Yaw2 PRO"));
        }
    }

    #[test]
    fn simconnect_native_import_flags_compare_exact_yes() {
        use openrigcue_model::native_value::LegacyValue;

        for simulator in [Simulator::Fsx, Simulator::Prepar3d, Simulator::Msfs] {
            for (text, expected) in [
                ("yes", true),
                ("Y", false),
                ("true", false),
                ("1", false),
                ("Yes", false),
                ("\" yes \"", false),
                ("no", false),
            ] {
                let parsed = openrigcue_config::import::configobj::parse(&format!(
                    "[CUSTOM_FEATURES]\np3d_invert_yaw = {text}\np3d_invert_roll = {text}\np3d_invert_pitch = {text}\n"
                ))
                .unwrap();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.insert("CUSTOM_FEATURES", parsed.get("CUSTOM_FEATURES").unwrap().clone());
                });
                let inputs = simconnect_metadata_inputs(&settings);
                let configured = inputs.flight_simulator_with(simulator, |settings, _| settings).unwrap();
                assert_eq!(
                    [configured.inversion.yaw, configured.inversion.roll, configured.inversion.pitch],
                    [expected; 3],
                    "{simulator:?}: {text}"
                );
            }
            for value in
                [LegacyValue::Bool(true), LegacyValue::Int(1), LegacyValue::None, LegacyValue::List(vec!["yes".into()])]
            {
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    let features = raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap();
                    for key in ["p3d_invert_yaw", "p3d_invert_roll", "p3d_invert_pitch"] {
                        features.insert(key, value.clone());
                    }
                });
                let inputs = simconnect_metadata_inputs(&settings);
                let configured = inputs.flight_simulator_with(simulator, |settings, _| settings).unwrap();
                assert_eq!(
                    [configured.inversion.yaw, configured.inversion.roll, configured.inversion.pitch],
                    [false; 3],
                    "{simulator:?}: {value:?}"
                );
            }
            for active in 0..3 {
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    let features = raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap();
                    for (axis, key) in ["p3d_invert_yaw", "p3d_invert_roll", "p3d_invert_pitch"].into_iter().enumerate()
                    {
                        features.insert(key, if axis == active { "yes" } else { "no" }.into());
                    }
                    for key in ["fsx_invert_yaw", "fsx_invert_roll", "fsx_invert_pitch"] {
                        features.insert(key, "yes".into());
                    }
                });
                let inputs = simconnect_metadata_inputs(&settings);
                let configured = inputs.flight_simulator_with(simulator, |settings, _| settings).unwrap();
                let mut expected = [false; 3];
                expected[active] = true;
                assert_eq!(
                    [configured.inversion.yaw, configured.inversion.roll, configured.inversion.pitch],
                    expected,
                    "{simulator:?}: axis {active}"
                );
            }
        }
    }

    #[test]
    fn simconnect_native_import_requires_all_flags_before_constructing() {
        for simulator in [Simulator::Fsx, Simulator::Prepar3d, Simulator::Msfs] {
            for missing in ["CUSTOM_FEATURES", "p3d_invert_yaw", "p3d_invert_roll", "p3d_invert_pitch", "nonmapping"] {
                let mut settings = Settings::default();
                settings.edit_native(|raw| match missing {
                    "CUSTOM_FEATURES" => {
                        raw.remove("CUSTOM_FEATURES");
                    }
                    "nonmapping" => {
                        raw.insert("CUSTOM_FEATURES", "yes".into());
                    }
                    key => {
                        raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap().remove(key);
                    }
                });
                let inputs = simconnect_metadata_inputs(&settings);
                assert!(inputs
                    .flight_simulator_with(simulator, |_, _| panic!("constructor ran: {simulator:?} {missing}"))
                    .is_none());
            }
        }
    }

    fn simconnect_metadata_inputs(settings: &Settings) -> ListenerInputs<'_> {
        ListenerInputs {
            settings,
            documents: Path::new("/unused"),
            runtime_dir: Path::new("/unused"),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: None,
            pause_reader: None,
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: None,
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        }
    }

    #[test]
    fn native_simconnect_import_preserves_integer_ports_for_all_three_listeners() {
        for (simulator, key) in [(Simulator::Fsx, "fsx"), (Simulator::Prepar3d, "p3d"), (Simulator::Msfs, "fsx")] {
            for (text, imported) in [("70000", true), ("-1", true), ("+0033445", true), ("bad", false)] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                // Metadata-only constructor avoids the actual receive worker.
                let result = inputs.flight_simulator_with(simulator, |settings, port| {
                    assert_eq!(settings.simulator, simulator);
                    assert_eq!(
                        port,
                        if text == "+0033445" { ImportedPort::Valid(33445) } else { ImportedPort::OutOfRange }
                    );
                    (settings, port)
                });
                assert_eq!(result.is_some(), imported, "{simulator:?} {text}");
            }
        }
    }

    #[test]
    fn native_warthunder_does_not_validate_worker_port_at_import() {
        for text in ["70000", "bad"] {
            let parsed =
                openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\nwarthunder = {text}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("warthunder").unwrap().clone();
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("warthunder", value);
            });
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            // Metadata-only constructor; do not start any worker.
            let reader = inputs.warthunder_listener_with(|reader| reader);
            assert_eq!(reader().get("warthunder").and_then(|value| value.py_str()).as_deref(), Some(text));
        }
    }

    #[test]
    fn native_warthunder_factory_defers_live_reader_until_worker_entry() {
        use openrigcue_model::native_value::{Dict, LegacyValue};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let reads = Arc::new(AtomicUsize::new(0));
        let ports = Arc::new(Mutex::new(Dict::new()));
        let reader: NetworkPortsReader = {
            let reads = reads.clone();
            let ports = ports.clone();
            Arc::new(move || {
                reads.fetch_add(1, Ordering::Relaxed);
                ports.lock().unwrap().clone()
            })
        };
        let settings = Settings::default();
        let tmp = tempfile::tempdir().unwrap();
        let inputs = ListenerInputs {
            settings: &settings,
            documents: tmp.path(),
            runtime_dir: tmp.path(),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: Some(reader),
            pause_reader: None,
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: None,
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        };
        let reader = inputs.warthunder_listener_with(|reader| reader);
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        ports.lock().unwrap().insert("warthunder", LegacyValue::Str("008111".into()));
        assert_eq!(reader().get("warthunder"), Some(&LegacyValue::Str("008111".into())));
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn native_bussim_import_keeps_original_port_text() {
        for text in ["0037337", "+37337", "70000", "bad"] {
            let parsed =
                openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\nbussim = {text}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("bussim").unwrap().clone();
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("bussim", value);
            });
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            let port = inputs.bussim_listener_with(|port| port);
            assert_eq!(port.as_deref(), Some(text), "str() retains native port text {text}");
            let source = inputs.bussim_listener_with(BusSim::from_port_text).unwrap();
            assert_eq!(openrigcue_sources::Source::id(&source), "bussim");
        }
    }

    #[test]
    fn native_nl2_import_preserves_integer_until_connect() {
        for (text, imported) in [("70000", true), ("-1", true), ("+0015151", true), ("bad", false)] {
            let parsed =
                openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\nnl2 = {text}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("nl2").unwrap().clone();
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("nl2", value);
            });
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            let source = inputs.nl2_listener_with(NoLimits2::from_imported_port);
            assert_eq!(source.is_some(), imported, "nl2 {text}");
            if let Some(source) = source {
                assert_eq!(openrigcue_sources::Source::id(&source), "nl2");
            }
        }
    }

    #[test]
    fn native_vracer_import_preserves_cached_integer_port() {
        for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
            let parsed =
                openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\nvracer = {text}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("vracer").unwrap().clone();
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("vracer", value);
            });
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            let source = inputs.vracer_listener_with(vracer);
            assert_eq!(source.is_some(), imported, "vracer {text}");
            if let Some(source) = source {
                assert_eq!(openrigcue_sources::Source::id(&source), "vracer");
            }
        }
    }

    #[test]
    fn native_xplane_import_keeps_destination_until_send() {
        for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
            let parsed =
                openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\nxplane = {text}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("xplane").unwrap().clone();
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("xplane", value);
            });
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            let port = inputs.xplane_listener_with(|port, _| port);
            assert_eq!(port.is_some(), imported, "xplane {text}");
            if let Some(port) = port {
                assert_eq!(port, if text == "+006001" { ImportedPort::Valid(6001) } else { ImportedPort::OutOfRange });
            }
        }
    }

    #[test]
    fn native_il2_import_preserves_integer_before_worker_setup() {
        for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
            let parsed =
                openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\nil2 = {text}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("il2").unwrap().clone();
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("il2", value);
            });
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            let port = inputs.il2_listener_with(|port| port);
            assert_eq!(port.is_some(), imported, "il2 {text}");
            if let Some(port) = port {
                assert_eq!(port, if text == "+006001" { ImportedPort::Valid(6001) } else { ImportedPort::OutOfRange });
            }
        }
    }

    #[test]
    fn native_dcs_api_import_keeps_integer_ports_until_bind() {
        for key in ["dcs", "api"] {
            for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let outputs = Shared::default();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs,
                };
                let source = match key {
                    "dcs" => inputs.diagnostic_flight_listener::<DcsDecoder>(),
                    "api" => inputs.diagnostic_flight_listener::<ApiDecoder>(),
                    _ => unreachable!(),
                };
                assert_eq!(source.is_some(), imported, "{key}: {text}");
                if let Some(source) = source {
                    assert_eq!(source.id(), key);
                }
            }
        }
    }

    #[test]
    fn native_aces_and_flyinside_import_keeps_integer_ports_until_bind() {
        for key in ["aceshigh", "flyinside"] {
            for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let source = match key {
                    "aceshigh" => inputs.text_flight_listener::<AcesHighDecoder>(),
                    "flyinside" => inputs.text_flight_listener::<FlyInsideDecoder>(),
                    _ => unreachable!(),
                };
                assert_eq!(source.is_some(), imported, "{key}: {text}");
                if let Some(source) = source {
                    assert_eq!(source.id(), key);
                }
            }
        }
    }

    #[test]
    fn native_quiet_flight_import_keeps_integer_ports_until_bind() {
        for key in ["condor", "silentwings", "aeroflyfs2"] {
            for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let source = match key {
                    "condor" => inputs.quiet_flight_listener::<CondorDecoder>(),
                    "silentwings" => inputs.quiet_flight_listener::<SilentWingsDecoder>(),
                    "aeroflyfs2" => inputs.quiet_flight_listener::<AeroflyDecoder>(),
                    _ => unreachable!(),
                };
                assert_eq!(source.is_some(), imported, "{key}: {text}");
                if let Some(source) = source {
                    assert_eq!(source.id(), key);
                }
            }
        }
    }

    #[test]
    fn native_lfs_import_keeps_integer_port_until_bind() {
        for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
            let parsed =
                openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\nlfs = {text}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("lfs").unwrap().clone();
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("lfs", value);
            });
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            let source = inputs.lfs_listener();
            assert_eq!(source.is_some(), imported, "lfs: {text}");
            if let Some(source) = source {
                assert_eq!(source.id(), "lfs");
            }
        }
    }

    #[test]
    fn native_rbr_import_keeps_integer_port_until_bind() {
        for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
            let parsed =
                openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\nrbr = {text}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("rbr").unwrap().clone();
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("rbr", value);
            });
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            let source = inputs.rbr_listener();
            assert_eq!(source.is_some(), imported, "rbr: {text}");
            if let Some(source) = source {
                assert_eq!(source.id(), "rbr");
            }
        }
    }

    #[test]
    fn native_text_stream_import_keeps_cached_integer_before_range_check() {
        for key in ["redrover", "epic"] {
            for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let source = inputs.text_stream_listener(key);
                assert_eq!(source.is_some(), imported, "{key}: {text}");
                if let Some(source) = source {
                    assert_eq!(source.id(), key);
                }
            }
        }
    }

    #[test]
    fn native_compact_listener_import_keeps_cached_integer_before_range_check() {
        for key in ["driftphysicscrew", "hyperstorm", "vector36"] {
            for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("bad", false)] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let source = inputs.compact_listener(key);
                assert_eq!(source.is_some(), imported, "{key}: {text}");
                if let Some(source) = source {
                    assert_eq!(source.id(), key);
                }
            }
        }
    }

    #[test]
    fn native_simple_listener_import_preserves_successful_int_before_socket_range() {
        for key in ["ironrebellion", "thecrew", "gtav"] {
            for (text, imported) in [("70000", true), ("-1", true), ("+006001", true), ("0", true), ("bad", false)] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let listener = inputs.simple_listener(key);
                assert_eq!(listener.is_some(), imported, "{key}: {text}");
                if let Some(listener) = listener {
                    assert_eq!(listener.id(), key);
                }
            }
        }
        let mut settings = Settings::default();
        settings.edit_native(|raw| {
            let ports = raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap();
            for key in ["ironrebellion", "thecrew", "gtav"] {
                ports.remove(key);
            }
        });
        let tmp = tempfile::tempdir().unwrap();
        let inputs = ListenerInputs {
            settings: &settings,
            documents: tmp.path(),
            runtime_dir: tmp.path(),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: None,
            pause_reader: None,
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: None,
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        };
        for key in ["ironrebellion", "thecrew", "gtav"] {
            assert!(inputs.simple_listener(key).is_none());
        }
    }

    #[test]
    fn native_error_set_listener_import_keeps_successful_integer_ports() {
        for key in ["wrc2023", "kartkraft"] {
            for (text, expected) in [
                ("70000", Some(ImportedPort::OutOfRange)),
                ("-1", Some(ImportedPort::OutOfRange)),
                ("999999999999999999999999999999999999999999", Some(ImportedPort::OutOfRange)),
                ("+0065535", Some(ImportedPort::Valid(65535))),
                ("0", Some(ImportedPort::Valid(0))),
                ("bad", None),
            ] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let mut constructions = 0;
                assert_eq!(
                    inputs.error_set_listener_with(key, |port| {
                        constructions += 1;
                        port
                    }),
                    expected,
                    "{key} {text}"
                );
                assert_eq!(constructions, usize::from(expected.is_some()));
            }
        }
    }

    #[test]
    fn native_2018_import_keeps_integer_ports_until_check_status() {
        for (text, expected) in [
            ("70000", Some(ImportedPort::OutOfRange)),
            ("-1", Some(ImportedPort::OutOfRange)),
            ("999999999999999999999999999999999999999999", Some(ImportedPort::OutOfRange)),
            ("+0065535", Some(ImportedPort::Valid(65535))),
            ("0", Some(ImportedPort::Valid(0))),
            ("bad", None),
        ] {
            let parsed =
                openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\nf12018 = {text}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("f12018").unwrap().clone();
            let mut settings = Settings::default();
            settings.edit_native(|raw| {
                raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("f12018", value);
            });
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            let mut constructions = 0;
            let result = inputs.season_2018_listener_with(|port| {
                constructions += 1;
                port
            });
            assert_eq!(result, expected, "f12018 int({text})");
            assert_eq!(constructions, usize::from(expected.is_some()));
        }
    }

    #[test]
    fn native_season_import_preserves_wide_and_negative_ports_before_worker_setup() {
        for (season, key, _) in SEASONS {
            for text in ["70000", "-1", "999999999999999999999999999999999999999999"] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value.clone());
                });
                assert_eq!(settings.network_port(key), None);
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let mut constructions = 0;
                let listener = inputs.season_listener_with(season, |season, port| {
                    constructions += 1;
                    (season.id(), port)
                });
                assert_eq!(listener, Some((key, ImportedPort::OutOfRange)), "{key} int({text}) succeeds before bind");
                assert_eq!(constructions, 1);
                assert_eq!(
                    settings.native_snapshot().get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key),
                    Some(&value)
                );
            }
        }
    }

    #[test]
    fn native_season_import_keeps_numeric_values_and_rejects_only_failed_conversions() {
        use openrigcue_config::import::value::LegacyValue;
        let cases = [
            (Some(LegacyValue::from("0")), Some(ImportedPort::Valid(0))),
            (Some(LegacyValue::from("+0065535")), Some(ImportedPort::Valid(65535))),
            (Some(LegacyValue::Bool(true)), Some(ImportedPort::Valid(1))),
            (Some(LegacyValue::Float(6001.9)), Some(ImportedPort::Valid(6001))),
            (None, None),
            (Some(LegacyValue::from("bad")), None),
            (Some(LegacyValue::None), None),
            (Some(LegacyValue::Float(f64::INFINITY)), None),
        ];
        for (season, key, _) in SEASONS {
            for (value, expected) in &cases {
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    let ports = raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap();
                    match value {
                        Some(value) => {
                            ports.insert(key, value.clone());
                        }
                        None => {
                            ports.remove(key);
                        }
                    }
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let mut constructions = 0;
                let listener = inputs.season_listener_with(season, |actual, port| {
                    constructions += 1;
                    assert_eq!(actual, season);
                    port
                });
                assert_eq!(listener, *expected, "{key} {value:?}");
                assert_eq!(constructions, usize::from(expected.is_some()));
            }
        }
    }

    #[test]
    fn native_probe_import_keeps_wide_and_negative_integer_ports_registered() {
        for key in ["f1", "grid"] {
            for text in ["70000", "-1", "999999999999999999999999999999999999999999"] {
                let parsed =
                    openrigcue_config::import::configobj::parse(&format!("[NETWORK_PORT]\n{key} = {text}\n")).unwrap();
                let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get(key).unwrap().clone();
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert(key, value);
                });
                assert_eq!(settings.network_port(key), None, "the socket compatibility view stays bounded");
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                let listener = inputs.probe_listener(key);
                assert!(listener.is_some(), "{key} int({text}) imports before bind checks its range");
                assert_eq!(listener.unwrap().id(), key);
            }
        }
    }

    #[test]
    fn native_probe_import_does_not_use_defaults_for_missing_or_failed_conversions() {
        use openrigcue_config::import::value::LegacyValue;
        for key in ["f1", "grid"] {
            for value in
                [None, Some(LegacyValue::from("bad")), Some(LegacyValue::None), Some(LegacyValue::Float(f64::INFINITY))]
            {
                let mut settings = Settings::default();
                settings.edit_native(|raw| {
                    let ports = raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap();
                    if let Some(value) = value {
                        ports.insert(key, value);
                    } else {
                        ports.remove(key);
                    }
                });
                let tmp = tempfile::tempdir().unwrap();
                let inputs = ListenerInputs {
                    settings: &settings,
                    documents: tmp.path(),
                    runtime_dir: tmp.path(),
                    steam: None,
                    joystick_inputs: JoystickInputs::default(),
                    joystick_runtime: None,
                    network_ports_reader: None,
                    pause_reader: None,
                    game_reader: None,
                    iracing_dams_reader: None,
                    xplane_lateral_reader: None,
                    console_reader: None,
                    windows_reader: None,
                    context: Shared::default(),
                    outputs: Shared::default(),
                };
                assert!(inputs.probe_listener(key).is_none());
                assert!(inputs.probe_listener("api").is_none(), "only F1 and GRID use this cache path");
            }
        }
    }

    #[test]
    fn pmr_reader_observes_native_configuration_without_a_dispatcher_refresh() {
        use openrigcue_config::import::value::LegacyValue;
        let config =
            Shared::new(crate::store::ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()));
        let reader = network_ports_reader(config.clone());
        for value in [LegacyValue::from("9001"), LegacyValue::None, LegacyValue::Bytes(b"9002".to_vec())] {
            config.write().update(|config| {
                let mut native = config.settings.native_snapshot();
                native.get_mut("NETWORK_PORT").and_then(LegacyValue::as_dict_mut).unwrap().insert("p", value.clone());
                config.settings = Settings::from_legacy(&native).0;
            });
            assert_eq!(reader().get("p"), Some(&value));
        }
        config.write().update(|config| {
            let mut native = config.settings.native_snapshot();
            native.remove("NETWORK_PORT");
            config.settings = Settings::from_legacy(&native).0;
        });
        assert!(reader().is_empty());
    }

    #[test]
    fn forwarding_reader_observes_live_config_without_dispatcher_refresh() {
        use openrigcue_config::import::value::LegacyValue;
        let config =
            Shared::new(crate::store::ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()));
        let reader = forwarding_reader(config.clone());
        for targets in [vec!["first:20777"], vec!["second:20777", "third:20778"], Vec::new()] {
            config.write().update(|config| {
                config.settings.edit_native(|native| {
                    native.get_mut("PORT_FORWARDING").unwrap().as_dict_mut().unwrap().insert(
                        "f12025",
                        LegacyValue::List(targets.iter().map(|target| LegacyValue::from(*target)).collect()),
                    );
                });
            });
            assert_eq!(reader("f12025"), Some(targets.iter().map(|target| (*target).to_owned()).collect()));
        }
        config.write().update(|config| {
            config.settings.edit_native(|native| {
                native.get_mut("PORT_FORWARDING").unwrap().as_dict_mut().unwrap().remove("f12025");
            })
        });
        assert_eq!(reader("f12025"), None);
    }

    #[test]
    fn native_pause_reader_observes_current_required_config_and_actual_listener_reads() {
        use openrigcue_config::import::value::LegacyValue;
        use openrigcue_sources::shared_memory::block_reader::{BlockOpener, BlockReader, MappingSpec};
        use openrigcue_sources::shared_memory::raceroom::{RaceRoomCarNames, RACEROOM_MAPPING};
        use openrigcue_sources::shared_memory::rfactor2::{RF2_SCORING_MAPPING, RF2_TELEMETRY_MAPPING};
        use openrigcue_sources::{NativeRead, Source};

        #[derive(Clone)]
        struct Blocks(std::collections::HashMap<&'static str, Vec<u8>>);
        struct Bytes(Vec<u8>);
        impl BlockReader for Bytes {
            fn read(&mut self) -> std::io::Result<&[u8]> {
                Ok(&self.0)
            }
        }
        impl BlockOpener for Blocks {
            fn open(&mut self, spec: MappingSpec) -> std::io::Result<Box<dyn BlockReader>> {
                Ok(Box::new(Bytes(self.0[spec.name].clone())))
            }
        }

        let config =
            Shared::new(crate::store::ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()));
        let set = |value: Option<LegacyValue>| {
            config.write().update(|configuration| {
                configuration.settings.edit_native(|raw| {
                    let app = raw.get_mut("APP").unwrap().as_dict_mut().unwrap();
                    match value {
                        Some(value) => {
                            app.insert("pause_telemetry_when_not_playing", value);
                        }
                        None => {
                            app.remove("pause_telemetry_when_not_playing");
                        }
                    }
                });
            });
        };
        let reader = pause_reader(config.clone());
        let mut blocks = Blocks(Default::default());
        let mut rf = vec![0; RF2_TELEMETRY_MAPPING.size];
        rf[16..20].copy_from_slice(&77_i32.to_le_bytes());
        rf[372..380].copy_from_slice(&9000_f64.to_le_bytes());
        rf[548..556].copy_from_slice(&9500_f64.to_le_bytes());
        let mut scoring = vec![0; RF2_SCORING_MAPPING.size];
        scoring[560..564].copy_from_slice(&77_i32.to_le_bytes());
        scoring[756] = 1;
        scoring[757] = 1;
        blocks.0.insert(RF2_TELEMETRY_MAPPING.name, rf);
        blocks.0.insert(RF2_SCORING_MAPPING.name, scoring);
        let mut rf = Rfactor2::with_opener(Box::new(blocks.clone()), true);
        rf.set_pause_reader(reader.clone());
        assert!(Source::is_running(&mut rf).unwrap());
        let mut rre = vec![0; RACEROOM_MAPPING.size];
        rre[1396..1400].copy_from_slice(&600_f32.to_le_bytes());
        rre[1400..1404].copy_from_slice(&900_f32.to_le_bytes());
        rre[1388..1392].copy_from_slice(&1_i32.to_le_bytes());
        blocks.0.insert(RACEROOM_MAPPING.name, rre);
        let mut rre = RaceRoom::with_opener(Box::new(blocks), true, RaceRoomCarNames::default());
        rre.set_pause_reader(reader.clone());
        assert!(Source::is_running(&mut rre).unwrap());
        let context = Shared::new(SourceContext { pause_when_not_playing: true, ..Default::default() });
        let mut rf = GameListener::new(rf, context.clone(), Shared::default());
        let mut rre = GameListener::new(rre, context, Shared::default());
        rf.receive();
        rre.receive();
        for (value, rf_value, rre_value) in [
            (LegacyValue::from("no"), LegacyValue::Int(9000), LegacyValue::from("5729")),
            (LegacyValue::from("Y"), LegacyValue::Int(0), LegacyValue::Int(0)),
            (LegacyValue::Bool(false), LegacyValue::Int(9000), LegacyValue::from("5729")),
            (LegacyValue::Int(1), LegacyValue::Int(0), LegacyValue::Int(0)),
        ] {
            set(Some(value));
            assert_eq!(rf.read_native("rpm"), NativeRead::Value(rf_value));
            assert_eq!(rre.read_native("rpm"), NativeRead::Value(rre_value));
        }
        for value in [Some(LegacyValue::from("invalid")), Some(LegacyValue::None), None] {
            set(value);
            assert_eq!(rf.read_native("unknown"), NativeRead::Raised);
            assert_eq!(rre.read_native("unknown"), NativeRead::Raised);
        }
        set(Some(LegacyValue::from("no")));
        assert_eq!(rf.read_native("rpm"), NativeRead::Value(LegacyValue::Int(9000)));
        assert_eq!(rre.read_native("rpm"), NativeRead::Value(LegacyValue::from("5729")));
        let raw = reader();
        config.write().update(|_| ());
        assert_eq!(raw, Some(LegacyValue::from("no")), "returned value holds no config guard");
        config.write().update(|configuration| {
            configuration.settings.edit_native(|raw| {
                raw.remove("APP");
            });
        });
        assert_eq!(reader(), None);
    }

    #[test]
    fn native_live_pause_pcars_factories_keep_current_config_without_snapshot_access() {
        use openrigcue_config::import::value::LegacyValue;
        use openrigcue_sources::NativeRead;
        let config =
            Shared::new(crate::store::ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()));
        let settings = config.read().settings().clone();
        let tmp = tempfile::tempdir().unwrap();
        let inputs = ListenerInputs {
            settings: &settings,
            documents: tmp.path(),
            runtime_dir: tmp.path(),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: None,
            pause_reader: Some(pause_reader(config.clone())),
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: None,
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        };
        // These constructors only allocate state. Do not call the status check or receive:
        // their real mapping transports are intentionally left unopened.
        let listeners = [
            inputs.wrap(inputs.project_cars()),
            inputs.wrap(inputs.project_cars2()),
            inputs.wrap(inputs.automobilista2()),
        ];
        for (value, expected) in [
            (Some(LegacyValue::from("no")), NativeRead::Value(LegacyValue::Int(0))),
            (Some(LegacyValue::from("yes")), NativeRead::Raised),
            (Some(LegacyValue::from("invalid")), NativeRead::Raised),
            (None, NativeRead::Raised),
            (Some(LegacyValue::Bool(false)), NativeRead::Value(LegacyValue::Int(0))),
        ] {
            config.write().update(|configuration| {
                configuration.settings.edit_native(|raw| {
                    let app = raw.get_mut("APP").unwrap().as_dict_mut().unwrap();
                    if let Some(value) = value {
                        app.insert("pause_telemetry_when_not_playing", value);
                    } else {
                        app.remove("pause_telemetry_when_not_playing");
                    }
                });
            });
            for listener in &listeners {
                assert_eq!(listener.read_native("unknown"), expected, "{}", listener.id());
            }
        }
    }

    #[test]
    fn native_kunos_reader_access_tracks_session_and_required_settings_independently() {
        use openrigcue_model::native_value::LegacyValue;
        let config =
            Shared::new(crate::store::ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()));
        let settings = config.read().settings().clone();
        let session = Shared::new(crate::runtime::SessionView::default());
        let tmp = tempfile::tempdir().unwrap();
        let inputs = ListenerInputs {
            settings: &settings,
            documents: tmp.path(),
            runtime_dir: tmp.path(),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: None,
            pause_reader: Some(pause_reader(config.clone())),
            game_reader: Some({
                let session = session.clone();
                std::sync::Arc::new(move || session.read().game_key().to_owned())
            }),
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: None,
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        };
        let game = inputs.native_game_reader();
        let pause = inputs.native_pause_reader();
        assert_eq!(game(), "none");
        for name in ["ac", "acc", "acrally", "acevo"] {
            session.write().game = Some(name.to_owned());
            assert_eq!(game(), name);
        }
        session.write().game = None;
        assert_eq!(game(), "none");
        config.write().update(|configuration| {
            configuration.settings.edit_native(|native| {
                native
                    .get_mut("APP")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("pause_telemetry_when_not_playing", LegacyValue::from("no"));
            });
        });
        assert_eq!(pause(), Some(LegacyValue::from("no")));
        assert_eq!(game(), "none", "reading setting does not cache or change game");
        assert!(inputs.context.read().game_running.is_none(), "no dispatcher prepare required");
    }

    #[test]
    fn native_iracing_factory_retains_lazy_pause_reader() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let settings = Settings::default();
        let tmp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let inputs = ListenerInputs {
            settings: &settings,
            documents: tmp.path(),
            runtime_dir: tmp.path(),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: None,
            pause_reader: Some({
                let calls = calls.clone();
                Arc::new(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    None
                })
            }),
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: None,
            console_reader: None,
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        };
        let source = inputs.iracing();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "construction does not evaluate getter settings");
        assert_eq!(openrigcue_sources::Source::read_native(&source, "speed"), openrigcue_sources::NativeRead::Raised);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "missing SDK memory precedes setting read");
    }

    #[test]
    fn native_iracing_dams_reader_tracks_current_config_without_projection() {
        use openrigcue_model::native_value::LegacyValue;
        let config =
            Shared::new(crate::store::ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()));
        let read = iracing_dams_reader(config.clone());
        for value in [LegacyValue::from(" +００2 "), LegacyValue::Int(-70000), LegacyValue::None] {
            config.write().update(|configuration| {
                configuration.settings.edit_native(|native| {
                    native
                        .get_mut("CUSTOM_FEATURES")
                        .unwrap()
                        .as_dict_mut()
                        .unwrap()
                        .insert("dams_front_tl_mode", value.clone());
                })
            });
            assert_eq!(read(), Some(value));
        }
        config.write().update(|configuration| {
            configuration.settings.edit_native(|native| {
                native.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap().remove("dams_front_tl_mode");
            })
        });
        assert_eq!(read(), None);
    }

    #[test]
    fn native_xplane_factory_retains_current_required_lateral_reader() {
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_sources::{NativeRead, Source};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let config =
            Shared::new(crate::store::ConfigStore::in_memory(openrigcue_config::settings::Configuration::first_run()));
        let settings = config.read().settings().clone();
        let read = xplane_lateral_reader(config.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let current = read.clone();
        let tmp = tempfile::tempdir().unwrap();
        let inputs = ListenerInputs {
            settings: &settings,
            documents: tmp.path(),
            runtime_dir: tmp.path(),
            steam: None,
            joystick_inputs: JoystickInputs::default(),
            joystick_runtime: None,
            network_ports_reader: None,
            pause_reader: None,
            game_reader: None,
            iracing_dams_reader: None,
            xplane_lateral_reader: Some(Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                current()
            })),
            console_reader: None,
            windows_reader: None,
            context: Shared::default(),
            outputs: Shared::default(),
        };
        let source = inputs.xplane(ImportedPort::Valid(49000), inputs.xplane_import_options().unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(source.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(1000)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for (value, valid) in [
            (LegacyValue::from("Y"), true),
            (LegacyValue::from("no"), true),
            (LegacyValue::Bool(true), true),
            (LegacyValue::from("invalid"), false),
            (LegacyValue::None, false),
        ] {
            config.write().update(|configuration| {
                configuration.settings.edit_native(|native| {
                    native
                        .get_mut("CUSTOM_FEATURES")
                        .unwrap()
                        .as_dict_mut()
                        .unwrap()
                        .insert("rollback_xplane_latg", value.clone());
                })
            });
            assert_eq!(read(), Some(value));
            let before = calls.load(Ordering::SeqCst);
            assert_eq!(
                source.read_native("lateral_acceleration"),
                if valid { NativeRead::Value(LegacyValue::Int(0)) } else { NativeRead::Raised }
            );
            assert_eq!(calls.load(Ordering::SeqCst), before + 1);
        }
        config.write().update(|configuration| {
            configuration.settings.edit_native(|native| {
                native.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap().remove("rollback_xplane_latg");
            })
        });
        assert_eq!(read(), None);
        assert_eq!(source.read_native("lateral_acceleration"), NativeRead::Raised);
        config.write().update(|configuration| {
            configuration.settings.edit_native(|native| {
                native.remove("CUSTOM_FEATURES");
            })
        });
        assert_eq!(read(), None);
        assert_eq!(source.read_native("lateral_acceleration"), NativeRead::Raised);
    }

    #[test]
    fn native_port_conversion_does_not_read_the_compatibility_default() {
        use openrigcue_config::import::value::LegacyValue;
        for (value, expected) in [
            (LegacyValue::from("23_111"), Some(23111)),
            (LegacyValue::from("１２３"), Some(123)),
            (LegacyValue::Float(-0.25), Some(0)),
            (LegacyValue::Bool(true), Some(1)),
            (LegacyValue::from("bad"), None),
            (LegacyValue::List(vec![LegacyValue::from("23111")]), None),
        ] {
            let mut raw = isolated_settings().native_snapshot();
            raw.get_mut("NETWORK_PORT").and_then(LegacyValue::as_dict_mut).unwrap().insert("api", value);
            let (settings, _) = Settings::from_legacy(&raw);
            let tmp = tempfile::tempdir().unwrap();
            let inputs = ListenerInputs {
                settings: &settings,
                documents: tmp.path(),
                runtime_dir: tmp.path(),
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            };
            assert_eq!(inputs.configured_port("api"), expected);
        }
    }

    /// The registered listeners: 80 listener ids, each once, in registration order.
    #[test]
    fn every_registered_listener_once() {
        let tmp = tempfile::tempdir().unwrap();
        let listeners = isolated_listeners(tmp.path());
        // The DynamicCab and Farming Simulator pipe servers start with their listeners, on the relay sockets below the runtime dir.
        for pipe in ["DynamicCabPipe", "FMTelemetry"] {
            let socket = openrigcue_sources::process_memory::pipe_relay::relay_socket_path_in(tmp.path(), pipe);
            assert!(socket.exists(), "{}", socket.display());
        }
        let ids: Vec<&str> = listeners.iter().map(|l| l.id()).collect();
        assert_eq!(ids.len(), 80);
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 80, "duplicate ids in {ids:?}");
        assert_eq!(
            &ids[..19],
            &[
                "grid",
                "f1",
                "f12018",
                "f12019",
                "f12020",
                "f12021",
                "f12022",
                "f12023",
                "f12024",
                "f12025",
                "forza",
                "gt7",
                "gtsport",
                "api",
                "joy",
                "acudp",
                "projectcarsudp",
                "projectcars2udp",
                "ironrebellion",
            ]
        );
        for id in ["iracing", "dirt", "rf2", "xplane", "fs2k", "fs", "warplanes", "ccd", "ed", "wrs", "acevo"] {
            assert!(ids.contains(&id), "{id} missing");
        }
    }

    /// Every catalog game's listener is registered (except the one the catalog never detects).
    #[test]
    fn catalog_listeners_are_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let listeners = isolated_listeners(tmp.path());
        let ids: Vec<&str> = listeners.iter().map(|l| l.id()).collect();
        for game in openrigcue_config::catalog::Catalog::builtin().games() {
            if game.id == "emulator" {
                continue;
            }
            assert!(ids.contains(&game.listener.as_str()), "{} -> {}", game.id, game.listener);
        }
    }

    /// `import_listeners` on isolated settings after `change` to the raw `NETWORK_PORT` section.
    fn imported_after(tmp: &Path, change: impl FnOnce(&mut openrigcue_model::native_value::Dict)) -> ImportedListeners {
        let mut settings = isolated_settings();
        settings.edit_native(|raw| {
            change(raw.get_mut("NETWORK_PORT").and_then(|section| section.as_dict_mut()).unwrap());
        });
        import_listeners(
            &ListenerInputs {
                settings: &settings,
                documents: tmp,
                runtime_dir: tmp,
                steam: None,
                joystick_inputs: JoystickInputs::default(),
                joystick_runtime: None,
                network_ports_reader: None,
                pause_reader: None,
                game_reader: None,
                iracing_dams_reader: None,
                xplane_lateral_reader: None,
                console_reader: None,
                windows_reader: None,
                context: Shared::default(),
                outputs: Shared::default(),
            },
            None,
            None,
        )
    }

    fn sorted_ids(listeners: &[Box<dyn Listener>]) -> Vec<&str> {
        let mut ids: Vec<&str> = listeners.iter().map(|listener| listener.id()).collect();
        ids.sort_unstable();
        ids
    }

    /// The load order names every registered listener once, and every port key it converts is
    /// a `NETWORK_PORT` key of the first-run configuration.
    #[test]
    fn native_import_order_names_every_registered_listener() {
        let tmp = tempfile::tempdir().unwrap();
        let listeners = isolated_listeners(tmp.path());
        let registered = sorted_ids(&listeners);
        let mut ordered: Vec<&str> = IMPORT_ORDER.iter().map(|(listener, _)| *listener).collect();
        ordered.sort_unstable();
        assert_eq!(ordered, registered);
        let defaults = Settings::default();
        for (listener, key) in IMPORT_ORDER {
            if let Some(key) = key {
                assert!(defaults.network_ports.get(key).is_some(), "{listener}: NETWORK_PORT[{key}]");
            }
        }
    }

    /// Valid ports: the load completes and every listener is registered.
    #[test]
    fn native_valid_ports_register_every_listener() {
        let tmp = tempfile::tempdir().unwrap();
        let imported = imported_after(tmp.path(), |_| {});
        assert_eq!(imported.failure, None);
        assert_eq!(imported.registered.len(), 80);
        assert!(imported.detached.is_empty());
    }

    /// A port that is not an integer fails its listener's load: nothing is registered, and only the
    /// listeners loaded before the failing one stay loaded (the joystick listener excepted).
    #[test]
    fn native_damaged_port_fails_the_import_at_its_module() {
        use openrigcue_config::import::value::LegacyValue;
        let tmp = tempfile::tempdir().unwrap();
        let imported = imported_after(tmp.path(), |ports| {
            ports.insert("grid", LegacyValue::from("abc"));
            ports.insert("xplane", LegacyValue::from("also bad"));
        });
        assert_eq!(
            imported.failure,
            Some(ImportFailure {
                listener: "grid",
                key: "grid",
                text: "invalid literal for int() with base 10: 'abc'".into()
            })
        );
        assert!(imported.registered.is_empty());
        assert_eq!(
            sorted_ids(&imported.detached),
            ["api", "f1", "f12018", "f12019", "f12020", "f12021", "f12022", "f12023", "f12024", "f12025", "forza"]
        );

        for (key, value, listener, text) in [
            ("f12018", None, "f12018", "'f12018'".to_owned()),
            (
                "gt7",
                Some(LegacyValue::None),
                "gt7",
                "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".to_owned(),
            ),
            ("fsx", Some(LegacyValue::from("1.5")), "fsx", "invalid literal for int() with base 10: '1.5'".to_owned()),
            (
                "tdusc",
                Some(LegacyValue::from("x".repeat(250))),
                "tdusc",
                format!("invalid literal for int() with base 10: '{}", "x".repeat(199)),
            ),
        ] {
            let imported = imported_after(tmp.path(), |ports| match value {
                None => {
                    ports.remove(key);
                }
                Some(value) => {
                    ports.insert(key, value);
                }
            });
            let failure = imported.failure.expect(key);
            assert_eq!((failure.listener, failure.key, failure.text), (listener, key, text));
            assert!(imported.registered.is_empty(), "{key}");
            assert!(imported.detached.iter().all(|detached| detached.id() != listener && detached.id() != "joy"));
        }

        // The integer parse takes surrounding white space, a sign and underscores; the bus simulator only reads text.
        let imported = imported_after(tmp.path(), |ports| {
            ports.insert("grid", LegacyValue::from(" +0 "));
            ports.insert("f1", LegacyValue::from("0_0"));
            ports.insert("bussim", LegacyValue::from("not a number"));
        });
        assert_eq!(imported.failure, None);
        assert_eq!(imported.registered.len(), 80);
    }

    /// The joystick listener reads the selected profile: the stored entries become assignments.
    #[test]
    fn joystick_profile_from_store() {
        // The Linux first run assigns the magicbox pedals to `Default` (Windows' is empty).
        let profiles = openrigcue_config::import::first_run_for(openrigcue_config::import::migrate::Platform::Linux)
            .joysticks
            .clone();
        let profile = joystick_profile(&profiles, "Default");
        assert_eq!(profile.name, "Default");
        assert_eq!(profile.assignments.len(), 3);
        assert!(joystick_profile(&profiles, "missing").assignments.is_empty());
    }
}
