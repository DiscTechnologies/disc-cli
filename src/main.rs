mod cli;
mod command;
mod config;
mod http;
mod output;
mod ws;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use cli::Cli;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    exit_code(run(cli).await)
}

fn exit_code(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    command::run(cli).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{ConfigCommand, RootCommand};

    #[test]
    fn exit_codes_reflect_command_results() {
        assert_eq!(exit_code(Ok(())), ExitCode::SUCCESS);
        assert_eq!(exit_code(Err(anyhow::anyhow!("failed"))), ExitCode::FAILURE);
    }

    #[tokio::test]
    async fn run_dispatches_a_parsed_cli() {
        let cli = Cli {
            api_key: None,
            http_base_url: None,
            ws_url: None,
            client_id: None,
            command: RootCommand::Config(ConfigCommand::Show),
        };

        run(cli).await.expect("config show");
    }
}
