use godot_bridge::cli::{self, Command, Invocation};
use godot_bridge::{dap, doc, log, lsp, open_editor, run, status};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    log::init();

    let command = match cli::parse(std::env::args().skip(1)) {
        Ok(Invocation::Help) => {
            print!("{}", cli::HELP);
            return ExitCode::SUCCESS;
        }
        Ok(Invocation::Command(command)) => command,
        Err(message) => {
            eprintln!("godot-bridge: {message}\n\n{}", cli::HELP);
            return ExitCode::from(2);
        }
    };

    match command {
        Command::Lsp => match lsp::run().await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("lsp: {error}");
                ExitCode::from(1)
            }
        },
        Command::Dap { file } => match dap::run(file.map(PathBuf::from)).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("dap: {error}");
                ExitCode::from(1)
            }
        },
        Command::Status => match status::run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("status: {error}");
                ExitCode::from(1)
            }
        },
        Command::Run { file, scene } => match run::run(Path::new(&file), scene.as_deref()) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("run: {error}");
                ExitCode::from(1)
            }
        },
        Command::ProjectDir { file } => match run::project_dir(Path::new(&file)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(1)
            }
        },
        Command::OpenEditor { file } => match open_editor::run(Path::new(&file)).await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("open-editor: {error}");
                ExitCode::from(1)
            }
        },
        Command::Doc { symbol } => match doc::open_doc(&symbol) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("doc: {error}");
                ExitCode::from(1)
            }
        },
    }
}
