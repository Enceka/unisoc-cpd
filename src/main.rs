use clap::Parser;
use std::process::exit;
use unisoc_cpd::channel::ChannelBusy;
use unisoc_cpd::cli::{self, Cli};

/// Accept the rig's spelling `mode vendor|native <capability>` as a synonym for
/// `--mode vendor|native <capability>`.
fn normalize(mut args: Vec<String>) -> Vec<String> {
    if args.len() >= 3 && args[1] == "mode" {
        args.insert(1, "--mode".to_string());
        // after the insert the literal "mode" sits at index 2
        args.remove(2);
    }
    args
}

fn main() {
    let args = normalize(std::env::args().collect());
    let cli = Cli::parse_from(args);
    match cli::run(cli) {
        Ok(code) => exit(code),
        Err(err) => {
            let environment = err.chain().any(|c| c.downcast_ref::<ChannelBusy>().is_some());
            if environment {
                eprintln!("unisoc-cpd: {err:#}");
                exit(3);
            }
            eprintln!("unisoc-cpd: {err:#}");
            exit(2);
        }
    }
}
