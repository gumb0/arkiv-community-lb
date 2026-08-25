//! Thin binary over the `lb` library.

use std::{error::Error as _, path::Path, process};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".into());
    match lb::config::Config::load(Path::new(&path)) {
        Ok(config) => {
            println!(
                "arkiv-lb: {path} valid — {} provider(s) configured; nothing served yet",
                config.providers.len()
            );
        }
        Err(error) => {
            eprint!("arkiv-lb: {error}");
            let mut source = error.source();
            while let Some(cause) = source {
                eprint!(": {cause}");
                source = cause.source();
            }
            eprintln!();
            process::exit(1);
        }
    }
}
