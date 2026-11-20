//! The FanaLab routine.

use super::desktop::{find_program_window, Desktop, ProgramWindow, UiaWindow};

/// The window and program of `go_to_profile_tab` (the app name in the message follows the naming rule).
pub const PROGRAM: ProgramWindow = ProgramWindow {
    prefix: "FanaLab V1.",
    exe: "%ProgramFiles(x86)%\\Fanatec\\FanaLab\\Control\\FanaLab.exe",
    cannot_find: "OpenRigCue could not locate FanaLab.exe",
    wait_too_long: "FanaLab did not answer in time",
    exists_too_long: "FanaLab did not answer in time",
};

/// Finds the FanaLab window and brings it to the front; else a failure message.
pub fn go_to_profile_tab(desktop: &dyn Desktop) -> Result<UiaWindow, String> {
    find_program_window(desktop, &PROGRAM, false)
}

/// FanaLab's profiles: a fixed list, read without touching FanaLab.
pub fn get_list_profiles(_desktop: &dyn Desktop) -> Result<Vec<String>, String> {
    Ok(["Auto Setup", "1", "2", "3", "4", "5"].map(str::to_owned).to_vec())
}

/// Activates a profile in FanaLab: `Tuning Menu` is clicked, the `Advanced` mode switched on
/// (`ToggleButtonMode`) and the profile clicked; a failure message without the window.
pub fn activate_profile(desktop: &dyn Desktop, profile_name: Option<&str>) -> Result<String, String> {
    activation_result(desktop, profile_name).map_err(super::RoutineFailure::message)
}

pub(super) fn activation_result(desktop: &dyn Desktop, profile_name: Option<&str>) -> super::RoutineResult<String> {
    use super::RoutineFailure::{Raised, Rejected};
    let app_window = super::desktop::find_program_window_result(desktop, &PROGRAM, false)?;
    app_window.named("Tuning Menu", 5).map_err(Raised)?.click().map_err(Raised)?;
    let _connection_control =
        app_window.named("Wheel device detected", 5).map_err(|_| Rejected("Setting the profile failed".into()))?;
    // Printing the lazy Advanced control resolves its properties.
    if app_window.named("Advanced", 5).and_then(|control| control.name()).is_err() {
        app_window.by_id("ToggleButtonMode", 5).map_err(Raised)?.click().map_err(Raised)?
    }
    let select = || app_window.named(profile_name.ok_or("Profile name is missing")?, 5)?.click();
    select().map_err(|_| Rejected("Setting the Profile failed".to_owned()))?;
    Ok("OK".into())
}

#[cfg(test)]
mod tests {
    use super::super::desktop::fake::FakeDesktop;
    use super::*;

    /// The profile list: `Auto Setup` and `1`…`5`, without touching FanaLab.
    #[test]
    fn list_is_fixed() {
        let desktop = FakeDesktop::default();
        assert_eq!(get_list_profiles(&desktop).unwrap(), ["Auto Setup", "1", "2", "3", "4", "5"]);
        assert!(desktop.calls().is_empty());
    }

    /// Activating a profile without FanaLab: starting the program fails.
    #[test]
    fn activate_without_fanalab() {
        let desktop = FakeDesktop::default();
        assert_eq!(activate_profile(&desktop, Some("1")).unwrap_err(), "OpenRigCue could not locate FanaLab.exe");
    }
}
