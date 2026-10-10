#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod model;
mod monitor;
mod window;
mod worker;
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
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
        println!("GUI by default; --tray starts hidden; --once or --samples N prints readings.");
    } else if let Err(error) = window::run(args.iter().any(|a| a == "--tray")) {
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
