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

use crate::fan::{Fan, FanCurve};
use crate::platform::create_controller;

// ---------------------------------------------------------------------------
// Worker <-> UI protocol
// ---------------------------------------------------------------------------

enum WorkerCommand {
    Refresh,
    SetPwm { fan_id: String, pwm: u8 },
}

enum WorkerResponse {
    FanData(Vec<Fan>),
    CurveData(HashMap<String, Vec<FanCurve>>),
    PwmSet { fan_id: String, pwm: u8 },
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
        let controller = create_controller();
        // Last PWM value set by the user per fan. Re-applied each poll
        // cycle so Fn+Q or other BIOS overrides don't stick.
        let mut held_pwm: HashMap<String, u8> = HashMap::new();

        // Initial discovery — includes curve data on first call.
        match controller.discover() {
            Ok(ref fans) => {
                // Extract curve data from the first discovery and send
                // separately so the UI can cache it without re-querying.
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
            // Wait for a command, or timeout to auto-poll.
            let command = match command_rx.recv_timeout(Duration::from_millis(1500)) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => WorkerCommand::Refresh,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };

            match command {
                WorkerCommand::Refresh => {
                    // Re-apply held PWM values before polling.
                    for (fan_id, pwm) in &held_pwm {
                        debug!("re-applying held PWM: {fan_id}={pwm}");
                        if let Err(error) = controller.set_pwm(fan_id, *pwm) {
                            warn!("re-apply {fan_id}={pwm} failed: {error}");
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
                }
                WorkerCommand::SetPwm { fan_id, pwm } => {
                    info!("user SetPwm: {fan_id}={pwm}");
                    match controller.set_pwm(&fan_id, pwm) {
                        Ok(()) => {
                            if pwm == 0 {
                                // PWM 0 = return to BIOS auto; stop re-applying.
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
            }

            repaint_ctx.request_repaint();
        }
    });
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct FanControlApp {
    fans: Vec<Fan>,
    slider_values: HashMap<String, f32>,
    /// Curve data per fan, sent once at startup.
    fan_curves: HashMap<String, Vec<FanCurve>>,
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
                            self.slider_values.entry(fan.id.clone())
                                .or_insert(pwm as f32);
                        }
                    }
                    self.fans = fans;
                    self.status_message = "OK".into();
                }
                WorkerResponse::CurveData(curves) => {
                    self.fan_curves = curves;
                }
                WorkerResponse::PwmSet { fan_id, pwm } => {
                    self.status_message = format!("Set {} PWM to {}", fan_id, pwm);
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

        // Top panel — header.
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.heading("Fan Control");
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

                        // Collapsible fan curve section.
                        if let Some(curves) = self.fan_curves.get(&fan.id) {
                            if !curves.is_empty() {
                                ui.add_space(4.0);
                                egui::CollapsingHeader::new("Fan Curve")
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for curve in curves {
                                            let active_tag = if curve.active { "Active" } else { "Inactive" };
                                            ui.label(format!(
                                                "Sensor {} [{}] \u{2014} {}\u{2013}{}\u{00B0}C",
                                                curve.sensor_id, active_tag,
                                                curve.min_temp, curve.max_temp
                                            ));

                                            egui::Grid::new(format!(
                                                "curve_{}_{}", curve.fan_id, curve.sensor_id
                                            ))
                                            .striped(true)
                                            .show(ui, |ui| {
                                                ui.strong("Temp");
                                                ui.strong("RPM");
                                                ui.end_row();
                                                for point in &curve.points {
                                                    ui.label(format!("{}\u{00B0}C", point.temperature));
                                                    ui.label(format!("{}", point.fan_speed));
                                                    ui.end_row();
                                                }
                                            });

                                            ui.add_space(4.0);
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
