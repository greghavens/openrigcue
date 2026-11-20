//! [`GameSource`] for every listener: which rate channels it answers, which runtime values it reads and
//! which error texts it reports.

use std::collections::BTreeSet;

use openrigcue_sources::api::{ApiDecoder, JoystickSource};
use openrigcue_sources::process_memory::{
    bus_sim::BusSim, dynamic_cab::DynamicCab, elite_dangerous::EliteDangerous, farming_simulator::FarmingSimulator,
    no_limits2::NoLimits2, simconnect::FlightSimulator, vracer::VRacer, war_thunder::WarThunder,
};
use openrigcue_sources::shared_memory::answers::MotionRates;
use openrigcue_sources::shared_memory::{
    assetto_corsa::{AcGame, AssettoCorsa},
    assetto_corsa_evo::AssettoCorsaEvo,
    automobilista2::Automobilista2,
    falcon_bms::FalconBms,
    il2_cliffs::Il2Cliffs,
    iracing::{Iracing, IracingOptions},
    piboso::Piboso,
    project_cars::ProjectCars,
    project_cars2::ProjectCars2,
    raceroom::RaceRoom,
    rfactor1::RfactorOne,
    rfactor2::Rfactor2,
    simbin::SimBin,
    space_monkey::SpaceMonkey,
    trackmania::TrackMania,
    trucksim::TruckSim,
    warplanes::Warplanes,
};
use openrigcue_sources::udp_flight::{
    aces_high::AcesHighDecoder, aerofly::AeroflyDecoder, condor::CondorDecoder, dcs::DcsDecoder,
    fly_inside::FlyInsideDecoder, il2::Il2Source, silent_wings::SilentWingsDecoder, xplane::XPlaneSource, AngularRates,
    FlightSource, PolledUdpSource,
};
use openrigcue_sources::udp_racing::{
    assetto_corsa_udp::AssettoCorsaUdpSource,
    beamng::BeamNgSource,
    codemasters::{
        dirt::DirtSource, f1_classic::F1ClassicSource, grid::GridSource, tdusc::TduscSource, wrc::WrcSource,
    },
    drift_physics::DriftPhysicsSource,
    ea_f1::{season2018::F1Season2018Source, seasons::F1SeasonSource},
    epic_roller_coasters::EpicRollerCoastersSource,
    forza::ForzaSource,
    gran_turismo::GranTurismoSource,
    gtav::GtavSource,
    hyperstorm::HyperStormSource,
    iron_rebellion::IronRebellionSource,
    kartkraft::KartKraftSource,
    live_for_speed::LiveForSpeedSource,
    motogp::MotoGpSource,
    project_cars::ProjectCarsSource,
    project_cars2::ProjectCars2Source,
    project_motor_racing::PmRacingSource,
    rbr::RbrSource,
    red_rover::RedRoverSource,
    the_crew::TheCrewSource,
    touring_karts::TouringKartsSource,
    vector36::Vector36Source,
    wrc2023::Wrc2023Source,
    wreckfest2::Wreckfest2Source,
};

use super::{GameSource, Rates, SourceContext};

impl From<AngularRates> for Rates {
    fn from(rates: AngularRates) -> Self {
        Rates { yaw: Some(rates.yaw), pitch: Some(rates.pitch), roll: Some(rates.roll) }
    }
}

impl From<MotionRates> for Rates {
    fn from(rates: MotionRates) -> Self {
        Rates { yaw: rates.yaw_rate, pitch: rates.pitch_rate, roll: rates.roll_rate }
    }
}

/// Listeners without rate branches, runtime reads or error texts.
macro_rules! plain_sources {
    ($($source:ty),* $(,)?) => {
        $(impl GameSource for $source {})*
    };
}

plain_sources!(
    AssettoCorsaUdpSource,
    DirtSource,
    F1ClassicSource,
    GridSource,
    TduscSource,
    WrcSource,
    DriftPhysicsSource,
    F1Season2018Source,
    F1SeasonSource,
    EpicRollerCoastersSource,
    GtavSource,
    HyperStormSource,
    IronRebellionSource,
    LiveForSpeedSource,
    MotoGpSource,
    ProjectCarsSource,
    ProjectCars2Source,
    PmRacingSource,
    RbrSource,
    RedRoverSource,
    TheCrewSource,
    TouringKartsSource,
    Vector36Source,
    RfactorOne,
    SimBin,
    TruckSim,
    FalconBms,
);

/// Listeners that add their socket errors to the game errors.
macro_rules! error_reporting_sources {
    ($($source:ty),* $(,)?) => {
        $(impl GameSource for $source {
            fn game_errors(&self) -> Option<&BTreeSet<String>> {
                Some(<$source>::game_errors(self))
            }
        })*
    };
}

error_reporting_sources!(ForzaSource, Wrc2023Source, BeamNgSource, KartKraftSource);

/// Flight and other listeners with their own rate branches.
macro_rules! flight_sources {
    ($($source:ty),* $(,)?) => {
        $(impl GameSource for $source {
            fn rates(&self) -> Rates {
                self.angular_rates().into()
            }
        })*
    };
}

