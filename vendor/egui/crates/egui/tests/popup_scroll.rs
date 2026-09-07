use std::cell::Cell;

use egui::{Align, CentralPanel, Context, Frame, Popup, RawInput, Rect, ScrollArea, Sense, pos2, vec2};

/// A popup is painted on a separate layer. Scroll requests from that layer must
/// not be consumed by a ScrollArea whose content closure happens to be building
/// the popup, or opening a row context menu moves the list underneath it.
#[test]
fn popup_scroll_target_does_not_escape_to_parent_scroll_area() {
    let ctx = Context::default();
    let outer_offset = Cell::new(0.0_f32);
    let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0));

    let mut output = ctx.run_ui(
        RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        },
        |root| {
            CentralPanel::default()
                .frame(Frame::NONE)
                .show(root, |ui| {
                    let out = ScrollArea::vertical()
                        .id_salt("outer")
                        .max_height(100.0)
                        .vertical_scroll_offset(120.0)
                        .animated(false)
                        .show(ui, |ui| {
                            ui.set_min_height(1_000.0);
                            let (_, owner) =
                                ui.allocate_exact_size(vec2(100.0, 20.0), Sense::hover());

                            Popup::from_response(&owner)
                                .open(true)
                                .at_position(pos2(20.0, 180.0))
                                .show(|ui| {
                                    let (rect, _) = ui
                                        .allocate_exact_size(vec2(120.0, 20.0), Sense::hover());
                                    ui.scroll_to_rect(rect, Some(Align::Center));
                                });
                        });
                    outer_offset.set(out.state.offset.y);
                });
        },
    );
    output.textures_delta.clear();

    assert_eq!(
        outer_offset.get(),
        120.0,
        "popup-local scroll targets must not move the parent ScrollArea"
    );
}
