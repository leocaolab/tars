// TARS Review — a VSCode extension that activates the agent-2 reviewer over the workspace and
// renders each finding as an inline comment thread pinned at file:line.
//
// End-to-end flow:
//   command "TARS: Review with agent"  →  spawn the `review-cli` binary over the target .rs files
//   →  it prints a JSON array of Finding  →  parse  →  create a CommentThread per finding.
//
// The Finding JSON shape is the frontend contract emitted by `crates/tars-agent2/src/bin/review-cli.rs`:
//   { "file": string, "line": number | null, "message": string, "status": string, "severity": string }
// `file` is echoed exactly as passed to the CLI; this extension passes absolute paths, so it
// resolves each with `vscode.Uri.file(finding.file)`.

import * as vscode from "vscode";
import { execFile } from "child_process";
import * as path from "path";
import * as fs from "fs";

/** The stable finding shape the review-cli emits (the frontend contract). */
interface Finding {
  file: string;
  line: number | null;
  message: string;
  status: string;
  severity: string;
}

/** The one comment controller for this extension; created on activation. */
let controller: vscode.CommentController | undefined;
/** Live threads, so a re-run / clear can dispose the previous batch. */
let threads: vscode.CommentThread[] = [];

export function activate(context: vscode.ExtensionContext): void {
  controller = vscode.comments.createCommentController("tars-review", "TARS Review");
  context.subscriptions.push(controller);

  context.subscriptions.push(
    vscode.commands.registerCommand("tars.review", () => runReview()),
    vscode.commands.registerCommand("tars.clearReview", () => clearThreads()),
  );
}

export function deactivate(): void {
  clearThreads();
  controller?.dispose();
  controller = undefined;
}

/** Dispose every rendered thread. */
function clearThreads(): void {
  for (const t of threads) {
    t.dispose();
  }
  threads = [];
}

/** Resolve the review-cli binary path: the `tars-review.cliPath` setting, else the workspace's
 *  `target/debug/review-cli`. */
function resolveCliPath(workspaceRoot: string): string {
  const configured = vscode.workspace.getConfiguration("tars-review").get<string>("cliPath");
  if (configured && configured.trim().length > 0) {
    return configured;
  }
  return path.join(workspaceRoot, "target", "debug", "review-cli");
}

/** Collect the target .rs files: the active file only (if the setting is on), else the workspace. */
async function collectTargetFiles(): Promise<string[]> {
  const cfg = vscode.workspace.getConfiguration("tars-review");
  const activeOnly = cfg.get<boolean>("reviewActiveFileOnly") ?? false;
  const maxFiles = cfg.get<number>("maxFiles") ?? 40;

  if (activeOnly) {
    const active = vscode.window.activeTextEditor?.document;
    if (active && active.uri.scheme === "file" && active.fileName.endsWith(".rs")) {
      return [active.fileName];
    }
    return [];
  }

  const uris = await vscode.workspace.findFiles("**/*.rs", "**/target/**", maxFiles);
  return uris.map((u) => u.fsPath);
}

/** Run the reviewer and render findings as comment threads. */
async function runReview(): Promise<void> {
  const workspaceFolder = vscode.workspace.workspaceFolders?.[0];
  if (!workspaceFolder) {
    vscode.window.showErrorMessage("TARS Review: open a folder/workspace first.");
    return;
  }
  const workspaceRoot = workspaceFolder.uri.fsPath;
  const cliPath = resolveCliPath(workspaceRoot);

  if (!fs.existsSync(cliPath)) {
    vscode.window.showErrorMessage(
      `TARS Review: review-cli not found at ${cliPath}. Build it with \`cargo build --bin review-cli\`, ` +
        `or set \`tars-review.cliPath\`.`,
    );
    return;
  }

  const files = await collectTargetFiles();
  if (files.length === 0) {
    vscode.window.showInformationMessage("TARS Review: no .rs files to review.");
    return;
  }

  clearThreads();

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: `TARS: reviewing ${files.length} file(s) with agent-2…`,
      cancellable: false,
    },
    async () => {
      let findings: Finding[];
      try {
        findings = await runCli(cliPath, files, workspaceRoot);
      } catch (err) {
        vscode.window.showErrorMessage(`TARS Review failed: ${String(err)}`);
        return;
      }

      for (const f of findings) {
        renderFinding(f);
      }
      vscode.window.showInformationMessage(
        `TARS Review: ${findings.length} finding(s) across ${files.length} file(s).`,
      );
    },
  );
}

/** Spawn review-cli with the file paths as args; parse its stdout as Finding[]. Inherits the
 *  process env so `DEEPSEEK_API_KEY` (if the user exported it before launching VSCode) reaches the
 *  reviewer; without it the CLI falls back to its mock stream. */
function runCli(cliPath: string, files: string[], cwd: string): Promise<Finding[]> {
  return new Promise((resolve, reject) => {
    execFile(
      cliPath,
      files,
      { cwd, env: process.env, maxBuffer: 32 * 1024 * 1024 },
      (error, stdout, stderr) => {
        // review-cli exits non-zero only on usage error; a normal run with findings is exit 0.
        // stderr carries logs (the provider banner) — surface on failure only.
        if (error && (!stdout || stdout.trim().length === 0)) {
          reject(new Error(`${error.message}\n${stderr}`));
          return;
        }
        try {
          const parsed = JSON.parse(stdout) as Finding[];
          resolve(Array.isArray(parsed) ? parsed : []);
        } catch (e) {
          reject(new Error(`could not parse review-cli output as JSON: ${String(e)}\nstdout: ${stdout}`));
        }
      },
    );
  });
}

/** Create one comment thread for a finding, pinned at file:line. A `null` line pins the thread at
 *  the top of the file (line 0). */
function renderFinding(f: Finding): void {
  if (!controller) {
    return;
  }
  const uri = vscode.Uri.file(f.file);
  const lineIdx = f.line != null && f.line > 0 ? f.line - 1 : 0;
  const range = new vscode.Range(lineIdx, 0, lineIdx, 0);

  const body = new vscode.MarkdownString(
    `**[${f.severity}]** ${f.message}\n\n_status: ${f.status}_`,
  );
  const comment: vscode.Comment = {
    body,
    mode: vscode.CommentMode.Preview,
    author: { name: `TARS (${f.severity})` },
    label: f.status,
  };

  const thread = controller.createCommentThread(uri, range, [comment]);
  thread.label = `TARS: ${f.severity} finding`;
  thread.collapsibleState = vscode.CommentThreadCollapsibleState.Expanded;
  threads.push(thread);
}