flight_sources!(
    PolledUdpSource<AeroflyDecoder>,
    PolledUdpSource<CondorDecoder>,
    PolledUdpSource<DcsDecoder>,
    PolledUdpSource<AcesHighDecoder>,
    PolledUdpSource<FlyInsideDecoder>,
    PolledUdpSource<SilentWingsDecoder>,
    Il2Source,
    TrackMania,
    Warplanes,
    Il2Cliffs,
    BusSim,
    DynamicCab,
    EliteDangerous,
    FarmingSimulator,
    NoLimits2,
    WarThunder,
);

impl<S: std::io::Read + std::io::Write + Send + 'static> GameSource for FlightSimulator<S> {
    fn rates(&self) -> Rates {
        self.angular_rates().into()
    }
}

/// Shared-memory listeners that answer the rates themselves and read `pause_telemetry_when_not_playing`.
macro_rules! pausing_rate_sources {
    ($($source:ty),* $(,)?) => {
        $(impl GameSource for $source {
            fn rates(&self) -> Rates {
                self.motion_rates().into()
            }
            fn prepare(&mut self, context: &SourceContext) {
                self.set_pause_when_not_playing(context.pause_when_not_playing);
            }
        })*
    };
}

pausing_rate_sources!(AssettoCorsaEvo, ProjectCars, ProjectCars2, Automobilista2);

/// Shared-memory listeners that read `pause_telemetry_when_not_playing` and answer no rates.
macro_rules! pausing_sources {
    ($($source:ty),* $(,)?) => {
        $(impl GameSource for $source {
            fn prepare(&mut self, context: &SourceContext) {
                self.set_pause_when_not_playing(context.pause_when_not_playing);
            }
        })*
    };
}

pausing_sources!(RaceRoom, Rfactor2);

/// Gran Turismo reads `APP.playstation_ip` in its status check, its receive pass and its channel reads.
impl GameSource for GranTurismoSource {
    fn prepare(&mut self, context: &SourceContext) {
        self.set_playstation_ip(&context.playstation_ip);
    }
}

/// Wreckfest 2 answers the channel reads the dispatcher makes for its settings of the tick.
impl GameSource for Wreckfest2Source {
    fn prepare(&mut self, context: &SourceContext) {
        self.set_pause_when_not_playing(context.pause_when_not_playing);
        self.set_dispatcher_reads(openrigcue_sources::udp_racing::wreckfest2::DispatcherReads {
            dams_front_tl_mode: context.dams_front_tl_mode.unwrap_or(0),
            pitch_angle: context.pitch_angle,
            roll_angle: context.roll_angle,
        });
    }
}

/// Numeric view; the lateral getters read the required setting directly.
impl GameSource for XPlaneSource {
    fn rates(&self) -> Rates {
        self.angular_rates().into()
    }

    fn prepare(&mut self, context: &SourceContext) {
        self.set_lateral_g_from_local_ax(context.rollback_xplane_lateral_g);
    }
}

/// The API listener: its `lla` datagrams drive the fans directly.
impl GameSource for PolledUdpSource<ApiDecoder> {
    fn fan_override(&self) -> bool {
        self.decoder().fan_override()
    }
}

/// The Assetto Corsa listener serves Assetto Corsa, Competizione and Rally; it reads which one runs.
impl GameSource for AssettoCorsa {
    fn rates(&self) -> Rates {
        self.motion_rates().into()
    }

    fn prepare(&mut self, context: &SourceContext) {
        if let Some(game) = context.game_running.as_deref().and_then(assetto_corsa_game) {
            self.set_game(game);
        }
        self.set_pause_when_not_playing(context.pause_when_not_playing);
    }
}

/// The Assetto Corsa title a catalog game id stands for.
fn assetto_corsa_game(game_id: &str) -> Option<AcGame> {
    match game_id {
        "ac" => Some(AcGame::AssettoCorsa),
        "acc" => Some(AcGame::Competizione),
        "acrally" => Some(AcGame::Rally),
        _ => None,
    }
}

impl GameSource for Iracing {
    fn rates(&self) -> Rates {
        self.motion_rates().into()
    }

    fn prepare(&mut self, context: &SourceContext) {
        self.set_options(IracingOptions {
            pause_when_not_playing: context.pause_when_not_playing,
            dams_front_tl_mode: context.dams_front_tl_mode,
        });
    }
}

impl GameSource for Piboso {
    fn rates(&self) -> Rates {
        self.motion_rates().into()
    }
}

/// Space Monkey decides from the open window titles which game it serves.
impl GameSource for SpaceMonkey {
    fn rates(&self) -> Rates {
        self.angular_rates().into()
    }

    fn prepare(&mut self, context: &SourceContext) {
        self.set_open_windows(context.window_titles.clone());
    }
}

