use godot_bridge::cli::{self, Command, Invocation};
use godot_bridge::{dap, doc, lsp, open_editor, run, status};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = std::env::args_os()
        .skip(1)
        .map(|argument| {
            argument
                .into_string()
                .map_err(|argument| format!("argument is not valid UTF-8: {argument:?}"))
        })
        .collect::<Result<Vec<String>, String>>();

    let command = match arguments.and_then(cli::parse) {
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

    let (label, result): (&str, Result<ExitCode, String>) = match command {
        Command::Lsp => ("lsp", lsp::run().await.map_err(|error| error.to_string())),
        Command::Dap { file } => (
            "dap",
            dap::run(file.map(PathBuf::from))
                .await
                .map_err(|error| error.to_string()),
        ),
        Command::Status => (
            "status",
            status::run()
                .await
                .map(|()| ExitCode::SUCCESS)
                .map_err(|error| error.to_string()),
        ),
        Command::Run { file, scene } => (
            "run",
            run::run(Path::new(&file), scene.as_deref()).map_err(|error| error.to_string()),
        ),
        Command::ProjectDir { file } => (
            "",
            run::project_dir(Path::new(&file))
                .map(|()| ExitCode::SUCCESS)
                .map_err(|error| error.to_string()),
        ),
        Command::OpenEditor { file } => (
            "open-editor",
            open_editor::run(Path::new(&file))
                .await
                .map_err(|error| error.to_string()),
        ),
        Command::Doc { symbol } => (
            "doc",
            doc::open_doc(&symbol)
                .map(|()| ExitCode::SUCCESS)
                .map_err(|error| error.to_string()),
        ),
    };

    match result {
        Ok(code) => code,
        Err(error) => {
            if label.is_empty() {
                eprintln!("{error}");
            } else {
                eprintln!("{label}: {error}");
            }
            ExitCode::from(1)
        }
    }
}
