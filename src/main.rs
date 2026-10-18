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
            if args.print_config {
                println!("{}", flatshot::config::load().dump());
                return std::process::ExitCode::SUCCESS;
            }
            if args.diagnose {
                print!("{}", flatshot::diagnose::report());
                return std::process::ExitCode::SUCCESS;
            }
            if args.tray {
                flatshot::app::replace_older_tray();
            }
            // A tray app running takes it (it's ready, and owns the shortcuts).
            if let Some(command) = args.forwardable()
                && flatshot::ipc::send(&command)
            {
                return std::process::ExitCode::SUCCESS;
            }
            if args.quit {
                return std::process::ExitCode::SUCCESS;
            }
            // Ctrl+C and logging out as events: a recording is saved first.
            flatshot::signals::take();
            let code = if args.tray {
                flatshot::app::run_tray(&args)
            } else if args.image.is_some() || args.from_clipboard {
                flatshot::app::run_editor(&args)
            } else if args.settings {
                flatshot::app::run_settings()
            } else if args.recent {
                flatshot::app::run_recent()
            } else {
                flatshot::app::run_once(&args)
            };
            std::process::ExitCode::from(code as u8)
        }
    }
}
