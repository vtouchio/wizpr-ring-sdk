//! Pending-connect demo: reconnect to a remembered ring without scanning.
//!
//! Usage:
//!   cargo run --example known_ring              # scan 10s and print device ids
//!   cargo run --example known_ring <device-id>  # pending connect to that id
//!
//! With a device id, the connect request is handed to the OS with no timeout:
//! power the ring off before running this, then power it on — the connection
//! completes as soon as the ring wakes up, with no scanning involved.

use std::time::Duration;

use wizpr_ring::{RingEvent, RingScanner};

#[tokio::main]
async fn main() -> wizpr_ring::Result<()> {
    let scanner = RingScanner::new().await?;

    let Some(device_id) = std::env::args().nth(1) else {
        println!("No device id given — scanning 10s for rings…");
        let devices = scanner.scan(Duration::from_secs(10)).await?;
        if devices.is_empty() {
            println!("No rings found.");
        }
        for device in devices {
            println!("  {}  {}", device.id(), device.name().unwrap_or("(unnamed)"));
        }
        println!("Re-run with one of the ids above to test pending connect.");
        return Ok(());
    };

    let known = scanner.known_ring(&device_id).await?;
    println!("Retrieved known ring {} — issuing pending connect.", known.id());
    println!("(If the ring is powered off, power it on now — waiting indefinitely…)");

    let conn = known.connect().await?;
    println!("Connected! Listening for events — Ctrl-C to quit.");

    let mut events = conn.events()?;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(RingEvent::BatteryUpdate { voltage, level }) => {
                    println!("battery: {level}% ({voltage:.2} V)");
                }
                Some(other) => println!("event: {other:?}"),
                None => {
                    println!("Event stream closed (disconnected).");
                    break;
                }
            },
            _ = tokio::signal::ctrl_c() => {
                println!("Cancelling / disconnecting…");
                conn.disconnect().await?;
                break;
            }
        }
    }
    Ok(())
}
