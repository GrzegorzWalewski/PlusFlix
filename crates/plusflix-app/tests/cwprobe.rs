use i_slint_backend_testing::ElementQuery;

slint::include_modules!();

#[test]
fn probe_cw() {
    i_slint_backend_testing::init_no_event_loop();
    let win = TestWindow::new().unwrap();
    let card = ElementQuery::from_root(&win)
        .match_id("TestWindow::cw-test")
        .find_first()
        .expect("cw card");
    let cs = card.size();
    let cp = card.absolute_position();
    eprintln!("card at ({},{}) {}x{}", cp.x, cp.y, cs.width, cs.height);
    for id in ["CwCard::grad", "CwCard::pbar"] {
        match ElementQuery::from_root(&win).match_id(id).find_first() {
            Some(el) => {
                let p = el.absolute_position();
                let s = el.size();
                eprintln!("{id} at ({},{}) {}x{}", p.x, p.y, s.width, s.height);
            }
            None => eprintln!("{id} NOT FOUND"),
        }
    }
}

#[test]
fn probe_search_click() {
    i_slint_backend_testing::init_no_event_loop();
    let win = TestWindow::new().unwrap();
    let f = ElementQuery::from_root(&win)
        .match_id("TestWindow::search-test")
        .find_first()
        .expect("search field");
    eprintln!("before: {}x{}", f.size().width, f.size().height);
    f.mock_single_click(i_slint_core::items::PointerEventButton::Left);
    eprintln!("after click: {}x{}", f.size().width, f.size().height);
    // click the inner TextInput area too (middle of the field)
    let p = f.absolute_position();
    eprintln!("pos ({}, {})", p.x, p.y);
}
