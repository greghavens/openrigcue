# OpenRigCue

OpenRigCue reads telemetry from racing and flight games and drives motion platforms, bass shakers, wind fans,
LEDs, belt tensioners and other rig accessories. It runs natively on Linux (x86_64). Windows games running under
Steam Proton are supported through a small bridge program.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/greghavens/openrigcue/main/install.sh | sh
```

This installs OpenRigCue for your user and adds it to your application menu. Run it again to update.

OpenRigCue is compatible with Sim Racing Studio (SRS) 2026.6.14: it supports the same games and devices, uses the
same defaults, and reads existing SRS settings and profiles. It is free and open source.

OpenRigCue is an independent project. It is not affiliated with or endorsed by Sim Racing Studio.

> **Status: early software.** Expect bugs, and keep the motion platform's emergency stop within reach the first
> time you run it.

## What it does

- **Game detection and telemetry** for 125 catalog entries (80 game listeners). Games are detected by process name
  and window title (X11/XWayland, on the desktop and in a gamescope Game Mode session). Telemetry arrives over UDP,
  shared memory, named pipes, HTTP/TCP, SimConnect or process memory, depending on the game.
- **Motion cueing** for 73 platform models: smoothing, shaping, soft start, gear smoothing, wheel and canned
  effects, test mode, and the Tuning → Motion screen's **Autotune**, which measures game telemetry to help choose each
  effect's input range.
- **Bass shakers** on any multi-channel sound card (stereo, quad, 5.1, 7.1), with speaker layouts, custom matrices
  and effect tests.
- **Accessories**: wind, LED strip and tach, shift beep, belt tensioner, HaptiCR, SIM3D, iFlag, gear indicator,
  the OBD2HUD Bluetooth head-up display, and motion compensation from an IMU.
- **Hotkeys**: keyboard, gamepad, joystick and button-box shortcuts (wind/shaker/motion toggles, canned effects,
  slider steps, motion-compensation actions, CSV snapshot). Settings → Shortcuts lists every action; a control's
  **Options** and a screen's **Actions** also open its shortcut editor.
- **VR motion compensation**: a SteamVR driver and an OpenXR API layer (see [VR](#vr-motion-compensation)).
- **Desktop and browser UI** on `http://<this computer>:33733`, served by the `openrigcue run` service. The native
  GTK/WebKit window has a splash screen, tray Open/Quit, saved geometry and start-minimized; `--headless` serves the
  page without opening a window. The interface has four sections, **Tuning** (the start screen with the session
  instruments, Motion, Shake, Wind, Belt, SIM3D, HapticR, IFlag, LED, Tach, Routines, Profiles and Downloaded
  Profiles), **Games** (PC, Console, Telemetry), **Hardware** (Hardware, Joystick, Motion Compensation) and
  **Settings** (App, Shortcuts, Help), a rig panel with every device's status, a search, the language switcher,
  and a save bar with **Save**, **Revert** and **Compare**, which lists the values changed since the last save. It
  works at desktop and phone widths.
- **Themes**: Redline (default), Apex and Vector, chosen under Settings → App → General → Theme and applied at once;
  the screen, open dialogs and unsaved values stay. Each theme has its own navigation, top bar, session
  instruments (live telemetry readouts and **Inspect channels** in Redline and Vector, a speed trace in Apex) and
  wording.
- **Notifications**: toast notifications (successes hide after 6 s, paused while the pointer or focus is on them;
  at most two at a time; warnings stay until dismissed), connection faults shown inline until they are resolved,
  and the **Activity** drawer, which keeps the history of every notification.
- **Other features**: CSV telemetry export, joystick as a motion source, UDP forwarding, game-side config
  patching, and eight UI languages. A new configuration starts in your session's language when it is one of them.
  To change texts, put a file named after the language (`de.json`, `fr.json`, …) in `locales` inside the
  configuration directory (`~/.config/openrigcue/locales/`) holding only the keys you want to change, as
  `"English text": "your text"`; the rest stays built in. Changes apply at the next start.

## Supported games

