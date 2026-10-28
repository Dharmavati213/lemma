//! What to build, and the exact latexmk invocation for it (§6).

use crate::Engine;
use crate::paths::normalize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One latexmk run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildRequest {
    /// Absolute path of the root `.tex`; latexmk runs in its directory.
    pub root: PathBuf,
    pub engine: Engine,
    /// Output directory relative to the root's directory; `.build` by default.
    pub outdir: String,
    /// Directories (relative to the root's directory) of `\include`d files. They are created under
    /// `outdir` before the build, because TeX cannot write `<outdir>/<dir>/<file>.aux` otherwise.
    /// Absolute paths and paths containing `..` are ignored.
    pub mirror_dirs: Vec<PathBuf>,
    /// The build is cancelled after this long; 180 s by default.
    pub timeout: Duration,
    /// The latexmk executable; `latexmk` (looked up in `PATH`) by default.
    pub latexmk: PathBuf,
}

/// Environment that widens TeX's log lines so the log parser sees whole messages and full error
/// context instead of 79-column fragments.
pub(crate) const LOG_ENV: [(&str, &str); 3] = [
    ("max_print_line", "10000"),
    ("error_line", "254"),
    ("half_error_line", "238"),
];

/// The arguments (without the program) and extra environment for latexmk:
///
/// ```text
/// max_print_line=10000 error_line=254 half_error_line=238 \
/// latexmk [-pdf|-lualatex|-xelatex] -synctex=1 -interaction=nonstopmode -file-line-error -recorder -outdir=<outdir> ./<root file name>
/// ```
///
/// [`Engine::Latexmkrc`] passes no engine flag. The root is passed by file name because latexmk
/// runs in the root's directory, prefixed with `./` so that a name starting with `-` (such as
/// `-notes.tex` or `-pdflatex=<command>.tex`) is never parsed as an option, by latexmk or by the
/// TeX engine it runs. The job name, and so the output file names, stay the file's stem. The
/// prefix is `./` on Windows too, which TeX takes there: with `.\main.tex` it would read `\main`
/// as a command.
pub fn command_line(req: &BuildRequest) -> (Vec<OsString>, Vec<(String, String)>) {
    let mut args: Vec<OsString> = Vec::with_capacity(7);
    if let Some(flag) = engine_flag(req.engine) {
        args.push(flag.into());
    }
    args.extend(
        [
            "-synctex=1",
            "-interaction=nonstopmode",
            "-file-line-error",
            "-recorder",
        ]
        .map(OsString::from),
    );
    args.push(outdir_arg(req));
    args.push(root_arg(req));
    let env = LOG_ENV
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    (args, env)
}

/// The arguments (without the program) that remove what the build of `req` generated:
///
/// ```text
/// latexmk [-pdf|-lualatex|-xelatex] [-bibtex-cond1] -c -outdir=<outdir> ./<root file name>
/// ```
///
/// With the engine flag and output directory of [`command_line`], latexmk deletes the files it
/// knows that build to have generated in the output directory (`.aux`, `.toc`, `.fls`, the log,
/// its own database, …), so the next build starts from the sources. `-c` rather than `-C`: the
/// PDF and the SyncTeX file stay, so the preview does not go blank. `-bibtex-cond1` also deletes
/// the `.bbl` when its `.bib` files exist (a stale one, say biblatex's after a switch to
/// BibTeX, fails the next build before BibTeX can replace it), but only in an output directory
/// of its own: in the root's directory (`outdir = "."`) the `.bbl` may be a source (arXiv
/// projects come with one and no `.bib`), and there latexmk deletes it too when the project
/// was never built. Sources are never deleted.
pub fn clean_command_line(req: &BuildRequest) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::with_capacity(5);
    if let Some(flag) = engine_flag(req.engine) {
        args.push(flag.into());
    }
    if !outdir_is_root_dir(req) {
        args.push("-bibtex-cond1".into());
    }
    args.push("-c".into());
    args.push(outdir_arg(req));
    args.push(root_arg(req));
    args
}

/// Whether the output directory is the root's directory (`outdir = "."`).
fn outdir_is_root_dir(req: &BuildRequest) -> bool {
    let root_dir = req.root.parent().unwrap_or(Path::new(""));
    normalize(&root_dir.join(&req.outdir)) == normalize(root_dir)
}

/// `-outdir=<outdir>`.
fn outdir_arg(req: &BuildRequest) -> OsString {
    let mut outdir = OsString::from("-outdir=");
    outdir.push(&req.outdir);
    outdir
}

