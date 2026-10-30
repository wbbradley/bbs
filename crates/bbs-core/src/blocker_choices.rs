//! Structured blocker response options (blocker-choices.v1). The headless
//! harness validates with these before its RPC and the server validates again,
//! so both refuse the same requests with the same reasons.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::proto::{self, BlockerChoiceMode};

pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 8;
pub const MAX_ID: usize = 32;
pub const MAX_LABEL: usize = 120;
pub const MAX_DETAIL: usize = 500;

/// The stored form; the column is written only from validated choices.
#[derive(Serialize, Deserialize)]
struct Stored {
    mode: String,
    options: Vec<StoredOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max: Option<u32>,
}

#[derive(Serialize, Deserialize)]
struct StoredOption {
    id: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

fn valid_id(id: &str) -> bool {
    (1..=MAX_ID).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Checks `choices` and returns its canonical form: SELECT_MANY bounds are
/// filled in with their defaults.
pub fn validate(choices: &proto::BlockerChoices) -> Result<proto::BlockerChoices> {
    let mode = BlockerChoiceMode::try_from(choices.mode).ok();
    let n = choices.options.len();
    if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&n) {
        bail!("blocker choices need {MIN_OPTIONS} to {MAX_OPTIONS} options");
    }
    for (i, option) in choices.options.iter().enumerate() {
        if !valid_id(&option.id) {
            bail!(
                "blocker option ID {:?} must be 1 to {MAX_ID} characters of a-z, 0-9, _ or -",
                option.id
            );
        }
        if choices.options[..i].iter().any(|o| o.id == option.id) {
            bail!("blocker option ID {:?} is repeated", option.id);
        }
        if option.label.trim().is_empty()
            || option.label.len() > MAX_LABEL
            || option.label.chars().any(char::is_control)
        {
            bail!(
                "blocker option {:?} label must be one nonblank line of at most {MAX_LABEL} UTF-8 bytes",
                option.id
            );
        }
        if option.detail.as_ref().is_some_and(|d| d.len() > MAX_DETAIL) {
            bail!(
                "blocker option {:?} detail must be at most {MAX_DETAIL} UTF-8 bytes",
                option.id
            );
        }
    }
    let (min, max) = match mode {
        Some(BlockerChoiceMode::BlockerSelectOne) => {
            if choices.min.is_some() || choices.max.is_some() {
                bail!("select_one blocker choices take no min or max");
            }
            (None, None)
        }
        Some(BlockerChoiceMode::BlockerSelectMany) => {
            let min = choices.min.unwrap_or(1);
            let max = choices.max.unwrap_or(n as u32);
            if min < 1 || min > max || max as usize > n {
                bail!("select_many blocker choices need 1 <= min <= max <= {n} options");
            }
            (Some(min), Some(max))
        }
        _ => bail!("blocker choices mode must be select_one or select_many"),
    };
    Ok(proto::BlockerChoices {
        mode: choices.mode,
        options: choices.options.clone(),
        min,
        max,
    })
}

/// The canonical JSON stored for validated choices.
pub fn to_json(choices: &proto::BlockerChoices) -> String {
    let mode = match choices.mode() {
        BlockerChoiceMode::BlockerSelectMany => "select_many",
        _ => "select_one",
    };
    serde_json::to_string(&Stored {
        mode: mode.into(),
        options: choices
            .options
            .iter()
            .map(|o| StoredOption {
                id: o.id.clone(),
                label: o.label.clone(),
                detail: o.detail.clone(),
            })
            .collect(),
        min: choices.min,
        max: choices.max,
    })
    .expect("choices serialize")
}

pub fn from_json(json: &str) -> Result<proto::BlockerChoices> {
    let stored: Stored = serde_json::from_str(json)?;
    let mode = match stored.mode.as_str() {
        "select_one" => BlockerChoiceMode::BlockerSelectOne,
        "select_many" => BlockerChoiceMode::BlockerSelectMany,
        other => bail!("unknown stored blocker choice mode {other:?}"),
    };
    Ok(proto::BlockerChoices {
        mode: mode.into(),
        options: stored
            .options
            .into_iter()
            .map(|o| proto::BlockerOption {
                id: o.id,
                label: o.label,
                detail: o.detail,
            })
            .collect(),
        min: stored.min,
        max: stored.max,
    })
}

/// A checked answer: the selection in declared option order, and the text
/// stored and mailed for it.
#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    pub selected: Vec<String>,
    pub text: String,
}

impl Answer {
    /// The canonical JSON stored for the selection; none for free text.
    pub fn selected_json(&self) -> Option<String> {
        (!self.selected.is_empty())
            .then(|| serde_json::to_string(&self.selected).expect("IDs serialize"))
    }
}

