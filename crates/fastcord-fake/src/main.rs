//! `fastcord-fake [--port N] [--world PATH] [--log PATH] [--chatter SECONDS]`

use std::path::PathBuf;
use std::time::Duration;

use fastcord_fake::{Config, Switches, start};

const USAGE: &str = "usage: fastcord-fake [--port N] [--world PATH] [--log PATH] [--chatter SECONDS]\n       [--zero-retry-429] [--close-gateway-every SECONDS] [--forbid-gateway-discovery]";

fn parse(args: impl Iterator<Item = String>) -> Result<Config, String> {
    let mut config = Config {
        port: 8765,
        world: None,
        log: Some(PathBuf::from("fake-requests.jsonl")),
        echo: true,
        chatter: None,
        switches: Switches::default(),
    };
    let mut args = args;
    while let Some(flag) = args.next() {
        // Switches without a value.
        match flag.as_str() {
            "--zero-retry-429" => {
                config.switches.zero_retry_429 = true;
                continue;
            }
            "--forbid-gateway-discovery" => {
                config.switches.forbid_gateway_discovery = true;
                continue;
            }
            _ => {}
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--port" => config.port = value.parse().map_err(|_| "bad --port".to_owned())?,
            "--world" => config.world = Some(PathBuf::from(value)),
            "--log" => config.log = Some(PathBuf::from(value)),
            "--chatter" => {
                let seconds: u64 = value.parse().map_err(|_| "bad --chatter".to_owned())?;
                if seconds == 0 {
                    return Err("--chatter must be at least 1 second".to_owned());
                }
                config.chatter = Some(Duration::from_secs(seconds));
            }
            "--close-gateway-every" => {
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| "bad --close-gateway-every".to_owned())?;
                if seconds == 0 {
                    return Err("--close-gateway-every must be at least 1 second".to_owned());
                }
                config.switches.close_gateway_every = Some(Duration::from_secs(seconds));
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(config)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let config = match parse(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let log = config.log.clone();
    let handle = match start(config).await {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    println!(
        "fastcord-fake listening on {} (log: {}). Nothing here is real Discord.",
        handle.addr(),
        log.as_deref()
            .map_or("off".into(), |p| p.display().to_string())
    );
    println!(
        "Start the app with: cargo run -p fastcord-app --features fake-discord -- --fake-discord {}",
        handle.port()
    );
    std::future::pending::<()>().await;
}
