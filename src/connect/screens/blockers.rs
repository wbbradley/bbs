//! The blockers page. The queue itself is the app's, as it polls on every
//! screen; the app draws it here and gives it this page's keys.

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::connect::app::{Ctx, Screen, Transition};
use crate::connect::keys::Action;

pub struct BlockersPage;

impl Screen for BlockersPage {
    fn title(&self) -> String {
        "Blockers".into()
    }

    fn is_blockers(&self) -> bool {
        true
    }

    fn navigation_blocked(&self) -> bool {
        true
    }

    fn handle(&mut self, _action: Action, _ctx: &Ctx) -> Transition {
        Transition::Stay
    }

    fn render(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &Ctx) {}
}
