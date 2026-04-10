use std::collections::HashMap;
use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use log::{debug, info, warn};
use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::config;
use crate::fan::{CustomFanCurve, Fan, FanCurve};
use crate::platform::create_controller;

// ---------------------------------------------------------------------------
// Viridis color palette — perceptually uniform, colorblind-friendly
// ---------------------------------------------------------------------------

/// 11-point viridis gradient (steps 0–10) as RGB tuples.
/// Canonical matplotlib viridis colormap sampled at t = 0.0, 0.1, ..., 1.0.
const VIRIDIS: [(u8, u8, u8); 11] = [
    ( 68,   1,  84),  //  0  #440154  deep indigo-violet
    ( 72,  37, 118),  //  1  #482576  dark purple
    ( 65,  68, 135),  //  2  #414487  muted blue-purple
    ( 53,  95, 141),  //  3  #355f8d  steel blue
    ( 38, 130, 142),  //  4  #26828e  teal-blue
    ( 31, 158, 137),  //  5  #1f9e89  teal-green (midpoint)
    ( 53, 183, 121),  //  6  #35b779  sea green
    ( 94, 201,  98),  //  7  #5ec962  bright green
    (159, 218,  58),  //  8  #9fda3a  yellow-green
    (216, 226,  25),  //  9  #d8e219  chartreuse-yellow
    (253, 231,  37),  // 10  #fde725  bright yellow
];

/// Interpolate viridis color for a value in [0.0, 1.0].
fn viridis_color(t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let scaled = t * 10.0;
    let idx = (scaled as usize).min(9);
    let frac = scaled - idx as f32;

    let (r1, g1, b1) = VIRIDIS[idx];
    let (r2, g2, b2) = VIRIDIS[idx + 1];

    let r = (r1 as f32 + frac * (r2 as f32 - r1 as f32)) as u8;
    let g = (g1 as f32 + frac * (g2 as f32 - g1 as f32)) as u8;
    let b = (b1 as f32 + frac * (b2 as f32 - b1 as f32)) as u8;

    Color::Rgb(r, g, b)
}

/// Get viridis color for a step index (0–10).
fn viridis_step(step: u8) -> Color {
    let idx = (step as usize).min(10);
    let (r, g, b) = VIRIDIS[idx];
    Color::Rgb(r, g, b)
}

/// Map RPM to viridis color given a min/max range.
fn viridis_rpm(rpm: u32, min_rpm: u32, max_rpm: u32) -> Color {
    if max_rpm <= min_rpm {
        return viridis_color(0.0);
    }
    let t = (rpm.saturating_sub(min_rpm) as f32) / (max_rpm - min_rpm) as f32;
    viridis_color(t)
}

/// Map temperature to viridis color (30°C–100°C range).
fn viridis_temp(temp: u32) -> Color {
    let t = ((temp.saturating_sub(30)) as f32 / 70.0).clamp(0.0, 1.0);
    viridis_color(t)
}

// Viridis accent colors for UI chrome
const VIRIDIS_BORDER: Color = Color::Rgb(68, 1, 84);       // step 0 — deep purple
const VIRIDIS_TITLE: Color = Color::Rgb(31, 158, 137);     // step 5 — teal-green
const VIRIDIS_SELECTED: Color = Color::Rgb(253, 231, 37);  // step 10 — bright yellow
const VIRIDIS_CUSTOM: Color = Color::Rgb(159, 218, 58);    // step 8 — yellow-green
const VIRIDIS_BIOS: Color = Color::Rgb(53, 95, 141);       // step 3 — steel blue
const VIRIDIS_HOT: Color = Color::Rgb(253, 231, 37);       // step 10 — bright yellow

// ---------------------------------------------------------------------------
// Protocol: messages between UI thread and background poller
// ---------------------------------------------------------------------------

/// Messages from the background poller to the UI thread.
enum PollMsg {
    FanData(Vec<Fan>),
    SmartFanMode(Option<u32>),
    CustomCurveSet { fan_id: u32, sensor_id: u32 },
    CustomCurvesCleared,
    Error(String),
}

/// Commands from the UI thread to the background poller.
enum CmdMsg {
    SetFullSpeed(bool),
    SetCustomCurve(CustomFanCurve),
    ClearCustomCurves,
}

// ---------------------------------------------------------------------------
// TUI state
// ---------------------------------------------------------------------------

/// Editable curve state for a single fan+sensor pair.
struct TuiCurveEditor {
    fan_id: u32,
    sensor_id: u32,
    /// The 10 step values being edited (0-10 each).
    steps: [u8; 10],
    /// Snapshot of steps before editing started, for Esc revert.
    steps_snapshot: [u8; 10],
    /// Whether this curve is currently held (applied to EC).
    held: bool,
}

/// Navigation mode.
enum Mode {
    /// Navigating the fan list.
    FanSelect,
    /// Editing a curve; inner value is the selected step index (0-9).
    CurveEdit { step_idx: usize },
}

struct App {
    fans: Vec<Fan>,
    /// Per fan+sensor curve editors. Key: (fan_id, sensor_id).
    /// Uses keyed lookup (not positional indexing) so fan list changes
    /// between poll cycles cannot desynchronize editors from fans.
    curve_editors: HashMap<(u32, u32), TuiCurveEditor>,
    /// Per-fan list of sensor IDs (in discovery order).
    fan_sensor_ids: HashMap<String, Vec<u32>>,
    /// Per-fan currently displayed sensor index.
    selected_sensor_idx: HashMap<String, usize>,
    /// Index into fans vec.
    selected_fan: usize,
    /// Current interaction mode.
    mode: Mode,
    /// SmartFanMode readback from EC.
    smart_fan_mode: Option<u32>,
    status: String,
    status_until: Option<Instant>,
    quit: bool,
    /// Peak RPM observed per fan ID while full speed mode was active.
    full_speed_rpm: HashMap<String, u32>,
    /// Whether curve editors have been initialized from fan data.
    editors_initialized: bool,
}

