use std::env;

use exasol_udf_sdk::connect_back::ConnectionObject;
use mongodb_vs::discovery::{InferenceConfig, infer};
use serde_json::json;

fn usage() -> ! {
    eprintln!(
        "usage: MONGODB_URI=<uri> cargo run -p mongodb-vs --example infer_probe -- \
         <database> <collection> [sample-size] [max-depth] [array-elements]"
    );
    std::process::exit(2);
}

fn parse<T: std::str::FromStr>(value: Option<&String>, default: T) -> T {
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.len() < 2 {
        usage();
    }
    let uri = env::var("MONGODB_URI").map_err(|_| "MONGODB_URI is required")?;
    let defaults = InferenceConfig::default();
    let config = InferenceConfig {
        sample_size: parse(args.get(2), defaults.sample_size),
        max_depth: parse(args.get(3), defaults.max_depth),
        max_array_elements: parse(args.get(4), defaults.max_array_elements),
        ..defaults
    };
    let connection = ConnectionObject {
        kind: String::new(),
        address: uri,
        user: String::new(),
        password: String::new(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(infer(&connection, &args[0], &args[1], &config))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "config": config,
            "fingerprint": result.fingerprint,
            "manifest": result.manifest,
            "report": result.report,
        }))?
    );
    Ok(())
}
