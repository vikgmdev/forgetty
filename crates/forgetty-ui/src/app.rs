//! Top-level application state and event loop.
//!
//! Manages the winit event loop, coordinates the PTY, terminal emulator,
//! and GPU renderer into a working terminal application.

use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use forgetty_config::schema::Config;
use forgetty_pty::{PtyProcess, PtySize};
use forgetty_renderer::TerminalRenderer;
use forgetty_vt::Terminal;
use tracing::{debug, error, info, warn};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowAttributes, WindowId};

use crate::input::encode_key;

/// Custom user event to wake the event loop when PTY data is available.
#[derive(Debug, Clone)]
pub enum UserEvent {
    PtyOutput,
}

/// The main Forgetty application.
pub struct App {
    config: Config,
    window: Option<Arc<Window>>,
    renderer: Option<TerminalRenderer>,
    terminal: Terminal,
    pty: Option<PtyProcess>,
    pty_output_rx: Option<mpsc::Receiver<Vec<u8>>>,
    modifiers: ModifiersState,
    /// Track whether the shell process has exited.
    shell_exited: bool,
}

impl App {
    /// Create a new application instance.
    pub fn new(config: &Config) -> Self {
        let terminal = Terminal::new(24, 80);

        Self {
            config: config.clone(),
            window: None,
            renderer: None,
            terminal,
            pty: None,
            pty_output_rx: None,
            modifiers: ModifiersState::empty(),
            shell_exited: false,
        }
    }

    /// Run the application. This blocks until the window is closed.
    pub fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
        let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
        event_loop.set_control_flow(ControlFlow::Wait);

        let proxy = event_loop.create_proxy();
        let mut app = App::new(&config);

        // Spawn the PTY process before entering the event loop.
        let shell_cmd: Option<Vec<String>> = config.shell.as_ref().map(|s| vec![s.clone()]);
        let cmd_refs: Option<&[String]> = shell_cmd.as_deref();
        let mut pty = PtyProcess::spawn(PtySize::default(), None, cmd_refs)
            .map_err(|e| format!("Failed to spawn shell: {e}"))?;

        info!(pid = ?pty.pid(), "shell process spawned");

        // Take the reader and spawn a background thread for PTY output.
        let reader = pty.take_reader().expect("reader should be available on fresh PtyProcess");

        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                pty_reader_thread(reader, tx, proxy);
            })
            .expect("failed to spawn PTY reader thread");

        app.pty = Some(pty);
        app.pty_output_rx = Some(rx);

        event_loop.run_app(&mut app)?;
        Ok(())
    }

    /// Drain all pending PTY output and feed it to the terminal.
    fn drain_pty_output(&mut self) {
        if let Some(rx) = &self.pty_output_rx {
            loop {
                match rx.try_recv() {
                    Ok(data) => {
                        self.terminal.feed(&data);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if !self.shell_exited {
                            info!("PTY reader disconnected — shell exited");
                            self.shell_exited = true;
                        }
                        break;
                    }
                }
            }
        }
    }

    /// Write bytes to the PTY.
    fn write_to_pty(&mut self, data: &[u8]) {
        if let Some(pty) = &mut self.pty {
            if let Err(e) = pty.write(data) {
                warn!("failed to write to PTY: {e}");
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: UserEvent) {
        // PTY data available — request a redraw to process it
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            // Already have a window; nothing to do.
            return;
        }

        let attrs = WindowAttributes::default()
            .with_title("Forgetty")
            .with_inner_size(winit::dpi::LogicalSize::new(960, 640));

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                error!("failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };

        let renderer = match TerminalRenderer::new(
            window.clone(),
            &self.config.font_family,
            self.config.font_size,
        ) {
            Ok(r) => r,
            Err(e) => {
                error!("failed to create renderer: {e}");
                event_loop.exit();
                return;
            }
        };

        // Calculate grid size and resize terminal + PTY to match.
        let (rows, cols) = renderer.grid_size();
        debug!(rows, cols, "initial grid size");
        self.terminal.resize(rows, cols);

        if let Some(pty) = &self.pty {
            let pty_size =
                PtySize { rows: rows as u16, cols: cols as u16, pixel_width: 0, pixel_height: 0 };
            if let Err(e) = pty.resize(pty_size) {
                warn!("failed to resize PTY: {e}");
            }
        }

        self.renderer = Some(renderer);
        self.window = Some(window.clone());

        // Kick off the first redraw.
        window.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("close requested");
                // Kill the shell process if still running.
                if let Some(pty) = &mut self.pty {
                    pty.kill().ok();
                }
                event_loop.exit();
            }

            WindowEvent::Resized(size) => {
                if size.width == 0 || size.height == 0 {
                    return;
                }
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                    let (rows, cols) = renderer.grid_size();
                    debug!(rows, cols, "resized grid");
                    self.terminal.resize(rows, cols);

                    if let Some(pty) = &self.pty {
                        let pty_size = PtySize {
                            rows: rows as u16,
                            cols: cols as u16,
                            pixel_width: size.width as u16,
                            pixel_height: size.height as u16,
                        };
                        if let Err(e) = pty.resize(pty_size) {
                            warn!("failed to resize PTY: {e}");
                        }
                    }
                }
                // Request a redraw after resize.
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                // 1. Drain PTY output into the terminal.
                self.drain_pty_output();

                // 2. Render the current terminal state.
                if let Some(renderer) = &mut self.renderer {
                    if let Err(e) = renderer.render(&self.terminal) {
                        // Surface lost errors are recoverable — just skip the frame.
                        warn!("render error: {e}");
                    }
                }
            }

            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let Some(bytes) = encode_key(&event, self.modifiers) {
                        self.write_to_pty(&bytes);
                        // Request redraw so the echo appears immediately
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                // Translate mouse wheel into scroll-up / scroll-down sequences.
                // Many terminal applications handle these as arrow-key sequences
                // when in the alternate screen, or the terminal handles scrollback.
                let lines = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y as i32,
                    winit::event::MouseScrollDelta::PixelDelta(pos) => {
                        // Approximate: 20 pixels per line
                        (pos.y / 20.0) as i32
                    }
                };

                if lines > 0 {
                    // Scroll up (send arrow-up sequences for alternate screen apps)
                    for _ in 0..lines.unsigned_abs() {
                        self.write_to_pty(b"\x1b[A");
                    }
                } else if lines < 0 {
                    // Scroll down
                    for _ in 0..lines.unsigned_abs() {
                        self.write_to_pty(b"\x1b[B");
                    }
                }
            }

            _ => {}
        }
    }
}

/// Background thread that reads from the PTY and sends data to the main thread.
fn pty_reader_thread(
    mut reader: Box<dyn std::io::Read + Send>,
    tx: mpsc::Sender<Vec<u8>>,
    proxy: EventLoopProxy<UserEvent>,
) {
    let mut buf = [0u8; 65536];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                debug!("PTY reader: EOF");
                break;
            }
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    // Receiver dropped — main thread is shutting down.
                    break;
                }
                // Wake the event loop to process the new data
                let _ = proxy.send_event(UserEvent::PtyOutput);
            }
            Err(e) => {
                debug!("PTY reader error: {e}");
                break;
            }
        }
    }
}
