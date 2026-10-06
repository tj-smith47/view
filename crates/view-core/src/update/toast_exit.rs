//! The toast stack's exit motion for a notice that just left.

use crate::model::{Model, Tier};
use crate::msg::Effect;
use crate::native::toast::ToastMotion;
use crate::native::views::Span;

/// Starts the stack's exit motion for a notice that just left, and asks for
/// the first frame's wakeup.
///
/// The gate is the tier and nothing else: below `Tier::Full` there is no
/// interpolation at all, so no motion is started, no tick is ever scheduled,
/// and the stack paints the state it is already in. The slot timers and the
/// pause key are untouched either way. Motion is presentation and timing is
/// behavior.
pub(super) fn start_toast_exit(
    model: &mut Model,
    departed: Option<(Vec<Vec<Span>>, usize)>,
) -> Vec<Effect> {
    let Some((lines, slot)) = departed else {
        return Vec::new();
    };
    if model.caps.tier != Tier::Full {
        return Vec::new();
    }
    // one wakeup chain at a time: a second dismissal landing while the
    // first is still playing replaces the motion and is driven by the chain
    // already running, because two chains ticking one clock advance it twice
    // per interval and the stack arrives in half the time it was given
    let already_ticking = model.toast_motion.is_some();
    let width = model.notice_column().rect.2;
    model.toast_motion = Some(ToastMotion::exit_right(lines, slot, width));
    model.dirty = true;
    if already_ticking {
        return Vec::new();
    }
    vec![Effect::ScheduleAnimTick {
        after: crate::native::toast::MOTION_STEP,
    }]
}
