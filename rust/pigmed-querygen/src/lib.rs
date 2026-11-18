//! PigMed query generator, config-driven (querygen/querygen.toml). Recipe v3.1d text arm (persona-first intent
//! cards, then persona surface queries, then typing noise and filters) plus the v3.2 figure arm. Spec:
//! docs/QUERYGEN_SPEC.md.

pub mod client;
pub mod config;
pub mod figure;
pub mod judge;
pub mod mix;
pub mod mock;
pub mod persona;
pub mod report;
pub mod rules;
pub mod text;
pub mod util;

use anyhow::Result;
use config::Config;
use persona::Persona;
use rules::Rules;
use std::path::Path;

/// Everything the generator needs: config, compiled rules, the persona library and its cards.
pub struct Qg {
    pub cfg: Config,
    pub rules: Rules,
    pub lib: Vec<Persona>,
    pub cards: Vec<String>,
}

impl Qg {
    pub fn load(path: &Path) -> Result<Qg> {
        Qg::new(Config::load(path)?)
    }

    pub fn new(cfg: Config) -> Result<Qg> {
        let rules = Rules::new(&cfg)?;
        let lib = persona::library(&cfg.personas);
        let cards = lib.iter().map(|p| persona::card(&cfg.personas, p)).collect();
        Ok(Qg { cfg, rules, lib, cards })
    }

    /// Candidate reader from stratum `groups`: the first library persona at or after `start` in one of the groups.
    pub fn candidate(&self, start: usize, groups: &[String]) -> usize {
        let n = self.lib.len();
        (0..n).map(|j| (start + j) % n).find(|&i| groups.iter().any(|g| *g == self.lib[i].group)).unwrap_or(start)
    }

    /// Field expert when no candidate fits: a non-lay persona with professional expertise.
    pub fn fallback_persona(&self, seed: usize) -> usize {
        let n = self.lib.len();
        (0..n).map(|k| (seed + k) % n).find(|&i| self.lib[i].group != "lay" && self.lib[i].expertise >= 2).unwrap_or(seed % n)
    }
}
