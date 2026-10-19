//! Plain-English explanations and quick fixes for build errors.
//!
//! The Problems page (WP3.7's additive part of `diagnostics_panel.rs`) asks this module for
//! each build problem's [`explanation`] (from the table of `lemma_build::explain`: tooltip and
//! expander) and its [`proposal`]: a quick fix where one is safe. [`apply`] applies it as
//! **reviewable hunks** (a named review turn, "Fix: …", so the banner and the Changes panel say
//! what made them and Reject restores the text). "Ask Agent to Fix" stays available on every
//! row.
//!
//! The fixes, each recomputed from the current text right before it is applied:
//!
//! | Error | Fix |
//! |---|---|
//! | Missing $ inserted | `$…$` around the math-only token run at TeX's error point (`\alpha_n`, `x^2 + y^2 = 1`); none for the `$` TeX inserts at the end of a paragraph |
//! | Undefined control sequence | `\usepackage{…}` for the package whose cwl table (or [`lemma_build::explain::COMMAND_PACKAGES`]) defines it; else the closest known macro by edit distance (the project's and the loaded packages'), on the error line. Nothing when the name is defined after all (the undefined command is then inside a macro) |
//! | Environment undefined | `\newtheorem` for theorem-like names (sharing the counter of an existing one), else the package that defines it, else the closest known environment (`\begin` and its `\end`) |
//! | `\begin{x}` ended by `\end{y}` | the `\end{y}` on the error line becomes `\end{x}` |
//! | File not found | the closest existing file of the same kind in that directory |
//! | Missing `\begin{document}` | `\begin{document}` before the line where text started, when the file has none |
//!
//! Hooks: [`install`] once per window (nothing to set up: everything is computed on demand).

use crate::diagnostics_panel::Problem;
use crate::document::DocRef;
use crate::review_ui;
use crate::window::WindowState;
use lemma_build::explain::{self, Explanation, FixHint};
use lemma_complete::{CompletionDb, Mode};
use lemma_core::{DiagSource, Origin, TextEdit};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Called once per window after the M2 install hooks. Nothing to set up.
pub fn install(_state: &Rc<WindowState>) {}

/// The explanation of a build problem, when the table knows its error (TeX's errors and the
/// common warnings; not biber's or BibTeX's messages).
pub fn explanation(problem: &Problem) -> Option<Explanation> {
    if problem.source != DiagSource::Tex {
        return None;
    }
    explain::explain(&problem.message, problem.context.as_deref())
}

/// A quick fix that applies to the current text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    /// The button text ("Add \usepackage{amssymb}").
    pub label: String,
    /// What it changes, for the tooltip.
    pub description: String,
}

/// The fix: edits of one document, computed from its current text.
struct Plan {
    label: String,
    description: String,
    doc: DocRef,
    /// Descending, non-overlapping.
    edits: Vec<TextEdit>,
}

/// The quick fix of `problem` (its line in the current text), if a safe one applies.
pub fn proposal(
    state: &Rc<WindowState>,
    problem: &Problem,
) -> impl Future<Output = Option<Proposal>> + use<> {
    let plan = plan(state, problem);
    async move {
        plan.await.map(|p| Proposal {
            label: p.label,
            description: p.description,
        })
    }
}

/// Applies the quick fix of `problem` (its line in the current text) as reviewable hunks: the
/// file is opened, the edits are made as a named review turn ("Fix: …") and the cursor goes to
/// the change. A document that had no unsaved changes is saved, as agent edits are. Fails when
/// no fix applies (any more).
pub fn apply(
    state: &Rc<WindowState>,
    problem: &Problem,
) -> impl Future<Output = Result<(), String>> + use<> {
    let state = state.clone();
    let problem = problem.clone();
    async move {
        // The plan is computed after the last await, so its edits fit the text they change.
        let plan = plan(&state, &problem)
            .await
            .ok_or("This quick fix no longer applies to the text")?;
        let doc = plan.doc.clone();
        let clean = !doc.is_dirty();
        let first = plan.edits.iter().map(|e| e.range.start).min().unwrap_or(0);
        review_ui::start_review(&doc, false);
        let turn = review_ui::begin_named_turn(&state, &format!("Fix: {}", plan.label));
        review_ui::in_turn(&doc, turn, || {
            doc.apply_edits(&plan.edits, Origin::Agent { call: 0 })
        })
        .map_err(|e| format!("Could not apply the fix: {e}"))?;
        review_ui::recompute(&doc);
        let line = doc.line_index().line_of(first);
        review_ui::show_line(&state, &doc, line);
        if clean {
            doc.save()
                .await
                .map_err(|e| format!("Fixed, but saving {} failed: {e}", doc.display_name()))?;
        }
        Ok(())
    }
}