impl App {
    fn new() -> Self {
        Self {
            fans: Vec::new(),
            curve_editors: HashMap::new(),
            fan_sensor_ids: HashMap::new(),
            selected_sensor_idx: HashMap::new(),
            selected_fan: 0,
            mode: Mode::FanSelect,
            smart_fan_mode: None,
            status: "Loading...".into(),
            status_until: None,
            quit: false,
            full_speed_rpm: HashMap::new(),
            editors_initialized: false,
        }
    }

    /// Set a status message that persists for the given duration.
    fn set_status(&mut self, msg: String, hold: Duration) {
        self.status = msg;
        self.status_until = Some(Instant::now() + hold);
    }

    /// Update fan data from poller. Initialize curve editors on first data.
    fn update_fans(&mut self, fans: Vec<Fan>) {
        // Track peak RPM when full speed mode is active
        let full_speed_active = fans.iter().any(|f| f.full_speed_active);
        if full_speed_active {
            for fan in &fans {
                let peak = self.full_speed_rpm.entry(fan.id.clone()).or_insert(0);
                if fan.speed_rpm > *peak {
                    *peak = fan.speed_rpm;
                }
            }
        }

        // Initialize curve editors on first fan data arrival
        if !self.editors_initialized && !fans.is_empty() {
            self.init_curve_editors(&fans);
            self.editors_initialized = true;
        }

        // Clamp selected fan
        if self.selected_fan >= fans.len() && !fans.is_empty() {
            self.selected_fan = fans.len() - 1;
        }

        self.fans = fans;

        // Only reset status to OK if no timed message is active.
        let status_expired = self
            .status_until
            .map(|t| Instant::now() > t)
            .unwrap_or(true);
        if status_expired {
            self.status = "OK".into();
            self.status_until = None;
        }
    }

    /// Build curve editors from discovered fan data and saved config.
    fn init_curve_editors(&mut self, fans: &[Fan]) {
        let saved_config = config::load_config();

        for fan in fans {
            let mut sensor_ids: Vec<u32> = fan.curves.iter().map(|c| c.sensor_id).collect();
            // Deduplicate while preserving order
            let mut seen = Vec::new();
            sensor_ids.retain(|id| {
                if seen.contains(id) {
                    false
                } else {
                    seen.push(*id);
                    true
                }
            });

            self.fan_sensor_ids
                .insert(fan.id.clone(), sensor_ids.clone());
            self.selected_sensor_idx.insert(fan.id.clone(), 0);

            for curve in &fan.curves {
                let key = (curve.fan_id, curve.sensor_id);
                if self.curve_editors.contains_key(&key) {
                    continue;
                }

                // Default steps: derive from EC curve points (identity mapping)
                let default_steps: [u8; 10] = std::array::from_fn(|i| i as u8);

                // Check if config has a saved curve for this fan+sensor
                let mut steps = saved_config
                    .custom_curves
                    .iter()
                    .find(|c| c.fan_id == curve.fan_id && c.sensor_id == curve.sensor_id)
                    .map(|c| c.steps)
                    .unwrap_or(default_steps);

                // Sanitize loaded steps so they never violate safety floors.
                enforce_safety_minimums(&mut steps);

                let held = saved_config
                    .custom_curves
                    .iter()
                    .any(|c| c.fan_id == curve.fan_id && c.sensor_id == curve.sensor_id);

                self.curve_editors.insert(
                    key,
                    TuiCurveEditor {
                        fan_id: curve.fan_id,
                        sensor_id: curve.sensor_id,
                        steps,
                        steps_snapshot: steps,
                        held,
                    },
                );
            }
        }
    }

    /// Get the currently selected fan, if any.
    fn selected_fan(&self) -> Option<&Fan> {
        self.fans.get(self.selected_fan)
    }

    /// Get the sensor ID for the currently displayed curve of the selected fan.
    fn current_sensor_id(&self) -> Option<u32> {
        let fan = self.selected_fan()?;
        let sensor_ids = self.fan_sensor_ids.get(&fan.id)?;
        let idx = self.selected_sensor_idx.get(&fan.id).copied().unwrap_or(0);
        sensor_ids.get(idx).copied()
    }

    /// Get the curve editor for the currently selected fan+sensor.
    fn current_editor(&self) -> Option<&TuiCurveEditor> {
        let fan = self.selected_fan()?;
        let sensor_id = self.current_sensor_id()?;
        let fan_id = fan.curves.first().map(|c| c.fan_id)?;
        self.curve_editors.get(&(fan_id, sensor_id))
    }

    /// Get the curve editor for the currently selected fan+sensor (mutable).
    fn current_editor_mut(&mut self) -> Option<&mut TuiCurveEditor> {
        let fan = self.fans.get(self.selected_fan)?;
        let sensor_ids = self.fan_sensor_ids.get(&fan.id)?;
        let idx = self.selected_sensor_idx.get(&fan.id).copied().unwrap_or(0);
        let sensor_id = sensor_ids.get(idx).copied()?;
        let fan_id = fan.curves.first().map(|c| c.fan_id)?;
        self.curve_editors.get_mut(&(fan_id, sensor_id))
    }

    /// Get the EC curve for the currently selected fan+sensor.
    fn current_ec_curve(&self) -> Option<&FanCurve> {
        let fan = self.selected_fan()?;
        let sensor_id = self.current_sensor_id()?;
        fan.curves.iter().find(|c| c.sensor_id == sensor_id)
    }

