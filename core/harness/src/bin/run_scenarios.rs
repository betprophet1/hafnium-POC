//! `run-scenarios PATH...` runs compiled scenarios (files or directories) against the Rust engine.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: run-scenarios PATH...   (compiled scenario files or directories)");
        return ExitCode::from(2);
    }
    let paths = venue_harness::collect_paths(&args);
    let mut failures = 0usize;
    let mut first_failure = None;
    for path in &paths {
        match venue_harness::run_file(path) {
            Ok(_) => println!("ok   {}", path.display()),
            Err(e) => {
                failures += 1;
                println!("FAIL {}", path.display());
                if first_failure.is_none() {
                    first_failure = Some(e);
                }
            }
        }
    }
    if let Some(e) = first_failure {
        println!("\nfirst failure:\n{e}");
    }
    println!("{}/{} pass", paths.len() - failures, paths.len());
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