| Transport | Games |
|---|---|
| UDP | F1 2011 – F1 25, Dirt 2/3/4/Showdown, Dirt Rally 1/2.0, GRID 2/2019/Legends, EA Sports WRC, WRC Generations, Gran Turismo 7 / Sport (PlayStation), Forza Motorsport 7/8 and Horizon 4/5/6, Project Cars 1/2/3 on Xbox/PlayStation, Assetto Corsa PS4, Project Motor Racing, Wreckfest 2, BeamNG, Live for Speed, Richard Burns Rally, GTA V, KartKraft, Touring Karts, MotoGP 2018, The Crew / The Crew 2, HyperStorm, Iron Rebellion, Vector36, Red Rover, Epic Roller Coasters, Test Drive Unlimited Solar Crown, X-Plane, DCS World, IL-2 Sturmovik, Condor, Silent Wings, Aces High III, Aerofly FS2, FlyInside, the SRS public API, and others |
| Shared memory (through the Proton bridge) | Assetto Corsa / Competizione / EVO / Rally, Project Cars 1/2/3 / Pro (PC), iRacing, Automobilista 1/2, rFactor 1/2, Le Mans Ultimate, rFpro, RaceRoom, GTR/GTR 2/GT Legends/Race 07 and other SimBin titles, Euro Truck Simulator 2, American Truck Simulator, Falcon BMS, GP Bikes, MX Bikes, Kart Racing Pro, World Racing Series, TrackMania, Space Monkey, Warplanes WW1 / Battles over Pacific, IL-2 Cliffs of Dover |
| Named pipe (through the bridge) | Farming Simulator 19/22/25 (needs the game-side mod, see below) |
| HTTP / TCP / JSON | War Thunder, Aces of Thunder, The Bus / Fernbus / Tourist Bus Simulator, NoLimits 2, V-Racer Hoverbike |
| SimConnect | Flight Simulator X, Prepar3D, Flight Simulator 2020 / 2024 |
| Process memory | Elite Dangerous |
| Local input | joystick / wheel as a motion source |