/// The joystick listener follows the joystick switch and the selected joystick profile.
impl GameSource for JoystickSource {
    fn prepare(&mut self, context: &SourceContext) {
        if !self.refresh_runtime() {
            self.set_enabled(context.joystick_enabled);
            match &context.joystick_raw_profile {
                Some((name, profile)) => self.set_raw_profile(name, profile.clone()),
                None => self.set_profile(context.joystick_profile.clone()),
            }
        }
        self.set_options(context.joystick_options);
    }
}

/// V-Racer, whose datagrams go to `udp_forward` and whose roll channel follows the game's `roll_type`.
pub struct ForwardingVRacer<P: openrigcue_sources::process_memory::vracer::DatagramPort = openrigcue_sources::process_memory::vracer::LocalUdpPort> {
    pub source: VRacer<P>,
}

use openrigcue_sources::Source;

impl<P: openrigcue_sources::process_memory::vracer::DatagramPort> Source for ForwardingVRacer<P> {
    fn id(&self) -> &'static str {
        self.source.id()
    }

    fn is_running(&mut self) -> Result<bool, openrigcue_sources::SourceError> {
        self.source.is_running()
    }

    fn poll(&mut self, telemetry: &mut openrigcue_model::Telemetry) -> Result<bool, openrigcue_sources::SourceError> {
        self.source.poll(telemetry)
    }

    fn receive_native(
        &mut self,
        telemetry: &mut openrigcue_model::Telemetry,
    ) -> Result<bool, openrigcue_sources::SourceError> {
        self.source.receive_native(telemetry)
    }

    fn read_native(&self, name: &str) -> openrigcue_sources::NativeRead {
        self.source.read_native(name)
    }

    fn native_channel(&self, name: &str) -> Option<openrigcue_model::native_value::LegacyValue> {
        self.source.native_channel(name)
    }

    fn stop(&mut self) {
        self.source.stop();
    }
}

impl<P: openrigcue_sources::process_memory::vracer::DatagramPort> GameSource for ForwardingVRacer<P> {
    fn rates(&self) -> Rates {
        self.source.angular_rates().into()
    }

    fn prepare(&mut self, context: &SourceContext) {
        self.source.set_roll_angle_mode(context.roll_angle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_vracer_product_wrapper_forwards_each_getter_once() {
        use crate::listeners::GameListener;
        use crate::runtime::Shared;
        use openrigcue_model::native_value::LegacyValue;
        use openrigcue_sources::process_memory::vracer::DatagramPort;
        use openrigcue_sources::{derive::Listener, NativeRead};
        struct Port(Vec<Vec<u8>>);
        impl DatagramPort for Port {
            fn receive_one(&mut self) -> std::io::Result<Vec<u8>> {
                Ok(self.0.remove(0))
            }
        }
        let packet = br#"{"onTrack":true,"paused":false,"accelerationX":0.1,"accelerationZ":0.2,"accelerationY":0.3,"angularSpeedX":1,"angularSpeedZ":2,"angularSpeedY":3,"rotationX":4,"rotationZ":12.5,"rotationY":6,"gear":true,"rpm":7000,"speed":33}"#.to_vec();
        let source = ForwardingVRacer { source: VRacer::with_port(Port(vec![packet, b"{bad".to_vec()])) };
        let mut listener = GameListener::new(source, Shared::default(), Shared::default());
        assert!(listener.is_running());
        listener.receive();
        assert_eq!(listener.read_native("gear"), NativeRead::Value(LegacyValue::Bool(true)));
        assert_eq!(listener.read_native("roll"), NativeRead::Value(LegacyValue::Float(-12.5)));
        assert_eq!(listener.read_native("roll"), NativeRead::Value(LegacyValue::Float(12.5)));
        assert!(listener.is_running());
        listener.receive();
        assert_eq!(
            listener.read_native("roll"),
            NativeRead::Value(LegacyValue::Float(-12.5)),
            "invalid JSON leaves prior getter state"
        );
        assert_eq!(listener.read_native("rpm"), NativeRead::Value(LegacyValue::Int(7000)));
    }

    /// The Assetto Corsa listener: the running catalog id selects the title; other ids leave it unchanged.
    #[test]
    fn assetto_corsa_title_follows_the_running_game() {
        assert_eq!(assetto_corsa_game("acc"), Some(AcGame::Competizione));
        assert_eq!(assetto_corsa_game("acrally"), Some(AcGame::Rally));
        assert_eq!(assetto_corsa_game("ac"), Some(AcGame::AssettoCorsa));
        assert_eq!(assetto_corsa_game("dirt4"), None);
    }

    /// Rate answers: flight listeners answer every rate, shared-memory listeners may fail for some.
    #[test]
    fn rate_conversions() {
        let flight: Rates = AngularRates { pitch: 1.0, roll: 2.0, yaw: 3.0 }.into();
        assert_eq!(flight, Rates { yaw: Some(3.0), pitch: Some(1.0), roll: Some(2.0) });
        let raised: Rates = MotionRates::RAISED.into();
        assert_eq!(raised, Rates::default());
        let zero: Rates = MotionRates::ZERO.into();
        assert_eq!(zero, Rates::ZERO);
    }
}
