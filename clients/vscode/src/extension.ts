import * as cp from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import { LanguageClient } from "vscode-languageclient/node";

const MACHINE_KEYS = ["godotPath", "projectDir", "lspPort", "dapPort", "extraArgs"];
const WORKSPACE_KEYS = ["projectDiagnostics", "diagnoseAddons", "startupTimeoutS"];

const clients = new Map<string, { client: LanguageClient; folder: vscode.WorkspaceFolder; spawnKey: string }>();
let queue: Promise<void> = Promise.resolve();

function serialize(step: () => Promise<unknown>): Promise<void> {
  queue = queue.catch(() => {}).then(async () => {
    await step();
  });
  return queue;
}

const resolvedBridge = new Map<string, string>();

// Bare and relative names are resolved here because Windows would search the project cwd first.
function bridgePath(): string {
  const value = vscode.workspace.getConfiguration("godot").inspect<string>("bridgePath")?.globalValue;
  const configured = (typeof value === "string" && value.trim()) || "godot-bridge";
  const cached = resolvedBridge.get(configured);
  if (cached) {
    return cached;
  }
  const name = configured.replace(/^~(?=[\\/])/, os.homedir());
  const found = name === path.basename(name) ? onPath(name) : path.resolve(os.homedir(), name);
  if (found) {
    resolvedBridge.set(configured, found);
  }
  return found ?? path.join(os.homedir(), ".cargo", "bin", name);
}

function isFile(candidate: string): boolean {
  try {
    return fs.statSync(candidate).isFile();
  } catch {
    return false;
  }
}

function onPath(name: string): string | undefined {
  const extensions = process.platform === "win32" ? ["", ...(process.env.PATHEXT ?? ".EXE").split(";")] : [""];
  for (const entry of (process.env.PATH ?? "").split(path.delimiter)) {
    const dir = entry.replace(/^"|"$/g, "");
    for (const extension of extensions) {
      const candidate = path.join(dir, name + extension);
      if (path.isAbsolute(dir) && isFile(candidate)) {
        return candidate;
      }
    }
  }
  return undefined;
}

