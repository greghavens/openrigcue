//! The Games page table and the per-game patch status.

use super::context::PatchContext;
use super::env::PatchEnvironment;
use super::steam::SteamPlugin;
use super::{
    bundled, codemasters, il2, iracing, lfs, pmracing, rfactor2, tdusc, template, wrc2023, wreckfest2, PatchError,
};

/// A game-side patch module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Patcher {
    Codemasters(&'static codemasters::CodemastersGame),
    Template(&'static template::TemplateGame),
    Bundled(&'static bundled::BundledFile),
    SteamPlugin(SteamPlugin),
    IRacing,
    Lfs,
    Il2,
    RFactor2,
    LeMans,
    Wrc2023,
    Wreckfest2,
    PmRacing,
    Tdusc,
}

impl Patcher {
    /// `installed()` of the module.
    pub fn installed(&self, env: &PatchEnvironment<'_>) -> bool {
        match self {
            Patcher::Codemasters(game) => game.installed(env),
            Patcher::Template(game) => game.installed(env),
            Patcher::Bundled(file) => file.installed(env),
            Patcher::SteamPlugin(plugin) => env.steam.is_some_and(|steam| plugin.installed(steam)),
            Patcher::IRacing => iracing::installed(env),
            Patcher::Lfs => lfs::installed(env),
            Patcher::Il2 => il2::installed(env),
            Patcher::RFactor2 => env.steam.is_some_and(|steam| rfactor2::RFACTOR2_PLUGIN.installed(steam)),
            Patcher::LeMans => env.steam.is_some_and(|steam| rfactor2::LE_MANS_PLUGIN.installed(steam)),
            Patcher::Wrc2023 => wrc2023::installed(env),
            Patcher::Wreckfest2 => wreckfest2::installed(env),
            Patcher::PmRacing => pmracing::installed(env),
            Patcher::Tdusc => tdusc::installed(env),
        }
    }

    /// Whether the module finds its game through Steam only; such a module does not depend on the prefix, which
    /// only gives the drive root the Steam lookup falls back to.
    pub fn locates_through_steam(&self) -> bool {
        matches!(self, Patcher::SteamPlugin(_) | Patcher::RFactor2 | Patcher::LeMans)
    }

    /// `patched()` of the module; `Err` when the check itself fails.
    pub fn patched(&self, env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
        match self {
            Patcher::Codemasters(game) => game.patched(env),
            Patcher::Template(game) => game.patched(env),
            Patcher::Bundled(file) => file.patched(env),
            Patcher::SteamPlugin(plugin) => Ok(env.steam.is_some_and(|steam| plugin.patched(steam, env.library))),
            Patcher::IRacing => Ok(iracing::patched(env)),
            Patcher::Lfs => lfs::patched(env),
            Patcher::Il2 => il2::patched(env),
            Patcher::RFactor2 => rfactor2::rfactor2_patched(env),
            Patcher::LeMans => rfactor2::le_mans_patched(env),
            Patcher::Wrc2023 => wrc2023::patched(env),
            Patcher::Wreckfest2 => wreckfest2::patched(env),
            Patcher::PmRacing => pmracing::patched(env),
            Patcher::Tdusc => tdusc::patched(env),
        }
    }

    /// `patch()` of the module: `Ok(true)` when the change was applied, `Ok(false)` when it was not,
    /// `Err` when it failed outright.
    pub fn patch(&self, env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
        match self {
            Patcher::Codemasters(game) => game.patch(env),
            Patcher::Template(game) => game.patch(env),
            Patcher::Bundled(file) => file.patch(env),
            Patcher::SteamPlugin(plugin) => {
                plugin.patch(env.steam.ok_or(PatchError::NotInstalled)?, env.library, env.drive_root().as_deref())
            }
            Patcher::IRacing => iracing::patch(env),
            Patcher::Lfs => lfs::patch(env),
            Patcher::Il2 => il2::patch(env),
            Patcher::RFactor2 => rfactor2::rfactor2_patch(env),
            Patcher::LeMans => rfactor2::le_mans_patch(env),
            Patcher::Wrc2023 => wrc2023::patch(env),
            Patcher::Wreckfest2 => wreckfest2::patch(env),
            Patcher::PmRacing => pmracing::patch(env),
            Patcher::Tdusc => tdusc::patch(env),
        }
    }
}

const fn steam(location: &'static str) -> SteamPlugin {
    SteamPlugin { location }
}

pub const RFACTOR: SteamPlugin = steam("rFactor\\Plugins\\SRS_Rfactor1.dll");
pub const RFACTOR_DEMO: SteamPlugin = steam("rFactor Demo\\Plugins\\SRS_Rfactor1.dll");
pub const AUTOMOBILISTA: SteamPlugin = steam("Automobilista\\Plugins\\SRS_Rfactor1.dll");
pub const ACTC: SteamPlugin = steam("ACTC\\Plugins\\SRS_Rfactor1.dll");
pub const ARCA: SteamPlugin = steam("ARCA Remax\\Plugins\\SRS_Rfactor1.dll");
pub const FTRUCK2013: SteamPlugin = steam("FTruck2013\\Plugins\\SRS_Rfactor1.dll");
pub const GSC2012: SteamPlugin = steam("GSC2012\\Plugins\\SRS_Rfactor1.dll");
pub const MARCAS: SteamPlugin = steam("Marcas\\Plugins\\SRS_Rfactor1.dll");
pub const SIMRACEWAY: SteamPlugin = steam("Simraceway\\Plugins\\SRS_Rfactor1.dll");
pub const ATS: SteamPlugin = steam("American Truck Simulator\\bin\\win_x64\\plugins\\ets2-telemetry.dll");
pub const ETS2: SteamPlugin = steam("Euro Truck Simulator 2\\bin\\win_x64\\plugins\\ets2-telemetry.dll");
pub const WARPLANES_WW1: SteamPlugin = steam("Warplanes WW1 Fighters\\WW1_Data\\StreamingAssets\\smc.cfg");
pub const WARPLANES_BOP: SteamPlugin = steam("Warplanes Battles over Pacific\\WW2VR_Data\\StreamingAssets\\smc.cfg");

/// What the Games page does with an entry (`type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Nothing to configure.
    ReadyToPlay,
    /// The user follows a help page.
    Instructions,
    /// A settings file openrigcue can change.
    Config(Patcher),
    /// A plugin file openrigcue can install.
    Dll(Patcher),
}

impl EntryKind {
    /// The patch module of a `config` or `dll` entry.
    pub fn patcher(&self) -> Option<&Patcher> {
        match self {
            EntryKind::Config(patcher) | EntryKind::Dll(patcher) => Some(patcher),
            EntryKind::ReadyToPlay | EntryKind::Instructions => None,
        }
    }
}

/// Pages an entry is listed on (`platform`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platforms {
    pub pc: bool,
    pub console: bool,
}

