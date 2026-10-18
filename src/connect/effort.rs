//! Effort UI decisions over driver-advertised values; no provider defaults.

#[derive(Debug, PartialEq, Eq)]
pub enum Step<'a> {
    /// The current or pending value has no advertised rank: ask explicitly.
    Choose,
    Value(&'a str),
    /// Already at the requested boundary; do not enqueue a redundant change.
    Boundary,
}

/// Only rank orders choices. Advertised array order and unranked alternatives
/// do not imply a relative effort. Callers use a pending value before current.
pub fn step<'a>(
    choices: &[(&'a str, Option<u32>)],
    basis: Option<&str>,
    increase: bool,
) -> Step<'a> {
    let rank = basis
        .and_then(|basis| choices.iter().find(|(id, _)| *id == basis))
        .and_then(|(_, rank)| *rank);
    let Some(rank) = rank else {
        return Step::Choose;
    };
    choices
        .iter()
        .filter_map(|(id, candidate)| {
            let candidate = (*candidate)?;
            ((increase && candidate > rank) || (!increase && candidate < rank))
                .then_some((*id, candidate))
        })
        .min_by(|(a_id, a), (b_id, b)| {
            let order = if increase { a.cmp(b) } else { b.cmp(a) };
            order.then_with(|| a_id.cmp(b_id))
        })
        .map_or(Step::Boundary, |(id, _)| Step::Value(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_use_ranks_skip_unranked_options_and_clamp() {
        let choices = [
            ("high", Some(9)),
            ("default", None),
            ("low", Some(1)),
            ("unrecognized", None),
            ("medium", Some(4)),
        ];
        assert_eq!(step(&choices, Some("low"), true), Step::Value("medium"));
        assert_eq!(step(&choices, Some("medium"), true), Step::Value("high"));
        assert_eq!(step(&choices, Some("high"), false), Step::Value("medium"));
        assert_eq!(step(&choices, Some("medium"), false), Step::Value("low"));
        assert_eq!(step(&choices, Some("low"), false), Step::Boundary);
        assert_eq!(step(&choices, Some("high"), true), Step::Boundary);
    }

    #[test]
    fn unknown_default_and_missing_values_never_get_an_invented_rank() {
        let choices = [("high", Some(9)), ("default", None), ("custom", None)];
        for basis in [
            None,
            Some("default"),
            Some("custom"),
            Some("not-advertised"),
        ] {
            for increase in [false, true] {
                assert_eq!(step(&choices, basis, increase), Step::Choose);
            }
        }
        assert_eq!(step(&[], Some("low"), true), Step::Choose);
        assert_eq!(
            step(&[("minimal", Some(0))], Some("minimal"), false),
            Step::Boundary
        );
    }

    #[test]
    fn rank_ties_have_a_stable_choice_independent_of_report_array_order() {
        let mut choices = [
            ("medium", Some(1)),
            ("high-b", Some(2)),
            ("high-a", Some(2)),
        ];
        assert_eq!(step(&choices, Some("medium"), true), Step::Value("high-a"));
        choices.reverse();
        assert_eq!(step(&choices, Some("medium"), true), Step::Value("high-a"));
    }
}
