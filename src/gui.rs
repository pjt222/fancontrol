//! Graphical fan control interface using egui/eframe.
//!
//! The controller lives on a dedicated worker thread (required because WMI COM
//! objects are `!Send`). Communication happens over `mpsc` channels. The worker
//! auto-polls fan data every 1.5 s via `recv_timeout`.

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::fan::Fan;
use crate::platform::create_controller;

// ---------------------------------------------------------------------------
// Worker ↔ UI protocol
// ---------------------------------------------------------------------------

enum WorkerCommand {
    Refresh,
    SetPwm { fan_id: String, pwm: u8 },
}

enum WorkerResponse {
    FanData(Vec<Fan>),
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

        // Initial discovery.
        match controller.discover() {
            Ok(fans) => { let _ = response_tx.send(WorkerResponse::FanData(fans)); }
            Err(error) => { let _ = response_tx.send(WorkerResponse::Error(error.to_string())); }
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
                    match controller.discover() {
                        Ok(fans) => { let _ = response_tx.send(WorkerResponse::FanData(fans)); }
                        Err(error) => { let _ = response_tx.send(WorkerResponse::Error(error.to_string())); }
                    }
                }
                WorkerCommand::SetPwm { fan_id, pwm } => {
                    match controller.set_pwm(&fan_id, pwm) {
                        Ok(()) => { let _ = response_tx.send(WorkerResponse::PwmSet { fan_id, pwm }); }
                        Err(error) => { let _ = response_tx.send(WorkerResponse::Error(error.to_string())); }
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
    pending_pwm: HashMap<String, (f32, Instant)>,
    dragging: HashMap<String, bool>,
    auto_mode: HashMap<String, bool>,
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
            pending_pwm: HashMap::new(),
            dragging: HashMap::new(),
            auto_mode: HashMap::new(),
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
                        // Initialize auto_mode for newly discovered fans.
                        self.auto_mode.entry(fan.id.clone()).or_insert(true);

                        if let Some(pwm) = fan.pwm {
                            let is_dragging = self.dragging.get(&fan.id).copied().unwrap_or(false);
                            if !is_dragging && !self.pending_pwm.contains_key(&fan.id) {
                                self.slider_values.insert(fan.id.clone(), pwm as f32);
                            }
                        }
                    }
                    self.fans = fans;
                    self.status_message = "OK".into();
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

    fn flush_pending_pwm(&mut self) {
        let cutoff = Duration::from_millis(300);
        let now = Instant::now();

        let ready: Vec<(String, f32)> = self
            .pending_pwm
            .iter()
            .filter(|(_, (_, timestamp))| now.duration_since(*timestamp) >= cutoff)
            .map(|(fan_id, (value, _))| (fan_id.clone(), *value))
            .collect();

        for (fan_id, value) in ready {
            self.pending_pwm.remove(&fan_id);
            let _ = self.command_tx.send(WorkerCommand::SetPwm {
                fan_id,
                pwm: value as u8,
            });
        }
    }
}

impl eframe::App for FanControlApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_responses();
        self.flush_pending_pwm();

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

                // Clone fan list to avoid borrow conflicts with self.
                let fans: Vec<Fan> = self.fans.clone();

                for fan in &fans {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());

                        ui.strong(&fan.label);
                        ui.label(format!("{} RPM", fan.speed_rpm));

                        if fan.controllable {
                            let is_auto = self.auto_mode.get(&fan.id).copied().unwrap_or(true);

                            // Auto mode toggle.
                            ui.horizontal(|ui| {
                                let mut auto_checked = is_auto;
                                if ui.checkbox(&mut auto_checked, "Auto").changed() {
                                    self.auto_mode.insert(fan.id.clone(), auto_checked);
                                    if auto_checked {
                                        // Return to BIOS control.
                                        self.pending_pwm.remove(&fan.id);
                                        let _ = self.command_tx.send(WorkerCommand::SetPwm {
                                            fan_id: fan.id.clone(),
                                            pwm: 0,
                                        });
                                    }
                                }
                            });

                            // PWM slider — disabled in auto mode.
                            if let Some(slider_value) = self.slider_values.get_mut(&fan.id) {
                                let previous_value = *slider_value;

                                ui.horizontal(|ui| {
                                    ui.label("PWM");
                                    ui.add_enabled_ui(!is_auto, |ui| {
                                        let response = ui.add(
                                            egui::Slider::new(slider_value, 1.0..=255.0)
                                                .step_by(1.0)
                                                .fixed_decimals(0),
                                        );

                                        if response.drag_started() {
                                            self.dragging.insert(fan.id.clone(), true);
                                        }

                                        if response.changed() && *slider_value != previous_value {
                                            self.pending_pwm
                                                .insert(fan.id.clone(), (*slider_value, Instant::now()));
                                        }

                                        if response.drag_stopped() {
                                            self.dragging.insert(fan.id.clone(), false);
                                            if let Some((value, _)) = self.pending_pwm.remove(&fan.id) {
                                                let _ = self.command_tx.send(WorkerCommand::SetPwm {
                                                    fan_id: fan.id.clone(),
                                                    pwm: value as u8,
                                                });
                                            }
                                        }
                                    });
                                });
                            }
                        } else {
                            ui.label("read-only");
                        }
                    });

                    ui.add_space(4.0);
                }
            });
        });

        // Request repaint while there are pending PWM values so flush_pending_pwm fires.
        if !self.pending_pwm.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 500.0])
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
