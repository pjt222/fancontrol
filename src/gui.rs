// put id:"gui_init", label:"Launch GUI + Worker Thread", output:"command_channel.internal, response_channel.internal"
// put id:"worker_loop", label:"Worker Poll Loop (1.5s)", input:"command_channel.internal", output:"response_channel.internal"
// put id:"worker_refresh", label:"Re-apply held_pwm + Discover", input:"held_pwm.internal", output:"fan_data.internal"
// put id:"ui_render", label:"Render Fan Cards", input:"response_channel.internal"
// put id:"ui_set_pwm", label:"User Sets PWM", output:"command_channel.internal, held_pwm.internal"

//! Graphical fan control interface using egui/eframe.
//!
//! The controller lives on a dedicated worker thread (required because WMI COM
//! objects are `!Send`). Communication happens over `mpsc` channels. The worker
//! auto-polls fan data every 1.5 s via `recv_timeout`.

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use eframe::egui;
use log::{debug, info, warn};

use crate::config;
use crate::fan::{CustomFanCurve, Fan, FanCurve};
use crate::platform::create_controller;

// ---------------------------------------------------------------------------
// Worker <-> UI protocol
// ---------------------------------------------------------------------------

enum WorkerCommand {
    Refresh,
    SetPwm { fan_id: String, pwm: u8 },
    SetCustomCurve(CustomFanCurve),
    ClearCustomCurves,
}

enum WorkerResponse {
    FanData(Vec<Fan>),
    CurveData(HashMap<String, Vec<FanCurve>>),
    PwmSet { fan_id: String, pwm: u8 },
    CustomCurveSet { fan_id: u32, sensor_id: u32 },
    CustomCurvesCleared,
    SmartFanMode(Option<u32>),
    Error(String),
}

// ---------------------------------------------------------------------------
// Worker thread
// ---------------------------------------------------------------------------

