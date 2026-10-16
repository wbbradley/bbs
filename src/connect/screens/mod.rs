//! The app's screens.

pub mod agents;
pub mod groups;
pub mod help;
pub mod menu;
pub mod splash;

use ratatui::widgets::TableState;

use super::keys::Action;

/// Move a list's selection for a navigation action; `page` is the number of
/// rows in view. Returns whether the action was a navigation action.
pub fn navigate(state: &mut TableState, len: usize, page: usize, action: Action) -> bool {
    let last = len.saturating_sub(1);
    let at = state.selected().unwrap_or(0);
    let page = page.max(1);
    let to = match action {
        Action::Up => at.saturating_sub(1),
        Action::Down => at + 1,
        Action::Top => 0,
        Action::Bottom => last,
        Action::HalfPageUp => at.saturating_sub(page / 2),
        Action::HalfPageDown => at + page / 2,
        Action::PageUp => at.saturating_sub(page),
        Action::PageDown => at + page,
        _ => return false,
    };
    state.select((len > 0).then_some(to.min(last)));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_stays_in_the_list() {
        let mut state = TableState::default();
        assert!(navigate(&mut state, 10, 4, Action::Down));
        assert_eq!(state.selected(), Some(1));
        navigate(&mut state, 10, 4, Action::PageDown);
        assert_eq!(state.selected(), Some(5));
        navigate(&mut state, 10, 4, Action::HalfPageDown);
        assert_eq!(state.selected(), Some(7));
        navigate(&mut state, 10, 4, Action::PageDown);
        assert_eq!(state.selected(), Some(9));
        navigate(&mut state, 10, 4, Action::Top);
        assert_eq!(state.selected(), Some(0));
        navigate(&mut state, 10, 4, Action::Up);
        assert_eq!(state.selected(), Some(0));
        navigate(&mut state, 0, 4, Action::Down);
        assert_eq!(state.selected(), None);
        assert!(!navigate(&mut state, 10, 4, Action::Open));
    }
}