/// Plans the fix of `problem`: loads what it needs, then computes the edits synchronously from
/// the current text (no await after that).
fn plan(state: &Rc<WindowState>, problem: &Problem) -> impl Future<Output = Option<Plan>> + use<> {
    let state = state.clone();
    let problem = problem.clone();
    async move {
        let hint = explanation(&problem)?.fix?;
        let line = problem.line?;
        let doc = state.workspace.get_or_load(&problem.path).await.ok()?;
        match hint {
            FixHint::WrapInMath => wrap_in_math(&state, &doc, line, &problem),
            FixHint::UndefinedCommand { name } => {
                let root = root_document(&state, &doc).await;
                undefined_command(&state, &doc, root.as_ref(), line, &name)
            }
            FixHint::UndefinedEnvironment { name } => {
                let root = root_document(&state, &doc).await;
                undefined_environment(&state, &doc, root.as_ref(), line, &name)
            }
            FixHint::MismatchedEnd { begin, end } => {
                let (start, text) = line_text(&doc, line)?;
                let edit = explain::fix_end_edit(&text, start, &begin, &end)?;
                Some(Plan {
                    label: format!("Change to \\end{{{begin}}}"),
                    description: format!(
                        "Replaces \\end{{{end}}} by \\end{{{begin}}} on line {line}"
                    ),
                    doc,
                    edits: vec![edit],
                })
            }
            FixHint::FileNotFound { name } => file_not_found(&state, doc, line, &name).await,
            FixHint::MissingBeginDocument => {
                let edit = doc.with_text(|text| explain::begin_document_edit(text, line))?;
                Some(Plan {
                    label: "Insert \\begin{document}".to_owned(),
                    description: format!(
                        "Starts the document body before line {line}, where the text begins"
                    ),
                    doc,
                    edits: vec![edit],
                })
            }
        }
    }
}

/// The 1-based `line` of `doc`: its start offset and text (without the line break).
fn line_text(doc: &DocRef, line: u32) -> Option<(usize, String)> {
    let start = {
        let index = doc.line_index();
        if line == 0 || line > index.line_count() {
            return None;
        }
        index.line_start(line - 1)
    };
    let text = doc.line_text(line - 1)?;
    Some((start, text))
}

/// The root document of `doc`'s project: the last build's root, else `doc` itself when it has
/// a `\documentclass`.
async fn root_document(state: &Rc<WindowState>, doc: &DocRef) -> Option<DocRef> {
    let root = state.build.get().and_then(|b| b.root());
    if let Some(root) = root
        && let Ok(root) = state.workspace.get_or_load(&root).await
    {
        return Some(root);
    }
    doc.with_text(|t| t.contains("\\documentclass"))
        .then(|| doc.clone())
}

/// The completion tables of what the project actually loads: the LaTeX kernel, the document
/// class and every package of its `\usepackage`s (not the built-in AMS tables completion
/// offers everywhere: an undefined `\mathbb` is the point of the fix).
fn project_tables(state: &WindowState, root: Option<&DocRef>) -> CompletionDb {
    let mut db = CompletionDb::empty();
    let mut packages: Vec<String> = vec!["latex-document".into(), "latex-dev".into()];
    packages.extend(state.workspace.index.borrow().packages());
    if let Some(root) = root {
        packages.extend(root.with_text(|t| {
            let index = lemma_syntax::index_file(t);
            index.packages
        }));
    }
    if let Some(root) = root
        && let Some(class) =
            root.with_text(|t| lemma_complete::document_class(t).map(str::to_owned))
    {
        packages.push(format!("class-{class}"));
    }
    db.load_packages(&packages);
    db
}

/// The package that defines a command or environment in a table included by a more common
/// one is suggested as that one.
const PACKAGE_ALIASES: &[(&str, &str)] = &[
    ("amsfonts", "amssymb"),
    ("amsopn", "amsmath"),
    ("amstext", "amsmath"),
    ("amsbsy", "amsmath"),
    ("tikzlibrarycd", "tikz-cd"),
];

/// Packages searched first (the most common definition of a name wins).
const PREFERRED_PACKAGES: &[&str] = &[
    "amsmath",
    "amssymb",
    "amsthm",
    "mathtools",
    "graphicx",
    "xcolor",
    "hyperref",
    "booktabs",
    "tikz",
    "tikz-cd",
    "geometry",
    "enumitem",
    "siunitx",
];

/// Every table, the preferred packages first.
fn all_tables() -> CompletionDb {
    let mut db = CompletionDb::empty();
    let preferred: Vec<String> = PREFERRED_PACKAGES.iter().map(|p| (*p).to_owned()).collect();
    db.load_packages(&preferred);
    let rest: Vec<String> = CompletionDb::available_packages()
        .into_iter()
        .map(str::to_owned)
        .collect();
    db.load_packages(&rest);
    db
}