const PC: Platforms = Platforms { pc: true, console: false };
const CONSOLE: Platforms = Platforms { pc: false, console: true };
const BOTH: Platforms = Platforms { pc: true, console: true };

/// One row of the Games page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameEntry {
    /// Entry key; the part before the first `_` is the catalog id it stands for.
    pub key: &'static str,
    pub name: &'static str,
    pub kind: EntryKind,
    /// `[NETWORK_PORT]` key edited from the page.
    pub port: Option<&'static str>,
    pub platforms: Platforms,
}

impl GameEntry {
    /// Catalog id targeted by the entry (the key up to its first `_`).
    pub fn catalog_id(&self) -> &'static str {
        self.key.split('_').next().unwrap_or(self.key)
    }
}

const fn entry(
    key: &'static str,
    name: &'static str,
    kind: EntryKind,
    port: Option<&'static str>,
    platforms: Platforms,
) -> GameEntry {
    GameEntry { key, name, kind, port, platforms }
}

/// Every Games page entry in table order (`testmode` first).
pub const GAME_ENTRIES: &[GameEntry] = &[
    entry("testmode", "*Stand By Mode", EntryKind::ReadyToPlay, None, CONSOLE),
    entry("acudp", "Assetto Corsa PS4", EntryKind::ReadyToPlay, None, CONSOLE),
    entry("epic_oculus", "Epic Roller Coasters Oculus Quest", EntryKind::Instructions, Some("epic"), CONSOLE),
    entry("f12016", "F1 2016", EntryKind::Config(Patcher::Codemasters(&codemasters::F12016)), Some("f1"), BOTH),
    entry("f12017", "F1 2017", EntryKind::Config(Patcher::Codemasters(&codemasters::F12017)), Some("f1"), BOTH),
    entry("f12018", "F1 2018", EntryKind::Config(Patcher::Codemasters(&codemasters::F12018)), Some("f12018"), BOTH),
    entry("f12019", "F1 2019", EntryKind::Config(Patcher::Codemasters(&codemasters::F12019)), Some("f12019"), BOTH),
    entry("f12020", "F1 2020", EntryKind::Config(Patcher::Codemasters(&codemasters::F12020)), Some("f12020"), BOTH),
    entry("f12021", "F1 2021", EntryKind::Config(Patcher::Codemasters(&codemasters::F12021)), Some("f12021"), BOTH),
    entry("f12022", "F1 22", EntryKind::Config(Patcher::Codemasters(&codemasters::F12022)), Some("f12022"), BOTH),
    entry("f12023", "F1 23", EntryKind::Config(Patcher::Codemasters(&codemasters::F12023)), Some("f12023"), BOTH),
    entry("f12024", "F1 24", EntryKind::Config(Patcher::Codemasters(&codemasters::F12024)), Some("f12024"), BOTH),
    entry("f12025", "F1 25", EntryKind::Config(Patcher::Codemasters(&codemasters::F12025)), Some("f12025"), BOTH),
    entry("forzahorizon4", "Forza Horizon 4", EntryKind::Instructions, Some("forza"), BOTH),
    entry("forzahorizon5", "Forza Horizon 5", EntryKind::Instructions, Some("forza"), BOTH),
    entry("forzahorizon6", "Forza Horizon 6", EntryKind::Instructions, Some("forza"), BOTH),
    entry("forzamotorsports7", "Forza Motorsports 7", EntryKind::Instructions, Some("forza"), BOTH),
    entry("forzamotorsports8", "Forza Motorsports 8", EntryKind::Instructions, Some("forza"), BOTH),
    entry("projectcarsudp", "Project Cars XBOX/PS4", EntryKind::Instructions, Some("projectcarsudp"), CONSOLE),
    entry("projectcars2udp", "Project Cars 2 XBOX/PS4", EntryKind::Instructions, Some("projectcarsudp2"), CONSOLE),
    entry("gt7", "Grand Turismo 7 PS4/PS5", EntryKind::Instructions, Some("gt7"), CONSOLE),
    entry("gtsport", "Grand Turismo Sport PS4", EntryKind::Instructions, Some("gtsport"), CONSOLE),
    entry("iracing", "IRacing", EntryKind::Config(Patcher::IRacing), None, PC),
    entry("ironrebellion", "Iron Rebellion", EntryKind::ReadyToPlay, None, PC),
    entry("wrc", "WRC Generations", EntryKind::Instructions, Some("wrc"), PC),
    entry(
        "f12022_vr",
        "F1 22 VR",
        EntryKind::Config(Patcher::Codemasters(&codemasters::F12022_VR)),
        Some("f12022"),
        PC,
    ),
    entry(
        "f12023_vr",
        "F1 23 VR",
        EntryKind::Config(Patcher::Codemasters(&codemasters::F12023_VR)),
        Some("f12023"),
        PC,
    ),
    entry(
        "f12024_vr",
        "F1 24 VR",
        EntryKind::Config(Patcher::Codemasters(&codemasters::F12024_VR)),
        Some("f12024"),
        PC,
    ),
    entry(
        "f12025_vr",
        "F1 25 VR",
        EntryKind::Config(Patcher::Codemasters(&codemasters::F12025_VR)),
        Some("f12025"),
        PC,
    ),
    entry("aceshigh", "Aces High III", EntryKind::Instructions, Some("aceshigh"), PC),
    entry("acesofthunder", "Aces of Thunder", EntryKind::ReadyToPlay, None, PC),
    entry("ac", "Assetto Corsa", EntryKind::ReadyToPlay, None, PC),
    entry("acc", "Assetto Corsa Competizione", EntryKind::ReadyToPlay, None, PC),
    entry("acevo", "Assetto Corsa EVO", EntryKind::ReadyToPlay, None, PC),
    entry("acrally", "Assetto Corsa Rally", EntryKind::ReadyToPlay, None, PC),
    entry("aeroflyfs2", "Aerofly FS2", EntryKind::Config(Patcher::Bundled(&bundled::AEROFLY_FS2)), None, PC),
    entry(
        "fs19",
        "Farming Simulator 19",
        EntryKind::Config(Patcher::Bundled(&bundled::FARMING_SIMULATOR_19)),
        None,
        PC,
    ),
    entry(
        "fs22",
        "Farming Simulator 22",
        EntryKind::Config(Patcher::Bundled(&bundled::FARMING_SIMULATOR_22)),
        None,
        PC,
    ),
    entry(
        "fs25",
        "Farming Simulator 25",
        EntryKind::Config(Patcher::Bundled(&bundled::FARMING_SIMULATOR_25)),
        None,
        PC,
    ),
    entry("arca", "ARCA Simracing", EntryKind::Dll(Patcher::SteamPlugin(ARCA)), None, PC),
    entry("ats", "American Truck Simulator", EntryKind::Dll(Patcher::SteamPlugin(ATS)), None, PC),
    entry("automobilista", "Automobilista", EntryKind::Dll(Patcher::SteamPlugin(AUTOMOBILISTA)), None, PC),
    entry("automobilista2", "Automobilista 2", EntryKind::Instructions, None, PC),
    entry("beamng", "BeamNG", EntryKind::Instructions, Some("beamng"), PC),
    entry("condor", "Condor 2", EntryKind::Config(Patcher::Template(&template::CONDOR2)), Some("condor"), PC),
    entry("condor3", "Condor 3", EntryKind::Config(Patcher::Template(&template::CONDOR3)), Some("condor"), PC),
    entry("dcs", "DCS World", EntryKind::Config(Patcher::Template(&template::DCS)), Some("dcs"), PC),
    entry("dcs_beta", "DCS World Beta", EntryKind::Config(Patcher::Template(&template::DCS_BETA)), Some("dcs"), PC),
    entry("dirt2", "Dirt 2", EntryKind::Config(Patcher::Codemasters(&codemasters::DIRT2)), Some("dirt"), PC),
    entry("dirt3", "Dirt 3", EntryKind::Config(Patcher::Codemasters(&codemasters::DIRT3)), Some("dirt"), PC),
    entry("dirt4", "Dirt 4", EntryKind::Config(Patcher::Codemasters(&codemasters::DIRT4)), Some("dirt"), PC),
    entry(
        "dirtrally",
        "Dirt Rally",
        EntryKind::Config(Patcher::Codemasters(&codemasters::DIRT_RALLY)),
        Some("dirt"),
        PC,
    ),
    entry(
        "dirtrally_vr",
        "Dirt Rally VR",
        EntryKind::Config(Patcher::Codemasters(&codemasters::DIRT_RALLY_VR)),
        Some("dirt"),
        PC,
    ),
    entry(
        "dirtrally2.0_oculus",
        "Dirt Rally 2.0 Oculus",
        EntryKind::Config(Patcher::Codemasters(&codemasters::DIRT_RALLY2_OCULUS)),
        Some("dirt"),
        PC,
    ),
    entry(
        "dirtrally2.0_steam",
        "Dirt Rally 2.0 Steam",
        EntryKind::Config(Patcher::Codemasters(&codemasters::DIRT_RALLY2_STEAM)),
        Some("dirt"),
        PC,
    ),
    entry(
        "dirtrally2.0_vr_oculus",
        "Dirt Rally 2.0 VR Oculus",
        EntryKind::Config(Patcher::Codemasters(&codemasters::DIRT_RALLY2_VR_OCULUS)),
        Some("dirt"),
        PC,
    ),
    entry(
        "dirtrally2.0_vr_steam",
        "Dirt Rally 2.0 VR Steam",
        EntryKind::Config(Patcher::Codemasters(&codemasters::DIRT_RALLY2_VR_STEAM)),
        Some("dirt"),
        PC,
    ),
    entry(
        "dirtshowdown",
        "Dirt Showdown",
        EntryKind::Config(Patcher::Codemasters(&codemasters::DIRT_SHOWDOWN)),
        Some("dirt"),
        PC,
    ),
    entry("ed", "Elite Dangerous", EntryKind::ReadyToPlay, None, PC),
    entry("epic", "Epic Roller Coasters", EntryKind::Instructions, Some("epic"), PC),
    entry("ets", "Euro Truck Simulator 2", EntryKind::Dll(Patcher::SteamPlugin(ETS2)), None, PC),
    entry("falconbms", "Falcon BMS", EntryKind::ReadyToPlay, None, PC),
    entry("f12011", "F1 2011", EntryKind::Config(Patcher::Codemasters(&codemasters::F12011)), Some("dirt"), PC),
    entry("f12012", "F1 2012", EntryKind::Config(Patcher::Codemasters(&codemasters::F12012)), Some("dirt"), PC),
    entry("f12013", "F1 2013", EntryKind::Config(Patcher::Codemasters(&codemasters::F12013)), Some("f1"), PC),
    entry("f12014", "F1 2014", EntryKind::Config(Patcher::Codemasters(&codemasters::F12014)), Some("f1"), PC),
    entry("f12015", "F1 2015", EntryKind::Config(Patcher::Codemasters(&codemasters::F12015)), Some("f1"), PC),
    entry("fernbus", "Fernbus Simulator", EntryKind::Instructions, None, PC),
    entry("flydangerous", "Fly Dangerous", EntryKind::Instructions, None, PC),
    entry("flyinside", "FlyInside Flight Simulator", EntryKind::Instructions, Some("flyinside"), PC),
    entry(
        "fs2020_steam",
        "Flight Simulator 2020 Steam",
        EntryKind::Config(Patcher::Template(&template::FS2020_STEAM)),
        Some("fsx"),
        PC,
    ),
    entry(
        "fs2020_xbox",
        "Flight Simulator 2020 Non-Steam",
        EntryKind::Config(Patcher::Template(&template::FS2020_STORE)),
        Some("fsx"),
        PC,
    ),
    entry(
        "fs2024_steam",
        "Flight Simulator 2024 Steam",
        EntryKind::Config(Patcher::Template(&template::FS2024_STEAM)),
        Some("fsx"),
        PC,
    ),
    entry(
        "fs2024_xbox",
        "Flight Simulator 2024 Non-Steam",
        EntryKind::Config(Patcher::Template(&template::FS2024_STORE)),
        Some("fsx"),
        PC,
    ),
    entry("fsx", "Flight Simulator X", EntryKind::Config(Patcher::Template(&template::FSX)), Some("fsx"), PC),
    entry("ftruck2013", "FTrucks", EntryKind::Dll(Patcher::SteamPlugin(FTRUCK2013)), None, PC),
    entry("furidashi", "FURIDASHI: Drift Cyber Sport", EntryKind::Instructions, None, PC),
    entry("gsc2012", "Game Stock Car Extreme", EntryKind::Dll(Patcher::SteamPlugin(GSC2012)), None, PC),
    entry("gpbikes", "GP Bike", EntryKind::ReadyToPlay, None, PC),
    entry("grid2019", "GRID (2019)", EntryKind::Config(Patcher::Codemasters(&codemasters::GRID2019)), Some("grid"), PC),
    entry("grid2", "GRID 2", EntryKind::Config(Patcher::Codemasters(&codemasters::GRID2)), Some("grid"), PC),
    entry(
        "gridlegends",
        "GRID Legends",
        EntryKind::Config(Patcher::Codemasters(&codemasters::GRID_LEGENDS)),
        Some("grid"),
        PC,
    ),
    entry("gtav", "GTAV", EntryKind::Instructions, Some("gtav"), PC),
    entry("gtl", "GT Legends", EntryKind::ReadyToPlay, None, PC),
    entry("gtr", "GTR", EntryKind::ReadyToPlay, None, PC),
    entry("gtr2", "GTR 2", EntryKind::ReadyToPlay, None, PC),
    entry("hyperstorm", "Hyperstorm", EntryKind::ReadyToPlay, None, PC),
    entry("vector36", "Vector36", EntryKind::Instructions, Some("vector36"), PC),
    entry("il2", "Il-2 Sturmovik", EntryKind::Instructions, Some("il2"), PC),
    entry("il2_steam", "Il-2 Sturmovik Steam", EntryKind::Config(Patcher::Il2), Some("il2"), PC),
    entry("il2_cliffs", "IL-2 Sturmovik: Cliffs of Dover Steam", EntryKind::ReadyToPlay, None, PC),
    entry("flashout3", "FLASHOUT 3", EntryKind::ReadyToPlay, None, PC),
    entry("kartkraft", "KartKraft", EntryKind::Config(Patcher::Template(&template::KARTKRAFT)), Some("kartkraft"), PC),
    entry("kart", "Kart Racing Pro", EntryKind::ReadyToPlay, None, PC),
    entry("kayakvr", "KayakVR", EntryKind::Instructions, None, PC),
    entry("whitewatervr", "Whitewater VR: Extreme Kayaking Adventure", EntryKind::Instructions, None, PC),
    entry("lfs", "Live for Speed", EntryKind::Config(Patcher::Lfs), Some("lfs"), PC),
    entry("marcas", "Marcas", EntryKind::Dll(Patcher::SteamPlugin(MARCAS)), None, PC),
    entry("motogp", "MotoGP 2018", EntryKind::ReadyToPlay, None, PC),
    entry("mxbikes", "MX Bikes", EntryKind::ReadyToPlay, None, PC),
    entry("nl2", "NoLimits2 Roller Coaster", EntryKind::Instructions, Some("nl2"), PC),
    entry("nl2_steam", "NoLimits2 Roller Coaster Steam", EntryKind::Instructions, Some("nl2"), PC),
    entry("p3d", "Prepar3d v4", EntryKind::Config(Patcher::Template(&template::P3D_V4)), Some("p3d"), PC),
    entry("p3d_v5", "Prepar3d v5", EntryKind::Config(Patcher::Template(&template::P3D_V5)), Some("p3d"), PC),
    entry("p3d_v6", "Prepar3d v6", EntryKind::Config(Patcher::Template(&template::P3D_V6)), Some("p3d"), PC),
    entry("projectcars", "Project Cars", EntryKind::Instructions, None, PC),
    entry("projectcars2", "Project Cars 2", EntryKind::Instructions, None, PC),
    entry("projectcars3", "Project Cars 3", EntryKind::Instructions, None, PC),
    entry("projectcarspro", "Project Cars Pro", EntryKind::Instructions, None, PC),
    entry("pmracing", "Project Motor Racing", EntryKind::Config(Patcher::PmRacing), Some("pmracing"), PC),
    entry("wtcc", "Race - WTCC Game", EntryKind::ReadyToPlay, None, PC),
    entry("race07", "Race 07", EntryKind::ReadyToPlay, None, PC),
    entry("raceinjection", "Race Injection", EntryKind::ReadyToPlay, None, PC),
    entry("redrover", "Red Rover", EntryKind::Instructions, Some("redrover"), PC),
    entry("rf1", "RFactor", EntryKind::Dll(Patcher::SteamPlugin(RFACTOR)), None, PC),
    entry("rfpro", "rFpro", EntryKind::Instructions, None, PC),
    entry("warplanes_ww1", "Warplanes WW1 Fighters", EntryKind::Dll(Patcher::SteamPlugin(WARPLANES_WW1)), None, PC),
    entry(
        "warplanes_bop",
        "Warplanes Battles over Pacific",
        EntryKind::Dll(Patcher::SteamPlugin(WARPLANES_BOP)),
        None,
        PC,
    ),
    entry("rf1_demo", "RFactor Demo", EntryKind::Dll(Patcher::SteamPlugin(RFACTOR_DEMO)), None, PC),
    entry("rf2", "RFactor 2", EntryKind::Dll(Patcher::RFactor2), None, PC),
    entry("lemans", "Le Mans Ultimate", EntryKind::Dll(Patcher::LeMans), None, PC),
    entry("rre", "RaceRoom Racing Experience", EntryKind::ReadyToPlay, None, PC),
    entry("rrevo", "RaceRoom EvO", EntryKind::ReadyToPlay, None, PC),
    entry("rbr", "Richard Burns Rally", EntryKind::Instructions, None, PC),
    entry("rds", "RDS: The Official Drift Videogame", EntryKind::Instructions, None, PC),
    entry("silentwings", "Silent Wings", EntryKind::Instructions, Some("silentwings"), PC),
    entry("simraceway", "Simraceway", EntryKind::Dll(Patcher::SteamPlugin(SIMRACEWAY)), None, PC),
    entry("actc", "Simulador Turismo Carretera", EntryKind::Dll(Patcher::SteamPlugin(ACTC)), None, PC),
    entry("spacemonkey", "Space Monkey", EntryKind::Instructions, Some("spacemonkey"), PC),
    entry("tdusc", "Test Drive Unlimited Solar Crown", EntryKind::Config(Patcher::Tdusc), Some("tdusc"), PC),
    entry("thebus", "The Bus", EntryKind::Instructions, None, PC),
    entry("thecrew", "The Crew", EntryKind::Dll(Patcher::Template(&template::THE_CREW)), Some("thecrew"), PC),
    entry("thecrew2", "The Crew 2", EntryKind::Dll(Patcher::Template(&template::THE_CREW_2)), Some("thecrew"), PC),
    entry("touringkarts", "Touring Karts", EntryKind::ReadyToPlay, None, PC),
    entry("touristbus", "Tourist Bus Simulator", EntryKind::Instructions, None, PC),
    entry("torquedrift", "Torque Drift", EntryKind::Instructions, None, PC),
    entry("trackmania2", "TrackMania2", EntryKind::ReadyToPlay, None, PC),
    entry("trackmaniaturbo", "TrackManiaTurbo", EntryKind::ReadyToPlay, None, PC),
    entry("volvo", "Volvo The Game", EntryKind::ReadyToPlay, None, PC),
    entry("vracer", "V-Racer Hoverbike", EntryKind::Config(Patcher::Template(&template::VRACER)), Some("vracer"), PC),
    entry("warthunder", "War Thunder", EntryKind::ReadyToPlay, None, PC),
    entry("wreckfest2", "Wreckfest 2", EntryKind::Config(Patcher::Wreckfest2), Some("wreckfest2"), PC),
    entry("wrs", "World Racing Series", EntryKind::ReadyToPlay, None, PC),
    entry("wrc2023", "EA Sports WRC", EntryKind::Config(Patcher::Wrc2023), None, PC),
    entry("xplane10", "Xplane 10", EntryKind::Instructions, Some("xplane"), PC),
    entry("xplane", "Xplane 11", EntryKind::Instructions, Some("xplane"), PC),
    entry("xplane_12", "Xplane 12", EntryKind::Instructions, Some("xplane"), PC),
];

