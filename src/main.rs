//! shurectl — Interactive TUI configurator for the Shure MVX2U on Linux
//!
//! Usage:
//!   shure               # Connect to device, launch TUI
//!   shure --demo        # Run without a device (for testing)
//!   shure --list        # List detected devices and exit
//!
//! See README.md for udev setup and permissions.

mod app;
mod device;
mod meter;
mod protocol;
mod ui;

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use app::{App, DeviceAction};
use device::Mvx2u;
use meter::{MeterStatus, start_meter};
use protocol::InputMode;

#[derive(Parser)]
#[command(
    name = "shurectl",
    about = "shurectl — TUI configurator for the Shure MVX2U audio interface"
)]
struct Cli {
    /// Run in demo mode without a real device
    #[arg(long, short)]
    demo: bool,

    /// List connected MVX2U devices and exit
    #[arg(long)]
    list: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.list {
        let devs = device::list_devices();
        if devs.is_empty() {
            println!("No Shure MVX2U devices found.");
            println!("Check that the device is plugged in and the udev rule is installed.");
        } else {
            println!("Found {} MVX2U device(s):", devs.len());
            for d in devs {
                println!("{d}");
            }
        }
        return Ok(());
    }

    let (device, demo_mode) = if cli.demo {
        (None, true)
    } else {
        match Mvx2u::open() {
            Ok(d) => (Some(d), false),
            Err(e) => {
                eprintln!("Warning: {e}");
                eprintln!("Launching in demo mode. Use --demo to suppress this warning.");
                (None, true)
            }
        }
    };

    let mut app = App {
        demo_mode,
        ..App::default()
    };

    if let Some(ref dev) = device {
        match dev.get_state() {
            Ok(mut state) => {
                state.serial_number = dev.serial_number.clone();
                app.device_state = state;
                app.set_ok("Connected — state loaded from device.");
            }
            Err(e) => {
                app.set_err(format!("Connected but failed to read state: {e}"));
            }
        }
    } else {
        app.set_ok("Demo mode — changes will not be sent to a device.");
    }

    // Start the input level meter. _meter_stream must stay alive —
    // dropping it stops the capture thread.
    let _meter_stream = if !demo_mode {
        match start_meter(Arc::clone(&app.meter_level), Arc::clone(&app.peak_window)) {
            MeterStatus::Running(s) => Some(s),
            MeterStatus::Failed(e) => {
                app.set_err(format!("Meter unavailable: {e}"));
                None
            }
        }
    } else {
        None
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_event_loop(&mut terminal, &mut app, &device);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }

    Ok(())
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    device: &Option<Mvx2u>,
) -> Result<()> {
    let tick_rate = Duration::from_millis(100);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_default();

        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && let Some(action) = handle_key(app, key.code, key.modifiers)
        {
            apply_action(app, device, action);
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Option<DeviceAction> {
    if matches!(code, KeyCode::Char('q') | KeyCode::Char('Q'))
        || (code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL))
    {
        app.should_quit = true;
        return None;
    }

    if app.help_visible {
        if matches!(code, KeyCode::Char('?') | KeyCode::Esc) {
            app.help_visible = false;
        }
        return None;
    }

    match code {
        KeyCode::Char('?') => {
            app.help_visible = true;
            None
        }
        KeyCode::Char('r') => Some(DeviceAction::Refresh),
        KeyCode::Tab => {
            if mods.contains(KeyModifiers::SHIFT) {
                app.prev_tab();
            } else {
                app.next_tab();
            }
            None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.focus_prev();
            None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.focus_next();
            None
        }
        KeyCode::Left | KeyCode::Char('h') => app.adjust_focused(-1),
        KeyCode::Right | KeyCode::Char('l') => app.adjust_focused(1),
        KeyCode::Enter | KeyCode::Char(' ') => app.toggle_focused(),
        _ => None,
    }
}

fn apply_action(app: &mut App, device: &Option<Mvx2u>, action: DeviceAction) {
    let result = match &action {
        DeviceAction::Refresh => {
            if let Some(dev) = device {
                match dev.get_state() {
                    Ok(mut state) => {
                        state.serial_number = dev.serial_number.clone();
                        app.device_state = state;
                        app.set_ok("State refreshed from device.");
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            } else {
                app.set_ok("Demo mode — no device to refresh.");
                Ok(())
            }
        }
        DeviceAction::SetGain(g) => {
            app.set_ok(format!("Gain → {} dB", g));
            send_if_connected(device, |d| d.set_gain(*g))
        }
        DeviceAction::SetMode(mode) => {
            app.set_ok(format!("Mode → {}", mode));
            send_if_connected(device, |d| d.set_mode(*mode == InputMode::Auto))
        }
        DeviceAction::SetAutoPosition(pos) => {
            app.set_ok(format!("Mic Position → {}", pos));
            send_if_connected(device, |d| d.set_auto_position(pos))
        }
        DeviceAction::SetAutoTone(tone) => {
            app.set_ok(format!("Tone → {}", tone));
            send_if_connected(device, |d| d.set_auto_tone(tone))
        }
        DeviceAction::SetAutoGain(gain) => {
            app.set_ok(format!("Auto Gain → {}", gain));
            send_if_connected(device, |d| d.set_auto_gain(gain))
        }
        DeviceAction::SetMute(m) => {
            app.set_ok(format!("Mute → {}", if *m { "ON" } else { "Off" }));
            send_if_connected(device, |d| d.set_mute(*m))
        }
        DeviceAction::SetPhantom(p) => {
            app.set_ok(format!("Phantom → {}", if *p { "48V ON" } else { "Off" }));
            send_if_connected(device, |d| d.set_phantom(*p))
        }
        DeviceAction::SetLock(locked) => {
            if *locked {
                app.set_ok("Device locked — SET commands will be ignored until unlocked.");
            } else {
                app.set_ok("Device unlocked.");
            }
            send_if_connected(device, |d| d.set_lock(*locked))
        }
        DeviceAction::SetMonitorMix(m) => {
            app.set_ok(format!("Monitor mix → {}%", m));
            send_if_connected(device, |d| d.set_monitor_mix(*m))
        }
        DeviceAction::SetLimiter(en) => {
            app.set_ok(format!("Limiter → {}", if *en { "ON" } else { "Off" }));
            send_if_connected(device, |d| d.set_limiter(*en))
        }
        DeviceAction::SetCompressor(preset) => {
            app.set_ok(format!("Compressor → {}", preset));
            send_if_connected(device, |d| d.set_compressor(preset))
        }
        DeviceAction::SetHpf(freq) => {
            app.set_ok(format!("HPF → {}", freq));
            send_if_connected(device, |d| d.set_hpf(freq))
        }
        DeviceAction::SetEqEnable(en) => {
            app.set_ok(format!("EQ → {}", if *en { "Enabled" } else { "Bypass" }));
            send_if_connected(device, |d| d.set_eq_enable(*en))
        }
        DeviceAction::SetEqBandEnable(band, en) => {
            app.set_ok(format!(
                "EQ Band {} → {}",
                band + 1,
                if *en { "ON" } else { "Off" }
            ));
            send_if_connected(device, |d| d.set_eq_band_enable(*band, *en))
        }
        DeviceAction::SetEqBandGain(band, gain_db) => {
            app.set_ok(format!("EQ Band {} → {:+} dB", band + 1, gain_db));
            send_if_connected(device, |d| d.set_eq_band_gain(*band, *gain_db))
        }
    };

    if let Err(e) = result {
        app.set_err(format!("Device error: {e}"));
    }
}

fn send_if_connected<F>(device: &Option<Mvx2u>, f: F) -> Result<()>
where
    F: FnOnce(&Mvx2u) -> Result<()>,
{
    match device {
        Some(dev) => f(dev),
        None => Ok(()),
    }
}