/// A package `\usepackage` can load (not the kernel's or a class's word list).
fn loadable(package: &str) -> Option<&str> {
    if package.starts_with("latex-") || package.starts_with("class-") || package.is_empty() {
        return None;
    }
    Some(
        PACKAGE_ALIASES
            .iter()
            .find(|(p, _)| *p == package)
            .map_or(package, |(_, alias)| alias),
    )
}

fn wrap_in_math(state: &WindowState, doc: &DocRef, line: u32, problem: &Problem) -> Option<Plan> {
    let context = problem.context.as_deref()?;
    let (start, text) = line_text(doc, line)?;
    // Math-only commands: the built-in tables (AMS included) and the project's packages.
    let mut db = CompletionDb::builtin();
    db.load_packages(&state.workspace.index.borrow().packages());
    let is_math = |name: &str| db.command(name).is_some_and(|c| c.mode == Mode::MathOnly);
    let edits = explain::wrap_in_math_edit(&text, start, context, &is_math)?;
    let run = &text[edits[1].range.start - start..edits[0].range.start - start];
    let shown = shorten(run);
    Some(Plan {
        label: format!("Put {shown} in $…$"),
        description: format!("Makes {run} a formula on line {line}"),
        doc: doc.clone(),
        edits,
    })
}

/// `text`, cut to 24 characters for a button.
fn shorten(text: &str) -> String {
    if text.chars().count() <= 24 {
        text.to_owned()
    } else {
        let cut: String = text.chars().take(23).collect();
        format!("{cut}…")
    }
}

/// Whether the project defines `\name` itself (`\newcommand`, `\DeclareMathOperator`, …).
fn project_defines(state: &WindowState, name: &str, env: bool) -> bool {
    state
        .workspace
        .index
        .borrow()
        .files
        .values()
        .any(|f| f.macros.iter().any(|m| m.is_env == env && m.name == name))
}

fn undefined_command(
    state: &WindowState,
    doc: &DocRef,
    root: Option<&DocRef>,
    line: u32,
    name: &str,
) -> Option<Plan> {
    let tables = project_tables(state, root);
    // Defined after all: the undefined command is inside this macro's definition (TeX's context
    // shows the macro the user typed); no safe fix.
    if tables.command(name).is_some() || project_defines(state, name, false) {
        return None;
    }
    let package = all_tables()
        .command(name)
        .and_then(|c| loadable(c.package))
        .map(str::to_owned)
        .or_else(|| {
            explain::COMMAND_PACKAGES
                .iter()
                .find(|(c, _)| *c == name)
                .map(|(_, p)| (*p).to_owned())
        });
    if let (Some(package), Some(root)) = (package, root)
        && let Some(edit) = root.with_text(|t| explain::usepackage_edit(t, &package))
    {
        return Some(Plan {
            label: format!("Add \\usepackage{{{package}}}"),
            description: format!(
                "\\{name} comes from the package {package}: loads it in {}",
                root.display_name()
            ),
            doc: root.clone(),
            edits: vec![edit],
        });
    }
    // A typo: the closest command the project knows.
    let user: Vec<String> = state
        .workspace
        .index
        .borrow()
        .files
        .values()
        .flat_map(|f| {
            f.macros
                .iter()
                .filter(|m| !m.is_env)
                .map(|m| m.name.clone())
        })
        .collect();
    let known = user
        .iter()
        .map(String::as_str)
        .chain(tables.commands().map(|c| c.name))
        .filter(|c| c.chars().all(|ch| ch.is_ascii_alphabetic()));
    let best = explain::closest(name, known)?.to_owned();
    let (start, text) = line_text(doc, line)?;
    let edits = explain::replace_command_edits(&text, start, name, &best);
    (!edits.is_empty()).then(|| Plan {
        label: format!("Use \\{best}"),
        description: format!("Replaces \\{name} by \\{best} on line {line}"),
        doc: doc.clone(),
        edits,
    })
}