    /// Cycle to the next sensor for the selected fan.
    fn cycle_sensor(&mut self, forward: bool) {
        if let Some(fan) = self.fans.get(self.selected_fan) {
            if let Some(sensor_ids) = self.fan_sensor_ids.get(&fan.id) {
                if sensor_ids.len() <= 1 {
                    return;
                }
                let idx = self
                    .selected_sensor_idx
                    .entry(fan.id.clone())
                    .or_insert(0);
                if forward {
                    *idx = (*idx + 1) % sensor_ids.len();
                } else if *idx == 0 {
                    *idx = sensor_ids.len() - 1;
                } else {
                    *idx -= 1;
                }
            }
        }
    }

    /// Build a CustomFanCurve from the current editor state.
    fn build_custom_curve(&self) -> Option<CustomFanCurve> {
        let editor = self.current_editor()?;
        Some(CustomFanCurve {
            fan_id: editor.fan_id,
            sensor_id: editor.sensor_id,
            steps: editor.steps,
        })
    }

    /// Collect all held curves for config saving.
    fn held_curves(&self) -> Vec<CustomFanCurve> {
        self.curve_editors
            .values()
            .filter(|e| e.held)
            .map(|e| CustomFanCurve {
                fan_id: e.fan_id,
                sensor_id: e.sensor_id,
                steps: e.steps,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Curve step validation helpers
// ---------------------------------------------------------------------------

/// Enforce non-decreasing constraint after changing step[idx].
/// When raising a value, auto-raise subsequent steps that are below it.
/// When lowering a value, auto-lower preceding steps that are above it.
fn enforce_non_decreasing(steps: &mut [u8; 10], idx: usize) {
    let val = steps[idx];
    // Propagate upward: subsequent steps must be >= val
    for step in &mut steps[(idx + 1)..] {
        if *step < val {
            *step = val;
        }
    }
    // Propagate downward: preceding steps must be <= val
    for step in steps[..idx].iter_mut().rev() {
        if *step > val {
            *step = val;
        }
    }
}

/// Enforce safety minimums for high-temperature steps.
///
/// Only propagates upward (step 8 clamp raises step 9 if needed; step 9 clamp
/// raises nothing). Does NOT propagate backward into lower steps — a low value
/// on step 7 is valid even if step 8 is at its minimum.
fn enforce_safety_minimums(steps: &mut [u8; 10]) {
    if steps[8] < 3 {
        steps[8] = 3;
    }
    if steps[9] < 5 {
        steps[9] = 5;
    }
    // Propagate upward only: step 8's raised value must not exceed step 9.
    if steps[9] < steps[8] {
        steps[9] = steps[8];
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run() -> Result<()> {
    // Validate controller BEFORE entering raw mode so failures don't leave
    // the terminal in a broken state. We create and immediately drop this
    // controller — the poller thread creates its own because WMI COM objects
    // are !Send and cannot cross thread boundaries. This matches the pattern
    // used in gui.rs (worker thread with separate controller).
    drop(create_controller()?);

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;

    let result = run_inner();

    // Always restore terminal, even on error.
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    result
}

// ---------------------------------------------------------------------------
// Poller thread
// ---------------------------------------------------------------------------

fn run_inner() -> Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_poller = stop.clone();

    let (tx, rx) = mpsc::channel::<PollMsg>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<CmdMsg>();

    let poll_handle = thread::spawn(move || {
        let ctrl = match create_controller() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(PollMsg::Error(format!("Init error: {e}")));
                return;
            }
        };

        // Held custom curves: re-applied each poll cycle to resist BIOS overrides.
        let mut held_curves: Vec<CustomFanCurve> = Vec::new();

        // Load saved curves from config and apply on startup.
        let saved_config = config::load_config();
        for curve in &saved_config.custom_curves {
            info!("TUI poller: applying saved curve fan{}->sensor{}", curve.fan_id, curve.sensor_id);
            match ctrl.set_custom_curve(curve) {
                Ok(()) => {
                    held_curves.push(curve.clone());
                    let _ = tx.send(PollMsg::CustomCurveSet {
                        fan_id: curve.fan_id,
                        sensor_id: curve.sensor_id,
                    });
                }
                Err(e) => {
                    warn!("TUI poller: saved curve failed: {e}");
                    let _ = tx.send(PollMsg::Error(format!("Saved curve failed: {e}")));
                }
            }
        }

        // Track known fan IDs for full speed toggle.
        let mut known_fan_ids: Vec<String> = Vec::new();

        // Initial discovery.
        info!("TUI poller: initial discover()");
        match ctrl.discover() {
            Ok(fans) => {
                known_fan_ids = fans.iter().map(|f| f.id.clone()).collect();
                let _ = tx.send(PollMsg::FanData(fans));
            }
            Err(e) => {
                let _ = tx.send(PollMsg::Error(format!("Init discover: {e}")));
            }
        }

        // Read initial SmartFanMode.
        if let Ok(mode) = ctrl.get_smart_fan_mode() {
            let _ = tx.send(PollMsg::SmartFanMode(mode));
        }

        while !stop_poller.load(std::sync::atomic::Ordering::Relaxed) {
            // Drain incoming commands.
            let mut had_command = false;
            while let Ok(cmd) = cmd_rx.try_recv() {
                had_command = true;
                match cmd {
                    CmdMsg::SetFullSpeed(on) => {
                        let pwm = if on { 255u8 } else { 0u8 };
                        info!("TUI poller: SetFullSpeed({on})");
                        for fan_id in &known_fan_ids {
                            if let Err(e) = ctrl.set_pwm(fan_id, pwm) {
                                warn!("TUI poller: set_pwm({fan_id}, {pwm}) failed: {e}");
                            }
                        }
                    }
                    CmdMsg::SetCustomCurve(curve) => {
                        info!(
                            "TUI poller: SetCustomCurve fan{}->sensor{} steps={:?}",
                            curve.fan_id, curve.sensor_id, curve.steps
                        );
                        match ctrl.set_custom_curve(&curve) {
                            Ok(()) => {
                                // Upsert in held_curves
                                held_curves.retain(|c| {
                                    !(c.fan_id == curve.fan_id
                                        && c.sensor_id == curve.sensor_id)
                                });
                                held_curves.push(curve.clone());
                                let _ = tx.send(PollMsg::CustomCurveSet {
                                    fan_id: curve.fan_id,
                                    sensor_id: curve.sensor_id,
                                });
                            }
                            Err(e) => {
                                warn!("TUI poller: set_custom_curve failed: {e}");
                                let _ = tx.send(PollMsg::Error(format!("{e}")));
                            }
                        }
                    }
                    CmdMsg::ClearCustomCurves => {
                        info!("TUI poller: ClearCustomCurves");
                        held_curves.clear();
                        // Switch back to Balanced (mode 2)
                        if let Err(e) = ctrl.set_smart_fan_mode(2) {
                            warn!("TUI poller: set_smart_fan_mode(2) failed: {e}");
                        }
                        let _ = tx.send(PollMsg::CustomCurvesCleared);
                    }
                }
            }

            // Re-apply held curves before polling.
            for curve in &held_curves {
                debug!(
                    "TUI poller: re-applying curve fan{}->sensor{}",
                    curve.fan_id, curve.sensor_id
                );
                if let Err(e) = ctrl.set_custom_curve(curve) {
                    warn!("TUI poller: re-apply curve failed: {e}");
                }
            }

            // Skip discover on command iterations (let EC settle).
            if !had_command {
                match ctrl.discover() {
                    Ok(fans) => {
                        // Update known fan IDs for full speed toggle.
                        known_fan_ids = fans.iter().map(|f| f.id.clone()).collect();
                        if tx.send(PollMsg::FanData(fans)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        if tx.send(PollMsg::Error(format!("{e}"))).is_err() {
                            break;
                        }
                    }
                }

                // Read SmartFanMode each cycle.
                if let Ok(mode) = ctrl.get_smart_fan_mode() {
                    let _ = tx.send(PollMsg::SmartFanMode(mode));
                }
            }

            // Sleep in short increments so the stop flag is checked promptly.
            for _ in 0..15 {
                if stop_poller.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    });

    // -----------------------------------------------------------------------
    // UI event loop
    // -----------------------------------------------------------------------

    let mut app = App::new();
    let tick_rate = Duration::from_millis(100);

    loop {
        // Drain poll messages.
        while let Ok(msg) = rx.try_recv() {
            match msg {
                PollMsg::FanData(fans) => app.update_fans(fans),
                PollMsg::SmartFanMode(mode) => {
                    app.smart_fan_mode = mode;
                }
                PollMsg::CustomCurveSet { fan_id, sensor_id } => {
                    // Mark the editor as held.
                    if let Some(editor) = app.curve_editors.get_mut(&(fan_id, sensor_id)) {
                        editor.held = true;
                    }
                    app.set_status(
                        format!("Curve applied: fan {} sensor {}", fan_id, sensor_id),
                        Duration::from_secs(5),
                    );
                }
                PollMsg::CustomCurvesCleared => {
                    for editor in app.curve_editors.values_mut() {
                        editor.held = false;
                    }
                    app.set_status("Reset to BIOS auto".into(), Duration::from_secs(5));
                }
                PollMsg::Error(e) => {
                    app.set_status(format!("Error: {e}"), Duration::from_secs(10));
                }
            }
        }

        if app.quit {
            break;
        }

        terminal.draw(|f| draw_ui(f, &app))?;

        // Handle input with timeout.
        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                handle_key(&mut app, key.code, key.modifiers, &cmd_tx);
            }
        }
    }

    // Signal poller to stop and wait for it.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = poll_handle.join();
    Ok(())
}

// ---------------------------------------------------------------------------
// Input handling
// ---------------------------------------------------------------------------

fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers, cmd_tx: &mpsc::Sender<CmdMsg>) {
    // Ctrl+C always quits.
    if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        app.quit = true;
        return;
    }

    match app.mode {
        Mode::FanSelect => handle_fan_select(app, code, cmd_tx),
        Mode::CurveEdit { step_idx } => handle_curve_edit(app, code, step_idx, cmd_tx),
    }
}

fn handle_fan_select(app: &mut App, code: KeyCode, cmd_tx: &mpsc::Sender<CmdMsg>) {
    match code {
        KeyCode::Char('q') => app.quit = true,

        KeyCode::Up | KeyCode::Char('k') => {
            if app.selected_fan > 0 {
                app.selected_fan -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if !app.fans.is_empty() && app.selected_fan < app.fans.len() - 1 {
                app.selected_fan += 1;
            }
        }

        KeyCode::Tab => app.cycle_sensor(true),
        KeyCode::BackTab => app.cycle_sensor(false),

        KeyCode::Enter => {
            if app.current_editor().is_some() {
                // Snapshot steps for Esc revert.
                if let Some(editor) = app.current_editor_mut() {
                    editor.steps_snapshot = editor.steps;
                }
                app.mode = Mode::CurveEdit { step_idx: 0 };
            }
        }

        KeyCode::Char('f') | KeyCode::Char('F') => {
            let is_full_speed = app.fans.iter().any(|f| f.full_speed_active);
            let _ = cmd_tx.send(CmdMsg::SetFullSpeed(!is_full_speed));
            let action = if is_full_speed { "off" } else { "on" };
            app.set_status(format!("Full speed {action}"), Duration::from_secs(3));
        }

        KeyCode::Char('a') => {
            if let Some(curve) = app.build_custom_curve() {
                let _ = cmd_tx.send(CmdMsg::SetCustomCurve(curve));
            }
        }

        KeyCode::Char('s') => {
            if let Some(curve) = app.build_custom_curve() {
                let _ = cmd_tx.send(CmdMsg::SetCustomCurve(curve));
            }
            // Save all held curves to config.
            let curves = app.held_curves();
            // Also include the just-applied curve.
            let mut all_curves = curves;
            if let Some(current) = app.build_custom_curve() {
                if !all_curves.iter().any(|c| c.fan_id == current.fan_id && c.sensor_id == current.sensor_id) {
                    all_curves.push(current);
                }
            }
            let cfg = config::Config {
                custom_curves: all_curves,
                auto_smart_fan_mode: true,
            };
            match config::save_config(&cfg) {
                Ok(()) => {
                    app.set_status(
                        format!("Saved {} curve(s) to config", cfg.custom_curves.len()),
                        Duration::from_secs(5),
                    );
                }
                Err(e) => {
                    app.set_status(format!("Error: save failed: {e}"), Duration::from_secs(10));
                }
            }
        }

        KeyCode::Char('r') => {
            let _ = cmd_tx.send(CmdMsg::ClearCustomCurves);
            // Reset all editors to default identity mapping.
            for editor in app.curve_editors.values_mut() {
                editor.steps = std::array::from_fn(|i| i as u8);
                editor.held = false;
            }
        }

        _ => {}
    }
}

fn handle_curve_edit(
    app: &mut App,
    code: KeyCode,
    step_idx: usize,
    cmd_tx: &mpsc::Sender<CmdMsg>,
) {
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Revert to snapshot and return to fan select.
            if let Some(editor) = app.current_editor_mut() {
                editor.steps = editor.steps_snapshot;
            }
            app.mode = Mode::FanSelect;
        }

        KeyCode::Up | KeyCode::Char('k') => {
            if step_idx > 0 {
                app.mode = Mode::CurveEdit {
                    step_idx: step_idx - 1,
                };
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if step_idx < 9 {
                app.mode = Mode::CurveEdit {
                    step_idx: step_idx + 1,
                };
            }
        }

        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('+') | KeyCode::Char('=') => {
            if let Some(editor) = app.current_editor_mut() {
                if editor.steps[step_idx] < 10 {
                    editor.steps[step_idx] += 1;
                    enforce_non_decreasing(&mut editor.steps, step_idx);
                    enforce_safety_minimums(&mut editor.steps);
                }
            }
        }

        KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') => {
            if let Some(editor) = app.current_editor_mut() {
                if editor.steps[step_idx] > 0 {
                    editor.steps[step_idx] -= 1;
                    enforce_non_decreasing(&mut editor.steps, step_idx);
                    enforce_safety_minimums(&mut editor.steps);
                }
            }
        }

        KeyCode::Enter | KeyCode::Char('a') => {
            if let Some(curve) = app.build_custom_curve() {
                let _ = cmd_tx.send(CmdMsg::SetCustomCurve(curve));
            }
            app.mode = Mode::FanSelect;
        }

        KeyCode::Char('s') => {
            if let Some(curve) = app.build_custom_curve() {
                let _ = cmd_tx.send(CmdMsg::SetCustomCurve(curve));
            }
            let curves = app.held_curves();
            let mut all_curves = curves;
            if let Some(current) = app.build_custom_curve() {
                if !all_curves.iter().any(|c| c.fan_id == current.fan_id && c.sensor_id == current.sensor_id) {
                    all_curves.push(current);
                }
            }
            let cfg = config::Config {
                custom_curves: all_curves,
                auto_smart_fan_mode: true,
            };
            match config::save_config(&cfg) {
                Ok(()) => {
                    app.set_status(
                        format!("Saved {} curve(s) to config", cfg.custom_curves.len()),
                        Duration::from_secs(5),
                    );
                }
                Err(e) => {
                    app.set_status(format!("Error: save failed: {e}"), Duration::from_secs(10));
                }
            }
            app.mode = Mode::FanSelect;
        }

        KeyCode::Char('r') => {
            let _ = cmd_tx.send(CmdMsg::ClearCustomCurves);
            for editor in app.curve_editors.values_mut() {
                editor.steps = std::array::from_fn(|i| i as u8);
                editor.held = false;
            }
            app.mode = Mode::FanSelect;
        }

        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn smart_fan_mode_label(mode: Option<u32>) -> &'static str {
    match mode {
        Some(1) => "Quiet",
        Some(2) => "Balanced",
        Some(3) => "Performance",
        Some(255) => "Custom",
        Some(v) => {
            // Log unknown values for future discovery
            log::debug!("Unknown SmartFanMode value: {v}");
            "Unknown"
        }
        None => "N/A",
    }
}

fn draw_ui(f: &mut Frame, app: &App) {
    let area = f.area();

    // Compute dynamic heights.
    // borders(2) + header(1) + fan rows + optional full speed banner row
    let full_speed_banner = app.fans.iter().any(|f| f.full_speed_active) as u16;
    let fan_list_height = ((app.fans.len() as u16).max(1) + 3 + full_speed_banner).min(8);
    let info_lines = build_info_lines(app);
    let info_height = ((info_lines.len() as u16).max(1) + 2).min(6);

    let layout = Layout::vertical([
        Constraint::Length(3),               // Title
        Constraint::Length(fan_list_height),  // Fan list (compact, capped)
        Constraint::Min(6),                  // Curve editor (flex)
        Constraint::Length(info_height),      // Info panel (capped)
        Constraint::Length(5),               // Help
        Constraint::Length(3),               // Status bar
    ])
    .split(area);

    draw_title(f, app, layout[0]);
    draw_fan_list(f, app, layout[1]);
    draw_curve_editor(f, app, layout[2]);
    draw_info_panel(f, info_lines, layout[3]);
    draw_help(f, app, layout[4]);
    draw_status(f, app, layout[5]);
}

fn draw_title(f: &mut Frame, app: &App, area: Rect) {
    let mode_label = smart_fan_mode_label(app.smart_fan_mode);
    let title = Block::bordered()
        .title(" Fan Control TUI ")
        .title_alignment(Alignment::Center)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(VIRIDIS_BORDER));
    let mode_color = match app.smart_fan_mode {
        Some(255) => VIRIDIS_CUSTOM,  // Custom — lime
        Some(1) => VIRIDIS_BIOS,      // Quiet — cool blue
        Some(3) => VIRIDIS_HOT,       // Performance — yellow
        _ => VIRIDIS_TITLE,           // Balanced/N/A — teal
    };
    let title_text = Paragraph::new(Line::from(vec![
        Span::styled("Fan Control", Style::default().fg(VIRIDIS_TITLE).bold()),
        Span::raw(" \u{2014} "),
        Span::styled(
            format!("SmartFanMode: {mode_label}"),
            Style::default().fg(mode_color),
        ),
    ]))
    .alignment(Alignment::Center)
    .block(title);
    f.render_widget(title_text, area);
}

fn draw_fan_list(f: &mut Frame, app: &App, area: Rect) {
    if app.fans.is_empty() {
        let msg = Paragraph::new("No fans detected. Waiting for data...")
            .alignment(Alignment::Center)
            .block(
                Block::bordered()
                    .title(" Fans ")
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(VIRIDIS_BORDER)),
            );
        f.render_widget(msg, area);
        return;
    }

