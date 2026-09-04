pub const HELP: &str = "\
godot-bridge: GDScript language server and debug adapter bridge for Zed

Usage: godot-bridge <command> [options] [-- ignored args]

Commands:
  lsp                                 proxy Zed's language server over stdio
  dap [--file <path>]                 proxy one Zed debug session over stdio
  project-dir --file <path>           print the resolved Godot project directory
  run --file <path> [--scene current] run the project, or the file's scene
  open-editor --file <path>           hand the project to the Godot GUI editor
  status                              print the state of every running bridge
  doc <symbol>                        open the Godot documentation for a symbol
";

pub enum Command {
    Lsp,
    Dap { file: Option<String> },
    ProjectDir { file: String },
    Run { file: String, scene: Option<String> },
    OpenEditor { file: String },
    Status,
    Doc { symbol: String },
}

pub enum Invocation {
    Help,
    Command(Command),
}

#[derive(Default)]
struct Options {
    file: Option<String>,
    scene: Option<String>,
    positional: Option<String>,
}

pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Invocation, String> {
    let mut arguments = arguments.into_iter();
    let Some(subcommand) = arguments.next() else {
        return Err("missing command".to_owned());
    };
    if subcommand == "-h" || subcommand == "--help" || subcommand == "help" {
        return Ok(Invocation::Help);
    }
    let allowed: &[&str] = match subcommand.as_str() {
        "dap" | "project-dir" | "open-editor" => &["--file"],
        "run" => &["--file", "--scene"],
        _ => &[],
    };
    let options = collect_options(&mut arguments, allowed)?;
    let command = match subcommand.as_str() {
        "lsp" => Command::Lsp,
        "dap" => Command::Dap { file: options.file },
        "project-dir" => Command::ProjectDir {
            file: required(options.file, "--file")?,
        },
        "run" => Command::Run {
            file: required(options.file, "--file")?,
            scene: options.scene,
        },
        "open-editor" => Command::OpenEditor {
            file: required(options.file, "--file")?,
        },
        "status" => Command::Status,
        "doc" => Command::Doc {
            symbol: required(options.positional, "<symbol>")?,
        },
        other => return Err(format!("unrecognized command {other:?}")),
    };
    Ok(Invocation::Command(command))
}

fn collect_options(
    arguments: &mut impl Iterator<Item = String>,
    allowed: &[&str],
) -> Result<Options, String> {
    let mut options = Options::default();
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            break;
        }
        if argument == "--file" && allowed.contains(&"--file") {
            options.file = Some(next_value(&argument, arguments)?);
        } else if argument == "--scene" && allowed.contains(&"--scene") {
            options.scene = Some(next_value(&argument, arguments)?);
        } else if let Some((key, value)) = argument.split_once('=') {
            if key == "--file" && allowed.contains(&"--file") {
                options.file = Some(value.to_owned());
            } else if key == "--scene" && allowed.contains(&"--scene") {
                options.scene = Some(value.to_owned());
            } else {
                return Err(format!("unexpected argument {argument:?}"));
            }
        } else if argument.starts_with('-') {
            return Err(format!("unexpected argument {argument:?}"));
        } else {
            options.positional = Some(argument);
            break;
        }
    }
    Ok(options)
}

fn next_value(flag: &str, arguments: &mut impl Iterator<Item = String>) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn required(value: Option<String>, name: &str) -> Result<String, String> {
    value.ok_or_else(|| format!("{name} is required"))
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, Invocation};

    fn command(arguments: &[&str]) -> Command {
        match parse(arguments.iter().map(|argument| (*argument).to_owned())) {
            Ok(Invocation::Command(command)) => command,
            Ok(Invocation::Help) => panic!("expected a command, got help"),
            Err(message) => panic!("expected a command, got {message}"),
        }
    }

    fn failure(arguments: &[&str]) -> String {
        match parse(arguments.iter().map(|argument| (*argument).to_owned())) {
            Err(message) => message,
            _ => panic!("expected a failure"),
        }
    }

    #[test]
    fn separate_and_joined_file_values_both_parse() {
        let Command::OpenEditor { file } = command(&["open-editor", "--file", "main.gd"]) else {
            panic!("expected open-editor");
        };
        assert_eq!(file, "main.gd");
        let Command::ProjectDir { file } = command(&["project-dir", "--file=main.gd"]) else {
            panic!("expected project-dir");
        };
        assert_eq!(file, "main.gd");
    }

    #[test]
    fn run_takes_a_scene_and_ignores_trailing_arguments() {
        let Command::Run { file, scene } = command(&[
            "run",
            "--file",
            "main.gd",
            "--scene",
            "current",
            "--",
            "--verbose",
        ]) else {
            panic!("expected run");
        };
        assert_eq!(file, "main.gd");
        assert_eq!(scene.as_deref(), Some("current"));
    }

    #[test]
    fn lsp_and_status_ignore_trailing_arguments() {
        assert!(matches!(
            command(&["lsp", "--", "--headless"]),
            Command::Lsp
        ));
        assert!(matches!(command(&["status", "--"]), Command::Status));
    }

    #[test]
    fn dap_file_is_optional() {
        assert!(matches!(command(&["dap"]), Command::Dap { file: None }));
        let Command::Dap { file } = command(&["dap", "--file=main.gd", "--", "-x"]) else {
            panic!("expected dap");
        };
        assert_eq!(file.as_deref(), Some("main.gd"));
    }

    #[test]
    fn doc_takes_a_symbol() {
        let Command::Doc { symbol } = command(&["doc", "Node.ready"]) else {
            panic!("expected doc");
        };
        assert_eq!(symbol, "Node.ready");
    }

    #[test]
    fn help_is_requested_by_flag_and_by_command() {
        assert!(matches!(parse(["--help".to_owned()]), Ok(Invocation::Help)));
        assert!(matches!(parse(["help".to_owned()]), Ok(Invocation::Help)));
    }

    #[test]
    fn bad_invocations_are_rejected() {
        assert_eq!(failure(&[]), "missing command");
        assert_eq!(
            failure(&["frobnicate"]),
            "unrecognized command \"frobnicate\""
        );
        assert_eq!(failure(&["run"]), "--file is required");
        assert_eq!(failure(&["doc"]), "<symbol> is required");
        assert_eq!(failure(&["run", "--file"]), "--file needs a value");
        assert_eq!(
            failure(&["run", "--filet=x"]),
            "unexpected argument \"--filet=x\""
        );
        assert_eq!(
            failure(&["lsp", "--file=x"]),
            "unexpected argument \"--file=x\""
        );
        assert_eq!(failure(&["status", "-x"]), "unexpected argument \"-x\"");
    }
}