function bridgeSettings(folder?: vscode.Uri): Record<string, unknown> {
  const config = vscode.workspace.getConfiguration("godot", folder);
  const settings: Record<string, unknown> = {};
  const add = (key: string, value: unknown) => {
    if (value !== undefined && value !== "" && !(Array.isArray(value) && value.length === 0)) {
      settings[key.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`)] = value;
    }
  };
  for (const key of MACHINE_KEYS) {
    add(key, config.inspect(key)?.globalValue);
  }
  for (const key of WORKSPACE_KEYS) {
    const inspected = config.inspect(key);
    add(key, inspected?.workspaceFolderValue ?? inspected?.workspaceValue ?? inspected?.globalValue);
  }
  return settings;
}

function bridgeEnv(folder?: vscode.Uri): Record<string, string> {
  return { GODOT_BRIDGE_SETTINGS: JSON.stringify(bridgeSettings(folder)) };
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function checkBridge(clientVersion: string): void {
  const resolved = bridgePath();
  cp.execFile(resolved, ["--version"], { timeout: 5000 }, (error, stdout) => {
    if (error) {
      vscode.window
        .showErrorMessage(
          `Godot Bridge: cannot run "${resolved} --version". Install the bridge and put it on PATH or set godot.bridgePath.`,
          "Open install instructions",
        )
        .then((choice) => {
          if (choice) {
            vscode.env.openExternal(vscode.Uri.parse("https://github.com/PlayKigai/godot-bridge#install"));
          }
        });
      return;
    }
    const bridgeVersion = /^godot-bridge (\d+\.\d+)/.exec(stdout)?.[1];
    if (bridgeVersion !== clientVersion.replace(/\.\d+$/, "")) {
      vscode.window.showWarningMessage(
        `Godot Bridge: extension ${clientVersion}, bridge ${bridgeVersion ?? stdout.trim().slice(0, 80)}. Install the matching bridge.`,
      );
    }
  });
}

function globEscape(fsPath: string): string {
  return fsPath.replace(/\\/g, "/").replace(/[[\]{}?*]/g, "[$&]");
}

function spawnKey(settingsJson: string): string {
  return bridgePath() + settingsJson;
}

async function startClient(folder: vscode.WorkspaceFolder, output: vscode.LogOutputChannel): Promise<void> {
  const key = folder.uri.toString();
  if (clients.has(key)) {
    return;
  }
  const env = bridgeEnv(folder.uri);
  const client = new LanguageClient(
    `godot-bridge-${folder.name}`,
    "Godot Bridge",
    {
      command: bridgePath(),
      args: ["lsp"],
      options: { cwd: folder.uri.fsPath, env: { ...process.env, ...env } },
    },
    {
      documentSelector: [
        { scheme: "file", language: "gdscript", pattern: `${globEscape(folder.uri.fsPath)}/**/*` },
      ],
      workspaceFolder: folder,
      outputChannel: output,
    },
  );
  clients.set(key, { client, folder, spawnKey: spawnKey(env.GODOT_BRIDGE_SETTINGS) });
  try {
    await client.start();
  } catch (error) {
    if (clients.get(key)?.client === client) {
      clients.delete(key);
    }
    vscode.window.showErrorMessage(`Godot Bridge failed to start: ${messageOf(error)}`);
  }
}

async function stopClients(folders: Iterable<vscode.WorkspaceFolder>): Promise<void> {
  const stops = [];
  for (const folder of folders) {
    const key = folder.uri.toString();
    const entry = clients.get(key);
    clients.delete(key);
    if (entry) {
      stops.push(entry.client.stop().catch(() => {}));
    }
  }
  await Promise.all(stops);
}

function gdscriptFolder(document: vscode.TextDocument): vscode.WorkspaceFolder | undefined {
  return document.languageId === "gdscript" ? vscode.workspace.getWorkspaceFolder(document.uri) : undefined;
}

function foldersWithGdscript(): vscode.WorkspaceFolder[] {
  const folders = new Set<vscode.WorkspaceFolder>();
  for (const document of vscode.workspace.textDocuments) {
    const folder = gdscriptFolder(document);
    if (folder) {
      folders.add(folder);
    }
  }
  return [...folders];
}

function runningFolders(): vscode.WorkspaceFolder[] {
  return [...clients.values()].map((entry) => entry.folder);
}

function restartClients(pick: () => vscode.WorkspaceFolder[], output: vscode.LogOutputChannel): Promise<void> {
  return serialize(async () => {
    const folders = pick();
    await stopClients(folders);
    await Promise.all(folders.map((folder) => startClient(folder, output)));
  });
}

function foldersWithChangedSettings(): vscode.WorkspaceFolder[] {
  return [...clients.values()]
    .filter((entry) => entry.spawnKey !== spawnKey(bridgeEnv(entry.folder.uri).GODOT_BRIDGE_SETTINGS))
    .map((entry) => entry.folder);
}

function activeSceneOrScript(): string | undefined {
  const file = vscode.window.activeTextEditor?.document.uri.fsPath;
  return file && /\.(gd|tscn)$/.test(file) ? file : undefined;
}

function folderOf(file: string | undefined): vscode.WorkspaceFolder | undefined {
  return file ? vscode.workspace.getWorkspaceFolder(vscode.Uri.file(file)) : vscode.workspace.workspaceFolders?.[0];
}

function runBridgeTask(name: string, args: string[], file: string | undefined): void {
  const folder = folderOf(file);
  const task = new vscode.Task(
    { type: "godot", task: name },
    folder ?? vscode.TaskScope.Workspace,
    name,
    "godot",
    new vscode.ProcessExecution(bridgePath(), args, { cwd: folder?.uri.fsPath, env: bridgeEnv(folder?.uri) }),
  );
  task.presentationOptions = { reveal: vscode.TaskRevealKind.Always, panel: vscode.TaskPanelKind.Dedicated, focus: false };
  vscode.tasks.executeTask(task).then(undefined, (error: unknown) => {
    vscode.window.showErrorMessage(`Godot: failed to start task: ${messageOf(error)}`);
  });
}

const FILE_COMMANDS: [string, string, string, string[]][] = [
  ["runProject", "run project", "run", []],
  ["runCurrentScene", "run current scene", "run", ["--scene", "current"]],
  ["openEditor", "open editor", "open-editor", []],
];

export function activate(context: vscode.ExtensionContext): void {
  if (process.platform === "darwin") {
    vscode.window.showErrorMessage("Godot Bridge is not supported on macOS. Linux and Windows only.");
    return;
  }
  const output = vscode.window.createOutputChannel("Godot Bridge", { log: true });
  checkBridge(context.extension.packageJSON.version);

  context.subscriptions.push(
    output,
    vscode.workspace.onDidOpenTextDocument((document) => {
      const folder = gdscriptFolder(document);
      if (folder) {
        void serialize(() => startClient(folder, output));
      }
    }),
    vscode.workspace.onDidChangeWorkspaceFolders((event) =>
      serialize(async () => {
        await stopClients(event.removed);
        const added = foldersWithGdscript().filter((folder) => event.added.includes(folder));
        await Promise.all(added.map((folder) => startClient(folder, output)));
      }),
    ),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration("godot")) {
        void restartClients(foldersWithChangedSettings, output);
      }
    }),
    vscode.debug.registerDebugAdapterDescriptorFactory("godot", {
      createDebugAdapterDescriptor(session) {
        const file = activeSceneOrScript();
        const folder = session.workspaceFolder ?? folderOf(file);
        return new vscode.DebugAdapterExecutable(bridgePath(), file ? ["dap", "--file", file] : ["dap"], {
          cwd: folder?.uri.fsPath,
          env: bridgeEnv(folder?.uri),
        });
      },
    }),
    ...FILE_COMMANDS.map(([id, label, verb, extra]) =>
      vscode.commands.registerCommand(`godot.${id}`, () => {
        const file = activeSceneOrScript();
        if (file) {
          runBridgeTask(`Godot: ${label}`, [verb, "--file", file, ...extra], file);
        } else {
          vscode.window.showErrorMessage("Godot: open a .gd or .tscn file first.");
        }
      }),
    ),
    vscode.commands.registerCommand("godot.openDocs", () => {
      const editor = vscode.window.activeTextEditor;
      const range = editor?.document.getWordRangeAtPosition(editor.selection.active, /[A-Za-z_][A-Za-z0-9_.]*/);
      if (!editor || !range) {
        vscode.window.showErrorMessage("Godot: no symbol under the cursor.");
        return;
      }
      const symbol = editor.document.getText(range);
      runBridgeTask(`Godot: docs for ${symbol}`, ["doc", symbol], editor.document.uri.fsPath);
    }),
    vscode.commands.registerCommand("godot.showStatus", () => {
      const options = { env: { ...process.env, ...bridgeEnv() }, timeout: 5000 };
      cp.execFile(bridgePath(), ["status"], options, (error, stdout, stderr) => {
        output.append(stdout + stderr + (error ? messageOf(error) + "\n" : ""));
        output.show(true);
      });
    }),
    vscode.commands.registerCommand("godot.restartLanguageServer", () => restartClients(runningFolders, output)),
  );
  void serialize(() => Promise.all(foldersWithGdscript().map((folder) => startClient(folder, output))));
}

export function deactivate(): Promise<void> {
  return serialize(() => stopClients(runningFolders()));
}