    let full_speed = app.fans.iter().any(|f| f.full_speed_active);
    let mut rows: Vec<Row> = Vec::new();

    if full_speed {
        rows.push(
            Row::new(vec![
                Cell::from(""),
                Cell::from(Span::styled(
                    "!! FULL SPEED MODE ACTIVE !!",
                    Style::default().fg(Color::Red).bold(),
                )),
                Cell::from(""),
                Cell::from(""),
            ])
            .height(1),
        );
    }

    for (i, fan) in app.fans.iter().enumerate() {
        let is_selected = i == app.selected_fan;
        let marker = if is_selected { "\u{25b6}" } else { " " };

        // Color RPM by speed relative to fan's range
        let (min_rpm, max_rpm) = (
            fan.min_rpm.unwrap_or(1600),
            fan.max_rpm.unwrap_or(5400),
        );
        let rpm_color = viridis_rpm(fan.speed_rpm, min_rpm, max_rpm);
        let rpm_text = Span::styled(
            format!("{} RPM", fan.speed_rpm),
            Style::default().fg(rpm_color),
        );

        // Determine curve status for this fan
        let fan_numeric_id = fan.curves.first().map(|c| c.fan_id);
        let has_held_curve = fan_numeric_id
            .map(|fid| {
                app.curve_editors
                    .values()
                    .any(|e| e.fan_id == fid && e.held)
            })
            .unwrap_or(false);

        let curve_status = if fan.full_speed_active {
            Span::styled("Full speed", Style::default().fg(Color::Red).bold())
        } else if has_held_curve {
            Span::styled("Custom curve", Style::default().fg(VIRIDIS_CUSTOM))
        } else {
            Span::styled("BIOS auto", Style::default().fg(VIRIDIS_BIOS))
        };

        let row_style = if is_selected {
            Style::default().fg(VIRIDIS_SELECTED).bold()
        } else {
            Style::default()
        };

        rows.push(
            Row::new(vec![
                Cell::from(marker),
                Cell::from(fan.label.clone()),
                Cell::from(rpm_text),
                Cell::from(curve_status),
            ])
            .style(row_style),
        );
    }

