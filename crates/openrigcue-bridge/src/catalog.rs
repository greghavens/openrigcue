//! The named shared-memory sections and named pipes the game listeners use, as data.
//!
//! Every name below is the Windows object name the game publishes or connects to; every size is the byte size
//! of the structure the listener maps.

use std::time::Duration;

/// How a listener opens a section, which decides whether the bridge may create it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opening {
    /// Read/write: opens the section or creates a zero-filled one, so the
    /// application can own the section before the game starts.
    CreateOrOpen,
    /// Read-only: creates a PAGE_READONLY zero-filled
    /// section when absent, or opens the existing section, then maps FILE_MAP_READ.
    CreateReadOnly,
    /// `OpenFileMappingW`: fails while the game has not created the section.
    OpenOnly,
}

/// How long a listener keeps its handle to a section, which decides whether the section (and the game's last
/// frame in it) outlives the game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retention {
    /// The mapping object stays open between polls, so after the game exits the section stays alive with its
    /// last frame and the listener's own checks decide that the game has stopped.
    Held,
    /// The section is mapped and released within each poll, so it disappears with the game.
    PerPoll,
}

/// One named section a listener reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionSpec {
    /// Windows object name exactly as it is opened (with or without `Local\`).
    pub name: &'static str,
    /// Number of bytes mapped and copied.
    pub size: usize,
    pub opening: Opening,
    pub retention: Retention,
    /// Named event the game signals after each update, if the listener waits on one before copying.
    pub ready_event: Option<&'static str>,
}

/// Whether a pipe is read as a byte stream or as messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeFraming {
    Byte,
    Message,
}

/// Which way data flows over a pipe, and when the server ends a client session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exchange {
    /// The game only writes; the server never answers. The session lasts until a `ReadFile` fails.
    Inbound,
    /// Every packet is answered with exactly one reply. The session continues after a reply only once a handshake
    /// (packet type 1, byte 2 of the 3-byte size-and-type header) has been answered; a packet shorter than that header
    /// stops the server for the rest of the run.
    HandshakeSession,
}

/// What the server does with the result of `ConnectNamedPipe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectRule {
    /// A client that connected before the call (`ERROR_PIPE_CONNECTED`) is served like any other; any other
    /// failure stops the server for the rest of the run.
    AcceptEarlyClient,
    /// Only a call that returns non-zero is served; on any zero return (`ERROR_PIPE_CONNECTED` included) the
    /// instance is closed unread and the next one is created at once.
    FreshClientOnly,
}

/// A named-pipe server created for a game to connect to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PipeSpec {
    /// Pipe name without the `\\.\pipe\` prefix.
    pub name: &'static str,
    pub framing: PipeFraming,
    pub exchange: Exchange,
    pub connect: ConnectRule,
    /// `DisconnectNamedPipe` is called on the instance after a client before it is closed.
    pub disconnects_client: bool,
    /// `nMaxInstances` passed to `CreateNamedPipe`.
    pub max_instances: u32,
    /// `nOutBufferSize` and `nInBufferSize`.
    pub buffer_size: u32,
    /// `nDefaultTimeOut` in milliseconds.
    pub default_timeout_ms: u32,
    /// Size of the buffer each `ReadFile` call uses.
    pub read_size: usize,
    /// Pause after a client disconnects before the pipe accepts the next one.
    pub rearm_delay: Duration,
    /// What a failed `CreateNamedPipe` does.
    pub create_failure: CreateFailure,
    /// Each client is read by its own thread while the server creates the next instance at once; otherwise
    /// the server serves one client after another.
    pub concurrent_clients: bool,
}

/// What the pipe server does when `CreateNamedPipe` fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateFailure {
    /// The server thread ends for the rest of the run.
    EndServer,
    /// On `ERROR_PIPE_BUSY` (231) the server sleeps for the given time; after any other failure the loop starts
    /// again at once.
    SleepWhenBusy(Duration),
}

/// The sections of one listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListenerSpec {
    /// Listener key as in the game catalog.
    pub key: &'static str,
    pub sections: &'static [SectionSpec],
}

const fn spec(name: &'static str, size: usize, opening: Opening, retention: Retention) -> SectionSpec {
    SectionSpec { name, size, opening, retention, ready_event: None }
}

/// A read/write section created or opened once and kept between polls.
const fn created_and_held(name: &'static str, size: usize) -> SectionSpec {
    spec(name, size, Opening::CreateOrOpen, Retention::Held)
}

/// A read/write section created or opened and released within one poll.
const fn created_per_poll(name: &'static str, size: usize) -> SectionSpec {
    spec(name, size, Opening::CreateOrOpen, Retention::PerPoll)
}

