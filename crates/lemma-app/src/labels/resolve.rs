//! What a place of the text refers to — a label, a citation key, a file, a package or a user
//! macro — and where that is defined: the goto resolver and the dispatch of the hovers of WP3.2
//! (`editor::goto::attach_resolvers`, `editor::hover::attach_providers` call them).

use super::scan::{self, FileKind};
use super::{hovers, label_in_document};
use crate::editor::goto::{GotoCtx, GotoTarget};
use crate::editor::hover::{HoverContent, HoverCtx};
use crate::editor::{TextPoint, Trigger};
use crate::window::WindowState;
use lemma_syntax::{Context, TokenKind};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// What a place of the text names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Referent {
    /// A label: in a reference (`definition == false`) or in its `\label`.
    Label { name: String, definition: bool },
    /// A citation key.
    Cite(String),
    /// A file argument of `cmd` (`\input`, `\includegraphics`, …); `dir` is the directory
    /// argument of `\import{dir}{file}`.
    File {
        cmd: String,
        target: String,
        dir: Option<String>,
    },
    /// A package or class argument of `cmd` (`\usepackage`, `\documentclass`).
    Package { cmd: String, name: String },
    /// A control word (`\cF`): a user macro if the project defines it.
    Macro(String),
}

/// What `point` names and the range of the name (the command for a macro).
pub fn referent(point: &TextPoint) -> Option<(Referent, Range<usize>)> {
    if let Some((range, item)) = point.item() {
        let item = item.to_owned();
        let referent = match point.token_kind()? {
            TokenKind::RefArg => Referent::Label {
                name: item,
                definition: false,
            },
            TokenKind::LabelArg => Referent::Label {
                name: item,
                definition: true,
            },
            TokenKind::CiteArg => Referent::Cite(item),
            TokenKind::FileArg => {
                let (cmd, idx) = argument_of(point)?;
                let dir = (idx == 1).then(|| import_dir(point)).flatten();
                if idx == 0 && is_import(&cmd) {
                    // `\import{dir}{…}`: a directory, nothing to open.
                    return None;
                }
                Referent::File {
                    cmd,
                    target: item,
                    dir,
                }
            }
            TokenKind::PackageArg => {
                let (cmd, _) = argument_of(point)?;
                Referent::Package { cmd, name: item }
            }
            _ => return None,
        };
        return Some((referent, range));
    }
    let (range, name) = point.control_word()?;
    // On the command of a reference, citation or file: its first name.
    if let Some(first) = first_argument_item(point)
        && let Some((referent, _)) = referent(&first)
    {
        return Some((referent, range));
    }
    Some((Referent::Macro(name.to_owned()), range))
}

/// The command and argument index of the argument `point` is in.
fn argument_of(point: &TextPoint) -> Option<(String, u8)> {
    match &point.context {
        Context::Arg { cmd, idx, .. } => Some((cmd.to_string(), *idx)),
        _ => None,
    }
}

fn is_import(cmd: &str) -> bool {
    matches!(
        cmd,
        "import" | "subimport" | "inputfrom" | "includefrom" | "subinputfrom" | "subincludefrom"
    )
}

/// The first argument of `\import{dir}{file}` on `point`'s line, before the point.
fn import_dir(point: &TextPoint) -> Option<String> {
    let before = &point.line_text[..point.column.min(point.line_text.len())];
    let open = before.rfind('{')?;
    let close = before[..open].trim_end().strip_suffix('}')?;
    let start = close.rfind('{')?;
    Some(close[start + 1..].trim().to_owned()).filter(|d| !d.is_empty())
}

/// The point at the first name of the argument of the command under `point` (`\eqref{a}` with
/// the point on `eqref` → the point at `a`), when that argument is a name list.
pub fn first_argument_item(point: &TextPoint) -> Option<TextPoint> {
    let (range, _) = point.control_word()?;
    let line = &point.line_text;
    let bytes = line.as_bytes();
    let mut i = range.end - point.line_start();
    if bytes.get(i) == Some(&b'*') {
        i += 1;
    }
    // Optional arguments (`\cite[p.~3]{…}`, `\includegraphics[width=…]{…}`).
    loop {
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        match bytes.get(i) {
            Some(b'[') => {
                let close = line[i..].find(']')?;
                i += close + 1;
            }
            Some(b'{') => break,
            _ => return None,
        }
    }
    i += 1;
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] == b'}' {
        return None;
    }
    let first = TextPoint::new(&point.document, point.line_start() + i)?;
    matches!(
        first.token_kind(),
        Some(
            TokenKind::RefArg
                | TokenKind::CiteArg
                | TokenKind::FileArg
                | TokenKind::PackageArg
                | TokenKind::LabelArg
        )
    )
    .then_some(first)
}

