//! Table building blocks: heat cells, sort arrows, column metadata.

use eframe::egui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcColumn {
    Name,
    Status,
    User,
    Cpu,
    Memory,
    Disk,
    Network,
    Gpu,
}

impl ProcColumn {
    pub const ALL: &'static [ProcColumn] = &[
        ProcColumn::Name,
        ProcColumn::Status,
        ProcColumn::User,
        ProcColumn::Cpu,
        ProcColumn::Memory,
        ProcColumn::Disk,
        ProcColumn::Network,
        ProcColumn::Gpu,
    ];

    pub fn header(self) -> &'static str {
        match self {
            ProcColumn::Name => "Name",
            ProcColumn::Status => "Status",
            ProcColumn::User => "Benutzer",
            ProcColumn::Cpu => "CPU",
            ProcColumn::Memory => "Arbeitsspeicher",
            ProcColumn::Disk => "Datenträger",
            ProcColumn::Network => "Netzwerk",
            ProcColumn::Gpu => "GPU",
        }
    }

    pub fn width(self) -> f32 {
        match self {
            ProcColumn::Name => 240.0,
            ProcColumn::Status | ProcColumn::User => 90.0,
            _ => 110.0,
        }
    }

    pub fn is_heat(self) -> bool {
        matches!(
            self,
            ProcColumn::Cpu
                | ProcColumn::Memory
                | ProcColumn::Disk
                | ProcColumn::Network
                | ProcColumn::Gpu
        )
    }
}

/// Right-aligned cell with optional heat-mapped background.
pub fn heat_cell_r(ui: &mut egui::Ui, pal: &crate::theme::Palette, intensity: f32, text: String) {
    let h = ui.available_height().clamp(18.0, 24.0);
    let w = ui.available_width().max(40.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    let intensity = intensity.clamp(0.0, 1.0);
    if intensity > 0.03 {
        let bg = crate::theme::heat_color(pal, intensity);
        ui.painter()
            .rect_filled(rect.shrink2(egui::vec2(1.5, 2.0)), 3.0, bg);
    }
    if !text.is_empty() {
        ui.painter().text(
            rect.right_center() + egui::vec2(-6.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            text,
            egui::FontId::proportional(12.0),
            pal.text,
        );
    }
}

/// Sort indicator triangle for active column headers.
pub fn sort_arrow(ui: &mut egui::Ui, ascending: bool, active: bool) {
    if !active {
        return;
    }
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(10.0, ui.available_height()),
        egui::Sense::hover(),
    );
    let c = rect.center();
    let color = ui.style().visuals.widgets.active.fg_stroke.color;
    let (a, b, cc) = if ascending {
        (
            egui::pos2(c.x - 4.0, c.y + 2.5),
            egui::pos2(c.x + 4.0, c.y + 2.5),
            egui::pos2(c.x, c.y - 3.5),
        )
    } else {
        (
            egui::pos2(c.x - 4.0, c.y - 2.5),
            egui::pos2(c.x + 4.0, c.y - 2.5),
            egui::pos2(c.x, c.y + 3.5),
        )
    };
    ui.painter().add(egui::Shape::convex_polygon(
        vec![a, b, cc],
        color,
        egui::Stroke::NONE,
    ));
}
