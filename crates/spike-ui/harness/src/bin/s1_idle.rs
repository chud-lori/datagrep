use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui::*;

static RENDER_CALLS: AtomicU64 = AtomicU64::new(0);

struct ZeroIdle;

impl Render for ZeroIdle {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        RENDER_CALLS.fetch_add(1, Ordering::Relaxed);
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xe0e0e0))
            .child("datagrep spike-ui -- S1 zero-idle proof")
            .child("static text, no animation")
    }
}

fn main() {
    eprintln!("[s1] pid={}", std::process::id());
    eprintln!("[s1] toolkit=gpui 0.2.2 (crates.io), no gpui-component");

    let app = Application::new();

    app.run(move |cx: &mut App| {
        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(480.), px(220.)), cx)),
            ..Default::default()
        };
        cx.open_window(window_options, |_window, cx| cx.new(|_| ZeroIdle))
            .expect("failed to open window");

        eprintln!("[s1] window opened, starting 60s idle measurement");

        let start = Instant::now();
        cx.spawn(async move |cx| {
            for tick in 1..=6u32 {
                Timer::after(Duration::from_secs(10)).await;
                let n = RENDER_CALLS.load(Ordering::Relaxed);
                eprintln!(
                    "[s1] t={:>3}s render_calls_total={} elapsed={:?}",
                    tick * 10,
                    n,
                    start.elapsed()
                );
            }
            let n = RENDER_CALLS.load(Ordering::Relaxed);
            eprintln!(
                "[s1] DONE render_calls_total={} elapsed={:?}",
                n,
                start.elapsed()
            );
            cx.update(|cx| cx.quit()).ok();
        })
        .detach();
    });
}
