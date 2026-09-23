use clap::Parser;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match dataexplorer::cli::run(dataexplorer::cli::Args::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "dataexplorer: {}",
                dataexplorer::safe_text(&format!("{error:#}"))
            );
            std::process::ExitCode::from(error.code)
        }
    }
}
