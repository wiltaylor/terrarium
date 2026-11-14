//! The harness pins from `harnesses.toml` at the repo root, read at compile
//! time by `build.rs`. The first install into the tools dir uses the same
//! versions (`terrariumd harness install`), so the adapters and the
//! compatibility suite agree with a fresh install. An update in place may
//! move a harness past its pin; any version can start an agent. Change a
//! pin with `just harness::update <name> --pin`, never here.

use crate::harness::HarnessId;

/// One `[harness.<name>]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessPin {
    pub harness: HarnessId,
    /// The npm package.
    pub package: &'static str,
    /// The pinned version: the one of the first install.
    pub version: &'static str,
    /// The command on PATH.
    pub binary: &'static str,
}

/// One `[extension.<name>]` table: an npm package that is not a harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionPin {
    pub name: &'static str,
    pub package: &'static str,
    pub version: &'static str,
    pub binary: &'static str,
    /// The harness that uses it.
    pub harness: HarnessId,
}

include!(concat!(env!("OUT_DIR"), "/harnesses.rs"));

/// The pin of `harness`. Every harness has one (`build.rs` checks it).
pub fn pin(harness: HarnessId) -> &'static HarnessPin {
    PINS.iter()
        .find(|p| p.harness == harness)
        .unwrap_or(&PINS[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The consts are the values in the file, read here a second way.
    #[test]
    fn consts_equal_the_toml() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../harnesses.toml");
        let text = std::fs::read_to_string(path).expect("harnesses.toml");
        let v: toml::Table = toml::from_str(&text).expect("toml");
        assert_eq!(v["node"]["image"].as_str(), Some(NODE_IMAGE));
        let tables = v["harness"].as_table().expect("[harness]");
        assert_eq!(tables.len(), PINS.len());
        for p in PINS {
            let t = &tables[p.harness.as_str()];
            assert_eq!(t["package"].as_str(), Some(p.package), "{}", p.harness);
            assert_eq!(t["version"].as_str(), Some(p.version), "{}", p.harness);
            assert!(t.get("tested").is_none(), "{}: no ranges", p.harness);
            assert_eq!(t["binary"].as_str(), Some(p.binary), "{}", p.harness);
            assert_eq!(pin(p.harness), p);
        }
        assert_eq!(claude::VERSION, pin(HarnessId::Claude).version);
        assert_eq!(codex::PACKAGE, pin(HarnessId::Codex).package);
        assert_eq!(copilot::PACKAGE, pin(HarnessId::Copilot).package);
        assert_eq!(pi::BINARY, pin(HarnessId::Pi).binary);
        assert_eq!(grok::PACKAGE, pin(HarnessId::Grok).package);
        let ext = v["extension"].as_table().expect("[extension]");
        assert_eq!(ext.len(), EXTENSIONS.len());
        for e in EXTENSIONS {
            let t = &ext[e.name];
            assert_eq!(t["package"].as_str(), Some(e.package));
            assert_eq!(t["version"].as_str(), Some(e.version));
            assert_eq!(t["binary"].as_str(), Some(e.binary));
            assert_eq!(t["for"].as_str(), Some(e.harness.as_str()));
        }
    }

    #[test]
    fn every_pin_is_semver() {
        for p in PINS {
            semver::Version::parse(p.version).expect("version");
        }
        assert_eq!(PINS.len(), HarnessId::ALL.len());
    }
}
