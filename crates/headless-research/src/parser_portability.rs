//! Execute the same bounded parser diagnostic without a Windows dependency.
#[path = "../../../src/parser_candidate.rs"]
mod parser_candidate;

fn main() {
    let report = parser_candidate::run_parser_candidate_probe();
    println!("{report:#}");
    if report.get("passed").and_then(serde_json::Value::as_bool) != Some(true) {
        std::process::exit(1);
    }
}
