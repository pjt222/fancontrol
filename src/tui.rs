use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::fan::Fan;
use crate::platform::create_controller;

/// Messages from the background poller to the UI thread.
enum PollMsg {
    FanData(Vec<Fan>),
    Error(String),
}

/// State for a controllable fan's PWM slider.
struct FanSlider {
    fan_id: String,
    label: String,
    pwm: u8,
}

struct App {
    fans: Vec<Fan>,
    sliders: Vec<FanSlider>,
    selected: usize,
    status: String,
    editing: bool,
    quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            fans: Vec::new(),
            sliders: Vec::new(),
            selected: 0,
            status: "Loading...".into(),
            editing: false,
            quit: false,
        }
    }

    fn update_fans(&mut self, fans: Vec<Fan>) {
        // Rebuild sliders if the fan set changed
        let fan_ids: Vec<&str> = fans.iter().map(|f| f.id.as_str()).collect();
        let slider_ids: Vec<&str> = self.sliders.iter().map(|s| s.fan_id.as_str()).collect();
        if fan_ids != slider_ids {
            self.sliders = fans
                .iter()
                .map(|f| FanSlider {
                    fan_id: f.id.clone(),
                    label: f.label.clone(),
                    pwm: f.pwm.unwrap_or(0),
                })
                .collect();
            if self.selected >= self.sliders.len() && !self.sliders.is_empty() {
                self.selected = self.sliders.len() - 1;
            }
        } else {
            // Update PWM readback for fans we're NOT actively editing
            for (slider, fan) in self.sliders.iter_mut().zip(fans.iter()) {
                if !self.editing {
                    slider.pwm = fan.pwm.unwrap_or(0);
                }
            }
        }
        self.fans = fans;
        self.status = "OK".into();
    }
}

pub fn run() -> Result<()> {
    // Initialize controller BEFORE entering raw mode so failures don't leave
    // the terminal in a broken state.
    let controller = create_controller()?;

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;

    let result = run_inner(controller);

    // Always restore terminal, even on error.
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    result
}

fn run_inner(controller: Box<dyn crate::platform::FanController>) -> Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    // Background poller with stop flag for clean shutdown.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_poller = stop.clone();

    let (tx, rx) = mpsc::channel::<PollMsg>();
    let poll_handle = thread::spawn(move || {
        let ctrl = match create_controller() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(PollMsg::Error(format!("Init error: {e}")));
                return;
            }
        };
        while !stop_poller.load(std::sync::atomic::Ordering::Relaxed) {
            match ctrl.discover() {
                Ok(fans) => {
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
            // Sleep in short increments so the stop flag is checked promptly.
            for _ in 0..15 {
                if stop_poller.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    });

    let mut app = App::new();
    let tick_rate = Duration::from_millis(100);

    loop {
        // Drain poll messages
        while let Ok(msg) = rx.try_recv() {
            match msg {
                PollMsg::FanData(fans) => app.update_fans(fans),
                PollMsg::Error(e) => app.status = format!("Error: {e}"),
            }
        }

        terminal.draw(|f| draw_ui(f, &app))?;

        if app.quit {
            break;
        }

        // Handle input with timeout
        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') if !app.editing => app.quit = true,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.quit = true
                    }
                    KeyCode::Up | KeyCode::Char('k') if !app.editing => {
                        if app.selected > 0 {
                            app.selected -= 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') if !app.editing => {
                        if !app.sliders.is_empty() && app.selected < app.sliders.len() - 1 {
                            app.selected += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if !app.sliders.is_empty() {
                            let sel = app.selected;
                            let fan = &app.fans.get(sel);
                            if fan.map(|f| f.controllable).unwrap_or(false) {
                                app.editing = !app.editing;
                                if !app.editing {
                                    // Commit the PWM value
                                    let slider = &app.sliders[sel];
                                    match controller.set_pwm(&slider.fan_id, slider.pwm) {
                                        Ok(()) => {
                                            app.status = format!(
                                                "Set {} PWM to {}",
                                                slider.label, slider.pwm
                                            );
                                        }
                                        Err(e) => {
                                            app.status = format!("Error: {e}");
                                        }
                                    }
                                }
                            } else {
                                app.status = "Fan is read-only".into();
                            }
                        }
                    }
                    KeyCode::Esc if app.editing => {
                        app.editing = false;
                        // Revert slider to actual readback
                        if let Some(fan) = app.fans.get(app.selected) {
                            app.sliders[app.selected].pwm = fan.pwm.unwrap_or(0);
                        }
                    }
                    KeyCode::Right | KeyCode::Char('l') if app.editing => {
                        let s = &mut app.sliders[app.selected];
                        s.pwm = s.pwm.saturating_add(5);
                    }
                    KeyCode::Left | KeyCode::Char('h') if app.editing => {
                        let s = &mut app.sliders[app.selected];
                        s.pwm = s.pwm.saturating_sub(5);
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') if app.editing => {
                        let s = &mut app.sliders[app.selected];
                        s.pwm = s.pwm.saturating_add(1);
                    }
                    KeyCode::Char('-') if app.editing => {
                        let s = &mut app.sliders[app.selected];
                        s.pwm = s.pwm.saturating_sub(1);
                    }
                    KeyCode::Home if app.editing => {
                        app.sliders[app.selected].pwm = 0;
                    }
                    KeyCode::End if app.editing => {
                        app.sliders[app.selected].pwm = 255;
                    }
                    _ => {}
                }
            }
        }
    }

    // Signal poller to stop and wait for it.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = poll_handle.join();
    Ok(())
}