/// `./<root file name>` (see [`command_line`]).
fn root_arg(req: &BuildRequest) -> OsString {
    match req.root.file_name() {
        Some(name) => {
            let mut arg = OsString::from("./");
            arg.push(name);
            arg
        }
        None => req.root.clone().into_os_string(),
    }
}

fn engine_flag(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::PdfLatex => Some("-pdf"),
        Engine::LuaLatex => Some("-lualatex"),
        Engine::XeLatex => Some("-xelatex"),
        Engine::Latexmkrc => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(engine: Engine) -> BuildRequest {
        BuildRequest {
            root: PathBuf::from("/home/u/thesis/main.tex"),
            engine,
            outdir: ".build".into(),
            mirror_dirs: vec![PathBuf::from("chapters")],
            timeout: Duration::from_secs(180),
            latexmk: PathBuf::from("latexmk"),
        }
    }

    fn argv(req: &BuildRequest) -> Vec<String> {
        command_line(req)
            .0
            .into_iter()
            .map(|a| a.into_string().expect("utf-8 arg"))
            .collect()
    }

    #[test]
    fn exact_argv_per_engine() {
        let tail = [
            "-synctex=1",
            "-interaction=nonstopmode",
            "-file-line-error",
            "-recorder",
            "-outdir=.build",
            "./main.tex",
        ];
        for (engine, flag) in [
            (Engine::PdfLatex, Some("-pdf")),
            (Engine::LuaLatex, Some("-lualatex")),
            (Engine::XeLatex, Some("-xelatex")),
            (Engine::Latexmkrc, None),
        ] {
            let want: Vec<&str> = flag.into_iter().chain(tail).collect();
            assert_eq!(argv(&request(engine)), want, "{engine:?}");
        }
    }

    #[test]
    fn exact_env() {
        let (_, env) = command_line(&request(Engine::PdfLatex));
        let want = [
            ("max_print_line", "10000"),
            ("error_line", "254"),
            ("half_error_line", "238"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        assert_eq!(env, want);
    }

    #[test]
    fn custom_outdir_and_spaces() {
        let mut req = request(Engine::XeLatex);
        req.outdir = "out dir".into();
        req.root = PathBuf::from("/p/my thesis.tex");
        let args = argv(&req);
        assert_eq!(
            args[args.len() - 2..],
            ["-outdir=out dir", "./my thesis.tex"]
        );
    }

    #[test]
    fn root_is_never_an_option() {
        let mut req = request(Engine::PdfLatex);
        req.root = PathBuf::from("/p/-pdflatex=touch x;true.tex");
        assert_eq!(
            argv(&req).last().map(String::as_str),
            Some("./-pdflatex=touch x;true.tex")
        );
        let clean = clean_command_line(&req);
        assert_eq!(
            clean.last().and_then(|a| a.to_str()),
            Some("./-pdflatex=touch x;true.tex")
        );
    }

    /// The clean-up names the build's engine flag, output directory and root, with `-c` (which
    /// keeps the PDF), and deletes the `.bbl` only in an output directory of its own.
    #[test]
    fn exact_clean_argv_per_engine() {
        let clean_argv = |req: &BuildRequest| -> Vec<String> {
            clean_command_line(req)
                .into_iter()
                .map(|a| a.into_string().expect("utf-8 arg"))
                .collect()
        };
        for (engine, flag) in [
            (Engine::PdfLatex, Some("-pdf")),
            (Engine::LuaLatex, Some("-lualatex")),
            (Engine::XeLatex, Some("-xelatex")),
            (Engine::Latexmkrc, None),
        ] {
            let mut req = request(engine);
            let want: Vec<&str> = flag
                .into_iter()
                .chain(["-bibtex-cond1", "-c", "-outdir=.build", "./main.tex"])
                .collect();
            assert_eq!(clean_argv(&req), want, "{engine:?}");
            for outdir in [".", "./", "", "out/..", "/home/u/thesis"] {
                req.outdir = outdir.into();
                let want: Vec<String> = flag
                    .into_iter()
                    .map(str::to_owned)
                    .chain(["-c".to_owned(), format!("-outdir={outdir}")])
                    .chain(["./main.tex".to_owned()])
                    .collect();
                assert_eq!(clean_argv(&req), want, "{engine:?} {outdir:?}");
            }
            req.outdir = "../build".into();
            assert!(clean_argv(&req).contains(&"-bibtex-cond1".to_owned()));
        }
    }
}