fn spawn_worker(
    command_rx: mpsc::Receiver<WorkerCommand>,
    response_tx: mpsc::Sender<WorkerResponse>,
    repaint_ctx: egui::Context,
) {
    thread::spawn(move || {
        let controller = match create_controller() {
            Ok(c) => c,
            Err(e) => {
                let _ = response_tx.send(WorkerResponse::Error(format!(
                    "Failed to initialize fan controller: {e}"
                )));
                repaint_ctx.request_repaint();
                return;
            }
        };
        // Last PWM value set by the user per fan. Re-applied each poll
        // cycle so Fn+Q or other BIOS overrides don't stick.
        let mut held_pwm: HashMap<String, u8> = HashMap::new();

        // Active custom curves. Re-applied each poll cycle (EC fights back).
        let mut held_curves: Vec<CustomFanCurve> = Vec::new();

        // Load saved curves from config on startup.
        let saved_config = config::load_config();
        if !saved_config.custom_curves.is_empty() {
            info!(
                "Applying {} saved custom curves from config",
                saved_config.custom_curves.len()
            );
            for curve in &saved_config.custom_curves {
                if let Err(e) = controller.set_custom_curve(curve) {
                    warn!(
                        "Failed to apply saved curve fan={} sensor={}: {e}",
                        curve.fan_id, curve.sensor_id
                    );
                } else {
                    let _ = response_tx.send(WorkerResponse::CustomCurveSet {
                        fan_id: curve.fan_id,
                        sensor_id: curve.sensor_id,
                    });
                }
            }
            held_curves = saved_config.custom_curves;
        }

        // Read initial SmartFanMode.
        match controller.get_smart_fan_mode() {
            Ok(mode) => {
                let _ = response_tx.send(WorkerResponse::SmartFanMode(mode));
            }
            Err(e) => {
                debug!("Could not read SmartFanMode: {e}");
            }
        }

        // Initial discovery — includes curve data on first call.
        match controller.discover() {
            Ok(ref fans) => {
                let mut curves_map: HashMap<String, Vec<FanCurve>> = HashMap::new();
                for fan in fans {
                    if !fan.curves.is_empty() {
                        curves_map.insert(fan.id.clone(), fan.curves.clone());
                    }
                }
                if !curves_map.is_empty() {
                    let _ = response_tx.send(WorkerResponse::CurveData(curves_map));
                }
                let _ = response_tx.send(WorkerResponse::FanData(fans.clone()));
            }
            Err(error) => {
                let _ = response_tx.send(WorkerResponse::Error(error.to_string()));
            }
        }
        repaint_ctx.request_repaint();

        loop {
            let command = match command_rx.recv_timeout(Duration::from_millis(1500)) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => WorkerCommand::Refresh,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };

            match command {
                WorkerCommand::Refresh => {
                    // Re-apply held PWM values.
                    for (fan_id, pwm) in &held_pwm {
                        debug!("re-applying held PWM: {fan_id}={pwm}");
                        if let Err(error) = controller.set_pwm(fan_id, *pwm) {
                            warn!("re-apply {fan_id}={pwm} failed: {error}");
                        }
                    }
                    // Re-apply held custom curves (EC fights back on mode changes).
                    for curve in &held_curves {
                        debug!(
                            "re-applying custom curve: fan={} sensor={} steps={:?}",
                            curve.fan_id, curve.sensor_id, curve.steps
                        );
                        if let Err(error) = controller.set_custom_curve(curve) {
                            warn!(
                                "re-apply curve fan={} sensor={} failed: {error}",
                                curve.fan_id, curve.sensor_id
                            );
                        }
                    }
                    match controller.discover() {
                        Ok(ref fans) => {
                            for fan in fans {
                                debug!("poll: {} {} RPM pwm={:?}", fan.id, fan.speed_rpm, fan.pwm);
                            }
                            let _ = response_tx.send(WorkerResponse::FanData(fans.clone()));
                        }
                        Err(error) => {
                            warn!("discover failed: {error}");
                            let _ = response_tx.send(WorkerResponse::Error(error.to_string()));
                        }
                    }
                    // Periodically refresh SmartFanMode.
                    if let Ok(mode) = controller.get_smart_fan_mode() {
                        let _ = response_tx.send(WorkerResponse::SmartFanMode(mode));
                    }
                }
                WorkerCommand::SetPwm { fan_id, pwm } => {
                    info!("user SetPwm: {fan_id}={pwm}");
                    match controller.set_pwm(&fan_id, pwm) {
                        Ok(()) => {
                            if pwm == 0 {
                                held_pwm.remove(&fan_id);
                            } else {
                                held_pwm.insert(fan_id.clone(), pwm);
                            }
                            info!("held_pwm updated: {:?}", held_pwm);
                            let _ = response_tx.send(WorkerResponse::PwmSet { fan_id, pwm });
                        }
                        Err(error) => {
                            warn!("SetPwm {fan_id}={pwm} failed: {error}");
                            let _ = response_tx.send(WorkerResponse::Error(error.to_string()));
                        }
                    }
                }
                WorkerCommand::SetCustomCurve(curve) => {
                    info!(
                        "user SetCustomCurve: fan={} sensor={} steps={:?}",
                        curve.fan_id, curve.sensor_id, curve.steps
                    );
                    match controller.set_custom_curve(&curve) {
                        Ok(()) => {
                            let fan_id = curve.fan_id;
                            let sensor_id = curve.sensor_id;
                            // Replace existing held curve for same fan+sensor.
                            held_curves
                                .retain(|c| !(c.fan_id == fan_id && c.sensor_id == sensor_id));
                            held_curves.push(curve);
                            let _ = response_tx
                                .send(WorkerResponse::CustomCurveSet { fan_id, sensor_id });
                        }
                        Err(error) => {
                            warn!("SetCustomCurve failed: {error}");
                            let _ = response_tx.send(WorkerResponse::Error(error.to_string()));
                        }
                    }
                }
                WorkerCommand::ClearCustomCurves => {
                    info!("user ClearCustomCurves");
                    held_curves.clear();
                    // Switch back to default SmartFanMode (Balanced = 0).
                    if let Err(e) = controller.set_smart_fan_mode(0) {
                        warn!("Reset SmartFanMode failed: {e}");
                    }
                    let _ = response_tx.send(WorkerResponse::CustomCurvesCleared);
                }
            }

            repaint_ctx.request_repaint();
        }
    });
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Editable step sliders for a single fan+sensor custom curve.
struct CurveEditor {
    fan_id: u32,
    sensor_id: u32,
    steps: [f32; 10],
}

