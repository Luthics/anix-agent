use std::process::ExitCode;

use clap::Parser;

use anix_agent::cli::{Cli, SubCommand};

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(SubCommand::ApproveWrap {
        ipc,
        worker_id,
        wrapped,
    }) = cli.command
    {
        return anix_agent::approve::run_approve_wrap(&ipc, &worker_id, &wrapped);
    }

    match anix_agent::app::run_interactive(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("anix-agent: {err}");
            ExitCode::FAILURE
        }
    }
}
