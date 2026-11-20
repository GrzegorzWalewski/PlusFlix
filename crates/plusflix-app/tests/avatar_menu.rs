//! Regression test for the avatar-popup menu: opening the popup from the top
//! bar and clicking a menu row must fire the `switch-profile` / `logout`
//! callbacks on the MainWindow. Previously the handler called
//! `avatar-menu.close()` *before* the callbacks, which tore the popup's item
//! tree down mid-handler and silently dropped the callback invocation.

use i_slint_backend_testing::ElementQuery;
use i_slint_core::items::PointerEventButton;
use std::cell::Cell;
use std::rc::Rc;

slint::include_modules!();

/// Clicks the `avatar-open` probe (kept in ui/main.slint for this test) and
/// returns the popup item TouchAreas (`ma`).
fn open_popup(win: &MainWindow) -> Vec<i_slint_backend_testing::ElementHandle> {
    let open = ElementQuery::from_root(win)
        .match_id("MainWindow::avatar-open")
        .find_first()
        .expect("missing MainWindow::avatar-open");
    open.mock_single_click(PointerEventButton::Left);
    let tas = ElementQuery::from_root(win)
        .match_id("MainWindow::ma")
        .find_all();
    assert!(!tas.is_empty(), "avatar popup has no item TouchAreas");
    tas
}

#[test]
fn switch_profile_opens_profiles_screen() {
    i_slint_backend_testing::init_no_event_loop();
    let win = MainWindow::new().unwrap();
    let sw = Rc::new(Cell::new(0usize));
    let lo = Rc::new(Cell::new(0usize));
    win.on_switch_profile({
        let sw = sw.clone();
        move || sw.set(sw.get() + 1)
    });
    win.on_logout({
        let lo = lo.clone();
        move || lo.set(lo.get() + 1)
    });
    win.set_screen("main".into());

    // "Switch profile" is the first menu row.
    let tas = open_popup(&win);
    tas[0].mock_single_click(PointerEventButton::Left);
    assert_eq!(sw.get(), 1, "Switch profile row must fire switch-profile");
    assert_eq!(lo.get(), 0, "Switch profile row must not fire logout");

    // "Log out" is the second (last) row.
    let tas = open_popup(&win);
    let last = tas.len() - 1;
    tas[last].mock_single_click(PointerEventButton::Left);
    assert_eq!(lo.get(), 1, "Log out row must fire logout");
    assert_eq!(sw.get(), 1, "Log out row must not fire switch-profile");
}
