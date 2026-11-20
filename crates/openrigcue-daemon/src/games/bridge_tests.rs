//! The Games page over a fixture Steam installation: which entries ask for configuration because Steam does not
//! start the bridge with them yet, and what the "config" action writes.

use std::fs;
use std::path::{Path, PathBuf};

use openrigcue_config::patch::GameStatus;
use openrigcue_config::settings::NetworkPorts;

use super::proton_bridge::{BridgeError, BridgeSetup, BRIDGE_EXE, BRIDGE_RUN};
use super::{GameSetup, GamesError, GamesPage, RowStatus};

pub(crate) const USER: &str = "51410156";
const SECOND_USER: &str = "77";

/// `localconfig.vdf` of the first user: Assetto Corsa with launch options of its own, DiRT Rally 2.0 (a UDP game)
/// with arguments, Assetto Corsa Competizione without any.
const LOCALCONFIG: &str = "\"UserLocalConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"244210\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LastPlayed\"\t\t\"1794000000\"\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"gamemoderun %command% -x\"\n\t\t\t\t\t}\n\t\t\t\t\t\"690790\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"-novid\"\n\t\t\t\t\t}\n\t\t\t\t\t\"805550\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LastPlayed\"\t\t\"1\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";
const EMPTY_STORE: &str = "\"UserLocalConfigStore\"\n{\n}\n";

pub(crate) struct Fixture {
    _directory: tempfile::TempDir,
    home: PathBuf,
    steam: PathBuf,
    source: PathBuf,
    data: PathBuf,
    proc_root: PathBuf,
}

