use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
enum Message {}

struct State {
    start: Instant,
}

static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn update(_state: &mut State, _msg: Message) -> iced::Task<Message> {
    iced::Task::none()
}

fn view(_state: &State) -> iced::Element<'_, Message> {
    let content = iced::widget::column![
        iced::widget::text("Bench").size(24),
        iced::widget::text("Hello from iced"),
        iced::widget::button("Button"),
        iced::widget::column((0..10).map(|i| {
            iced::widget::row![
                iced::widget::text(format!("Row {i}")),
                iced::widget::text(format!("{}%", i * 7)),
            ]
            .into()
        })),
    ]
    .spacing(4);

    if !REPORTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let t0 = *START.get_or_init(Instant::now);
        println!("PAINT_MS={}", t0.elapsed().as_millis());
        use std::io::Write;
        std::io::stdout().flush().ok();
        if std::env::args().any(|a| a == "--bench") {
            // Give the renderer a moment to actually present the first frame.
            std::thread::sleep(Duration::from_millis(120));
            std::process::exit(0);
        }
    }
    content.into()
}

fn main() -> iced::Result {
    let res = iced::application(|| State { start: Instant::now() }, update, view)
        .title(|_state: &State| String::from("BenchIced"))
        .window(iced::window::Settings {
            size: iced::Size::new(640.0, 480.0),
            ..Default::default()
        })
        .run();
    res
}