/// Notification-relevant status of a catalog game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameStatus {
    /// No entry targets the game (`''`).
    Unlisted,
    ReadyToPlay,
    Instructions,
    /// The instructions were already acknowledged (`'dot_not_show_notification'`).
    InstructionsAcknowledged,
    Patched,
    NotPatched,
}

/// Status of a catalog game over every entry targeting it, in table order. A game is forced
/// when it was picked manually: every entry then counts as instructions. Each module
/// runs in the prefix where its game is installed ([`PatchContext::environment_for`]).
///
/// An entry whose `patched()` fails ends the scan with that error; the caller shows no notification
/// for an `Err`.
pub fn game_status(
    game: &str,
    forced: bool,
    acknowledged: &dyn Fn(&str) -> bool,
    context: &PatchContext<'_>,
) -> Result<GameStatus, PatchError> {
    let mut status = GameStatus::Unlisted;
    for entry in GAME_ENTRIES.iter().filter(|entry| entry.catalog_id() == game) {
        let kind = if forced { EntryKind::Instructions } else { entry.kind };
        match kind {
            EntryKind::ReadyToPlay => status = GameStatus::ReadyToPlay,
            EntryKind::Instructions if acknowledged(game) => return Ok(GameStatus::InstructionsAcknowledged),
            EntryKind::Instructions => status = GameStatus::Instructions,
            EntryKind::Config(patcher) | EntryKind::Dll(patcher) => {
                if patcher.patched(&context.environment_for(&patcher))? {
                    return Ok(GameStatus::Patched);
                }
                status = GameStatus::NotPatched;
            }
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::env::{ModulePorts, PortKey};
    use crate::patch::prefix::WinePrefix;
    use crate::patch::testing::{ports, scratch};
    use std::fs;

    /// The table: 147 entries, testmode first, key/catalog relationships.
    #[test]
    fn table_shape() {
        assert_eq!(GAME_ENTRIES.len(), 147);
        assert_eq!(GAME_ENTRIES[0].key, "testmode");
        assert_eq!(GAME_ENTRIES.iter().filter(|e| e.kind.patcher().is_some()).count(), 74);
        let dirt = GAME_ENTRIES.iter().find(|e| e.key == "dirtrally2.0_vr_steam").unwrap();
        assert_eq!(dirt.catalog_id(), "dirtrally2.0");
        assert_eq!(dirt.port, Some("dirt"));
        let cliffs = GAME_ENTRIES.iter().find(|e| e.key == "il2_cliffs").unwrap();
        assert_eq!(cliffs.catalog_id(), "il2");
        let console = GAME_ENTRIES.iter().filter(|e| e.platforms.console).count();
        assert_eq!(console, 22);
    }

    /// Game status: iteration order decides; instructions acknowledged return early; forced games count
    /// as instructions.
    #[test]
    fn status_rules() {
        let lookup = ports(1);
        let loaded = ModulePorts::at_startup(&lookup);
        let context = PatchContext {
            prefixes: &[],
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        let never = |_: &str| false;
        let always = |_: &str| true;
        let status = |game: &str, forced: bool, acknowledged: &dyn Fn(&str) -> bool| {
            game_status(game, forced, acknowledged, &context).unwrap()
        };
        assert_eq!(status("il2", false, &never), GameStatus::ReadyToPlay);
        assert_eq!(status("il2", false, &always), GameStatus::InstructionsAcknowledged);
        assert_eq!(status("ac", false, &never), GameStatus::ReadyToPlay);
        assert_eq!(status("ac", true, &never), GameStatus::Instructions);
        assert_eq!(status("dirtrally2.0", false, &never), GameStatus::NotPatched);
        assert_eq!(status("wrcg", false, &never), GameStatus::Unlisted);
    }

    /// Game status: a failing patched() is not recovered from, so the first entry whose
    /// patched() fails ends the call and later entries are not consulted. Here dirtrally2.0_oculus (the first
    /// dirtrally2.0 entry) has no loaded port while dirtrally2.0_steam, later in the table, is patched.
    #[test]
    fn failing_patched_ends_the_scan() {
        let prefix_root = scratch("status-fails");
        let settings = prefix_root.join("drive_c/users/steamuser/Documents/My Games/DiRT Rally 2.0/hardwaresettings");
        fs::create_dir_all(&settings).unwrap();
        fs::write(
            settings.join("hardware_settings_config.xml"),
            "<hardware_settings_config><motion_platform /></hardware_settings_config>",
        )
        .unwrap();
        let prefixes = [WinePrefix::open(prefix_root)];
        let lookup = ports(20777);
        let loaded = ModulePorts::at_startup(&|_: PortKey| None);
        let context = PatchContext {
            prefixes: &prefixes,
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        let steam_entry = Patcher::Codemasters(&codemasters::DIRT_RALLY2_STEAM);
        steam_entry.patch(&context.environment_for(&steam_entry)).unwrap();
        assert!(steam_entry.patched(&context.environment_for(&steam_entry)).unwrap());

        let never = |_: &str| false;
        assert!(matches!(
            game_status("dirtrally2.0", false, &never, &context),
            Err(PatchError::MissingPort(PortKey::Dirt))
        ));
        assert_eq!(game_status("ac", false, &never, &context).unwrap(), GameStatus::ReadyToPlay);
    }
}
