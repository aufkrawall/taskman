use std::time::Instant;

slint::include_modules!();

fn main() {
    let t0 = Instant::now();
    let ui = MainWindow::new().unwrap();
    eprintln!("INIT_MS={}", t0.elapsed().as_millis());

    let bench = std::env::args().any(|a| a == "--bench");
    if bench {
        let weak = ui.as_weak();
        // Fire shortly after the event loop starts; window will have been shown.
        let timer = slint::Timer::default();
        let t_start = t0;
        timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(150),
            move || {
                println!("PAINT_MS={}", t_start.elapsed().as_millis());
                use std::io::Write;
                std::io::stdout().flush().ok();
                if let Some(ui) = weak.upgrade() {
                    ui.hide().unwrap();
                }
                std::process::exit(0);
            },
        );
    }
    ui.run().unwrap();
}
