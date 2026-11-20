//! What one bridge run mirrors and relays, derived from the catalog.
//!
//! The bridge creates and holds no section on its own: which sections exist because the application
//! made them (kept or dropped by its listeners) is decided by OpenRigCue, which runs
//! the listeners and their game detection, and sent over the control connection ([`crate::control`]).

use crate::catalog::{self, ExportSpec, ListenerSpec, PipeSpec};

/// A section to mirror into a Linux file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorTarget {
    pub name: String,
    /// View sizes to try, largest first. Several listeners can read the same name with different structure
    /// sizes (`$pcars2$`, `$rFactorShared$`); the largest view the section allows is mirrored.
    pub sizes: Vec<usize>,
    pub ready_event: Option<&'static str>,
}

impl MirrorTarget {
    /// A target for a single view size.
    pub fn sized(name: &str, size: usize) -> MirrorTarget {
        MirrorTarget { name: name.to_owned(), sizes: vec![size], ready_event: None }
    }

    /// Adds a view size, keeping the sizes largest first.
    pub fn add_size(&mut self, size: usize) {
        if !self.sizes.contains(&size) {
            self.sizes.push(size);
            self.sizes.sort_unstable_by(|a, b| b.cmp(a));
        }
    }
}

/// Everything one run does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub mirrors: Vec<MirrorTarget>,
    pub pipes: Vec<PipeSpec>,
    pub exports: Vec<ExportSpec>,
}

impl Plan {
    /// The plan of every run: every section any listener reads is watched (a listener reads whatever section
    /// exists), both always-served pipes are served and both exports published.
    pub fn watch_all() -> Plan {
        Plan {
            mirrors: merge_sections(catalog::LISTENERS),
            pipes: catalog::IMPORT_TIME_PIPES.to_vec(),
            exports: catalog::EXPORTS.to_vec(),
        }
    }
}

/// Names of every section any listener reads, in catalog order.
pub fn every_section_name() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for spec in catalog::LISTENERS.iter().flat_map(|l| l.sections) {
        if !names.contains(&spec.name) {
            names.push(spec.name);
        }
    }
    names
}

/// One target per distinct name, in catalog order, collecting every size it is read with.
fn merge_sections(listeners: &[ListenerSpec]) -> Vec<MirrorTarget> {
    let mut targets: Vec<MirrorTarget> = Vec::new();
    for spec in listeners.iter().flat_map(|l| l.sections) {
        match targets.iter_mut().find(|t| t.name == spec.name) {
            Some(target) => {
                target.add_size(spec.size);
                target.ready_event = target.ready_event.or(spec.ready_event);
            }
            None => targets.push(MirrorTarget {
                name: spec.name.to_owned(),
                sizes: vec![spec.size],
                ready_event: spec.ready_event,
            }),
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{DYNAMIC_CAB_PIPE, FM_TELEMETRY_PIPE};

    /// Shared names are mirrored once with every size, largest first; iRacing's section carries its event.
    #[test]
    fn watch_all_merges_shared_names() {
        let plan = Plan::watch_all();
        let pcars2 = plan.mirrors.iter().find(|m| m.name == "$pcars2$").unwrap();
        assert_eq!(pcars2.sizes, [11708, 11624]);
        let rf = plan.mirrors.iter().find(|m| m.name == "$rFactorShared$").unwrap();
        assert_eq!(rf.sizes, [39_676, 39_647]);
        assert_eq!(plan.mirrors.iter().filter(|m| m.name == "$pcars2$").count(), 1);
        assert_eq!(plan.mirrors.len(), every_section_name().len());
        let iracing = plan.mirrors.iter().find(|m| m.name == "Local\\IRSDKMemMapFileName").unwrap();
        assert_eq!(iracing.ready_event, Some("Local\\IRSDKDataValidEvent"));
    }

    /// Both pipes are served whatever the game.
    #[test]
    fn plan_serves_both_import_time_pipes_and_exports() {
        let plan = Plan::watch_all();
        assert_eq!(plan.pipes, [DYNAMIC_CAB_PIPE, FM_TELEMETRY_PIPE]);
        assert_eq!(plan.exports, catalog::EXPORTS);
    }

    #[test]
    fn added_sizes_stay_largest_first() {
        let mut target = MirrorTarget::sized("x", 10);
        target.add_size(30);
        target.add_size(10);
        target.add_size(20);
        assert_eq!(target.sizes, [30, 20, 10]);
    }
}