impl Fixture {
    /// A home with Steam, two Steam users, five installed games and a packaged bridge; no `steam` process.
    pub(crate) fn new() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let steam = home.join(".local/share/Steam");
        let fixture = Fixture {
            source: directory.path().join("app/usr/lib/openrigcue"),
            data: home.join(".local/share"),
            proc_root: directory.path().join("proc"),
            steam,
            home,
            _directory: directory,
        };
        fixture.app("244210", "Assetto Corsa", "assettocorsa", &["acs.exe"]);
        fixture.app("805550", "Assetto Corsa Competizione", "Assetto Corsa Competizione", &["acc.exe"]);
        fixture.app("1066890", "Automobilista 2", "Automobilista 2", &["AMS2AVX.exe"]);
        fixture.app("227300", "Euro Truck Simulator 2", "Euro Truck Simulator 2", &["bin/win_x64/eurotrucks2.exe"]);
        fixture.app("690790", "DiRT Rally 2.0", "DiRT Rally 2.0", &["dirtrally2.exe"]);
        fixture.write_config(USER, LOCALCONFIG);
        fixture.write_config(SECOND_USER, EMPTY_STORE);
        fs::create_dir_all(&fixture.source).unwrap();
        fs::write(fixture.source.join(BRIDGE_RUN), "#!/bin/bash\nexec \"$@\"\n").unwrap();
        fs::write(fixture.source.join(BRIDGE_EXE), b"MZ bridge 1").unwrap();
        fixture.process("100", "bash");
        fixture
    }

    fn app(&self, id: &str, name: &str, installdir: &str, files: &[&str]) {
        let steamapps = self.steam.join("steamapps");
        for file in files {
            let path = steamapps.join("common").join(installdir).join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"MZ").unwrap();
        }
        let manifest = format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{id}\"\n\t\"name\"\t\t\"{name}\"\n\t\"installdir\"\t\t\"{installdir}\"\n\t\"UserConfig\"\n\t{{\n\t\t\"language\"\t\t\"english\"\n\t}}\n}}\n"
        );
        fs::write(steamapps.join(format!("appmanifest_{id}.acf")), manifest).unwrap();
    }

    fn config_path(&self, user: &str) -> PathBuf {
        self.steam.join("userdata").join(user).join("config/localconfig.vdf")
    }

    fn write_config(&self, user: &str, text: &str) {
        let path = self.config_path(user);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    pub(crate) fn config(&self, user: &str) -> String {
        fs::read_to_string(self.config_path(user)).unwrap()
    }

    pub(crate) fn process(&self, pid: &str, comm: &str) {
        fs::create_dir_all(self.proc_root.join(pid)).unwrap();
        fs::write(self.proc_root.join(pid).join("comm"), format!("{comm}\n")).unwrap();
    }

    fn run(&self) -> PathBuf {
        self.data.join("openrigcue/bridge").join(BRIDGE_RUN)
    }

    pub(crate) fn setup(&self) -> GameSetup {
        GameSetup::discover(&self.home, &NetworkPorts::default()).with_bridge(BridgeSetup::with_locations(
            &self.home,
            Some(&self.source),
            &self.data,
            &self.proc_root,
        ))
    }

    fn status(&self, key: &str) -> RowStatus {
        let rows = self.setup().rows(GamesPage::Pc, &NetworkPorts::default(), &Default::default());
        rows.iter().find(|row| row.key == key).unwrap().status
    }

    fn game_status(&self, game: &str, forced: bool) -> GameStatus {
        self.setup().status(game, forced, &Default::default(), &NetworkPorts::default()).unwrap()
    }

    fn patch(&self, key: &str) -> Result<(), GamesError> {
        self.setup().patch(key, false, &NetworkPorts::default())
    }

    fn leftovers(&self, user: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.config_path(user).parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

fn entry_statuses(setup: &GameSetup, keys: &[&str]) -> Vec<RowStatus> {
    let rows = setup.rows(GamesPage::Pc, &NetworkPorts::default(), &Default::default());
    keys.iter().map(|key| rows.iter().find(|row| row.key == *key).unwrap().status).collect()
}

/// Installed Steam games of bridge listeners ask for configuration; everything else keeps its plain status.
#[test]
fn installed_bridge_games_are_to_configure() {
    let fixture = Fixture::new();
    let keys = ["ac", "acc", "automobilista2", "ets", "gtr2", "rre", "projectcars2", "dirtrally2.0_steam", "beamng"];
    let original = GameSetup::discover(&fixture.home, &NetworkPorts::default());
    assert_eq!(
        entry_statuses(&original, &keys),
        [
            RowStatus::ReadyToPlay,
            RowStatus::ReadyToPlay,
            RowStatus::Instructions,
            RowStatus::Config,
            RowStatus::ReadyToPlay,
            RowStatus::ReadyToPlay,
            RowStatus::Instructions,
            RowStatus::NotInstalled,
            RowStatus::Instructions,
        ]
    );
    assert_eq!(
        entry_statuses(&fixture.setup(), &keys),
        [
            RowStatus::Config,
            RowStatus::Config,
            RowStatus::Config,
            RowStatus::Config,
            // Not in a Steam library: the plain status.
            RowStatus::ReadyToPlay,
            RowStatus::ReadyToPlay,
            RowStatus::Instructions,
            // UDP games never get the bridge.
            RowStatus::NotInstalled,
            RowStatus::Instructions,
        ]
    );
    // The Console page is unchanged by the bridge.
    let console = fixture.setup().rows(GamesPage::Console, &NetworkPorts::default(), &Default::default());
    assert!(console.iter().all(|row| row.status != RowStatus::Config));
    // The notification status follows: a running game Steam starts without the bridge is not configured.
    assert_eq!(fixture.game_status("ac", false), GameStatus::NotPatched);
    assert_eq!(fixture.game_status("acc", false), GameStatus::NotPatched);
    assert_eq!(fixture.game_status("automobilista2", false), GameStatus::NotPatched);
    assert_eq!(fixture.game_status("gtr2", false), GameStatus::ReadyToPlay);
    assert_eq!(fixture.game_status("beamng", false), GameStatus::Instructions);
    assert_eq!(fixture.game_status("ac", true), GameStatus::Instructions, "a forced game counts as instructions");
    assert_eq!(fixture.config(USER), LOCALCONFIG, "looking changes nothing");
    assert!(!fixture.run().exists());
}

/// The config action deploys the bridge and puts the wrapper in front of `%command%` for every Steam user,
/// keeping the options that were there and every other byte of the files.
#[test]
fn config_sets_the_launch_options_of_every_user() {
    let fixture = Fixture::new();
    fixture.patch("ac").unwrap();
    let run = fixture.run();
    let run_text = run.to_str().unwrap();
    assert_eq!(
        fixture.config(USER),
        LOCALCONFIG.replace("\"gamemoderun %command% -x\"", &format!("\"gamemoderun {run_text} %command% -x\""))
    );
    assert_eq!(
        fixture.config(SECOND_USER),
        format!(
            "\"UserLocalConfigStore\"\n{{\n\t\"Software\"\n\t{{\n\t\t\"Valve\"\n\t\t{{\n\t\t\t\"Steam\"\n\t\t\t{{\n\t\t\t\t\"apps\"\n\t\t\t\t{{\n\t\t\t\t\t\"244210\"\n\t\t\t\t\t{{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"{run_text} %command%\"\n\t\t\t\t\t}}\n\t\t\t\t}}\n\t\t\t}}\n\t\t}}\n\t}}\n}}\n"
        )
    );
    // The user's files are kept once, and nothing else is left behind.
    assert_eq!(fixture.leftovers(USER), ["localconfig.vdf", "localconfigsrs_bck.vdf"]);
    assert_eq!(
        fs::read_to_string(fixture.config_path(USER).with_file_name("localconfigsrs_bck.vdf")).unwrap(),
        LOCALCONFIG
    );
    assert_eq!(fs::read(&run).unwrap(), fs::read(fixture.source.join(BRIDGE_RUN)).unwrap());
    assert_eq!(fs::read(run.with_file_name(BRIDGE_EXE)).unwrap(), b"MZ bridge 1");

    assert_eq!(fixture.status("ac"), RowStatus::ReadyToPlay);
    assert_eq!(fixture.game_status("ac", false), GameStatus::ReadyToPlay);
    assert_eq!(fixture.status("acc"), RowStatus::Config, "each game is configured on its own");
    // Again: nothing changes, the first backup stays.
    let configured = fixture.config(USER);
    fixture.patch("ac").unwrap();
    assert_eq!(fixture.config(USER), configured);
    fixture.patch("acc").unwrap();
    assert!(fixture.config(USER).contains(&format!(
        "\t\t\t\t\t\t\"LastPlayed\"\t\t\"1\"\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"{run_text} %command%\"\n"
    )));
    assert!(fixture.config(USER).contains("\"LaunchOptions\"\t\t\"-novid\""), "other games keep their options");
    assert_eq!(
        fs::read_to_string(fixture.config_path(USER).with_file_name("localconfigsrs_bck.vdf")).unwrap(),
        LOCALCONFIG
    );
    assert_eq!(fixture.status("acc"), RowStatus::ReadyToPlay);
}

/// An instructions entry returns to its instructions once the bridge is set up; a plugin entry gets its plugin
/// and the bridge in one action.
#[test]
fn instructions_and_plugin_entries() {
    let fixture = Fixture::new();
    fixture.patch("automobilista2").unwrap();
    assert_eq!(fixture.status("automobilista2"), RowStatus::Instructions);
    assert_eq!(fixture.game_status("automobilista2", false), GameStatus::Instructions);

    fixture.patch("ets").unwrap();
    let plugin = fixture.steam.join("steamapps/common/Euro Truck Simulator 2/bin/win_x64/plugins/ets2-telemetry.dll");
    assert!(plugin.is_file());
    assert!(fixture.config(USER).contains("\"227300\""));
    assert_eq!(fixture.status("ets"), RowStatus::ReadyToPlay);
    // Entries with nothing to configure, and that need no bridge, still have nothing.
    assert!(matches!(fixture.patch("gtr2"), Err(GamesError::NothingToConfigure(_))));
    assert!(matches!(fixture.patch("beamng"), Err(GamesError::NothingToConfigure(_))));
    assert!(matches!(fixture.setup().patch("ac", true, &NetworkPorts::default()), Err(GamesError::GameRunning)));
}

/// While Steam runs nothing is written to its files: it would replace them with its own copy when it exits.
#[test]
fn refused_while_steam_runs() {
    let fixture = Fixture::new();
    fixture.process("4242", "steam");
    assert_eq!(fixture.status("ac"), RowStatus::Config);
    let error = fixture.patch("ac").unwrap_err();
    assert!(matches!(error, GamesError::SteamRunning));
    assert_eq!(
        error.to_string(),
        "Steam is running, so OpenRigCue cannot set the game up. Close Steam, then try again."
    );
    assert_eq!(fixture.config(USER), LOCALCONFIG);
    assert_eq!(fixture.config(SECOND_USER), EMPTY_STORE);
    assert_eq!(fixture.leftovers(USER), ["localconfig.vdf"]);
    assert_eq!(fixture.status("ac"), RowStatus::Config);
    // A plugin entry's own files do not depend on Steam: they are written, the launch options are not.
    assert!(matches!(fixture.patch("ets"), Err(GamesError::SteamRunning)));
    assert!(fixture
        .steam
        .join("steamapps/common/Euro Truck Simulator 2/bin/win_x64/plugins/ets2-telemetry.dll")
        .is_file());
    assert_eq!(fixture.status("ets"), RowStatus::Config);
    // Steam closed: the same action completes.
    fs::remove_dir_all(fixture.proc_root.join("4242")).unwrap();
    fixture.patch("ets").unwrap();
    assert_eq!(fixture.status("ets"), RowStatus::ReadyToPlay);
    // An unreadable process list is treated as a running Steam.
    fs::remove_dir_all(&fixture.proc_root).unwrap();
    assert!(matches!(fixture.patch("ac"), Err(GamesError::SteamRunning)));
    assert_eq!(fixture.config(SECOND_USER).matches("LaunchOptions").count(), 1);
}

/// A Steam file this program cannot read as Steam's text is never rewritten.
#[test]
fn unreadable_configuration_is_left_alone() {
    let fixture = Fixture::new();
    let broken = "\"UserLocalConfigStore\"\n{\n\t\"Software\"\n\t{\n";
    fixture.write_config(SECOND_USER, broken);
    assert_eq!(fixture.status("ac"), RowStatus::Config);
    let error = fixture.patch("ac").unwrap_err();
    assert!(matches!(error, GamesError::Bridge(_, BridgeError::Unreadable(_))), "{error}");
    assert_eq!(fixture.config(SECOND_USER), broken);
    assert_eq!(fixture.leftovers(SECOND_USER), ["localconfig.vdf"]);
    assert_eq!(fixture.status("ac"), RowStatus::Config);
}

/// A new program version brings new bridge files: the entry asks for configuration again and the action copies
/// them; an existing wrapper elsewhere (the launch option `bridge-path` prints for a source build) is accepted.
#[test]
fn newer_bridge_files_and_other_wrappers() {
    let fixture = Fixture::new();
    fixture.patch("ac").unwrap();
    fs::write(fixture.source.join(BRIDGE_EXE), b"MZ bridge 2").unwrap();
    assert_eq!(fixture.status("ac"), RowStatus::Config);
    let configured = fixture.config(USER);
    fixture.process("4242", "steam");
    fixture.patch("ac").unwrap();
    assert_eq!(fs::read(fixture.run().with_file_name(BRIDGE_EXE)).unwrap(), b"MZ bridge 2");
    assert_eq!(fixture.config(USER), configured, "the launch options were already right");
    assert_eq!(fixture.status("ac"), RowStatus::ReadyToPlay);

    let other = fixture.home.join("src/openrigcue-bridge-run");
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    fs::write(&other, "").unwrap();
    let manual = LOCALCONFIG.replace(
        "\"LastPlayed\"\t\t\"1\"\n",
        &format!(
            "\"LastPlayed\"\t\t\"1\"\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"\\\"{}\\\" %command%\"\n",
            other.display()
        ),
    );
    fixture.write_config(USER, &manual);
    fixture.write_config(SECOND_USER, &manual);
    assert_eq!(fixture.status("acc"), RowStatus::ReadyToPlay);
    // The wrapper of a mount that is gone is replaced.
    let stale = manual.replace(other.to_str().unwrap(), "/tmp/.mount_gone/usr/lib/openrigcue/openrigcue-bridge-run");
    fixture.write_config(USER, &stale);
    fixture.write_config(SECOND_USER, &manual);
    assert_eq!(fixture.status("acc"), RowStatus::Config);
    fs::remove_dir_all(fixture.proc_root.join("4242")).unwrap();
    fixture.patch("acc").unwrap();
    assert!(fixture.config(USER).contains(&format!("\"LaunchOptions\"\t\t\"{} %command%\"", fixture.run().display())));
    assert_eq!(fixture.config(SECOND_USER), manual);
}

/// Without Steam, without a Steam user or without bridge files there is nothing this program could set up, and
/// every entry keeps its plain status.
#[test]
fn nothing_to_set_up_keeps_the_original() {
    let fixture = Fixture::new();
    let no_files = GameSetup::discover(&fixture.home, &NetworkPorts::default())
        .with_bridge(BridgeSetup::with_locations(&fixture.home, None, &fixture.data, &fixture.proc_root));
    assert_eq!(entry_statuses(&no_files, &["ac", "automobilista2"]), [RowStatus::ReadyToPlay, RowStatus::Instructions]);
    assert!(matches!(no_files.patch("ac", false, &NetworkPorts::default()), Err(GamesError::NothingToConfigure(_))));

    fs::remove_dir_all(fixture.steam.join("userdata")).unwrap();
    assert_eq!(fixture.status("ac"), RowStatus::ReadyToPlay);
    assert_eq!(fixture.game_status("ac", false), GameStatus::ReadyToPlay);

    let empty = tempfile::tempdir().unwrap();
    let setup = GameSetup::discover(empty.path(), &NetworkPorts::default()).with_bridge(BridgeSetup::with_locations(
        empty.path(),
        Some(&fixture.source),
        &empty.path().join("data"),
        Path::new("/proc"),
    ));
    assert_eq!(entry_statuses(&setup, &["ac"]), [RowStatus::ReadyToPlay]);
}

/// The launch steps for a launcher other than Steam: the wrapper's own folder when it is stable, else copies in
/// the stable folder; a missing bridge program is said so, a missing wrapper gives nothing.
#[test]
fn bridge_launch_names_the_bridge_the_wrapper_and_the_launch_option() {
    use super::proton_bridge::{bridge_launch, BridgeLaunch};
    let fixture = Fixture::new();
    let elsewhere = fixture.home.join("nothing-here");
    let candidates = [elsewhere.clone(), fixture.source.clone()];
    let source = fixture.source.canonicalize().unwrap();
    let launch = bridge_launch(&candidates, None).unwrap().unwrap();
    assert_eq!(
        launch,
        BridgeLaunch { bridge: source.join(BRIDGE_EXE), bridge_present: true, run: source.join(BRIDGE_RUN) }
    );
    assert_eq!(launch.option(), format!("\"{}\" %command%", source.join(BRIDGE_RUN).display()));
    assert!(!fixture.run().exists(), "nothing is copied from a stable folder");

    // A program whose folder changes on every start: the files are copied and the copies are named.
    let stable = fixture.data.join("openrigcue/bridge");
    let launch = bridge_launch(&candidates, Some(&stable)).unwrap().unwrap();
    assert_eq!(
        launch,
        BridgeLaunch { bridge: stable.join(BRIDGE_EXE), bridge_present: true, run: stable.join(BRIDGE_RUN) }
    );
    assert_eq!(fs::read(stable.join(BRIDGE_EXE)).unwrap(), b"MZ bridge 1");
    assert_eq!(fs::read(stable.join(BRIDGE_RUN)).unwrap(), fs::read(fixture.source.join(BRIDGE_RUN)).unwrap());

    // The set-up of the fixture answers the same; one without bridge answers nothing.
    assert_eq!(fixture.setup().bridge_launch().unwrap().unwrap().run, source.join(BRIDGE_RUN));
    let disabled =
        GameSetup::discover(&fixture.home, &NetworkPorts::default()).with_bridge(BridgeSetup::disabled(&fixture.home));
    assert_eq!(disabled.bridge_launch().unwrap(), None);

    fs::remove_file(fixture.source.join(BRIDGE_EXE)).unwrap();
    let launch = bridge_launch(&candidates, None).unwrap().unwrap();
    assert!(!launch.bridge_present);
    assert_eq!(launch.bridge, source.join(BRIDGE_EXE));
    fs::remove_file(fixture.source.join(BRIDGE_RUN)).unwrap();
    assert_eq!(bridge_launch(&candidates, None).unwrap(), None);
    assert_eq!(bridge_launch(&[elsewhere], Some(&stable)).unwrap(), None);
}

/// Every PC row of a game that needs the bridge offers the launch steps, whatever its status and whether or
/// not Steam has the game; other rows and the Console page do not.
#[test]
fn rows_of_bridge_games_offer_the_launch_steps() {
    use super::proton_bridge::{needs_bridge, BRIDGE_GAMES, NOT_STEAM_GAMES};
    let fixture = Fixture::new();
    let rows = fixture.setup().rows(GamesPage::Pc, &NetworkPorts::default(), &Default::default());
    for row in &rows {
        let expected = BRIDGE_GAMES.iter().any(|game| game.entry == row.key) || NOT_STEAM_GAMES.contains(&row.key);
        assert_eq!(row.bridge, expected, "{}", row.key);
        assert_eq!(needs_bridge(row.key), expected, "{}", row.key);
    }
    let flagged = |key: &str| rows.iter().find(|row| row.key == key).unwrap().bridge;
    // In Steam, not in Steam, not a Steam game, and a UDP game.
    assert!(flagged("ac") && flagged("iracing") && flagged("rfpro") && flagged("spacemonkey"));
    assert!(!flagged("beamng") && !flagged("dirtrally2.0_steam"));
    assert_eq!(serde_json::to_value(&rows[0]).unwrap()["bridge"], serde_json::json!(rows[0].bridge));
    let console = fixture.setup().rows(GamesPage::Console, &NetworkPorts::default(), &Default::default());
    assert!(console.iter().all(|row| !row.bridge));
}

/// Start-up brings a bridge folder that was set up before to the program's current files, so an update does not
/// turn configured games back to "to configure"; a folder that was never set up is not created.
#[test]
fn startup_refreshes_a_folder_that_was_set_up() {
    let fixture = Fixture::new();
    let bridge = BridgeSetup::with_locations(&fixture.home, Some(&fixture.source), &fixture.data, &fixture.proc_root);
    let setup = fixture.setup();
    bridge.refresh(setup.steam());
    assert!(!fixture.run().exists());
    fixture.patch("ac").unwrap();
    fs::write(fixture.source.join(BRIDGE_EXE), b"MZ bridge 2").unwrap();
    assert_eq!(fixture.status("ac"), RowStatus::Config);
    let configured = fixture.config(USER);
    fixture.process("4242", "steam");
    bridge.refresh(setup.steam());
    assert_eq!(fs::read(fixture.run().with_file_name(BRIDGE_EXE)).unwrap(), b"MZ bridge 2");
    assert_eq!(fixture.config(USER), configured);
    assert_eq!(fixture.status("ac"), RowStatus::ReadyToPlay);
}
