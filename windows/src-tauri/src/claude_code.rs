// Chat through the locally installed Claude Code CLI, so turns count against the
// user's Claude subscription instead of an API key. Each turn is one headless
// `claude -p` run; follow-ups resume the same CLI session.
//
// --restricted keeps the run contained: it ignores the user's settings files
// (so Coucou's own hooks never fire for its chat and it never shows up as a
// live session), drops every code-running tool, and confines Read to the
// directories passed here. --bare would be lighter but refuses OAuth.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

use crate::claude::{Chat, ChatContext, ChatReply, SYSTEM_PROMPT};

/// Web searches plus Claude Code's own startup make these slower than the API.
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

/// A tray app does not inherit the shell's PATH edits, so the installer's
/// default location is tried first.
fn claude_exe() -> PathBuf {
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let exe = PathBuf::from(home).join(".local").join("bin").join("claude.exe");
        if exe.is_file() {
            return exe;
        }
    }
    PathBuf::from("claude")
}

pub async fn send(
    chat: &Chat,
    model: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let session = chat.cli_session();
    let workdir = crate::files::inbox_dir();
    let _ = std::fs::create_dir_all(&workdir);

    let mut prompt = String::new();
    let mut extra_dir: Option<PathBuf> = None;
    // Context rides along with the first message only, as on the API path.
    if session.is_none() {
        match &context {
            Some(ChatContext::File { name, path }) => {
                prompt.push_str(&format!(
                    "File: {name}\nIt is at {path} — read it with the Read tool before answering.\n\n"
                ));
                extra_dir = std::path::Path::new(path).parent().map(PathBuf::from);
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                prompt.push_str(&format!("Context — App: {app_name}, Window: {title}"));
                if let Some(url) = url {
                    prompt.push_str(&format!(", URL: {url}"));
                }
                prompt.push_str("\n\n");
            }
            None => {}
        }
    }
    prompt.push_str(&query);

    let mut cmd = tokio::process::Command::new(claude_exe());
    cmd.current_dir(&workdir)
        .args(["-p", "--output-format", "json", "--restricted", "--strict-mcp-config"])
        .args(["--tools", "WebSearch,Read", "--allowedTools", "WebSearch,Read"])
        .args(["--system-prompt", SYSTEM_PROMPT, "--model", model]);
    if let Some(dir) = extra_dir.filter(|d| *d != workdir) {
        cmd.arg("--add-dir").arg(dir);
    }
    if let Some(id) = &session {
        cmd.args(["--resume", id]);
    }
    // The prompt goes in on stdin: no quoting surprises, no command-line limit.
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "Claude Code isn't installed. Install it, or switch the chat back to an API key in settings.".to_string()
        } else {
            format!("Could not start Claude Code: {e}")
        }
    })?;

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().ok_or("Claude Code stdin unavailable.")?;
        stdin.write_all(prompt.as_bytes()).await.map_err(|e| e.to_string())?;
        // Dropped here, closing stdin so the CLI starts.
    }

    let output = tokio::time::timeout(TURN_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "Claude Code took too long to answer.".to_string())?
        .map_err(|e| e.to_string())?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Ok(result) = serde_json::from_str::<Value>(stdout.trim()) else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim().lines().last().unwrap_or("no output");
        return Err(format!("Claude Code failed: {detail}"));
    };

    let text = result.get("result").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if result.get("is_error").and_then(Value::as_bool) == Some(true) {
        // Not logged in, usage limit reached, bad model name, …
        return Err(if text.is_empty() { "Claude Code returned an error.".into() } else { text });
    }
    if let Some(id) = result.get("session_id").and_then(Value::as_str) {
        chat.set_cli_session(id.to_string());
    }
    if text.is_empty() {
        return Err("No response text.".into());
    }
    Ok(ChatReply { text })
}
