//! Command line of `openrigcue-bridge.exe`.

use clap::Parser;

/// Mirrors the prefix's named shared memory into /dev/shm/openrigcue and relays the named pipes to openrigcue.
/// Runs inside the game's Wine/Proton prefix.
#[derive(Debug, Parser, PartialEq, Eq)]
#[command(name = "openrigcue-bridge", version)]
pub struct Options {
    /// Program to start, normally the game; the bridge exits when it and everything it started have exited.
    /// Without a program the bridge runs until interrupted.
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_after_double_dash() {
        let options = Options::try_parse_from(["openrigcue-bridge", "--", "Z:\\games\\acc.exe", "-x"]).unwrap();
        assert_eq!(options.command, ["Z:\\games\\acc.exe", "-x"]);
        assert!(Options::try_parse_from(["openrigcue-bridge"]).unwrap().command.is_empty());
    }

    /// The bridge has no options of its own: what the listeners read does not depend on any setting of it.
    #[test]
    fn no_bridge_options() {
        for removed in ["--game", "--rate-hz", "--no-pipes", "--pipe-tcp-port"] {
            assert!(Options::try_parse_from(["openrigcue-bridge", removed, "1", "--", "g.exe"]).is_err(), "{removed}");
        }
    }
}