struct FanControlApp {
    fans: Vec<Fan>,
    slider_values: HashMap<String, f32>,
    /// Curve data per fan, sent once at startup.
    fan_curves: HashMap<String, Vec<FanCurve>>,
    /// Custom curve editors per fan (keyed by fan_id string).
    curve_editors: HashMap<String, Vec<CurveEditor>>,
    /// Current SmartFanMode value.
    smart_fan_mode: Option<u32>,
    /// Whether custom curves are active (applied by worker).
    custom_curves_active: bool,
    status_message: String,
    command_tx: mpsc::Sender<WorkerCommand>,
    response_rx: mpsc::Receiver<WorkerResponse>,
}

impl FanControlApp {
    fn new(
        command_tx: mpsc::Sender<WorkerCommand>,
        response_rx: mpsc::Receiver<WorkerResponse>,
    ) -> Self {
        Self {
            fans: Vec::new(),
            slider_values: HashMap::new(),
            fan_curves: HashMap::new(),
            curve_editors: HashMap::new(),
            smart_fan_mode: None,
            custom_curves_active: false,
            status_message: "Discovering fans...".into(),
            command_tx,
            response_rx,
        }
    }

    fn drain_responses(&mut self) {
        while let Ok(response) = self.response_rx.try_recv() {
            match response {
                WorkerResponse::FanData(fans) => {
                    for fan in &fans {
                        if let Some(pwm) = fan.pwm {
                            self.slider_values
                                .entry(fan.id.clone())
                                .or_insert(pwm as f32);
                        }
                    }
                    self.fans = fans;
                    self.status_message = "OK".into();
                }
                WorkerResponse::CurveData(curves) => {
                    // Initialize curve editors from discovered curves.
                    for (fan_key, fan_curves) in &curves {
                        self.curve_editors
                            .entry(fan_key.clone())
                            .or_insert_with(|| {
                                fan_curves
                                    .iter()
                                    .map(|c| CurveEditor {
                                        fan_id: c.fan_id,
                                        sensor_id: c.sensor_id,
                                        steps: [1.0; 10],
                                    })
                                    .collect()
                            });
                    }
                    self.fan_curves = curves;
                }
                WorkerResponse::PwmSet { fan_id, pwm } => {
                    self.status_message = format!("Set {} PWM to {}", fan_id, pwm);
                }
                WorkerResponse::CustomCurveSet { fan_id, sensor_id } => {
                    self.custom_curves_active = true;
                    self.status_message =
                        format!("Custom curve applied: fan {} sensor {}", fan_id, sensor_id);
                }
                WorkerResponse::CustomCurvesCleared => {
                    self.custom_curves_active = false;
                    self.status_message = "Custom curves cleared, returned to BIOS control".into();
                }
                WorkerResponse::SmartFanMode(mode) => {
                    self.smart_fan_mode = mode;
                }
                WorkerResponse::Error(message) => {
                    self.status_message = format!("Error: {}", message);
                }
            }
        }
    }
}