/// An existing section opened (`OpenFileMappingW`) and released within one poll.
const fn opened_per_poll(name: &'static str, size: usize) -> SectionSpec {
    spec(name, size, Opening::OpenOnly, Retention::PerPoll)
}

const fn read_only(name: &'static str, size: usize, retention: Retention) -> SectionSpec {
    spec(name, size, Opening::CreateReadOnly, retention)
}

const fn sections(key: &'static str, sections: &'static [SectionSpec]) -> ListenerSpec {
    ListenerSpec { key, sections }
}

/// Size of the byte-packed SimBin telemetry structure shared by the four SimBin listeners.
const SIMBIN_MAP_SIZE: usize = 180;

/// `\\.\pipe\DynamicCabPipe`: a duplex byte pipe with one instance, 65536-byte buffers and a 300 ms default
/// timeout. A client that connected before `ConnectNamedPipe` is served; any other connect failure, or a failed
/// `CreateNamedPipe`, ends the server. Each client is read 4096 bytes at a time with one reply per packet, then
/// disconnected and its handle closed, and the next instance is created after two seconds.
pub const DYNAMIC_CAB_PIPE: PipeSpec = PipeSpec {
    name: "DynamicCabPipe",
    framing: PipeFraming::Byte,
    exchange: Exchange::HandshakeSession,
    connect: ConnectRule::AcceptEarlyClient,
    disconnects_client: true,
    max_instances: 1,
    buffer_size: 65536,
    default_timeout_ms: 300,
    read_size: 4096,
    rearm_delay: Duration::from_secs(2),
    create_failure: CreateFailure::EndServer,
    concurrent_clients: false,
};

/// `\\.\pipe\FMTelemetry`: a duplex message pipe with up to five instances, 4096-byte buffers and a 100 ms
/// default timeout. When creating an instance fails with `ERROR_PIPE_BUSY` the server waits two seconds before
/// trying again. A client is served only when `ConnectNamedPipe` returned non-zero; otherwise the instance is
/// closed and the next one created. Each client is read 4096 bytes per `ReadFile` until a read returns 0, and
/// nothing is written back; the instance is closed without `DisconnectNamedPipe`. One instance listens at a
/// time: each connected client gets a reader thread while the server closes ended clients whose data has been
/// drained and creates the next instance.
pub const FM_TELEMETRY_PIPE: PipeSpec = PipeSpec {
    name: "FMTelemetry",
    framing: PipeFraming::Message,
    exchange: Exchange::Inbound,
    connect: ConnectRule::FreshClientOnly,
    disconnects_client: false,
    max_instances: 5,
    buffer_size: 4096,
    default_timeout_ms: 100,
    read_size: 4096,
    rearm_delay: Duration::ZERO,
    create_failure: CreateFailure::SleepWhenBusy(Duration::from_secs(2)),
    concurrent_clients: true,
};

/// Pipes served whatever game is selected: both servers start with the application.
pub const IMPORT_TIME_PIPES: &[PipeSpec] = &[DYNAMIC_CAB_PIPE, FM_TELEMETRY_PIPE];

