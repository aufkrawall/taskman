// Minimal but representative benchmark app: labels, buttons, a table,
// and a painter-drawn line chart — the widget classes a task manager needs.
// egui 0.36 App trait: `ui(&mut self, ui: &mut Ui, frame)` is the render entry.
use eframe::egui;
use std::time::Instant;

struct BenchApp {
    start: Instant,
    reported: bool,
    bench: bool,
    samples: Vec<f32>,
}

impl BenchApp {
    fn new() -> Self {
        let args: Vec<String> = std::env::args().collect();
        Self {
            start: Instant::now(),
            reported: false,
            bench: args.iter().any(|a| a == "--bench"),
            samples: (0..120).map(|i| ((i as f32) * 0.31).sin().abs() * 100.0).collect(),
        }
    }
}

impl eframe::App for BenchApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let bg = if ui.visuals().dark_mode {
            egui::Color32::from_rgb(32, 32, 32)
        } else {
            egui::Color32::from_rgb(243, 243, 243)
        };
        egui::Frame::NONE.fill(bg).show(ui, |ui| {
            ui.heading("Bench");
            ui.label("Hello from egui");
            if ui.button("Button").clicked() {}
            egui::Grid::new("g").show(ui, |ui| {
                for i in 0..10 {
                    ui.label(format!("Row {i}"));
                    ui.label(format!("{}%", i * 7));
                    ui.end_row();
                }
            });
            let size = ui.available_size();
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            let pts: Vec<egui::Pos2> = self
                .samples
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    egui::pos2(
                        rect.left() + rect.width() * i as f32 / 119.0,
                        rect.bottom() - rect.height() * v / 100.0,
                    )
                })
                .collect();
            ui.painter()
                .add(egui::Shape::line(pts, egui::Stroke::new(1.5, egui::Color32::LIGHT_BLUE)));
        });

        if !self.reported {
            // First ui() call happens during the first rendered frame.
            println!("PAINT_MS={}", self.start.elapsed().as_millis());
            use std::io::Write;
            std::io::stdout().flush().ok();
            self.reported = true;
            if self.bench {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let t0 = Instant::now();
    let renderer = if cfg!(feature = "__glow_marker") { unreachable!() } else {
        eframe::Renderer::Glow
    };
    let options = eframe::NativeOptions {
        renderer,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 480.0])
            .with_title("BenchEguiBoth"),
        ..Default::default()
    };
    eprintln!("INIT_MS={}", t0.elapsed().as_millis());
    eframe::run_native("BenchEguiBoth", options, Box::new(|_cc| Ok(Box::new(BenchApp::new()))))
}