The game plugins for rFactor 1/2, Euro Truck Simulator 2 / American Truck Simulator and Aerofly FS 2 are built
from `game-plugins/` and embedded in OpenRigCue. Game windows are found on every X server of your session (a
desktop's XWayland and both servers of a gamescope session), also when OpenRigCue runs as a service without
`DISPLAY`. Native Wayland game windows are not seen by title detection; process names are.

FlyInside needs a startup script in the simulator's `Data/Scripts` folder. OpenRigCue carries its own,
`Startup_OpenRigCue.chai`: FlyInside's **Setup** dialog (Games → PC) has a link that saves it. It has not
been tried against the simulator yet.

## Supported hardware

| Kind | Devices |
|---|---|
| Motion platforms | DOF Reality (SMC3 controllers, one or two boxes), Thanos AMC (AASD15A, MDBOX, NANO, Thanos4U), PT-Actuator (UDP and CAN bridge), Yaw VR / Yaw2 / Yaw3, VNM Simulation, PROSIMU PRS200 family, Dyadic SCN6, Actuon8; 73 platform models in total |
| Bass shakers | any ALSA / PipeWire / PulseAudio output with 2, 4, 6 or 8 channels (ShakeKit and Shakebox layouts included) |
| Wind, LEDs, tach | SRS Intellibox v5 / v6 (firmware not included: imported from your SRS installer; flashing through `avrdude`) |
| Haptics | SRS HaptiCR, SIM3D pedals/seat/wind (SimHub ShakeIt protocol), belt tensioner (Dayond CAN over ESP32) |
| Displays | iFlag 8×8 panel (Colorduino/Uno and Mega 2560 + WS2812), SymProjects gear indicator |
| Motion compensation sensors | WitMotion JY901 IMU, IntelliComp |
| Head-up display | OBD2HUD (Bluetooth LE through BlueZ) |

Not supported: the Bosch laser. SimTech Revolution is implemented but no platform model offers it.

## Install details

The install command at the top downloads the latest AppImage to `~/.local/bin/openrigcue` and adds a menu entry;
it installs nothing system-wide and needs no root access. `OPENRIGCUE_INSTALL_DIR` chooses another folder.

To install by hand instead, download `openrigcue-x86_64.AppImage` from the
[latest release](https://github.com/greghavens/openrigcue/releases/latest), make it executable and start it:

```sh
chmod +x openrigcue-x86_64.AppImage
./openrigcue-x86_64.AppImage
```

With no arguments it runs the service and opens the window. With arguments it runs the CLI, for example
`openrigcue probe`.

To build it yourself instead, see [Build](#build); a source build also gives the plain binary
`target/release/openrigcue`, which you can copy to `~/.local/bin/`.

Runtime requirements:

- The desktop window needs GTK 3 and WebKitGTK 4.1 on the host (Fedora/Bazzite `webkit2gtk4.1`, Debian/Ubuntu
  `libwebkit2gtk-4.1-0`). They are loaded at run time and are not bundled. Tray controls need a desktop that
  supports StatusNotifierItem.
- Joystick input uses SDL2 when `APP.enable_sdl2` is on. The AppImage bundles SDL; a standalone binary needs the
  system `libSDL2-2.0.so.0`. Keyboard hotkeys use evdev.

On the first GUI launch the app creates `$XDG_CONFIG_HOME/autostart/openrigcue.desktop` (default
`~/.config/autostart/`) if it does not exist; the login setting under Settings → App decides whether a login start opens the
app. An existing entry is kept.

### Run as a service

```sh
install -Dm0644 packaging/systemd/openrigcue.service ~/.config/systemd/user/openrigcue.service
systemctl --user daemon-reload && systemctl --user enable --now openrigcue
```

Then open `http://127.0.0.1:33733`. The UI listens on every interface (`0.0.0.0`), so any device on your network
can open it. It has no authentication and it moves a motion rig: run it only on a network you trust.

### Device access

Serial devices need permission to open (the `dialout` group, or the udev rules):

```sh
sudo install -m 0644 packaging/udev/70-openrigcue.rules /etc/udev/rules.d/ && sudo udevadm control --reload
```

The rules also stop ModemManager from probing the devices. Hotkeys read game controllers and keyboards from `/dev/input` without grabbing them, so they work under Wayland and X11. Controllers are readable by
the logged-in user on a systemd desktop. Keyboards are not: keyboard shortcuts need the commented keyboard line of
the udev rules (or the `input` group), which lets every program you run read the keyboard. The shortcut editor
shows a note while the keyboard cannot be read.

### Proton games (shared memory and pipes)

Games that publish telemetry through Windows shared memory or named pipes need the bridge,
`openrigcue-bridge.exe`. It runs inside the game's Proton prefix and copies the data to `/dev/shm/openrigcue`.

For a game in a Steam library Games → PC sets this up. Until Steam starts the game with the bridge, its row
reads **Config**; **Apply setup** in the row's **Setup** dialog copies the bridge to `~/.local/share/openrigcue/bridge` and puts
`openrigcue-bridge-run` in front of `%command%` in the game's launch options, for every Steam user of the
installation. Existing launch options are kept, and the first change keeps your file as `localconfigsrs_bck.vdf`
next to Steam's `localconfig.vdf`. A game started without the bridge raises the "Game not configured"
notification.

Steam writes its own copy of the launch options back while it runs, so Apply setup answers "Steam Running" until
Steam is closed. In Game Mode Steam always runs: do this once from the desktop with Steam closed.
`openrigcue patch <game>` does the same from a terminal.

For a game started outside Steam (Lutris, Heroic, Bottles, plain Wine) nothing is set up automatically.
`openrigcue bridge-path` prints where the bridge is; start the game through it inside the game's prefix
(`openrigcue-bridge.exe -- <game.exe> <arguments>`). For a Steam game Games → PC does not recognise, set the
launch option to:

```
openrigcue-bridge-run %command%
```

UDP games need no bridge. Enable their telemetry output in the game, or use **Apply setup** in the game's Setup dialog
(`openrigcue patch`), which edits the game's files inside its Proton prefix and keeps a backup.

### Routines

Routines can control FanaLab, Simucube and Crew Chief through the bundled Windows automation helper. Use the Wine
prefix where those programs are installed, for example `WINEPREFIX="$HOME/.wine" ./openrigcue-x86_64.AppImage`.
The default runner is `wine`; set `OPENRIGCUE_WINE` to use another. Only routines enabled under Tuning → Routines run
automatically. The helper does not install those programs or choose their prefix.

## Build

Requirements: Rust 1.89 or newer and the ALSA development headers (`alsa-lib-devel` on Fedora). There is one
build: every part (shaker audio and shift beep, FMOD engine sound, OBD2HUD head-up display, joystick and hotkey
input, game detection by window title) is always compiled in.

```sh
cargo build --release -p openrigcue
cargo test --workspace
```

On an image-based system such as Bazzite, build inside a toolbox that has `alsa-lib-devel`, or build everything,
the AppImage included, in the build container:

```sh
podman build -t openrigcue-build -f packaging/build.Containerfile packaging
podman run --rm -v "$PWD":/src:Z -w /src openrigcue-build packaging/appimage/build-appimage.sh
```

This builds the service, both VR components and the SteamVR tracker driver, cross-builds the Windows bridge
(`x86_64-pc-windows-gnu`), and writes `target/appimage/openrigcue-x86_64.AppImage`. The bridge alone builds with
`cargo build --release -p openrigcue-bridge --target x86_64-pc-windows-gnu` (MinGW-w64 needed; see
`crates/openrigcue-bridge/container/Containerfile`).

- `packaging/appimage/build-appimage.sh --stage-only` prepares `target/.appimage-build/AppDir` without assembling
  the image; `--no-build` stages previously built artifacts. `CARGO_TARGET_DIR` selects the compiled inputs.
  `APPIMAGE_RUNTIME_FILE` supplies the AppImage runtime for offline assembly.
- Packaging checks every staged ELF and records its GLIBC requirements in
  `usr/share/openrigcue/glibc-baseline.json`. The default ceiling is 2.35 (the Ubuntu 22.04 build container);
  `OPENRIGCUE_GLIBC_MAX` raises it for host builds. GTK/WebKit come from the host and are outside this check.
- The AppImage contains the VR files under `usr/share/openrigcue/vr`; staging does not register or install them.

Checks: the workspace is formatted with `cargo fmt` (`rustfmt.toml`, 120 columns) and kept free of `cargo clippy`
warnings. `scripts/check.sh` runs formatting, build, tests (in a bubblewrap sandbox that keeps them away from
devices, the network and your runtime directory, when `bwrap` is available) and clippy, inside the toolbox that has
the ALSA headers (`openrigcue-audio`, or `$OPENRIGCUE_AUDIO_TOOLBOX`). `scripts/check-independent.sh` runs the host
tests of the stand-alone OBD2HUD firmware, plugin and tracker projects.

### Game plugins

`game-plugins/rfactor1`, `rfactor2`, `ets2` and `aerofly` are stand-alone Cargo projects that cross-compile the
Windows plugin DLLs embedded in OpenRigCue (`crates/openrigcue-config/src/patch/builtin/`). Each has
`scripts/build.sh` (MinGW-w64 linkers; the ETS2 build also needs the i686 toolchain).

### Firmware

The repository holds one firmware project, for the OBD2HUD. Firmware for the Intellibox and the other devices is not
in the repository: the app uses the images imported from your SRS installer (see "SRS sounds and firmware images"
below).

- **OBD2HUD** (`firmware/obd2hud`): ESP32 Rust firmware on the ESP-IDF 3.3 / Arduino-ESP32 1.0.6 runtime
  (`firmware/esp-idf33-sys`). Build the flash image with:

  ```sh
  bash firmware/obd2hud/make-image.sh
  ```

  The image is written to `<CARGO_TARGET_DIR>/image/` (default `target/bin/fw-obd2hud`).
  Prerequisites: the Rust `esp` toolchain installed with `espup`, the `espflash` CLI, libclang (the clang bundled
  by `espup` is found automatically; otherwise set `LIBCLANG_PATH`), Node.js, `curl`, `unzip`, `tar`, `git` and
  `flock`. On first use the build downloads the SHA-pinned Arduino-ESP32 1.0.6 / ESP-IDF 3.3.5 runtime, its GCC 5.2
  toolchain and NimBLE into the build cache; Python, CMake and newer ESP-IDF releases are not used. `CARGO` and
  `ESPFLASH` select other binaries. Started inside a toolbox, the script runs the build with the host's installed
  tools. The AppImage bundles the OBD2HUD application image listed in `packaging/firmware/images.tsv`. **The bundled OBD2HUD image has not yet been tested on a real OBD2HUD.** It is sent to a
  HUD only when no firmware has been imported from an SRS installer; an imported image always takes its place.
  The merged flash images remain build outputs for initial board programming; OTA uses the application image.

## Maintenance tools

`openrigcue tools` opens the maintenance tools (SMC3 motor setup, DOF firmware maintenance, USB diagnostics);
`tools --headless` serves them at `http://127.0.0.1:33734`, and `tools --usb-report /path/report.xml --xml` exports
USB diagnostics without opening a controller. The tools do not start the normal application's hardware workers;
every operation that changes a controller is started from the tools interface.

## Command line

```
openrigcue run [--config-dir DIR] [--profile-path FILE] [--login-calling] [--installer-calling] [--headless]
               [--srs-install PATH] [--pipe-tcp-port 33010 | --no-pipe-tcp]
openrigcue import-srs <SRS documents folder> [--force]
openrigcue games | patch | bridge-path | probe | platforms
openrigcue motion test | diag
openrigcue srs-key [--stdin] [--key-file FILE] [--replace]
openrigcue srs-sync [--game ID] [--vehicle NAME] [--page motion|shakers|belt|rumble|haptic]
```

`run` starts the application. `--config-dir` selects the configuration directory, `--profile-path` imports a
profile file (handed to the running instance when there is one), `--login-calling` starts only when "Start with
login" is on, `--installer-calling` is accepted for compatibility and ignored, and `--headless` runs the server
without opening the window. The web UI listens at `APP.webui_port` (33733 by default). `probe`, `motion test` and
`motion diag` talk to connected controllers, and **`motion test` moves the platform**.

## Configuration and SRS import

The configuration lives in `~/.config/openrigcue` by default: `config.ini`, `profile.db` and `metadata.db`, in the
same formats SRS uses. Older OpenRigCue TOML stores are upgraded on startup and kept as backups.

- **Settings and profiles**: `openrigcue import-srs "<path>/Documents/SimRacingStudio 2.0"`, or the import action in
  the UI. This imports `config.ini` with every SRS settings migration up to the current version, `profile.db`,
  `metadata.db`, shaker layouts and joystick profiles. Account and cloud settings are dropped. Your SRS folder is
  only read.
- **`.srs` profile files**: Tuning → Profiles → Import, export and export-all. A whole file can also be imported with
  `openrigcue run --profile-path /path/to/file.srs`.
- **SRS sounds and firmware images**: in Hardware → Hardware → **SRS Installer**, enter the path of the SRS installer
  `.exe` or your SRS installation folder (from a Windows partition or a Proton prefix) and press **Import**; or
  start once with `--srs-install` pointing at it. OpenRigCue reads the installer's archive and copies the sounds
  and every firmware image into `srs-import` in the configuration directory; later starts use that copy. After an
  import from the app, the sound samples can be used at once; voice prompts and firmware images are used from the
  next start. The copy
  holds the shaker samples, shift-beep sound and FMOD banks (used while Hardware → Hardware → "Use SRS Sound Samples"
  is on; otherwise the shakers play built-in synthesized samples and a built-in shift beep), the IntelliTune voice
  clips, and the firmware images, of which the flashers use the Intellibox and OBD2HUD ones. OpenRigCue itself
  ships no SRS firmware.

### Optional SRS Premium profile sync

If you have an SRS Premium key, enter it once in a terminal (the prompt hides what you type):

```sh
openrigcue srs-key
openrigcue srs-sync --game ac --page motion
```

The key is stored with mode `0600` in `${XDG_CONFIG_HOME:-$HOME/.config}/openrigcue/srs-sync/premium.key`. Use
`srs-key --replace` to change it; a password manager can pipe it to `srs-key --stdin`, and
`srs-sync --key-file /absolute/path` reads an existing private file. Never put the key in a command argument, shell
history or Git.

Downloads go to `${XDG_DATA_HOME:-$HOME/.local/share}/openrigcue/srs-profiles`: one `.srs` file per resource under
`profiles/`, and `manifest.json` with author (`[PRO]` for a pro tuner), equipment, target, remote version and content
hash. Running `srs-sync` again without options syncs the saved targets and downloads only new versions or missing
files. `srs-sync --list` shows the cached library without a key or network connection. With no target options on
first use it checks every known game and all five profile categories at game level. Add exact vehicles with
`--vehicle "Exact vehicle name"`; `--targets targets.json` takes a JSON array such as
`[{"game":"ac","page":"motion","vehicle":"Exact vehicle name"}]`. `--library-dir DIR` selects another library and
`--refresh` re-downloads unchanged versions to detect silent changes. Locally edited files and older versions are
kept. Each run prints the path of its private log under the library's `logs/` directory; the log never contains the
key, request bodies or server responses.

In the app, open **Downloaded Profiles** from Motion, Shake, Belt, SIM3D or HapticR, or from **Profile Manager**, to
browse the default library by game, vehicle, profile type, author and equipment. Choose **Select**, confirm with
**Apply**, then **Save** to keep the settings or **Cancel** to discard them. Only the chosen tuning category is
applied. The sync never changes your active tune or profile database.

## FMOD engine sound

The AppImage includes the FMOD Engine 2.02.35 Linux runtime, taken from the official FMOD Engine download. To
build the AppImage yourself, sign in at <https://www.fmod.com/download>, download FMOD Engine 2.02.35 for Linux
(`fmodstudioapi20235linux.tar.gz`) and put it at `target/native-deps/fmodstudioapi20235linux.tar.gz` (or set
`FMOD_ARCHIVE` to its path). The build unpacks only the two release libraries, verifies both with SHA-256 and
caches them under `target/native-deps/fmod`; the SDK headers, examples and media are excluded. `FMOD_SDK` can
select an unpacked compatible 2.02 SDK instead.

OpenRigCue plays its own synthesized engine sound by default. To use imported engine banks, import your SRS
installer in Hardware → Hardware → **SRS Installer** (or start once with
`openrigcue run --srs-install <installation-or-installer>`) and enable Hardware → Hardware → **Use SRS Sound
Samples**.
The recordings and banks come from that optional import. A standalone binary can find the runtime in
`~/.local/share/openrigcue/fmod` or `OPENRIGCUE_FMOD_DIR`.

FMOD remains proprietary software. Its [own license terms](https://www.fmod.com/legal) govern use and
redistribution. `LICENSE-EXCEPTION.md` supplies the separate GPL permission for combining OpenRigCue with FMOD.

## VR motion compensation

On a motion rig the headset's tracking sees the rig move, so the VR view moves with the platform. OpenRigCue
publishes the rig pose from the IMU (WitMotion JY901 / IntelliComp) to `/dev/shm/openrigcue/rig-pose`, and one of
two consumers subtracts it: a SteamVR driver (`crates/openrigcue-steamvr-driver`) or an OpenXR API layer
(`crates/openrigcue-xr-layer`). `steamvr-driver/` additionally provides the virtual tracker driver
(`driver_simracingstudio`) that other motion-compensation tools expect.

Open **Hardware → Motion Compensation**. Each runtime has **Install** and **Uninstall** buttons that register or
remove the integration; when SteamVR is running the app asks it to restart. Settings (calibrate, re-centre, centre
of rotation) are on the same screen. From a source tree, `crates/openrigcue-steamvr-driver/scripts/install.sh` and
`crates/openrigcue-xr-layer/scripts/install.sh [--always-on]` do the same. `openrigcue-rigpose-sim` feeds a
synthetic pose for a bench test without a rig.

## Repository layout

| Path | Role |
|---|---|
| `crates/openrigcue` | the `openrigcue` binary (service and CLI) |
| `crates/openrigcue-daemon` | the service: threads, the UI server, notifications, voice clips, export |
| `crates/openrigcue-sources` | game telemetry listeners and the telemetry dispatcher |
| `crates/openrigcue-config` | settings, SRS import and migrations, game catalog, tunes, profiles, game patching |
| `crates/openrigcue-cueing` | motion cueing and platform models |
| `crates/openrigcue-devices` | motion drivers, serial discovery |
| `crates/openrigcue-effects` | shakers, wind, LEDs, accessories, motion-compensation pose |
| `crates/openrigcue-model` | shared telemetry and value types |
| `crates/openrigcue-ui`, `crates/openrigcue-desktop` | the web UI and the native window |
| `crates/openrigcue-bridge` | Windows bridge for Proton games |
| `crates/openrigcue-rigpose`, `crates/openrigcue-steamvr-driver`, `crates/openrigcue-xr-layer`, `steamvr-driver` | VR motion compensation |
| `crates/openrigcue-tools` | development and bench tools |
| `game-plugins/` | Windows game plugins |
| `firmware/` | OBD2HUD firmware |
| `vendor/` | patched copies of `libsqlite3-sys`, `lewton` and `ogg` (see each `OPENRIGCUE.md`) |
| `packaging/` | AppImage, systemd, udev, VR and firmware staging |

## License

OpenRigCue is licensed under the GNU General Public License v3.0 or later (`COPYING`), with the additional
permission for FMOD in `LICENSE-EXCEPTION.md`. Third-party components keep their own licences; their notices are in `LICENSES/` and in the vendored
packages. Source: <https://github.com/greghavens/openrigcue>.
