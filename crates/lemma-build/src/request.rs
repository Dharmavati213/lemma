//! What to build, and the exact latexmk invocation for it (§6).

use crate::Engine;
use std::ffi::OsString;
use std::path::PathBuf;
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
const LOG_ENV: [(&str, &str); 3] = [
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
    let mut outdir = OsString::from("-outdir=");
    outdir.push(&req.outdir);
    args.push(outdir);
    args.push(match req.root.file_name() {
        Some(name) => {
            let mut arg = OsString::from("./");
            arg.push(name);
            arg
        }
        None => req.root.clone().into_os_string(),
    });
    let env = LOG_ENV
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    (args, env)
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
    }
}