impl eframe::App for FanControlApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_responses();

        // Top panel — header with SmartFanMode indicator.
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("Fan Control");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(mode) = self.smart_fan_mode {
                        let (mode_label, mode_color) = match mode {
                            0 => ("Balanced", egui::Color32::from_rgb(100, 180, 100)),
                            1 => ("Quiet", egui::Color32::from_rgb(100, 150, 220)),
                            2 => ("Performance", egui::Color32::from_rgb(220, 150, 50)),
                            3 => ("Custom", egui::Color32::from_rgb(180, 100, 220)),
                            _ => ("Unknown", egui::Color32::GRAY),
                        };
                        ui.colored_label(mode_color, format!("SmartFanMode: {mode_label}"));
                    }
                    if self.custom_curves_active {
                        ui.colored_label(
                            egui::Color32::from_rgb(180, 100, 220),
                            "Custom Curves Active",
                        );
                        ui.separator();
                    }
                });
            });
            ui.add_space(4.0);
        });

        // Bottom panel — status bar.
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label("Status:");
                ui.label(&self.status_message);
            });
            ui.add_space(2.0);
        });

        // Central panel — fan cards.
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if self.fans.is_empty() {
                    ui.label("No fans detected.");
                    return;
                }

                // Full speed mode banner.
                if self.fans.iter().any(|f| f.full_speed_active) {
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(180, 40, 40))
                        .inner_margin(8.0)
                        .rounding(4.0)
                        .show(ui, |ui| {
                            ui.colored_label(egui::Color32::WHITE, "FULL SPEED MODE ACTIVE");
                        });
                    ui.add_space(4.0);
                }

                let fans: Vec<Fan> = self.fans.clone();

                for fan in &fans {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());

                        ui.strong(&fan.label);

                        // RPM range from table data.
                        if let (Some(min_rpm), Some(max_rpm)) = (fan.min_rpm, fan.max_rpm) {
                            ui.label(format!("Range: {}\u{2013}{} RPM", min_rpm, max_rpm));
                        }

                        // Actual readback from hardware.
                        ui.horizontal(|ui| {
                            ui.label("Now:");
                            ui.label(format!("{} RPM", fan.speed_rpm));
                            if let Some(pwm) = fan.pwm {
                                ui.separator();
                                ui.label(format!("PWM {}", pwm));
                            }
                        });

                        if fan.controllable {
                            if let Some(slider_value) = self.slider_values.get_mut(&fan.id) {
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::Slider::new(slider_value, 0.0..=255.0)
                                            .step_by(1.0)
                                            .fixed_decimals(0)
                                            .text("PWM"),
                                    );
                                    if ui.button("Set").clicked() {
                                        let _ = self.command_tx.send(WorkerCommand::SetPwm {
                                            fan_id: fan.id.clone(),
                                            pwm: *slider_value as u8,
                                        });
                                    }
                                });
                            }
                        } else {
                            ui.label("read-only");
                        }

                        // Collapsible fan curve section with custom curve editor.
                        if let Some(curves) = self.fan_curves.get(&fan.id) {
                            if !curves.is_empty() {
                                ui.add_space(4.0);
                                egui::CollapsingHeader::new("Fan Curve")
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for curve in curves {
                                            let active_tag =
                                                if curve.active { "Active" } else { "Inactive" };
                                            ui.label(format!(
                                                "Sensor {} [{}] \u{2014} {}\u{2013}{}\u{00B0}C",
                                                curve.sensor_id,
                                                active_tag,
                                                curve.min_temp,
                                                curve.max_temp
                                            ));

                                            egui::Grid::new(format!(
                                                "curve_{}_{}",
                                                curve.fan_id, curve.sensor_id
                                            ))
                                            .striped(true)
                                            .show(
                                                ui,
                                                |ui| {
                                                    ui.strong("Temp");
                                                    ui.strong("RPM");
                                                    ui.end_row();
                                                    for point in &curve.points {
                                                        ui.label(format!(
                                                            "{}\u{00B0}C",
                                                            point.temperature
                                                        ));
                                                        ui.label(format!("{}", point.fan_speed));
                                                        ui.end_row();
                                                    }
                                                },
                                            );

                                            ui.add_space(4.0);
                                        }

                                        // Custom curve editor.
                                        ui.separator();
                                        ui.strong("Custom Curve Editor");
                                        ui.label("Set 10 speed step indices (0\u{2013}10)");

                                        if let Some(editors) = self.curve_editors.get_mut(&fan.id) {
                                            for editor in editors.iter_mut() {
                                                ui.label(format!(
                                                    "Fan {} \u{2192} Sensor {}",
                                                    editor.fan_id, editor.sensor_id
                                                ));

                                                egui::Grid::new(format!(
                                                    "editor_{}_{}",
                                                    editor.fan_id, editor.sensor_id
                                                ))
                                                .show(ui, |ui| {
                                                    for i in 0..10 {
                                                        ui.label(format!("S{i}:"));
                                                        ui.add(
                                                            egui::Slider::new(
                                                                &mut editor.steps[i],
                                                                0.0..=10.0,
                                                            )
                                                            .step_by(1.0)
                                                            .fixed_decimals(0),
                                                        );
                                                        if i == 4 {
                                                            ui.end_row();
                                                        }
                                                    }
                                                    ui.end_row();
                                                });

                                                ui.horizontal(|ui| {
                                                    if ui.button("Apply").clicked() {
                                                        let steps: [u8; 10] =
                                                            std::array::from_fn(|i| {
                                                                editor.steps[i] as u8
                                                            });
                                                        let _ = self.command_tx.send(
                                                            WorkerCommand::SetCustomCurve(
                                                                CustomFanCurve {
                                                                    fan_id: editor.fan_id,
                                                                    sensor_id: editor.sensor_id,
                                                                    steps,
                                                                },
                                                            ),
                                                        );
                                                    }
                                                    if ui.button("Save").clicked() {
                                                        let steps: [u8; 10] =
                                                            std::array::from_fn(|i| {
                                                                editor.steps[i] as u8
                                                            });
                                                        let curve = CustomFanCurve {
                                                            fan_id: editor.fan_id,
                                                            sensor_id: editor.sensor_id,
                                                            steps,
                                                        };
                                                        let _ = self.command_tx.send(
                                                            WorkerCommand::SetCustomCurve(
                                                                curve.clone(),
                                                            ),
                                                        );
                                                        // Persist to config.
                                                        let mut cfg = config::load_config();
                                                        cfg.custom_curves.retain(|c| {
                                                            !(c.fan_id == editor.fan_id
                                                                && c.sensor_id == editor.sensor_id)
                                                        });
                                                        cfg.custom_curves.push(curve);
                                                        if let Err(e) = config::save_config(&cfg) {
                                                            self.status_message =
                                                                format!("Save failed: {e}");
                                                        }
                                                    }
                                                    if ui.button("Reset to BIOS").clicked() {
                                                        let _ = self
                                                            .command_tx
                                                            .send(WorkerCommand::ClearCustomCurves);
                                                        // Clear saved curves from config too.
                                                        let mut cfg = config::load_config();
                                                        cfg.custom_curves.clear();
                                                        let _ = config::save_config(&cfg);
                                                    }
                                                });

                                                ui.add_space(4.0);
                                            }
                                        }
                                    });
                            }
                        }
                    });

                    ui.add_space(4.0);
                }
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 600.0])
            .with_min_inner_size([300.0, 300.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Fan Control",
        options,
        Box::new(|cc| {
            let (command_tx, command_rx) = mpsc::channel();
            let (response_tx, response_rx) = mpsc::channel();

            spawn_worker(command_rx, response_tx, cc.egui_ctx.clone());

            Ok(Box::new(FanControlApp::new(command_tx, response_rx)))
        }),
    )
    .map_err(|error| anyhow::anyhow!("eframe error: {}", error))
}