fn draw_ui(f: &mut Frame, app: &App) {
    let area = f.area();

    let layout = Layout::vertical([
        Constraint::Length(3), // Title
        Constraint::Min(6),    // Fan list
        Constraint::Length(5), // Help
        Constraint::Length(3), // Status bar
    ])
    .split(area);

    // Title
    let title = Block::bordered()
        .title(" Fan Control TUI ")
        .title_alignment(Alignment::Center)
        .border_type(BorderType::Rounded);
    let title_text = Paragraph::new(Line::from(vec![
        Span::styled("Fan Control", Style::default().fg(Color::Cyan).bold()),
        Span::raw(" — Interactive Dashboard"),
    ]))
    .alignment(Alignment::Center)
    .block(title);
    f.render_widget(title_text, layout[0]);

    // Fan list
    draw_fan_list(f, app, layout[1]);

    // Help
    draw_help(f, app, layout[2]);

    // Status bar
    let status_style = if app.status.starts_with("Error") {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    let status = Paragraph::new(Span::styled(&app.status, status_style)).block(
        Block::bordered()
            .title(" Status ")
            .border_type(BorderType::Rounded),
    );
    f.render_widget(status, layout[3]);
}

fn draw_fan_list(f: &mut Frame, app: &App, area: Rect) {
    if app.fans.is_empty() {
        let msg = Paragraph::new("No fans detected. Waiting for data...")
            .alignment(Alignment::Center)
            .block(
                Block::bordered()
                    .title(" Fans ")
                    .border_type(BorderType::Rounded),
            );
        f.render_widget(msg, area);
        return;
    }

    let full_speed = app.fans.iter().any(|f| f.full_speed_active);

    // Build rows
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
                Cell::from(""),
            ])
            .height(1),
        );
    }

    for (i, fan) in app.fans.iter().enumerate() {
        let is_selected = i == app.selected;
        let slider = &app.sliders[i];

        let marker = if is_selected { ">" } else { " " };

        let rpm_text = format!("{} RPM", fan.speed_rpm);

        let pwm_text = if app.editing && is_selected {
            format!("[{:>3}]", slider.pwm)
        } else {
            fan.pwm
                .map(|p| format!("{:>3}", p))
                .unwrap_or_else(|| " \u{2014}".into())
        };

        let status = if fan.controllable { "ctrl" } else { "r/o" };

        // PWM bar visualization
        let bar_width = 20;
        let filled = (slider.pwm as usize * bar_width) / 255;
        let bar: String = format!(
            "[{}{}]",
            "\u{2588}".repeat(filled),
            "\u{2591}".repeat(bar_width - filled),
        );

        let row_style = if is_selected {
            if app.editing {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::Cyan).bold()
            }
        } else {
            Style::default()
        };

        rows.push(
            Row::new(vec![
                Cell::from(marker),
                Cell::from(fan.label.clone()),
                Cell::from(rpm_text),
                Cell::from(format!("{} {}", pwm_text, bar)),
                Cell::from(status),
            ])
            .style(row_style),
        );
    }

    let widths = [
        Constraint::Length(2),
        Constraint::Length(20),
        Constraint::Length(12),
        Constraint::Min(30),
        Constraint::Length(6),
    ];

    let header = Row::new(vec!["", "FAN", "SPEED", "PWM", "TYPE"])
        .style(Style::default().fg(Color::DarkGray).bold())
        .bottom_margin(1);

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::bordered()
                .title(" Fans ")
                .border_type(BorderType::Rounded),
        )
        .column_spacing(1);

    f.render_widget(table, area);
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let help_text = if app.editing {
        vec![
            Line::from(vec![
                Span::styled(
                    " Left/Right (h/l) ",
                    Style::default().fg(Color::Yellow).bold(),
                ),
                Span::raw("Adjust PWM \u{00b1}5  "),
                Span::styled(" +/- ", Style::default().fg(Color::Yellow).bold()),
                Span::raw("Fine \u{00b1}1  "),
                Span::styled(" Home/End ", Style::default().fg(Color::Yellow).bold()),
                Span::raw("Min/Max"),
            ]),
            Line::from(vec![
                Span::styled(" Enter ", Style::default().fg(Color::Yellow).bold()),
                Span::raw("Apply  "),
                Span::styled(" Esc ", Style::default().fg(Color::Yellow).bold()),
                Span::raw("Cancel"),
            ]),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled(" Up/Down (j/k) ", Style::default().fg(Color::Cyan).bold()),
                Span::raw("Select fan  "),
                Span::styled(" Enter ", Style::default().fg(Color::Cyan).bold()),
                Span::raw("Edit PWM  "),
                Span::styled(" q ", Style::default().fg(Color::Cyan).bold()),
                Span::raw("Quit"),
            ]),
            Line::from(vec![
                Span::styled(" Ctrl+C ", Style::default().fg(Color::Cyan).bold()),
                Span::raw("Force quit"),
            ]),
        ]
    };

    let help = Paragraph::new(help_text).block(
        Block::bordered()
            .title(if app.editing { " Edit Mode " } else { " Keys " })
            .border_type(BorderType::Rounded)
            .border_style(if app.editing {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            }),
    );
    f.render_widget(help, area);
}
