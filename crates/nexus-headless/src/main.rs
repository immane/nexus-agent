#![forbid(unsafe_code)]

//! Headless binary: one argv task, fake-wired runtime, sequenced stdout.

use nexus_headless::{FAKE_BANNER, USAGE, outcome_name, run_task};

fn main() {
    eprintln!("{FAKE_BANNER}");
    let task = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if task.trim().is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    match run_task(&task) {
        Ok(report) => {
            for line in &report.lines {
                println!("{line}");
            }
            eprintln!(
                "nexus-headless: run={} outcome={} denied={} tool-started={} tool-finished={}",
                report.run,
                outcome_name(report.outcome),
                report.denied_calls,
                report.tool_started,
                report.tool_finished,
            );
            std::process::exit(report.exit_code);
        }
        Err(message) => {
            eprintln!("nexus-headless: error: {message}");
            std::process::exit(2);
        }
    }
}
