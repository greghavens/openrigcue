//! The game setup instructions, built into the program (`assets/help/` of this crate).
//!
//! The Instructions window shows the page `<name>.html`
//! with `^port^` replaced by the game's `NETWORK_PORT` and `^ip^` by the computer's address, then translated line
//! by line ([`crate::i18n::Translator::translate_html`]). The texts naming SRS options inside the games
//! ("the SRS Tab", "Enable Sim Racing Studio", "Telemetry -> Sim Racing Studio") are what those games show.
//! Forza Horizon 5's PC guide covers Steam/Proton telemetry.

/// The page names and their HTML, sorted by name.
const PAGES: &[(&str, &str)] = &[
    ("aceshigh", include_str!("../assets/help/aceshigh.html")),
    ("acudp_console", include_str!("../assets/help/acudp_console.html")),
    ("automobilista2", include_str!("../assets/help/automobilista2.html")),
    ("beamng", include_str!("../assets/help/beamng.html")),
    ("epic", include_str!("../assets/help/epic.html")),
    ("epic_oculus_console", include_str!("../assets/help/epic_oculus_console.html")),
    ("f12016_console", include_str!("../assets/help/f12016_console.html")),
    ("f12017_console", include_str!("../assets/help/f12017_console.html")),
    ("f12018_console", include_str!("../assets/help/f12018_console.html")),
    ("f12019_console", include_str!("../assets/help/f12019_console.html")),
    ("f12020_console", include_str!("../assets/help/f12020_console.html")),
    ("f12021_console", include_str!("../assets/help/f12021_console.html")),
    ("f12022_console", include_str!("../assets/help/f12022_console.html")),
    ("f12023_console", include_str!("../assets/help/f12023_console.html")),
    ("f12024_console", include_str!("../assets/help/f12024_console.html")),
    ("f12025_console", include_str!("../assets/help/f12025_console.html")),
    ("fernbus", include_str!("../assets/help/fernbus.html")),
    ("flydangerous", include_str!("../assets/help/flydangerous.html")),
    ("flyinside", include_str!("../assets/help/flyinside.html")),
    ("forzahorizon4", include_str!("../assets/help/forzahorizon4.html")),
    ("forzahorizon4_console", include_str!("../assets/help/forzahorizon4_console.html")),
    ("forzahorizon5", include_str!("../assets/help/forzahorizon5.html")),
    ("forzahorizon5_console", include_str!("../assets/help/forzahorizon5_console.html")),
    ("forzahorizon6", include_str!("../assets/help/forzahorizon6.html")),
    ("forzahorizon6_console", include_str!("../assets/help/forzahorizon6_console.html")),
    ("forzamotorsports7", include_str!("../assets/help/forzamotorsports7.html")),
    ("forzamotorsports7_console", include_str!("../assets/help/forzamotorsports7_console.html")),
    ("forzamotorsports8", include_str!("../assets/help/forzamotorsports8.html")),
    ("forzamotorsports8_console", include_str!("../assets/help/forzamotorsports8_console.html")),
    ("furidashi", include_str!("../assets/help/furidashi.html")),
    ("gt7_console", include_str!("../assets/help/gt7_console.html")),
    ("gtav", include_str!("../assets/help/gtav.html")),
    ("gtsport_console", include_str!("../assets/help/gtsport_console.html")),
    ("il2", include_str!("../assets/help/il2.html")),
    ("kartkraft", include_str!("../assets/help/kartkraft.html")),
    ("kayakvr", include_str!("../assets/help/kayakvr.html")),
    ("nl2", include_str!("../assets/help/nl2.html")),
    ("nl2_steam", include_str!("../assets/help/nl2_steam.html")),
    ("projectcars", include_str!("../assets/help/projectcars.html")),
    ("projectcars2", include_str!("../assets/help/projectcars2.html")),
    ("projectcars2udp_console", include_str!("../assets/help/projectcars2udp_console.html")),
    ("projectcars3", include_str!("../assets/help/projectcars3.html")),
    ("projectcars3udp", include_str!("../assets/help/projectcars3udp.html")),
    ("projectcarspro", include_str!("../assets/help/projectcarspro.html")),
    ("projectcarsudp_console", include_str!("../assets/help/projectcarsudp_console.html")),
    ("rbr", include_str!("../assets/help/rbr.html")),
    ("rds", include_str!("../assets/help/rds.html")),
    ("redrover", include_str!("../assets/help/redrover.html")),
    ("rfpro", include_str!("../assets/help/rfpro.html")),
    ("silentwings", include_str!("../assets/help/silentwings.html")),
    ("spacemonkey", include_str!("../assets/help/spacemonkey.html")),
    ("thebus", include_str!("../assets/help/thebus.html")),
    ("torquedrift", include_str!("../assets/help/torquedrift.html")),
    ("touristbus", include_str!("../assets/help/touristbus.html")),
    ("vector36", include_str!("../assets/help/vector36.html")),
    ("whitewatervr", include_str!("../assets/help/whitewatervr.html")),
    ("wrc", include_str!("../assets/help/wrc.html")),
    ("xplane", include_str!("../assets/help/xplane.html")),
    ("xplane10", include_str!("../assets/help/xplane10.html")),
    ("xplane_12", include_str!("../assets/help/xplane_12.html")),
];

/// The page `name`, or `None` when there is no page of that name (the window is then not opened).
pub fn page(name: &str) -> Option<&'static str> {
    PAGES.binary_search_by(|(key, _)| (*key).cmp(name)).ok().map(|index| PAGES[index].1)
}

/// Every page name.
pub fn names() -> impl Iterator<Item = &'static str> {
    PAGES.iter().map(|(name, _)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 60 page names, in name order (the lookup is a binary search).
    #[test]
    fn every_srs_page_name_is_built_in() {
        let names: Vec<&str> = names().collect();
        assert_eq!(names.len(), 60);
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        for name in ["aceshigh", "f12020_console", "forzahorizon5", "nl2_steam", "xplane_12", "xplane10", "wrc"] {
            assert!(page(name).is_some(), "{name}");
        }
        assert_eq!(page("ac"), None);
    }

    /// The pages that need `^port^` / `^ip^` carry them.
    #[test]
    fn placeholders_match_the_original_pages() {
        let with_port = [
            "aceshigh",
            "epic",
            "f12016_console",
            "f12025_console",
            "forzahorizon4",
            "forzahorizon6",
            "il2",
            "kartkraft",
            "nl2",
            "nl2_steam",
            "rbr",
            "silentwings",
            "vector36",
            "wrc",
            "xplane",
            "xplane_12",
        ];
        for name in with_port {
            assert!(page(name).unwrap().contains("^port^"), "{name}");
        }
        for name in ["acudp_console", "epic_oculus_console", "gt7_console", "xplane10", "torquedrift", "rds"] {
            assert!(!page(name).unwrap().contains("^port^"), "{name}");
        }
        for name in ["epic_oculus_console", "f12020_console", "forzahorizon4_console", "forzahorizon6_console"] {
            assert!(page(name).unwrap().contains("^ip^"), "{name}");
        }
        assert!(page("epic_oculus_console").unwrap().contains("33001"));
        assert!(page("f12020_console").unwrap().contains("20780"));
        assert!(page("xplane10").unwrap().contains("49000"));
    }
}