    let widths = [
        Constraint::Length(2),
        Constraint::Length(20),
        Constraint::Length(12),
        Constraint::Min(16),
    ];

    let header = Row::new(vec!["", "FAN", "SPEED", "STATUS"])
        .style(Style::default().fg(Color::DarkGray).bold())
        .bottom_margin(0);

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::bordered()
                .title(" Fans ")
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(VIRIDIS_BORDER)),
        )
        .column_spacing(1);

    f.render_widget(table, area);
}

fn draw_curve_editor(f: &mut Frame, app: &App, area: Rect) {
    let fan = match app.selected_fan() {
        Some(fan) => fan,
        None => {
            let msg = Paragraph::new("Select a fan to edit its curve")
                .alignment(Alignment::Center)
                .block(
                    Block::bordered()
                        .title(" Curve Editor ")
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(VIRIDIS_BORDER)),
                );
            f.render_widget(msg, area);
            return;
        }
    };

    let sensor_id = match app.current_sensor_id() {
        Some(id) => id,
        None => {
            let msg = Paragraph::new("No curve data available for this fan")
                .alignment(Alignment::Center)
                .block(
                    Block::bordered()
                        .title(" Curve Editor ")
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(VIRIDIS_BORDER)),
                );
            f.render_widget(msg, area);
            return;
        }
    };

    let ec_curve = app.current_ec_curve();
    let editor = app.current_editor();

    // Build header: fan label, sensor, navigation hint
    let sensor_ids = app.fan_sensor_ids.get(&fan.id);
    let sensor_idx = app
        .selected_sensor_idx
        .get(&fan.id)
        .copied()
        .unwrap_or(0);
    let sensor_count = sensor_ids.map(|s| s.len()).unwrap_or(0);
    let active_tag = ec_curve
        .map(|c| if c.active { "Active" } else { "Inactive" })
        .unwrap_or("?");

    let header_text = format!(
        "{} > Sensor {} ({})  [{}/{}]",
        fan.label,
        sensor_id,
        active_tag,
        sensor_idx + 1,
        sensor_count
    );

    let editing_step = match app.mode {
        Mode::CurveEdit { step_idx } => Some(step_idx),
        Mode::FanSelect => None,
    };

    let is_editing = editing_step.is_some();

    // Build step rows
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Column header
    let has_custom = editor.map(|e| e.held).unwrap_or(false);
    let header_label = if has_custom {
        "Step  Temp   Value  RPM     Bar          Default"
    } else {
        "Step  Temp   Value  RPM     Bar"
    };
    lines.push(Line::from(vec![Span::styled(
        header_label,
        Style::default().fg(Color::DarkGray),
    )]));

    if let (Some(ec), Some(ed)) = (ec_curve, editor) {
        // Compute EC default step indices by identity mapping (step i = index i)
        let default_steps: [u8; 10] = std::array::from_fn(|i| i as u8);

        for i in 0..10 {
            let temp = ec.points.get(i).map(|p| p.temperature).unwrap_or(0);
            let step_val = ed.steps[i];
            let default_val = default_steps[i];

            // Approximate RPM from step value using EC curve points
            let rpm = if (step_val as usize) < ec.points.len() {
                ec.points[step_val as usize].fan_speed
            } else {
                ec.max_speed
            };

            let is_selected_step = editing_step == Some(i);

            // Viridis-colored bar: each filled cell gets its gradient position
            let bar_filled = (step_val as usize).min(10);
            let mut bar_spans: Vec<Span> = Vec::new();
            for b in 0..10 {
                if b < bar_filled {
                    let color = viridis_step(b as u8);
                    bar_spans.push(Span::styled("\u{2588}", Style::default().fg(color)));
                } else {
                    bar_spans.push(Span::styled(
                        "\u{2591}",
                        Style::default().fg(VIRIDIS_BORDER),
                    ));
                }
            }

            // Safety annotation
            let safety = if i == 8 {
                " min:3"
            } else if i == 9 {
                " min:5"
            } else {
                ""
            };

            let value_display = if is_selected_step {
                format!("[>{:>2}]", step_val)
            } else {
                format!("[{:>2} ]", step_val)
            };

            // Show EC default dim next to custom value when they differ
            let default_hint = if has_custom && step_val != default_val {
                let default_rpm = if (default_val as usize) < ec.points.len() {
                    ec.points[default_val as usize].fan_speed
                } else {
                    ec.max_speed
                };
                format!("  ({:>2}={:>4})", default_val, default_rpm)
            } else {
                String::new()
            };

            let is_modified = step_val != default_val;

            // Temperature colored by viridis
            let temp_color = viridis_temp(temp);

            let text_style = if is_selected_step {
                Style::default().fg(VIRIDIS_SELECTED).bold()
            } else if is_editing {
                Style::default().fg(Color::White)
            } else if has_custom && is_modified {
                Style::default().fg(VIRIDIS_CUSTOM)
            } else {
                Style::default()
            };

            // Build the line with mixed spans for viridis gradient bar
            let mut spans: Vec<Span> = Vec::new();
            spans.push(Span::styled(
                format!(" {:>1}    ", i),
                text_style,
            ));
            spans.push(Span::styled(
                format!("{:>3}C", temp),
                Style::default().fg(temp_color),
            ));
            spans.push(Span::styled(
                format!("   {}  {:>4}    ", value_display, rpm),
                text_style,
            ));
            spans.extend(bar_spans);
            spans.push(Span::styled(safety.to_string(), Style::default().fg(Color::Red)));

            if !default_hint.is_empty() {
                spans.push(Span::styled(default_hint, Style::default().fg(Color::DarkGray)));
            }

            lines.push(Line::from(spans));
        }

        // EC reference line
        let ec_steps: Vec<String> = (0..10)
            .map(|i| {
                // Reverse-map: find which step index the EC's RPM corresponds to
                let ec_rpm = ec.points.get(i).map(|p| p.fan_speed).unwrap_or(0);
                // Find closest step index
                let mut best_idx = 0u8;
                for si in 0..ec.points.len() {
                    if ec.points[si].fan_speed <= ec_rpm {
                        best_idx = si as u8;
                    }
                }
                format!("{:>2}", best_idx)
            })
            .collect();
        lines.push(Line::from(vec![
            Span::styled("EC:          ", Style::default().fg(Color::DarkGray)),
            Span::styled(ec_steps.join(" "), Style::default().fg(Color::DarkGray)),
        ]));
    } else {
        lines.push(Line::from("No curve data available"));
    }

    let border_style = if is_editing {
        Style::default().fg(VIRIDIS_SELECTED)
    } else {
        Style::default().fg(VIRIDIS_BORDER)
    };

    let block = Block::bordered()
        .title(format!(" Curve: {header_text} "))
        .title_alignment(Alignment::Left)
        .border_type(BorderType::Rounded)
        .border_style(border_style);

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn build_info_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if app.fans.is_empty() {
        lines.push(Line::from(Span::raw("Waiting for fan data...")));
        return lines;
    }

    // Per-fan: RPM range, full speed peak, sensor mappings
    for fan in &app.fans {
        let mut parts: Vec<String> = Vec::new();

        if let (Some(min_rpm), Some(max_rpm)) = (fan.min_rpm, fan.max_rpm) {
            let mut rpm_text = format!("{}\u{2013}{} RPM", min_rpm, max_rpm);
            if let Some(&peak) = app.full_speed_rpm.get(&fan.id) {
                if peak > 0 {
                    rpm_text.push_str(&format!(" (full speed: {})", peak));
                }
            }
            parts.push(rpm_text);
        }

        let sensor_ids: Vec<u32> = fan.curves.iter().map(|c| c.sensor_id).collect();
        if !sensor_ids.is_empty() {
            let suffix = if sensor_ids.len() > 1 { "s" } else { "" };
            let ids: Vec<String> = sensor_ids.iter().map(|s| s.to_string()).collect();
            parts.push(format!("Sensor{} {}", suffix, ids.join(", ")));
        }

        if !parts.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}: ", fan.label),
                    Style::default().fg(VIRIDIS_TITLE),
                ),
                Span::raw(parts.join("  \u{2502}  ")),
            ]));
        }
    }

    // Detect fans with multiple sensor inputs
    for fan in &app.fans {
        let mut unique_sensors: Vec<u32> = fan.curves.iter().map(|c| c.sensor_id).collect();
        unique_sensors.sort();
        unique_sensors.dedup();
        if unique_sensors.len() > 1 {
            lines.push(Line::from(vec![
                Span::styled(
                    fan.label.to_string(),
                    Style::default().fg(VIRIDIS_CUSTOM),
                ),
                Span::raw(" responds to multiple sensors (EC uses max)"),
            ]));
        }
    }

    // Mode-aware status line
    let held_count = app
        .curve_editors
        .values()
        .filter(|e| e.held)
        .count();

    match app.smart_fan_mode {
        Some(255) => {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("Custom mode active -- {} curve(s) held", held_count),
                    Style::default().fg(VIRIDIS_CUSTOM).bold(),
                ),
            ]));
        }
        Some(mode) => {
            let label = smart_fan_mode_label(Some(mode));
            if held_count > 0 {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{label} mode -- {held_count} curve(s) held, will re-apply"),
                        Style::default().fg(VIRIDIS_CUSTOM),
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(format!("{label} mode"), Style::default().fg(VIRIDIS_BIOS)),
                    Span::raw(" -- BIOS fan control active"),
                ]));
            }
        }
        None => {}
    }

    lines.push(Line::from(vec![
        Span::styled("Fn+Q ", Style::default().fg(Color::DarkGray)),
        Span::raw("cycles Quiet/Balanced/Performance (~1.5s delay)"),
    ]));

    lines
}

