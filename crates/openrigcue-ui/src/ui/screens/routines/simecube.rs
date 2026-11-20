//! The Simucube 2 True Drive Paddock routine.

use super::desktop::{Desktop, ProgramWindow, UiaWindow};

/// The window and program of `go_to_profile_tab` (the app name in the message follows the naming rule).
pub const PROGRAM: ProgramWindow = ProgramWindow {
    prefix: "Simucube 2 True Drive 20",
    exe: "%ProgramFiles(x86)%\\Simucube 2 True Drive\\Simucube 2 True Drive Paddock.exe",
    cannot_find: "OpenRigCue could not locate the Simucube exe",
    wait_too_long: "Simucube did not answer in time",
    exists_too_long: "Simucube 2 True Drive did not answer in time",
};

/// Opens the Paddock's profile tab and answers the Paddock window: the window is found and brought to the
/// front, the third tab of `MW2.centralWidgetWindow.tabWidget.qt_tabwidget_tabbar` is clicked, and
/// `Go to offline (fallback) mode` is clicked when shown; else a failure message.
pub fn go_to_profile_tab(desktop: &dyn Desktop) -> Result<UiaWindow, String> {
    profile_tab(desktop).map_err(super::RoutineFailure::message)
}

fn profile_tab(desktop: &dyn Desktop) -> super::RoutineResult<UiaWindow> {
    use super::RoutineFailure::Raised;
    let window = super::desktop::find_program_window_result(desktop, &PROGRAM, false)?;
    let tabs = window
        .by_id("MW2.centralWidgetWindow.tabWidget.qt_tabwidget_tabbar", 5)
        .map_err(Raised)?
        .children()
        .map_err(Raised)?;
    if let Some(tab) = tabs.get(2) {
        tab.click().map_err(Raised)?;
    }
    if let Ok(button) = window.named("Go to offline (fallback) mode", 5) {
        let _ = button.click();
    }
    Ok(window)
}

/// Lists the Paddock's profiles. Without the Paddock, the answer is the failure message of opening the
/// profile tab. With the Paddock window the listing always fails with an error: the names of the cached
/// profiles list are never read, the error ends the calling `config_Button` thread, and no list is
/// returned.
pub fn get_list_profiles(desktop: &dyn Desktop) -> Result<Vec<String>, String> {
    list_result(desktop).map_err(super::RoutineFailure::message)
}

pub(super) fn list_result(desktop: &dyn Desktop) -> super::RoutineResult<Vec<String>> {
    let _app_window = profile_tab(desktop)?;
    Err(super::RoutineFailure::Raised("TypeError: cannot unpack non-iterable automation window object".into()))
}

/// Activates a profile: the profile named `profile_name` is clicked in the cached profiles list. Without the
/// window, or when the list cannot be read, the answer is this routine's failure message. With the window, a
/// matching profile is clicked (success, `OK`); with no match there is no result to report, which is an
/// error that ends the calling `launch_routine` thread.
pub fn activate_profile(desktop: &dyn Desktop, profile_name: Option<&str>) -> Result<String, String> {
    activation_result(desktop, profile_name).map_err(super::RoutineFailure::message)
}

pub(super) fn activation_result(desktop: &dyn Desktop, profile_name: Option<&str>) -> super::RoutineResult<String> {
    use super::RoutineFailure::{Raised, Rejected};
    // The profile tab is opened before the guarded part. A failure it reports becomes this
    // routine's failure message; an error it raises escapes instead.
    let window = match profile_tab(desktop) {
        Ok(window) => window,
        Err(Rejected(_)) => return Err(Rejected("Setting the profile failed".into())),
        Err(error @ Raised(_)) => return Err(error),
    };
    let select = || {
        let list=window.by_id("MW2.centralWidgetWindow.tabWidget.qt_tabwidget_stackedwidget.ProfilesTab.OfflineCachedProfilesDialog.cachedprofileslistWidget",10)?;
        for profile in list.children()? {
            if Some(profile.name()?.as_str()) == profile_name {
                profile.click()?;
                return Ok(Some("OK".into()));
            }
        }
        Ok::<_, String>(None)
    };
    match select() {
        Ok(Some(message)) => Ok(message),
        Ok(None) => Err(Raised("TypeError: cannot unpack non-iterable NoneType object".into())),
        Err(_) => Err(Rejected("Setting the profile failed".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::super::desktop::fake::FakeDesktop;
    use super::*;

    /// Listing the profiles without the Paddock: starting the program fails.
    #[test]
    fn list_without_paddock() {
        let desktop = FakeDesktop::default();
        assert_eq!(get_list_profiles(&desktop).unwrap_err(), "OpenRigCue could not locate the Simucube exe");
    }

    /// Activating a profile without the Paddock: the routine's failure message.
    #[test]
    fn activate_without_paddock() {
        let desktop = FakeDesktop::default();
        assert_eq!(activate_profile(&desktop, None).unwrap_err(), "Setting the profile failed");
    }
}
