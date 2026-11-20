//! Per-listener steering constants of the dispatcher.

/// Normalisation range of `steering_wheel_angle` published as `steering_wheel_max_angle` for a listener
/// (1 ⇒ ±1, 100 ⇒ ±100); listeners without an entry yield 0 ("unknown").
pub fn steering_range(listener: &str) -> f64 {
    match listener {
        "api" | "forza" | "gt7" | "gtsport" | "iracing" | "joy" | "rre" => 100.0,
        "ac" | "acevo" | "acudp" | "aeroflyfs2" | "automobilista2" | "bussim" | "driftphysicscrew" | "falconbms"
        | "gtav" | "hyperstorm" | "il2cliffs" | "ironrebellion" | "projectcars" | "projectcars2"
        | "projectcars2udp" | "projectcarsudp" | "redrover" | "rf1" | "rf2" | "rfpro" | "spacemonkey" | "thecrew"
        | "trackmania" | "vector36" | "vracer" => 1.0,
        _ => 0.0,
    }
}

/// A mapped value is returned as a float; a failed lookup returns integer 0.
pub(super) fn native_steering_range(listener: &str) -> openrigcue_model::native_value::LegacyValue {
    use openrigcue_model::native_value::LegacyValue;
    let range = steering_range(listener);
    if range == 0.0 {
        LegacyValue::Int(0)
    } else {
        LegacyValue::Float(range)
    }
}

/// Maximum steering angle handed to the dual wind fan controller when a listener starts delivering;
/// `None` when the listener has no entry.
pub fn wind_steering_angle(listener: &str) -> Option<u32> {
    let angle = match listener {
        "rbr" => 1,
        "api" => 2,
        "beamng" | "ccd" | "ed" | "gtl" | "gtr" | "gtr2" | "ironrebellion" | "kartkraft" | "lfs" | "race07" => 15,
        "joy" => 20,
        "ac" | "acevo" | "acudp" | "aeroflyfs2" | "automobilista2" | "bussim" | "dirt" | "driftphysicscrew" | "f1"
        | "f12018" | "f12019" | "f12020" | "f12021" | "f12022" | "f12023" | "f12024" | "f12025" | "falconbms"
        | "grid" | "gtav" | "hyperstorm" | "projectcars" | "projectcars2" | "projectcars2udp" | "projectcarsudp"
        | "redrover" | "rf1" | "rf2" | "rfpro" | "spacemonkey" | "tdusc" | "thecrew" | "trackmania" | "vector36"
        | "vracer" => 30,
        "aceshigh" | "condor" | "dcs" | "epic" | "flyinside" | "forza" | "fs" | "fs2k" | "fsx" | "gpbikes" | "gt7"
        | "gtsport" | "il2" | "il2cliffs" | "iracing" | "kart" | "motogp" | "mxbikes" | "nl2" | "p3d" | "pmracing"
        | "rre" | "silentwings" | "touringkarts" | "trucksim" | "warplanes" | "warthunder" | "wrc" | "wrc2023"
        | "wreckfest2" | "wrs" | "xplane" => 35,
        _ => return None,
    };
    Some(angle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table values and the 0 fallback.
    #[test]
    fn steering_range_per_listener() {
        assert_eq!(steering_range("iracing"), 100.0);
        assert_eq!(steering_range("ac"), 1.0);
        assert_eq!(steering_range("il2cliffs"), 1.0);
        assert_eq!(steering_range("dirt"), 0.0);
        assert_eq!(steering_range("none"), 0.0);
    }

    /// The maximum steering angle table covers the 80 registered listeners.
    #[test]
    fn wind_angle_per_listener() {
        assert_eq!(wind_steering_angle("rbr"), Some(1));
        assert_eq!(wind_steering_angle("api"), Some(2));
        assert_eq!(wind_steering_angle("ed"), Some(15));
        assert_eq!(wind_steering_angle("joy"), Some(20));
        assert_eq!(wind_steering_angle("f12025"), Some(30));
        assert_eq!(wind_steering_angle("wreckfest2"), Some(35));
        assert_eq!(wind_steering_angle("epic_oculus"), None);
    }
}