// ----- go to definition -----

/// The target of `ctx` (the goto resolver of WP3.2).
pub fn goto_target(state: &WindowState, ctx: &GotoCtx) -> Option<GotoTarget> {
    let (referent, _) = referent(ctx)?;
    match referent {
        Referent::Label {
            name,
            definition: false,
        } => label_target(state, &name),
        Referent::Label {
            definition: true, ..
        } => None,
        Referent::Cite(key) => entry_target(state, &key),
        Referent::File { cmd, target, dir } => {
            let kind = scan::file_kind(&cmd)?;
            let path = find_file(state, ctx.document.path(), kind, &target, dir.as_deref())?;
            Some(if kind == FileKind::Graphics {
                GotoTarget::External { path }
            } else {
                GotoTarget::File { path, offset: 0 }
            })
        }
        Referent::Package { cmd, name } => {
            let kind = scan::file_kind(&cmd)?;
            let path = find_file(state, ctx.document.path(), kind, &name, None)?;
            Some(GotoTarget::File { path, offset: 0 })
        }
        Referent::Macro(name) => macro_target(state, &name),
    }
}

/// Where `\label{name}` is: the live text of a document Lemma has, else the line the project
/// index gives.
pub fn label_target(state: &WindowState, name: &str) -> Option<GotoTarget> {
    let indexed = {
        let index = state.workspace.index.borrow();
        scan::find_label(&index, name).map(|(path, _, label)| (path.to_path_buf(), label.line))
    };
    if let Some((path, line)) = &indexed {
        if let Some(doc) = state.workspace.get(path)
            && let Some(offset) = label_in_document(&doc, name, Some(*line))
        {
            return Some(GotoTarget::File {
                path: doc.path().to_path_buf(),
                offset,
            });
        }
        return Some(GotoTarget::Line {
            path: path.clone(),
            line: *line,
            find: Some(format!("\\label{{{name}}}")),
        });
    }
    // Typed since the project index last looked: the open documents.
    state.workspace.documents().into_iter().find_map(|doc| {
        if !doc.is_open() {
            return None;
        }
        let offset = label_in_document(&doc, name, None)?;
        Some(GotoTarget::File {
            path: doc.path().to_path_buf(),
            offset,
        })
    })
}

/// The bibliography entry of `key`.
pub fn entry_target(state: &WindowState, key: &str) -> Option<GotoTarget> {
    let (path, line) = {
        let index = state.workspace.index.borrow();
        let (path, entry) = scan::find_entry(&index, key)?;
        (path.to_path_buf(), entry.line)
    };
    if let Some(doc) = state.workspace.get(&path)
        && let Some(offset) = on_line(&doc, line, |text| scan::bib_entry_offset(text, key))
            .or_else(|| doc.with_text(|text| scan::bib_entry_offset(text, key)))
    {
        return Some(GotoTarget::File {
            path: doc.path().to_path_buf(),
            offset,
        });
    }
    Some(GotoTarget::Line {
        path,
        line,
        find: Some(key.to_owned()),
    })
}

/// The definition of the user macro `\name`.
pub fn macro_target(state: &WindowState, name: &str) -> Option<GotoTarget> {
    let (path, line) = {
        let index = state.workspace.index.borrow();
        let (path, def) = scan::find_macro(&index, name)?;
        (path.to_path_buf(), def.line)
    };
    let command = format!("\\{name}");
    if let Some(doc) = state.workspace.get(&path)
        && let Some(offset) = on_line(&doc, line, |text| scan::definition_offset(text, name, 0))
            .or_else(|| doc.with_text(|text| scan::definition_offset(text, name, line)))
    {
        return Some(GotoTarget::File {
            path: doc.path().to_path_buf(),
            offset,
        });
    }
    Some(GotoTarget::Line {
        path,
        line,
        find: Some(command),
    })
}

