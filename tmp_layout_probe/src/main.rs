use std::cell::RefCell;
use std::rc::Rc;
use taffy::prelude::*;

fn main() {
    let mut taffy: TaffyTree<Rc<RefCell<Vec<String>>>> = TaffyTree::new();
    let log = Rc::new(RefCell::new(Vec::new()));
    let text = taffy.new_leaf_with_context(
        Style::default(),
        log.clone(),
    ).unwrap();
    let content = taffy.new_with_children(Style { flex_grow: 0.0, flex_shrink: 0.0, ..Default::default() }, &[text]).unwrap();
    let line = taffy.new_with_children(Style {
        display: Display::Flex,
        flex_direction: FlexDirection::Row,
        size: Size { width: Dimension::length(500.0), height: Dimension::length(30.0) },
        ..Default::default()
    }, &[content]).unwrap();
    taffy.compute_layout_with_measure(line, Size::MAX_CONTENT, |known, avail, _id, ctx, _style| {
        ctx.unwrap().borrow_mut().push(format!("known={known:?} avail={avail:?}"));
        Size { width: 4000.0, height: 18.0 }
    }).unwrap();
    println!("no width: {:?}", log.borrow());
    println!("line {:?} content {:?} text {:?}", taffy.layout(line).unwrap(), taffy.layout(content).unwrap(), taffy.layout(text).unwrap());
}
