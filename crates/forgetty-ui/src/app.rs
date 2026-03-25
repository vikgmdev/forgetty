//! Top-level application state and event loop.
//!
//! Manages the winit event loop, coordinates tabs, panes, the PTY,
//! terminal emulator, and GPU renderer into a working terminal application.

use std::collections::HashMap;
use std::sync::Arc;

use forgetty_config::schema::Config;
use forgetty_renderer::TerminalRenderer;
use tracing::{debug, error, info, warn};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowAttributes, WindowId};

use crate::clipboard::SmartClipboard;
use crate::input::encode_key;
use crate::keybindings::{Action, KeyBindings};
use crate::notifications;
use crate::pane::{Pane, PaneId};
use crate::pane_tree::SplitDirection;
use crate::tab::{Tab, TabId};

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
    modifiers: ModifiersState,

    // Tab and pane management.
    tabs: Vec<Tab>,
    active_tab: usize,
    panes: HashMap<PaneId, Pane>,

    // Keybindings and clipboard.
    keybindings: KeyBindings,
    clipboard: Option<SmartClipboard>,
}

impl App {
    /// Create a new application instance.
    pub fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
            window: None,
            renderer: None,
            modifiers: ModifiersState::empty(),
            tabs: Vec::new(),
            active_tab: 0,
            panes: HashMap::new(),
            keybindings: KeyBindings::default_bindings(),
            clipboard: SmartClipboard::new(),
        }
    }

    /// Run the application. This blocks until the window is closed.
    pub fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
        let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
        event_loop.set_control_flow(ControlFlow::Wait);

        let mut app = App::new(&config);

        // Create the initial pane and tab.
        let pane_id = PaneId::next();
        let pane = Pane::new(pane_id, 24, 80, None)
            .map_err(|e| format!("Failed to spawn initial shell: {e}"))?;
        info!(pane_id = pane_id.0, pid = ?pane.pty.pid(), "initial shell spawned");

        app.panes.insert(pane_id, pane);

        let tab_id = TabId::next();
        let tab = Tab::new(tab_id, pane_id);
        app.tabs.push(tab);

        event_loop.run_app(&mut app)?;
        Ok(())
    }

    /// Get the active tab, if any.
    fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    /// Get the active tab mutably.
    fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    /// Get the focused pane ID from the active tab.
    fn focused_pane_id(&self) -> Option<PaneId> {
        self.active_tab().map(|t| t.focused_pane)
    }

    /// Drain output from all panes.
    fn drain_all_pane_output(&mut self) {
        for pane in self.panes.values_mut() {
            pane.drain_output();
        }
    }

    /// Write bytes to the focused pane's PTY.
    fn write_to_focused_pty(&mut self, data: &[u8]) {
        if let Some(pane_id) = self.focused_pane_id() {
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.write(data);
            }
        }
    }

    /// Create a new tab with a fresh pane.
    fn new_tab(&mut self) {
        let (rows, cols) = self.current_grid_size();
        let pane_id = PaneId::next();
        match Pane::new(pane_id, rows, cols, None) {
            Ok(pane) => {
                info!(pane_id = pane_id.0, "new pane spawned for tab");
                self.panes.insert(pane_id, pane);

                let tab_id = TabId::next();
                let tab = Tab::new(tab_id, pane_id);
                self.tabs.push(tab);
                self.active_tab = self.tabs.len() - 1;
            }
            Err(e) => {
                warn!("failed to create new tab: {e}");
            }
        }
    }

    /// Close the active tab, killing all its panes.
    fn close_active_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }

        let tab = self.tabs.remove(self.active_tab);
        let pane_ids = tab.pane_tree.pane_ids();
        for id in pane_ids {
            if let Some(mut pane) = self.panes.remove(&id) {
                pane.pty.kill().ok();
            }
        }

        if self.tabs.is_empty() {
            // No tabs left — the app should exit.
            return;
        }

        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
    }

    /// Switch to the next tab.
    fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
    }

    /// Switch to the previous tab.
    fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab =
                if self.active_tab == 0 { self.tabs.len() - 1 } else { self.active_tab - 1 };
        }
    }

    /// Split the focused pane in the active tab.
    fn split_focused(&mut self, direction: SplitDirection) {
        let (rows, cols) = self.current_grid_size();
        let new_pane_id = PaneId::next();
        match Pane::new(new_pane_id, rows, cols, None) {
            Ok(pane) => {
                info!(pane_id = new_pane_id.0, "new pane spawned for split");
                self.panes.insert(new_pane_id, pane);

                if let Some(tab) = self.active_tab_mut() {
                    tab.split(direction, new_pane_id);
                }
            }
            Err(e) => {
                warn!("failed to create split pane: {e}");
            }
        }
    }

    /// Close the focused pane in the active tab.
    fn close_focused_pane(&mut self) {
        let pane_id = match self.focused_pane_id() {
            Some(id) => id,
            None => return,
        };

        let should_close_tab = if let Some(tab) = self.active_tab_mut() {
            tab.close_pane(pane_id)
        } else {
            return;
        };

        // Kill the pane's PTY.
        if let Some(mut pane) = self.panes.remove(&pane_id) {
            pane.pty.kill().ok();
        }

        if should_close_tab {
            self.close_active_tab();
        }
    }

    /// Get the current grid size from the renderer, or a sensible default.
    fn current_grid_size(&self) -> (usize, usize) {
        self.renderer.as_ref().map(|r| r.grid_size()).unwrap_or((24, 80))
    }

    /// Handle a keybinding action.
    fn handle_action(&mut self, action: Action) {
        match action {
            Action::NewTab => self.new_tab(),
            Action::CloseTab => self.close_active_tab(),
            Action::NextTab => self.next_tab(),
            Action::PrevTab => self.prev_tab(),
            Action::SplitHorizontal => self.split_focused(SplitDirection::Horizontal),
            Action::SplitVertical => self.split_focused(SplitDirection::Vertical),
            Action::ClosePane => self.close_focused_pane(),
            Action::FocusNext => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.focus_next();
                }
            }
            Action::FocusUp => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.focus_direction(SplitDirection::Vertical, false);
                }
            }
            Action::FocusDown => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.focus_direction(SplitDirection::Vertical, true);
                }
            }
            Action::FocusLeft => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.focus_direction(SplitDirection::Horizontal, false);
                }
            }
            Action::FocusRight => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.focus_direction(SplitDirection::Horizontal, true);
                }
            }
            Action::Copy => {
                // TODO: implement selection-based copy when selection is available
            }
            Action::Paste => {
                if let Some(clipboard) = &mut self.clipboard {
                    if let Some(text) = clipboard.paste() {
                        let pane_id = self.focused_pane_id();
                        if let Some(id) = pane_id {
                            if let Some(pane) = self.panes.get_mut(&id) {
                                pane.write(text.as_bytes());
                            }
                        }
                    }
                }
            }
            Action::ScrollPageUp | Action::ScrollUp => {
                // TODO: implement scrollback navigation
            }
            Action::ScrollPageDown | Action::ScrollDown => {
                // TODO: implement scrollback navigation
            }
            Action::ResetScroll => {
                // TODO: implement scrollback reset
            }
            Action::None => {}
        }
    }

    /// Resize all panes in the active tab to the current grid size.
    fn resize_active_panes(&mut self) {
        let (rows, cols) = self.current_grid_size();
        if let Some(tab) = self.active_tab() {
            let pane_ids = tab.pane_tree.pane_ids();
            for id in pane_ids {
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.resize(rows, cols);
                }
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: UserEvent) {
        // PTY data available — request a redraw to process it.
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
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

        // Calculate grid size and resize all panes.
        let (rows, cols) = renderer.grid_size();
        debug!(rows, cols, "initial grid size");

        self.renderer = Some(renderer);
        self.window = Some(window.clone());

        // Resize all panes in the active tab.
        self.resize_active_panes();

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
                // Kill all pane PTYs.
                for (_id, pane) in self.panes.iter_mut() {
                    pane.pty.kill().ok();
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
                }
                self.resize_active_panes();

                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                // 1. Drain PTY output from all panes.
                self.drain_all_pane_output();

                // 2. Check for notifications from the focused pane.
                if let Some(pane_id) = self.focused_pane_id() {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        let events = pane.terminal.drain_events();
                        let _notifs = notifications::check_notifications(&events);
                        // TODO: display/forward notifications
                    }
                }

                // 3. Render the focused pane of the active tab.
                //    For the MVP, we render only the focused pane full-screen.
                let focused = self.focused_pane_id();
                if let (Some(renderer), Some(pane_id)) = (&mut self.renderer, focused) {
                    if let Some(pane) = self.panes.get(&pane_id) {
                        if let Err(e) = renderer.render(&pane.terminal) {
                            warn!("render error: {e}");
                        }
                    }
                }

                // 4. Check if the app should exit (no tabs left).
                if self.tabs.is_empty() {
                    info!("all tabs closed, exiting");
                    event_loop.exit();
                }
            }

            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    // Check keybindings BEFORE encoding for PTY.
                    let action = self.keybindings.match_key(&event, self.modifiers);
                    if action != Action::None {
                        self.handle_action(action);
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                        return;
                    }

                    // No binding matched — encode and send to PTY.
                    if let Some(bytes) = encode_key(&event, self.modifiers) {
                        self.write_to_focused_pty(&bytes);
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y as i32,
                    winit::event::MouseScrollDelta::PixelDelta(pos) => (pos.y / 20.0) as i32,
                };

                if lines > 0 {
                    for _ in 0..lines.unsigned_abs() {
                        self.write_to_focused_pty(b"\x1b[A");
                    }
                } else if lines < 0 {
                    for _ in 0..lines.unsigned_abs() {
                        self.write_to_focused_pty(b"\x1b[B");
                    }
                }
            }

            _ => {}
        }
    }
}