/// All listeners that read Windows named sections.
pub const LISTENERS: &[ListenerSpec] = &[
    // Assetto Corsa: three read/write sections, held for the life of the process.
    sections(
        "ac",
        &[
            created_and_held("acpmf_physics", 580),
            created_and_held("acpmf_graphics", 1284),
            created_and_held("acpmf_static", 452),
        ],
    ),
    // Assetto Corsa EVO: opened read-only, mapped, copied, unmapped and closed on every refresh.
    sections(
        "acevo",
        &[
            opened_per_poll("acevo_pmf_physics", 800),
            opened_per_poll("acevo_pmf_graphics", 3940),
            opened_per_poll("acevo_pmf_static", 208),
        ],
    ),
    // Project CARS: 8000 bytes, kept mapped until the listener's own check drops it.
    sections("projectcars", &[created_and_held("$pcars$", 8000)]),
    // Project CARS 2: a temporary read/write mapping copied on every snapshot.
    sections("projectcars2", &[created_per_poll("$pcars2$", 11624)]),
    // Automobilista 2: a temporary read-only mapping copied on every snapshot.
    sections("automobilista2", &[read_only("$pcars2$", 11708, Retention::PerPoll)]),
    // iRacing: the data-valid event is opened for waiting and the read-only section is kept until shutdown.
    sections(
        "iracing",
        &[SectionSpec {
            ready_event: Some("Local\\IRSDKDataValidEvent"),
            ..read_only("Local\\IRSDKMemMapFileName", 1_191_936, Retention::Held)
        }],
    ),
    // PiBoSo games: each proxy-data section is mapped once and kept.
    sections("gpbikes", &[created_and_held("Local\\GPBProxyObject", 1027)]),
    sections("mxbikes", &[created_and_held("Local\\MXBProxyObject", 996)]),
    sections("kart", &[created_and_held("Local\\KRPProxyObject", 1112)]),
    sections("wrs", &[created_and_held("Local\\WRSProxyObject", 1048)]),
    // SimBin games: the telemetry section is mapped once and kept.
    sections("gtr", &[created_and_held("SimBin", SIMBIN_MAP_SIZE)]),
    sections("gtr2", &[created_and_held("$gtr2$", SIMBIN_MAP_SIZE)]),
    sections("gtl", &[created_and_held("$gtlegends$", SIMBIN_MAP_SIZE)]),
    sections("race07", &[created_and_held("$Race$", SIMBIN_MAP_SIZE)]),
    // Truck simulators: the telemetry-plugin section, mapped once and kept.
    sections("trucksim", &[created_and_held("Local\\SimTelemetryETS2", 1018)]),
    // Falcon BMS: the flight-data section, mapped once and kept.
    sections("falconbms", &[created_and_held("FalconSharedMemoryArea", 1920)]),
    // RaceRoom: the shared structure, mapped read-only once and kept.
    sections("rre", &[read_only("$R3E", 43_996, Retention::Held)]),
    // rFactor 1 / rFactor Pro: the shared structure, mapped once and kept.
    sections("rf1", &[created_and_held("$rFactorShared$", 39_676)]),
    sections("rfpro", &[created_and_held("$rFactorShared$", 39_647)]),
    // rFactor 2: the telemetry section, then the scoring section; both kept.
    sections(
        "rf2",
        &[created_and_held("$rFactor2SMMP_Telemetry$", 241_680), created_and_held("$rFactor2SMMP_Scoring$", 75_312)],
    ),
    // Trackmania: the telemetry section, mapped once and kept.
    sections("trackmania", &[created_and_held("ManiaPlanet_Telemetry", 1396)]),
    // SpaceMonkey: the filtered generic-provider section, mapped once and kept.
    sections("spacemonkey", &[created_and_held("GenericTelemetryProviderFiltered", 260)]),
    // Warplanes: a 24-byte section, mapped once and kept.
    sections("warplanes", &[created_and_held("Local\\ww1", 24)]),
    // IL-2 Cliffs of Dover: opened read-only, mapped, copied, unmapped and closed on every snapshot.
    sections("il2cliffs", &[opened_per_poll("CLODDeviceLink", 16_384)]),
    // The flight-sim pipe listener reads no section; its pipe is in IMPORT_TIME_PIPES.
    sections("fs", &[]),
];

/// A section the application publishes for other Windows programs (openrigcue writes the Linux file, the bridge
/// publishes it inside the prefix).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportSpec {
    /// Windows object name.
    pub name: &'static str,
    /// Byte size of the published structure.
    pub size: usize,
}

/// - `Local\SimRacingStudioMotionRigPose`: six doubles (48 bytes), read by Windows motion-compensation consumers
///   (a SteamVR driver and an OpenXR layer);
/// - `YawVRGEFile`: four floats and a bool, byte-packed (17 bytes).
pub const EXPORTS: &[ExportSpec] = &[
    ExportSpec { name: "Local\\SimRacingStudioMotionRigPose", size: 48 },
    ExportSpec { name: "YawVRGEFile", size: 17 },
];

#[cfg(test)]
mod tests {
    use super::*;
    use Opening::{CreateOrOpen, CreateReadOnly, OpenOnly};
    use Retention::{Held, PerPoll};