/// `find` on the 0-based `line` of `doc` alone (the place the project index gives, checked
/// without copying the whole text): the document offset of what it finds there.
fn on_line(
    doc: &crate::document::DocRef,
    line: u32,
    find: impl FnOnce(&str) -> Option<usize>,
) -> Option<usize> {
    let text = doc.line_text(line)?;
    let column = find(&text)?;
    Some(doc.line_index().line_start(line) + column)
}

/// The root document of the project `path` belongs to: the last build's root when the project
/// has `path`, else an indexed root document that includes it, else its `% !TEX root`, else
/// `path` itself.
pub fn project_root(state: &WindowState, path: &Path) -> PathBuf {
    let index = state.workspace.index.borrow();
    let contains =
        |root: &Path| root == path || index.project_files(root).iter().any(|p| p == path);
    if let Some(root) = state.build.get().and_then(|b| b.root())
        && contains(&root)
    {
        return root;
    }
    if let Some((root, _)) = index
        .files
        .iter()
        .find(|(root, file)| file.has_documentclass && contains(root))
    {
        return root.clone();
    }
    if let Some(file) = index.files.get(path)
        && let Some(root) = &file.magic.root
        && let Some(dir) = path.parent()
    {
        let mut root = scan::lexical_join(dir, root);
        if root.extension().is_none() {
            root.set_extension("tex");
        }
        return root;
    }
    path.to_path_buf()
}

/// The file a `kind` argument `target` of the document at `from` names, if it exists: looked
/// for in the root document's directory (where TeX runs) and in `from`'s directory, with the
/// usual extensions; images also in the `\graphicspath` directories of the open documents.
pub fn find_file(
    state: &WindowState,
    from: &Path,
    kind: FileKind,
    target: &str,
    dir: Option<&str>,
) -> Option<PathBuf> {
    let root = project_root(state, from);
    let root_dir = root.parent().unwrap_or(Path::new("/")).to_path_buf();
    let from_dir = from.parent().unwrap_or(Path::new("/")).to_path_buf();
    let mut bases: Vec<PathBuf> = match kind {
        // `\subfile` and `\import` are relative to the including file; `\bibliography` as the
        // project index reads it.
        FileKind::Bib => vec![from_dir.clone(), root_dir.clone()],
        _ => vec![root_dir.clone(), from_dir.clone()],
    };
    if let Some(dir) = dir {
        bases = bases.iter().map(|b| scan::lexical_join(b, dir)).collect();
        bases.insert(0, scan::lexical_join(&from_dir, dir));
    }
    if kind == FileKind::Graphics {
        let extra: Vec<String> = state
            .workspace
            .documents()
            .iter()
            .filter(|d| d.is_open())
            .flat_map(|d| d.with_text(scan::graphics_paths))
            .collect();
        let dirs: Vec<PathBuf> = bases
            .iter()
            .flat_map(|b| extra.iter().map(move |e| scan::lexical_join(b, e)))
            .collect();
        bases.extend(dirs);
    }
    let names = scan::file_names(kind, target);
    bases.iter().find_map(|base| {
        names
            .iter()
            .map(|name| scan::lexical_join(base, name))
            .find(|path| path.is_file())
    })
}

// ----- hovers -----

/// The hover of `ctx` (the hover provider of WP3.2).
pub fn hover_content(state: &Rc<WindowState>, ctx: &HoverCtx) -> Option<HoverContent> {
    let (referent, range) = referent(ctx)?;
    let content = match referent {
        Referent::Label {
            name,
            definition: false,
        } => hovers::reference(state, &name),
        Referent::Label {
            name,
            definition: true,
        } => hovers::label(state, &name),
        Referent::Cite(key) => hovers::citation(state, &key),
        Referent::Macro(name) => {
            // In math the pointer shows the rendered formula (WP3.3), unless that is off.
            let in_math = ctx.document.context_at_byte(range.start).is_math();
            if in_math && ctx.trigger == Trigger::Pointer && state.config.preview.math_hover {
                return None;
            }
            hovers::user_macro(state, &name)?
        }
        Referent::File { .. } | Referent::Package { .. } => return None,
    };
    ctx.set_range(range);
    Some(content)
}
