//! Build strategies by name.

use crate::params::{ParamError, ParamGrid, ParamSet, ParamSpec};
use crate::{Momentum, Orb, Strategy, VwapRevert};

/// Every strategy id, in report order.
pub const IDS: [&str; 3] = [Orb::ID, VwapRevert::ID, Momentum::ID];

/// The parameters a strategy accepts, with defaults and optimizer grids.
pub fn specs(id: &str) -> Result<Vec<ParamSpec>, ParamError> {
    match id {
        Orb::ID => Ok(Orb::specs()),
        VwapRevert::ID => Ok(VwapRevert::specs()),
        Momentum::ID => Ok(Momentum::specs()),
        _ => Err(ParamError::UnknownStrategy(id.to_string())),
    }
}

pub fn defaults(id: &str) -> Result<ParamSet, ParamError> {
    let mut p = ParamSet::default();
    for s in specs(id)? {
        p.set(s.name, s.default);
    }
    Ok(p)
}

pub fn grid(id: &str) -> Result<ParamGrid, ParamError> {
    Ok(ParamGrid::new(specs(id)?))
}

/// A fresh instance for one symbol. Missing parameters take defaults;
/// unknown or out-of-range ones are an error.
pub fn build(id: &str, p: &ParamSet) -> Result<Box<dyn Strategy>, ParamError> {
    Ok(match id {
        Orb::ID => Box::new(Orb::new(p)?),
        VwapRevert::ID => Box::new(VwapRevert::new(p)?),
        Momentum::ID => Box::new(Momentum::new(p)?),
        _ => return Err(ParamError::UnknownStrategy(id.to_string())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_strategy_builds_with_defaults_and_every_grid_point() {
        for id in IDS {
            let s = build(id, &ParamSet::default()).unwrap();
            assert_eq!(s.id(), id);
            let g = grid(id).unwrap();
            assert!(g.len() >= 9, "{id} grid too small to judge stability");
            for p in g.combos() {
                build(id, &p).unwrap();
            }
            for spec in specs(id).unwrap() {
                assert!((spec.min..=spec.max).contains(&spec.default), "{id}.{}", spec.name);
            }
        }
        assert!(matches!(
            build("nope", &ParamSet::default()),
            Err(ParamError::UnknownStrategy(_))
        ));
    }
}
