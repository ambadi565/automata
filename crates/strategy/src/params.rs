use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Named numeric parameters for one strategy, e.g. `{"atr_stop": 1.5}`.
///
/// A flat map keeps the walk-forward optimizer generic: it searches a grid of
/// these without knowing what the names mean.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParamSet(pub BTreeMap<String, f64>);

impl ParamSet {
    pub fn get(&self, name: &str) -> Option<f64> {
        self.0.get(name).copied()
    }

    pub fn set(&mut self, name: &str, v: f64) {
        self.0.insert(name.to_string(), v);
    }

    /// `self` with every missing key taken from `defaults`.
    pub fn with_defaults(&self, defaults: &ParamSet) -> ParamSet {
        let mut out = defaults.clone();
        out.0.extend(self.0.iter().map(|(k, v)| (k.clone(), *v)));
        out
    }
}

impl fmt::Display for ParamSet {
    /// `a=1 b=2.5`, stable order.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (k, v) in &self.0 {
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            write!(f, "{k}={v}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum ParamError {
    #[error("unknown strategy {0:?}")]
    UnknownStrategy(String),
    #[error("{strategy}: unknown parameter {name:?}")]
    UnknownParam { strategy: &'static str, name: String },
    #[error("{strategy}: {name}={value} is outside [{min}, {max}]")]
    OutOfRange {
        strategy: &'static str,
        name: String,
        value: f64,
        min: f64,
        max: f64,
    },
}

/// One tunable parameter: its default, hard bounds and the values the
/// optimizer tries.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamSpec {
    pub name: &'static str,
    pub default: f64,
    pub min: f64,
    pub max: f64,
    /// Sorted ascending. Neighbours for the stability check are adjacent
    /// entries.
    pub grid: Vec<f64>,
}

impl ParamSpec {
    pub fn new(name: &'static str, default: f64, min: f64, max: f64, grid: &[f64]) -> Self {
        let mut grid = grid.to_vec();
        grid.sort_by(f64::total_cmp);
        ParamSpec {
            name,
            default,
            min,
            max,
            grid,
        }
    }
}

/// Checks `p` against `specs` and fills defaults.
pub(crate) fn resolve(strategy: &'static str, specs: &[ParamSpec], p: &ParamSet) -> Result<ParamSet, ParamError> {
    let mut out = ParamSet::default();
    for s in specs {
        out.set(s.name, s.default);
    }
    for (k, &v) in &p.0 {
        let Some(s) = specs.iter().find(|s| s.name == k) else {
            return Err(ParamError::UnknownParam {
                strategy,
                name: k.clone(),
            });
        };
        if !(s.min..=s.max).contains(&v) {
            return Err(ParamError::OutOfRange {
                strategy,
                name: k.clone(),
                value: v,
                min: s.min,
                max: s.max,
            });
        }
        out.set(s.name, v);
    }
    Ok(out)
}

/// The cartesian product of each parameter's grid.
#[derive(Debug, Clone)]
pub struct ParamGrid {
    specs: Vec<ParamSpec>,
}

impl ParamGrid {
    pub fn new(specs: Vec<ParamSpec>) -> Self {
        ParamGrid { specs }
    }

    pub fn specs(&self) -> &[ParamSpec] {
        &self.specs
    }

    pub fn len(&self) -> usize {
        self.specs.iter().map(|s| s.grid.len().max(1)).product()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every combination, in a fixed order.
    pub fn combos(&self) -> Vec<ParamSet> {
        let mut out = vec![ParamSet::default()];
        for s in &self.specs {
            let values: &[f64] = if s.grid.is_empty() {
                std::slice::from_ref(&s.default)
            } else {
                &s.grid
            };
            out = out
                .into_iter()
                .flat_map(|p| {
                    values.iter().map(move |&v| {
                        let mut q = p.clone();
                        q.set(s.name, v);
                        q
                    })
                })
                .collect();
        }
        out
    }

    /// Grid points one step away from `p` along a single axis.
    pub fn neighbours(&self, p: &ParamSet) -> Vec<ParamSet> {
        let mut out = Vec::new();
        for s in &self.specs {
            let Some(v) = p.get(s.name) else { continue };
            let Some(i) = s.grid.iter().position(|g| g.total_cmp(&v).is_eq()) else {
                continue;
            };
            for j in [i.checked_sub(1), Some(i + 1)].into_iter().flatten() {
                if let Some(&nv) = s.grid.get(j) {
                    let mut q = p.clone();
                    q.set(s.name, nv);
                    out.push(q);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> ParamGrid {
        ParamGrid::new(vec![
            ParamSpec::new("a", 1.0, 0.0, 10.0, &[1.0, 2.0, 3.0]),
            ParamSpec::new("b", 0.5, 0.0, 1.0, &[0.5, 0.25]),
        ])
    }

    #[test]
    fn combos_are_the_cartesian_product() {
        let g = grid();
        assert_eq!(g.len(), 6);
        let c = g.combos();
        assert_eq!(c.len(), 6);
        assert_eq!(c[0].to_string(), "a=1 b=0.25");
    }

    #[test]
    fn neighbours_step_one_axis() {
        let g = grid();
        let mut p = ParamSet::default();
        p.set("a", 2.0);
        p.set("b", 0.5);
        let n: Vec<String> = g.neighbours(&p).iter().map(ToString::to_string).collect();
        assert_eq!(n, ["a=1 b=0.5", "a=3 b=0.5", "a=2 b=0.25"]);
    }

    #[test]
    fn resolve_checks_names_and_bounds() {
        let specs = grid().specs().to_vec();
        let mut p = ParamSet::default();
        p.set("a", 5.0);
        let r = resolve("t", &specs, &p).unwrap();
        assert_eq!(r.get("b"), Some(0.5));
        p.set("a", 11.0);
        assert!(matches!(resolve("t", &specs, &p), Err(ParamError::OutOfRange { .. })));
        let mut q = ParamSet::default();
        q.set("zzz", 1.0);
        assert!(matches!(resolve("t", &specs, &q), Err(ParamError::UnknownParam { .. })));
    }
}
