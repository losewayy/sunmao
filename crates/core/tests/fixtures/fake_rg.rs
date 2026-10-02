//! Fake ripgrep for tool/search.rs tests — behavior keyed off the search
//! pattern operand (the arg after `--`):
//!   "SLEEP"  → sleep 30s (the tool's timeout is expected to kill us first)
//!   "BOOM"   → write an error to stderr, exit 2 (rg's real-error code)
//!   "MATCH"  → write a file:line:match line to stdout, exit 0
//!   anything else → exit 1 (rg's no-match code)

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pat = args
        .iter()
        .position(|a| a == "--")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
        .unwrap_or("");
    match pat {
        "SLEEP" => {
            std::thread::sleep(std::time::Duration::from_secs(30));
            std::process::exit(0);
        }
        "BOOM" => {
            eprintln!("regex parse error:\n    (unclosed group");
            std::process::exit(2);
        }
        "MATCH" => {
            println!("f.txt:1:hit");
            std::process::exit(0);
        }
        _ => std::process::exit(1),
    }
}
