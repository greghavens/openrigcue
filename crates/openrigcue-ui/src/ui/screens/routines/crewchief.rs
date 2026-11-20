//! The CrewChief routine.

use super::desktop::{find_program_window, Desktop, ProgramWindow, UiaWindow};

/// The window and program of `go_to_profile_tab` (the app name in the message follows the naming rule; the
/// `rewChiefV4.exe` spelling is intended).
pub const PROGRAM: ProgramWindow = ProgramWindow {
    prefix: "Crew Chief V4 - ",
    exe: "%ProgramFiles(x86)%\\Britton IT Ltd\\CrewChiefV4\\CrewChiefV4.exe",
    cannot_find: "OpenRigCue could not locate CrewChiefV4.exe",
    wait_too_long: "Crewchief did not answer in time",
    exists_too_long: "CrewChief did not answer in time",
};

/// Finds the Crew Chief window (logging its progress) and brings it to the front; else a failure message.
pub fn go_to_profile_tab(desktop: &dyn Desktop) -> Result<UiaWindow, String> {
    find_program_window(desktop, &PROGRAM, true)
}

/// Crew Chief's profiles: the names in `gameDefinitionList` without `Vertical`, else the failure message.
pub fn get_list_profiles(desktop: &dyn Desktop) -> Result<Vec<String>, String> {
    list_result(desktop).map_err(super::RoutineFailure::message)
}

pub(super) fn list_result(desktop: &dyn Desktop) -> super::RoutineResult<Vec<String>> {
    use super::RoutineFailure::{Raised, Rejected};
    tracing::debug!("inside get list of profiles");
    let app_window = super::desktop::find_program_window_result(desktop, &PROGRAM, true)?;
    let list = app_window.by_id("gameDefinitionList", 5).map_err(Rejected)?;
    let mut names = Vec::new();
    for game in list.children().map_err(Raised)? {
        names.push(game.name().map_err(Raised)?);
    }
    if let Some(index) = names.iter().position(|name| name == "Vertical") {
        names.remove(index);
    }
    Ok(names)
}

/// Activates a profile in Crew Chief: `startApplicationButton` is clicked when it reads Stop, the game is
/// clicked in `gameDefinitionList`, and the button is clicked again; a failure message without the window.
pub fn activate_profile(desktop: &dyn Desktop, profile_name: Option<&str>) -> Result<String, String> {
    activation_result(desktop, profile_name).map_err(super::RoutineFailure::message)
}

pub(super) fn activation_result(desktop: &dyn Desktop, profile_name: Option<&str>) -> super::RoutineResult<String> {
    use super::RoutineFailure::{Raised, Rejected};
    let app_window = super::desktop::find_program_window_result(desktop, &PROGRAM, true)?;
    let switch = app_window.by_id("startApplicationButton", 5).map_err(Raised)?;
    if switch.name().map_err(Raised)? == "Stop" {
        switch.click().map_err(Raised)?;
    }
    let list = app_window.by_id("gameDefinitionList", 5).map_err(Rejected)?;
    for game in list.children().map_err(Raised)? {
        if Some(game.name().map_err(Raised)?.as_str()) == profile_name {
            game.focus().map_err(Raised)?;
            game.click().map_err(Raised)?;
        }
    }
    switch.click().map_err(|_| Rejected("Setting the game failed".into()))?;
    Ok("OK".into())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::desktop::fake::FakeDesktop;
    use super::*;

    /// Listing the profiles without Crew Chief: starting the program fails.
    #[test]
    fn list_without_crewchief() {
        let desktop = FakeDesktop::default();
        assert_eq!(get_list_profiles(&desktop).unwrap_err(), "OpenRigCue could not locate CrewChiefV4.exe");
    }

    /// Crew Chief open: the title is read again before the automation search, which finds nothing.
    #[test]
    fn crewchief_window_not_automated() {
        let desktop = FakeDesktop { titles: Mutex::new(vec!["Crew Chief V4 - 4.18".into()]), ..Default::default() };
        assert_eq!(activate_profile(&desktop, Some("rFactor 2")).unwrap_err(), "CrewChief did not answer in time");
        assert_eq!(desktop.calls(), ["exists Some(\"Crew Chief V4 - 4.18\") 3 1"]);
    }
}
