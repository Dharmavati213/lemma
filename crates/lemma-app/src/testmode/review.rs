//! Scripted review runs (WP1.2): `--apply-as-agent JSON` and `--demo review`.
//!
//! The JSON is a list of tool calls in the format of ARCHITECTURE §10.5:
//!
//! ```json
//! [{"tool": "edit", "file_path": "main.tex", "old_string": "…", "new_string": "…", "replace_all": false},
//!  {"tool": "write", "file_path": "new.tex", "content": "…"},
//!  {"tool": "read_file", "file_path": "main.tex", "offset": 1, "limit": 20}]
//! ```
//!
//! Each call goes through [`agent_edits::apply`] exactly as an MCP tool call would, numbered
//! from 1 (`Origin::Agent { call }`). Paths are relative to the project root.

use crate::agent_edits;
use crate::window::WindowState;
use gio::prelude::*;
use lemma_agent::{EditorRequest, ToolContent, ToolReply};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// One tool call of an `--apply-as-agent` script.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "tool", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScriptedCall {
    Edit {
        file_path: PathBuf,
        old_string: String,
        new_string: String,
        #[serde(default)]
        replace_all: bool,
    },
    Write {
        file_path: PathBuf,
        content: String,
    },
    ReadFile {
        file_path: PathBuf,
        #[serde(default)]
        offset: Option<u32>,
        #[serde(default)]
        limit: Option<u32>,
    },
}

impl From<ScriptedCall> for EditorRequest {
    fn from(call: ScriptedCall) -> Self {
        match call {
            ScriptedCall::Edit {
                file_path,
                old_string,
                new_string,
                replace_all,
            } => EditorRequest::Edit {
                path: file_path,
                old_string,
                new_string,
                replace_all,
            },
            ScriptedCall::Write { file_path, content } => EditorRequest::Write {
                path: file_path,
                content,
            },
            ScriptedCall::ReadFile {
                file_path,
                offset,
                limit,
            } => EditorRequest::ReadFile {
                path: file_path,
                offset,
                limit,
            },
        }
    }
}

/// Parses an `--apply-as-agent` script.
pub fn parse_script(json: &str) -> Result<Vec<ScriptedCall>, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid agent script: {e}"))
}

/// Applies the edits in `json` (`[{"tool":"edit","file_path":…,"old_string":…,"new_string":…,
/// "replace_all":false},{"tool":"write",…}]`) through `agent_edits::apply`, as an agent would.
/// Resolves when every edit is applied, saved and rendered.
pub async fn run_apply_as_agent(state: Rc<WindowState>, json: PathBuf) -> Result<(), String> {
    let (bytes, _etag) = gio::File::for_path(&json)
        .load_contents_future()
        .await
        .map_err(|e| format!("cannot read {}: {}", json.display(), e.message()))?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| format!("{} is not UTF-8 text", json.display()))?;
    run_script(&state, parse_script(text)?).await
}

/// Runs `calls` in order; stops at the first error reply.
pub async fn run_script(state: &Rc<WindowState>, calls: Vec<ScriptedCall>) -> Result<(), String> {
    for (i, call) in calls.into_iter().enumerate() {
        let n = i as u64 + 1;
        let reply = agent_edits::apply(&state.workspace, n, call.into()).await;
        let text = reply_text(&reply);
        if reply.is_error {
            return Err(format!("call {n} failed: {text}"));
        }
        tracing::info!("agent call {n}:\n{text}");
    }
    settle(state).await;
    Ok(())
}

/// The text of a tool reply.
pub fn reply_text(reply: &ToolReply) -> String {
    reply
        .content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Text(t) => Some(t.as_str()),
            ToolContent::Png(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Waits until pending review work (recomputes, the bar layout, the banner) has run.
async fn settle(state: &Rc<WindowState>) {
    // Default-idle callbacks queued before this one run first.
    let (tx, rx) = async_channel::bounded(1);
    glib::idle_add_local_full(glib::Priority::DEFAULT_IDLE, move || {
        let _ = tx.try_send(());
        glib::ControlFlow::Break
    });
    let _ = rx.recv().await;
    if let Some(review) = state.review.get() {
        review.refresh();
    }
}

/// The document `--demo review` opens.
const DEMO_TEX: &str = r"\documentclass{article}
\usepackage{amsmath,amssymb}
\begin{document}

\section{Setup}\label{sec:setup}
Let $X$ be a scheme.
Every affine scheme is quasi-compact.
This paragraph is obsolete.
It will be removed.

\section{Results}
\begin{theorem}
  The functor $\Gamma(X, -)$ is left exact.
\end{theorem}

\end{document}
";

/// `--demo review`: a document with a few reviewable hunks. It writes a small LaTeX file to a
/// fresh temporary directory, opens it and applies agent edits to it (a replacement, an
/// insertion, a deletion and a word-level change).
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    if !state.workspace.is_empty() {
        return Err("the review demo opens its own document; run it without FILE".into());
    }
    let dir = glib::tmp_dir().join(format!("lemma-review-demo-{}", std::process::id()));
    let path = dir.join("demo.tex");
    write_file(&path, DEMO_TEX).await?;
    state.open_file(&path).await?;
    let edit = |old: &str, new: &str| ScriptedCall::Edit {
        file_path: "demo.tex".into(),
        old_string: old.into(),
        new_string: new.into(),
        replace_all: false,
    };
    run_script(
        &state,
        vec![
            edit(
                "Let $X$ be a scheme.",
                "Let $X$ be a Noetherian scheme and $\\mathcal{F}$ a coherent sheaf on $X$.",
            ),
            edit(
                "\\section{Results}\n",
                "\\section{Results}\nWe keep the notation of Section~\\ref{sec:setup}.\n",
            ),
            edit("This paragraph is obsolete.\nIt will be removed.\n", ""),
            edit("is quasi-compact.", "is quasi-compact and quasi-separated."),
        ],
    )
    .await
}

async fn write_file(path: &Path, text: &str) -> Result<(), String> {
    let (path, text) = (path.to_path_buf(), text.to_owned());
    gio::spawn_blocking(move || {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text)
    })
    .await
    .unwrap_or_else(|_| Err(std::io::Error::other("the write job panicked")))
    .map_err(|e| format!("cannot write the demo document: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_parse() {
        let calls = parse_script(
            r#"[{"tool":"edit","file_path":"a.tex","old_string":"x","new_string":"y"},
                {"tool":"write","file_path":"b.tex","content":"z\n"},
                {"tool":"read_file","file_path":"a.tex","limit":3}]"#,
        )
        .unwrap();
        assert_eq!(
            calls,
            vec![
                ScriptedCall::Edit {
                    file_path: "a.tex".into(),
                    old_string: "x".into(),
                    new_string: "y".into(),
                    replace_all: false,
                },
                ScriptedCall::Write {
                    file_path: "b.tex".into(),
                    content: "z\n".into(),
                },
                ScriptedCall::ReadFile {
                    file_path: "a.tex".into(),
                    offset: None,
                    limit: Some(3),
                },
            ]
        );
        assert!(parse_script(r#"[{"tool":"delete","file_path":"a.tex"}]"#).is_err());
        assert!(parse_script(r#"[{"tool":"edit","file_path":"a.tex","old_string":"x"}]"#).is_err());
        assert!(
            parse_script(r#"[{"tool":"write","file_path":"a","content":"","extra":1}]"#).is_err()
        );
    }
}
