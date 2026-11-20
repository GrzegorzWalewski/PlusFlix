//! Tooltip test via i-slint-backend-testing with mock time: the single
//! TooltipLayer must appear after the 500 ms hover delay, show the button's
//! tooltip text, and hide on press / pointer leave (IconButton semantics in
//! components.slint: `TooltipState.hide()` on press and on `!has-hover`).
//!
//! One `#[test]` only: `init_integration_test_with_mock_time` installs the
//! platform backend once per thread (see the crate docs).

use i_slint_backend_testing::{mock_elapsed_time, ElementQuery};
use i_slint_core::api::LogicalPosition;
use i_slint_core::items::PointerEventButton;
use i_slint_core::platform::WindowEvent;
use std::time::Duration;

slint::include_modules!();

/// Comfortably past the 500 ms hover delay in IconButton's Timer.
const HOVER_DELAY: Duration = Duration::from_millis(600);

fn dispatch(win: &TestWindow, event: WindowEvent) {
    use i_slint_core::window::WindowInner;
    WindowInner::from_pub(win.window())
        .window_adapter()
        .window()
        .dispatch_event(event);
}

/// Center of the `tip-btn` IconButton, in window coordinates.
fn tip_center(win: &TestWindow) -> LogicalPosition {
    let btn = ElementQuery::from_root(win)
        .match_id("TestWindow::tip-btn")
        .find_first()
        .expect("tip-btn");
    let pos = btn.absolute_position();
    let size = btn.size();
    LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0)
}

fn assert_tooltip(win: &TestWindow, visible: bool, text: &str) {
    let state = win.global::<TooltipState>();
    assert_eq!(state.get_visible(), visible, "tooltip visible");
    assert_eq!(state.get_text().as_str(), text, "tooltip text");
}

#[test]
fn tooltip_shows_after_hover_delay_and_hides_on_press_and_leave() {
    i_slint_backend_testing::init_integration_test_with_mock_time();
    let win = TestWindow::new().unwrap();
    let center = tip_center(&win);
    // The TestWindow harness tooltip is the literal "Tip" (the real window
    // passes translated t-* strings; the layer renders whatever is set).
    const TIP: &str = "Tip";

    // Hovering alone shows nothing until the delay elapses.
    dispatch(&win, WindowEvent::PointerMoved { position: center });
    assert_tooltip(&win, false, "");

    // After 500 ms of hovering the button fills TooltipState.
    mock_elapsed_time(HOVER_DELAY);
    assert_tooltip(&win, true, TIP);

    // Pressing hides the tooltip immediately (pointer-event down).
    dispatch(
        &win,
        WindowEvent::PointerPressed {
            position: center,
            button: PointerEventButton::Left,
        },
    );
    assert_tooltip(&win, false, TIP);

    // Release + another hover delay while still hovering: shown again.
    dispatch(
        &win,
        WindowEvent::PointerReleased {
            position: center,
            button: PointerEventButton::Left,
        },
    );
    mock_elapsed_time(HOVER_DELAY);
    assert_tooltip(&win, true, TIP);

    // Moving away flips has-hover and hides the tooltip.
    dispatch(
        &win,
        WindowEvent::PointerMoved {
            position: LogicalPosition::new(10.0, 10.0),
        },
    );
    assert_tooltip(&win, false, TIP);
}
