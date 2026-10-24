use clap::Parser;
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match ouro_console::cli::run(ouro_console::cli::Cli::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
