//! S1 — zero-idle proof (design doc §8, gate: >2 presents/60s idle = FAIL,
//! >20ms CPU/60s idle = FAIL).
//!
//! Deliberately uses ONLY the bare `gpui` crate — no `gpui-component` — so
//! this measures gpui's own idle floor, uncontaminated by the component
//! library's init/theme/global state. One window, static text, no animation,
//! no timers driving redraws (the only timer here is the stderr heartbeat,
//! which does not touch the view or call `cx.notify()`).
//!
//! Present/redraw counter: `Render::render` is only invoked by gpui when the
//! compositor actually needs a new frame from this view (retained mode — see
//! design §5.1 "Retained over immediate mode"). We increment an `AtomicU64`
//! on every call as a present proxy. This is an application-level proxy, not
//! a wgpu swapchain-present callback — gpui 0.2.2's public API does not
//! expose one — so it is a lower bound on true presents, not an exact count;
//! stated plainly in SPIKE-REPORT.md.
//!
//! Driving script wraps this binary and samples `ps -o utime,stime,rss` and
//! (if available) `footprint` before/after the 60s idle window, per design §6.

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
        // WindowBounds::centered(..) matches the pattern verified against the
        // real gpui-component 0.5.1-era examples (table_in_scrollable) --
        // see SPIKE-REPORT.md for why this file could not actually be
        // compiled on this machine to confirm it end-to-end.
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
