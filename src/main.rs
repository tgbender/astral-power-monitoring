#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod actions;
mod model;
mod monitor;
mod protection;
mod window;
mod worker;
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--cancel-shutdown") {
        if let Err(error) = actions::emergency_cancel() {
            eprintln!("{error}");
            std::process::exit(1);
        }
    } else if args.first().is_some_and(|a| a == "--check-control") {
        match monitor::AstralPowerMonitor::new() {
            Ok(monitor) => {
                let gate = std::sync::Arc::new(actions::Gate::default());
                let controller = actions::RealActions::new(
                    monitor.subsystem,
                    worker::log_path().parent().unwrap().into(),
                    gate,
                );
                println!(
                    "{} — read-only control check\n{}",
                    monitor.name,
                    controller.readiness()
                );
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    } else if args
        .first()
        .is_some_and(|a| a == "--once" || a == "--samples")
    {
        let count = if args[0] == "--once" {
            1
        } else {
            args.get(1)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, 100)
        };
        if let Err(error) = sample(count) {
            eprintln!("{error}");
            std::process::exit(1);
        }
    } else if args.first().is_some_and(|a| a == "--help") {
        println!(
            "GUI starts disarmed; arm manually from the tray. --tray hides the window; --monitor-only disables arming; --once/--samples N reads sensors; --check-control checks NVML without writes; --cancel-shutdown aborts a pending Windows shutdown."
        );
    } else if let Err(error) = window::run(
        args.iter().any(|a| a == "--tray"),
        args.iter().any(|a| a == "--monitor-only"),
    ) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
fn sample(count: usize) -> Result<(), String> {
    let monitor = monitor::AstralPowerMonitor::new()?;
    println!("{} — read-only monitoring", monitor.name);
    for index in 0..count {
        let reading = monitor.read()?;
        println!("Pin       Volts        Amps       Watts");
        for (i, pin) in reading.pins.iter().enumerate() {
            println!(
                "{}        {:6.3}       {:5.2}       {:6.2}",
                i + 1,
                pin.volts,
                pin.amps,
                pin.watts()
            );
        }
        println!(
            "Connector {:.2} W; max pin {:.2} A",
            reading.watts(),
            reading.max_amps()
        );
        if index + 1 < count {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    Ok(())
}
