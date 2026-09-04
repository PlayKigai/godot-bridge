#![allow(dead_code)]

use clap::{Args, Parser, Subcommand};
use std::process::ExitCode;

mod dap;
mod doc;
mod docs_state;
mod framing;
mod godot_bin;
mod lsp;
mod open_editor;
mod process;
mod root;
mod run;
mod scene;
mod settings_file;
mod state;
mod status;
mod symbols;

#[derive(Parser)]
#[command(name = "godot-bridge")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct FileArgs {
    #[arg(long)]
    file: String,
    #[arg(trailing_var_arg = true)]
    extra_args: Vec<String>,
}

#[derive(Subcommand)]
enum Command {
    Lsp {
        #[arg(trailing_var_arg = true)]
        extra_args: Vec<String>,
    },
    Dap {
        #[arg(long)]
        file: Option<String>,
        #[arg(trailing_var_arg = true)]
        extra_args: Vec<String>,
    },
    ProjectDir(FileArgs),
    Run {
        #[arg(long)]
        file: String,
        #[arg(long)]
        scene: Option<String>,
        #[arg(trailing_var_arg = true)]
        extra_args: Vec<String>,
    },
    OpenEditor(FileArgs),
    Status {
        #[arg(trailing_var_arg = true)]
        extra_args: Vec<String>,
    },
    Doc {
        symbol: String,
        #[arg(trailing_var_arg = true)]
        extra_args: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("GODOT_BRIDGE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::Lsp { extra_args } => match lsp::run(extra_args).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("lsp: {error:#}");
                ExitCode::from(1)
            }
        },
        Command::Dap { file, extra_args } => {
            match dap::run(file.map(std::path::PathBuf::from), extra_args).await {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("dap: {error:#}");
                    ExitCode::from(1)
                }
            }
        }
        Command::Status { .. } => match status::run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("status: {error:#}");
                ExitCode::from(1)
            }
        },
        Command::Run { file, scene, .. } => {
            match run::run(std::path::Path::new(&file), scene.as_deref()) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("run: {error:#}");
                    ExitCode::from(1)
                }
            }
        }
        Command::ProjectDir(args) => match run::project_dir(std::path::Path::new(&args.file)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error:#}");
                ExitCode::from(1)
            }
        },
        Command::OpenEditor(args) => {
            match open_editor::run(std::path::Path::new(&args.file), args.extra_args).await {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("open-editor: {error:#}");
                    ExitCode::from(1)
                }
            }
        }
        Command::Doc { symbol, .. } => match doc::open_doc(&symbol) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("doc: {error:#}");
                ExitCode::from(1)
            }
        },
    }
}
