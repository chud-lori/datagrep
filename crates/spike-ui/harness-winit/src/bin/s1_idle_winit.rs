//! S1 fallback rung -- bare `winit` 0.30 + `wgpu` 30, `ControlFlow::Wait`.
//! Used because gpui 0.2.2 cannot build on this machine (see ../../harness
//! and SPIKE-REPORT.md: `xcrun metal` requires full Xcode, only Command
//! Line Tools is installed here). Per the task's own resolution ladder,
//! this "still valuable, it measures the floor gpui sits on."
//!
//! One window, one static frame (a solid clear color -- no text-rendering
//! crate is in scope for this fallback, see SPIKE-REPORT.md), no animation.
//! `ControlFlow::Poll` is never used (design doc §5.1: banned outright).
//! The only wakeups are winit's initial Resumed/RedrawRequested at startup
//! and our OWN 10s stderr heartbeat (WaitUntil) -- the heartbeat never calls
//! redraw() or touches the GPU, so it does not inflate the present counter;
//! it is disclosed separately in the report as harness overhead, not app
//! behavior under test.
//!
//! present/redraw counter: incremented immediately before
//! `SurfaceTexture::present()` -- this IS the true present call (unlike the
//! gpui S1, which could only proxy via an application-level render()).

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

static PRESENTS: AtomicU64 = AtomicU64::new(0);

struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    start: Instant,
    next_heartbeat_tick: u32,
    next_heartbeat_at: Instant,
    done: bool,
}

impl App {
    fn new() -> Self {
        let start = Instant::now();
        Self {
            window: None,
            gpu: None,
            start,
            next_heartbeat_tick: 1,
            next_heartbeat_at: start + Duration::from_secs(10),
            done: false,
        }
    }

    fn redraw(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            other => {
                eprintln!("[s1-winit] get_current_texture: {other:?}, skipping frame");
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("s1-static-frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.117,
                            g: 0.117,
                            b: 0.117,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                ..Default::default()
            });
        }
        gpu.queue.submit(Some(encoder.finish()));
        PRESENTS.fetch_add(1, Ordering::Relaxed);
        gpu.queue.present(frame);
    }

    fn maybe_heartbeat(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if self.done {
            return;
        }
        if now >= self.next_heartbeat_at {
            let n = PRESENTS.load(Ordering::Relaxed);
            eprintln!(
                "[s1-winit] t={:>3}s presents_total={} elapsed={:?}",
                self.next_heartbeat_tick * 10,
                n,
                self.start.elapsed()
            );
            if self.next_heartbeat_tick >= 6 {
                eprintln!(
                    "[s1-winit] DONE presents_total={} elapsed={:?}",
                    n,
                    self.start.elapsed()
                );
                self.done = true;
                event_loop.exit();
                return;
            }
            self.next_heartbeat_tick += 1;
            self.next_heartbeat_at = self.start + Duration::from_secs(10 * self.next_heartbeat_tick as u64);
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_heartbeat_at));
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("dbx spike-ui -- S1 zero-idle (winit+wgpu fallback)")
                        .with_inner_size(winit::dpi::LogicalSize::new(480.0, 220.0)),
                )
                .expect("create_window"),
        );

        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window.clone())
            .expect("create_surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .expect("request_adapter");
        eprintln!("[s1-winit] adapter: {:?}", adapter.get_info());

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
            ..Default::default()
        }))
        .expect("request_device");

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats[0];
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            ..surface.get_default_config(&adapter, size.width.max(1), size.height.max(1)).unwrap()
        };
        surface.configure(&device, &config);

        self.gpu = Some(GpuState {
            surface,
            device,
            queue,
            config,
        });
        self.window = Some(window);

        self.redraw();
        eprintln!(
            "[s1-winit] window opened, first (only) frame presented, entering Wait idle loop"
        );
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_heartbeat_at));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(new_size) => {
                if let Some(gpu) = &mut self.gpu {
                    if let (Some(w), Some(h)) =
                        (NonZeroU32::new(new_size.width), NonZeroU32::new(new_size.height))
                    {
                        gpu.config.width = w.get();
                        gpu.config.height = h.get();
                        gpu.surface.configure(&gpu.device, &gpu.config);
                        self.redraw();
                    }
                }
            }
            _ => {}
        }
        self.maybe_heartbeat(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.maybe_heartbeat(event_loop);
    }
}

fn main() {
    eprintln!("[s1-winit] pid={}", std::process::id());
    eprintln!("[s1-winit] toolkit=bare winit 0.30 + wgpu 30 (gpui fallback rung)");

    let event_loop = EventLoop::new().expect("EventLoop::new");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new();
    event_loop.run_app(&mut app).expect("run_app");
}