fn undefined_environment(
    state: &WindowState,
    doc: &DocRef,
    root: Option<&DocRef>,
    line: u32,
    name: &str,
) -> Option<Plan> {
    let tables = project_tables(state, root);
    if tables.environment(name).is_some() || project_defines(state, name, true) {
        return None;
    }
    let base = name.trim_end_matches('*');
    // Theorem-like: declare it, sharing the counter of the project's first theorem.
    if let Some(root) = root
        && explain::THEOREM_NAMES.iter().any(|(n, _)| *n == base)
        && base == name
    {
        let shared = state.workspace.index.borrow().theorem_names();
        let shared = ["theorem", "thm"]
            .iter()
            .find(|t| shared.iter().any(|s| s == *t))
            .map(|t| (*t).to_owned())
            .or_else(|| shared.first().cloned());
        if let Some(edit) = root.with_text(|t| explain::newtheorem_edit(t, name, shared.as_deref()))
        {
            let counter = shared.map_or_else(String::new, |s| format!("[{s}]"));
            return Some(Plan {
                label: format!("Declare \\newtheorem{{{name}}}"),
                description: format!(
                    "Adds \\newtheorem{{{name}}}{counter}{{…}} to the preamble of {}",
                    root.display_name()
                ),
                doc: root.clone(),
                edits: vec![edit],
            });
        }
    }
    let package = all_tables()
        .environment(name)
        .and_then(|e| loadable(e.package))
        .map(str::to_owned)
        .or_else(|| (name == "proof").then(|| "amsthm".to_owned()))
        .or_else(|| {
            explain::ENVIRONMENT_PACKAGES
                .iter()
                .find(|(e, _)| *e == name)
                .map(|(_, p)| (*p).to_owned())
        });
    if let (Some(package), Some(root)) = (package, root)
        && let Some(edit) = root.with_text(|t| explain::usepackage_edit(t, &package))
    {
        return Some(Plan {
            label: format!("Add \\usepackage{{{package}}}"),
            description: format!(
                "The environment {name} comes from the package {package}: loads it in {}",
                root.display_name()
            ),
            doc: root.clone(),
            edits: vec![edit],
        });
    }
    let user: Vec<String> = state
        .workspace
        .index
        .borrow()
        .files
        .values()
        .flat_map(|f| f.macros.iter().filter(|m| m.is_env).map(|m| m.name.clone()))
        .collect();
    let known = user
        .iter()
        .map(String::as_str)
        .chain(tables.environments().map(|e| e.name));
    let best = explain::closest(name, known)?.to_owned();
    let start = {
        let index = doc.line_index();
        if line == 0 || line > index.line_count() {
            return None;
        }
        index.line_start(line - 1)
    };
    let edits = doc.with_text(|t| explain::rename_environment_edits(t, start, name, &best))?;
    Some(Plan {
        label: format!("Use “{best}”"),
        description: format!("Renames the environment {name} to {best} (\\begin and \\end)"),
        doc: doc.clone(),
        edits,
    })
}

/// Image extensions `\includegraphics` finds without an extension.
const IMAGE_EXTENSIONS: &[&str] = &["pdf", "png", "jpg", "jpeg", "eps", "svg"];

async fn file_not_found(
    state: &Rc<WindowState>,
    doc: DocRef,
    line: u32,
    requested: &str,
) -> Option<Plan> {
    let (_, text) = line_text(&doc, line)?;
    let (_, written) = explain::file_argument(&text, 0, requested)?;
    // TeX resolves names against the root document's directory.
    let base = state
        .build
        .get()
        .and_then(|b| b.root())
        .and_then(|r| r.parent().map(Path::to_path_buf))
        .or_else(|| doc.path().parent().map(Path::to_path_buf))?;
    let wanted = PathBuf::from(written.as_str());
    let dir_part = wanted.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = wanted.file_stem()?.to_str()?.to_owned();
    let extension = wanted
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_owned);
    let graphics = text.contains("\\includegraphics");
    let dir = base.join(&dir_part);
    let names = gio::spawn_blocking(move || {
        std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default()
    })
    .await
    .ok()?;
    let kind_ok = |name: &str| {
        let ext = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match (&extension, graphics) {
            (Some(e), _) => ext.as_deref() == Some(e.to_ascii_lowercase().as_str()),
            (None, true) => ext.is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str())),
            (None, false) => ext.as_deref() == Some("tex"),
        }
    };
    let candidates: Vec<(String, String)> = names
        .into_iter()
        .filter(|n| kind_ok(n))
        .filter_map(|n| {
            let s = Path::new(&n).file_stem()?.to_str()?.to_owned();
            Some((s, n))
        })
        .collect();
    let best_stem = explain::closest(&stem, candidates.iter().map(|(s, _)| s.as_str()))?;
    let (_, file) = candidates.iter().find(|(s, _)| s == best_stem)?;
    let found = dir_part.join(file).to_string_lossy().into_owned();
    let replacement = explain::replacement_name(&written, &found);
    // The document may have changed while the directory was read: locate the argument again.
    let (start, text) = line_text(&doc, line)?;
    let (range, _) = explain::file_argument(&text, start, requested)?;
    Some(Plan {
        label: format!("Use {replacement}"),
        description: format!("{written} does not exist; {found} does"),
        doc,
        edits: vec![TextEdit {
            range,
            text: replacement,
        }],
    })
}
