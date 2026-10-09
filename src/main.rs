fn main() -> std::process::ExitCode {
    flatshot::mem::tune_allocator();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match flatshot::cli::parse(&argv) {
        flatshot::cli::Parsed::Exit(text) => {
            print!("{text}");
            std::process::ExitCode::SUCCESS
        }
        flatshot::cli::Parsed::Error(text) => {
            eprintln!("{text}");
            std::process::ExitCode::from(2)
        }
        flatshot::cli::Parsed::Run(args) => {
            if args.verbose {
                flatshot::timing::enable();
            }
            if args.self_test {
                return std::process::ExitCode::from(flatshot::selftest::run() as u8);
            }
            std::process::ExitCode::from(flatshot::app::run_once(&args) as u8)
        }
    }
}
