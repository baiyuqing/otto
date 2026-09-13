//! Otto command line entry point (Rust port, phase 0 skeleton).

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        println!("otto (rust) {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }
    eprintln!("usage: otto --version");
    std::process::ExitCode::from(2)
}
