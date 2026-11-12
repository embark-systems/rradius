use clap::Parser;
use rradius_bench::cli::{Cli, Command};
use serde::Serialize;

/// Results are 0, 1 and 2; this is for not having got as far as results.
const EXIT_SETUP: i32 = 3;

fn json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}

fn run(cli: Cli) -> Result<i32, String> {
    match cli.command {
        Command::Run(args) => {
            let report = rradius_bench::run::execute(&args)?;
            if args.json {
                println!("{}", json(&report)?);
            } else {
                print!("{}", report.to_text());
            }
            Ok(report.exit_code())
        }
        Command::Calibrate(args) => {
            let calibration = rradius_bench::calibrate::calibrate(args.duration)?;
            if args.json {
                println!("{}", json(&calibration)?);
            } else {
                print!("{}", calibration.to_text());
            }
            Ok(0)
        }
    }
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // `--help` and `--version` come this way too.
            let _ = error.print();
            std::process::exit(if error.use_stderr() { EXIT_SETUP } else { 0 });
        }
    };
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("rradius-bench: {error}");
            std::process::exit(EXIT_SETUP);
        }
    }
}