    /// Every section of every listener: (listener key, name, size, opening, retention).
    #[test]
    fn every_section_matches_its_original_listener() {
        let expected: &[(&str, &str, usize, Opening, Retention)] = &[
            // Assetto Corsa
            ("ac", "acpmf_physics", 580, CreateOrOpen, Held),
            ("ac", "acpmf_graphics", 1284, CreateOrOpen, Held),
            ("ac", "acpmf_static", 452, CreateOrOpen, Held),
            // Assetto Corsa EVO
            ("acevo", "acevo_pmf_physics", 800, OpenOnly, PerPoll),
            ("acevo", "acevo_pmf_graphics", 3940, OpenOnly, PerPoll),
            ("acevo", "acevo_pmf_static", 208, OpenOnly, PerPoll),
            // Project CARS
            ("projectcars", "$pcars$", 8000, CreateOrOpen, Held),
            // Project CARS 2
            ("projectcars2", "$pcars2$", 11624, CreateOrOpen, PerPoll),
            // Automobilista 2
            ("automobilista2", "$pcars2$", 11708, CreateReadOnly, PerPoll),
            // iRacing (kept until shutdown)
            ("iracing", "Local\\IRSDKMemMapFileName", 1_191_936, CreateReadOnly, Held),
            // PiBoSo games
            ("gpbikes", "Local\\GPBProxyObject", 1027, CreateOrOpen, Held),
            ("mxbikes", "Local\\MXBProxyObject", 996, CreateOrOpen, Held),
            ("kart", "Local\\KRPProxyObject", 1112, CreateOrOpen, Held),
            ("wrs", "Local\\WRSProxyObject", 1048, CreateOrOpen, Held),
            // SimBin games
            ("gtr", "SimBin", 180, CreateOrOpen, Held),
            ("gtr2", "$gtr2$", 180, CreateOrOpen, Held),
            ("gtl", "$gtlegends$", 180, CreateOrOpen, Held),
            ("race07", "$Race$", 180, CreateOrOpen, Held),
            // Truck simulators
            ("trucksim", "Local\\SimTelemetryETS2", 1018, CreateOrOpen, Held),
            // Falcon BMS
            ("falconbms", "FalconSharedMemoryArea", 1920, CreateOrOpen, Held),
            // RaceRoom (read-only)
            ("rre", "$R3E", 43_996, CreateReadOnly, Held),
            // rFactor 1 / rFactor Pro
            ("rf1", "$rFactorShared$", 39_676, CreateOrOpen, Held),
            ("rfpro", "$rFactorShared$", 39_647, CreateOrOpen, Held),
            // rFactor 2
            ("rf2", "$rFactor2SMMP_Telemetry$", 241_680, CreateOrOpen, Held),
            ("rf2", "$rFactor2SMMP_Scoring$", 75_312, CreateOrOpen, Held),
            // Trackmania
            ("trackmania", "ManiaPlanet_Telemetry", 1396, CreateOrOpen, Held),
            // SpaceMonkey
            ("spacemonkey", "GenericTelemetryProviderFiltered", 260, CreateOrOpen, Held),
            // Warplanes
            ("warplanes", "Local\\ww1", 24, CreateOrOpen, Held),
            // IL-2 Cliffs of Dover
            ("il2cliffs", "CLODDeviceLink", 16_384, OpenOnly, PerPoll),
        ];
        let actual: Vec<_> = LISTENERS
            .iter()
            .flat_map(|l| l.sections.iter().map(move |s| (l.key, s.name, s.size, s.opening, s.retention)))
            .collect();
        assert_eq!(actual, expected);
    }

    /// iRacing's data-valid event is waited on before the copy; no other section has an event.
    #[test]
    fn only_iracing_has_a_ready_event() {
        let with_event: Vec<_> =
            LISTENERS.iter().flat_map(|l| l.sections).filter_map(|s| s.ready_event.map(|e| (s.name, e))).collect();
        assert_eq!(with_event, [("Local\\IRSDKMemMapFileName", "Local\\IRSDKDataValidEvent")]);
    }

    /// The creation parameters of both pipes.
    #[test]
    fn pipe_parameters() {
        let ccd = DYNAMIC_CAB_PIPE;
        assert_eq!(
            (ccd.name, ccd.framing, ccd.exchange, ccd.max_instances, ccd.buffer_size, ccd.default_timeout_ms),
            ("DynamicCabPipe", PipeFraming::Byte, Exchange::HandshakeSession, 1, 65536, 300)
        );
        assert_eq!((ccd.read_size, ccd.rearm_delay), (4096, Duration::from_secs(2)));
        assert_eq!((ccd.create_failure, ccd.concurrent_clients), (CreateFailure::EndServer, false));
        let fm = FM_TELEMETRY_PIPE;
        assert_eq!(
            (fm.name, fm.framing, fm.exchange, fm.max_instances, fm.buffer_size, fm.default_timeout_ms),
            ("FMTelemetry", PipeFraming::Message, Exchange::Inbound, 5, 4096, 100)
        );
        assert_eq!(fm.read_size, 4096);
        assert_eq!(
            (fm.create_failure, fm.concurrent_clients),
            (CreateFailure::SleepWhenBusy(Duration::from_secs(2)), true)
        );
    }

    /// The DynamicCab pipe serves a client that connected early and disconnects each client; the FMTelemetry pipe
    /// serves only a non-zero `ConnectNamedPipe` and closes without disconnecting.
    #[test]
    fn pipe_connect_and_teardown_rules() {
        let ccd = DYNAMIC_CAB_PIPE;
        assert_eq!((ccd.connect, ccd.disconnects_client), (ConnectRule::AcceptEarlyClient, true));
        let fm = FM_TELEMETRY_PIPE;
        assert_eq!((fm.connect, fm.disconnects_client), (ConnectRule::FreshClientOnly, false));
    }

    /// Both pipes are served for every game.
    #[test]
    fn both_pipes_are_served_at_import() {
        assert_eq!(IMPORT_TIME_PIPES, [DYNAMIC_CAB_PIPE, FM_TELEMETRY_PIPE]);
    }
}
