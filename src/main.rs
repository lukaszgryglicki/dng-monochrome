use dng_monochrome::cli::{self, Cli};
use std::process::ExitCode;

#[cfg(all(target_os = "linux", target_env = "musl"))]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let cli = match Cli::try_parse_compat(std::env::args_os()) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code() as u8;
            if let Err(error) = error.print() {
                eprintln!("error printing CLI help/error: {error}");
                return ExitCode::FAILURE;
            }
            return ExitCode::from(code);
        }
    };
    match cli::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
