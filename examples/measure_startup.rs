//! Local process-start benchmark. No requests and no persistent writes.
use std::{process::Command, time::Instant};

fn main() {
    let executable = std::env::args()
        .nth(1)
        .expect("usage: measure_startup PATH_TO_FFETCH");
    let samples = 50;
    let mut durations = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let output = Command::new(&executable)
            .args(["capabilities", "--jq", ".version", "-r", "--no-store"])
            .output()
            .expect("start ffetch");
        assert!(
            output.status.success(),
            "benchmark command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty());
        durations.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let first = durations[0];
    durations.sort_by(f64::total_cmp);
    println!(
        "{}",
        serde_json::json!({"command":"capabilities --jq .version -r --no-store","samples":samples,"first_sample_ms":first,"p50_ms":durations[samples/2],"p95_ms":durations[(samples*95/100).min(samples-1)],"min_ms":durations[0],"max_ms":durations[samples-1],"note":"process start plus embedded query; warm filesystem possible; no network or browser work"})
    );
}