fn draw_info_panel(f: &mut Frame, lines: Vec<Line<'static>>, area: Rect) {
    let info = Paragraph::new(lines).block(
        Block::bordered()
            .title(" Info ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(VIRIDIS_BORDER)),
    );
    f.render_widget(info, area);
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let is_editing = matches!(app.mode, Mode::CurveEdit { .. });

    let key_color = if is_editing { VIRIDIS_SELECTED } else { VIRIDIS_TITLE };

    let help_text = if is_editing {
        vec![
            Line::from(vec![
                Span::styled(" j/k ", Style::default().fg(key_color).bold()),
                Span::raw("Select step  "),
                Span::styled(" h/l +/- ", Style::default().fg(key_color).bold()),
                Span::raw("Adjust value  "),
            ]),
            Line::from(vec![
                Span::styled(" Enter/a ", Style::default().fg(key_color).bold()),
                Span::raw("Apply  "),
                Span::styled(" s ", Style::default().fg(key_color).bold()),
                Span::raw("Save  "),
                Span::styled(" r ", Style::default().fg(key_color).bold()),
                Span::raw("Reset  "),
                Span::styled(" Esc ", Style::default().fg(key_color).bold()),
                Span::raw("Cancel"),
            ]),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled(" j/k ", Style::default().fg(key_color).bold()),
                Span::raw("Select fan  "),
                Span::styled(" Tab ", Style::default().fg(key_color).bold()),
                Span::raw("Cycle sensor  "),
                Span::styled(" Enter ", Style::default().fg(key_color).bold()),
                Span::raw("Edit curve"),
            ]),
            Line::from(vec![
                Span::styled(" a ", Style::default().fg(key_color).bold()),
                Span::raw("Apply  "),
                Span::styled(" s ", Style::default().fg(key_color).bold()),
                Span::raw("Save  "),
                Span::styled(" r ", Style::default().fg(key_color).bold()),
                Span::raw("Reset  "),
                Span::styled(" F ", Style::default().fg(key_color).bold()),
                Span::raw("Full speed  "),
                Span::styled(" q ", Style::default().fg(key_color).bold()),
                Span::raw("Quit"),
            ]),
        ]
    };

    let help = Paragraph::new(help_text).block(
        Block::bordered()
            .title(if is_editing {
                " Edit Curve "
            } else {
                " Keys "
            })
            .border_type(BorderType::Rounded)
            .border_style(if is_editing {
                Style::default().fg(VIRIDIS_SELECTED)
            } else {
                Style::default().fg(VIRIDIS_BORDER)
            }),
    );
    f.render_widget(help, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let is_error = app.status.starts_with("Error")
        || app.status.contains("failed")
        || app.status.contains("Failed");
    let status_style = if is_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(VIRIDIS_TITLE)
    };
    let status = Paragraph::new(Span::styled(&app.status, status_style)).block(
        Block::bordered()
            .title(" Status ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(VIRIDIS_BORDER)),
    );
    f.render_widget(status, area);
}