/// Checks `selected` against the version's `choices`. An empty selection is
/// a free-text answer and needs a nonblank `note`; a selection is composed as
/// one `Selected: <label> [<id>]` line per option, then any nonblank note.
pub fn answer(
    choices: Option<&proto::BlockerChoices>,
    selected: &[String],
    note: &str,
) -> Result<Answer> {
    if selected.is_empty() {
        if note.trim().is_empty() {
            bail!("a blocker answer needs a selection or a nonblank note");
        }
        return Ok(Answer {
            selected: Vec::new(),
            text: note.into(),
        });
    }
    let Some(choices) = choices else {
        bail!("this blocker version has no choices to select; answer with text");
    };
    for (i, id) in selected.iter().enumerate() {
        if selected[..i].contains(id) {
            bail!("blocker option {id:?} is selected more than once");
        }
        if !choices.options.iter().any(|o| &o.id == id) {
            bail!(
                "blocker option {id:?} is not a choice on this version; refresh before answering"
            );
        }
    }
    let n = selected.len() as u32;
    match choices.mode() {
        BlockerChoiceMode::BlockerSelectMany => {
            let (min, max) = (choices.min.unwrap_or(1), choices.max.unwrap_or(u32::MAX));
            if n < min || n > max {
                bail!("select {min} to {max} blocker options, or answer with only a note");
            }
        }
        _ if n != 1 => bail!("select exactly one blocker option, or answer with only a note"),
        _ => {}
    }
    let picked: Vec<_> = choices
        .options
        .iter()
        .filter(|o| selected.contains(&o.id))
        .collect();
    let mut text = picked
        .iter()
        .map(|o| format!("Selected: {} [{}]", o.label, o.id))
        .collect::<Vec<_>>()
        .join("\n");
    if !note.trim().is_empty() {
        text.push_str("\n\n");
        text.push_str(note);
    }
    Ok(Answer {
        selected: picked.iter().map(|o| o.id.clone()).collect(),
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: &str, label: &str) -> proto::BlockerOption {
        proto::BlockerOption {
            id: id.into(),
            label: label.into(),
            detail: None,
        }
    }

    fn choices(mode: BlockerChoiceMode, n: usize) -> proto::BlockerChoices {
        proto::BlockerChoices {
            mode: mode.into(),
            options: (0..n)
                .map(|i| option(&format!("o{i}"), &format!("Option {i}")))
                .collect(),
            min: None,
            max: None,
        }
    }

    fn refused(c: proto::BlockerChoices, needle: &str) {
        let error = validate(&c).unwrap_err().to_string();
        assert!(error.contains(needle), "{error:?} lacks {needle:?}");
    }

    #[test]
    fn choices_are_bounded_and_canonical() {
        use BlockerChoiceMode::*;
        let many = validate(&choices(BlockerSelectMany, 3)).unwrap();
        assert_eq!((many.min, many.max), (Some(1), Some(3)));
        let one = validate(&choices(BlockerSelectOne, 8)).unwrap();
        assert_eq!((one.min, one.max), (None, None));
        assert_eq!(from_json(&to_json(&many)).unwrap(), many);

        refused(choices(BlockerSelectOne, 1), "2 to 8 options");
        refused(choices(BlockerSelectOne, 9), "2 to 8 options");
        refused(choices(Unspecified, 2), "mode");
        refused(
            proto::BlockerChoices {
                mode: 7,
                ..choices(BlockerSelectOne, 2)
            },
            "mode",
        );
        refused(
            proto::BlockerChoices {
                max: Some(1),
                ..choices(BlockerSelectOne, 2)
            },
            "no min or max",
        );
        for (min, max) in [(Some(0), None), (Some(3), Some(2)), (None, Some(4))] {
            refused(
                proto::BlockerChoices {
                    min,
                    max,
                    ..choices(BlockerSelectMany, 3)
                },
                "1 <= min <= max <= 3",
            );
        }
        let mut c = choices(BlockerSelectOne, 2);
        c.options[1].id = "o0".into();
        refused(c, "repeated");
        for id in ["", "Upper", "has space", &"x".repeat(33)] {
            let mut c = choices(BlockerSelectOne, 2);
            c.options[0].id = id.into();
            refused(c, "a-z, 0-9");
        }
        for label in [" ", "two\nlines", &"x".repeat(121)] {
            let mut c = choices(BlockerSelectOne, 2);
            c.options[0].label = label.into();
            refused(c, "one nonblank line");
        }
        let mut c = choices(BlockerSelectOne, 2);
        c.options[0].detail = Some("x".repeat(501));
        refused(c, "detail");
    }

    #[test]
    fn answers_are_checked_and_composed_in_declared_order() {
        use BlockerChoiceMode::*;
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let many = proto::BlockerChoices {
            min: Some(1),
            max: Some(2),
            ..choices(BlockerSelectMany, 3)
        };
        let picked = answer(Some(&many), &ids(&["o2", "o0"]), "ship it").unwrap();
        assert_eq!(picked.selected, ids(&["o0", "o2"]));
        assert_eq!(
            picked.text,
            "Selected: Option 0 [o0]\nSelected: Option 2 [o2]\n\nship it"
        );
        assert_eq!(picked.selected_json().as_deref(), Some(r#"["o0","o2"]"#));
        assert_eq!(
            answer(Some(&many), &ids(&["o1"]), "  ").unwrap().text,
            "Selected: Option 1 [o1]"
        );
        let free = answer(Some(&many), &[], "none of these").unwrap();
        assert_eq!(
            (free.selected_json(), free.text.as_str()),
            (None, "none of these")
        );
        assert_eq!(answer(None, &[], "text").unwrap().text, "text");

        let one = validate(&choices(BlockerSelectOne, 3)).unwrap();
        for (choices, selected, needle) in [
            (Some(&many), ids(&[]), "nonblank note"),
            (Some(&many), ids(&["o0", "o1", "o2"]), "select 1 to 2"),
            (Some(&many), ids(&["o0", "o0"]), "more than once"),
            (Some(&many), ids(&["nope"]), "not a choice"),
            (Some(&one), ids(&["o0", "o1"]), "exactly one"),
            (None, ids(&["o0"]), "no choices"),
        ] {
            let error = answer(choices, &selected, "").unwrap_err().to_string();
            assert!(error.contains(needle), "{error:?} lacks {needle:?}");
        }
    }
}
