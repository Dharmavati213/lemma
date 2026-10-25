//! Acceptance tests of `Completer::complete` and the sources around it.

use lemma_complete::{
    Completer, CompletionDb, CompletionRequest, Insert, Item, ItemKind, MAX_ITEMS, WordList,
    glyph_for,
};
use lemma_snippets::{Chunk, SnippetSet, parse_body};
use lemma_syntax::{ArgKind, Context, ProjectIndex, index_file, parse_aux, scan_bib};
use std::path::PathBuf;
use std::sync::Arc;

fn fixture(rel: &str) -> String {
    let path = format!("{}/../../tests/fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The `tests/fixtures/proj` project: two `.tex` files, the `.bib` and the `.aux` of a build.
fn proj_index() -> ProjectIndex {
    let mut idx = ProjectIndex::default();
    for rel in ["main.tex", "chapters/yoneda.tex"] {
        let text = fixture(&format!("proj/{rel}"));
        idx.update_file(PathBuf::from("/p").join(rel), Arc::new(index_file(&text)));
    }
    idx.bibs.insert(
        PathBuf::from("/p/refs.bib"),
        Arc::new(scan_bib(&fixture("proj/refs.bib"))),
    );
    idx.aux = Arc::new(parse_aux(&fixture("proj/main.aux")));
    idx
}

fn index_of(text: &str) -> ProjectIndex {
    let mut idx = ProjectIndex::default();
    idx.update_file(PathBuf::from("/p/main.tex"), Arc::new(index_file(text)));
    idx
}

fn math() -> Context {
    Context::Math {
        display: false,
        env: None,
    }
}

/// The scanner's context in the argument of `cmd`: the item starts after the last `{` or `,`
/// and its blanks.
fn arg(cmd: &str, kind: ArgKind, line: &str) -> Context {
    let after = line.rfind(['{', ',']).map_or(0, |i| i + 1);
    let start = (after + line[after..].len() - line[after..].trim_start().len()) as u32;
    Context::Arg {
        cmd: cmd.into(),
        idx: 0,
        kind,
        start,
    }
}

struct Setup {
    db: CompletionDb,
    snippets: SnippetSet,
    idx: ProjectIndex,
    words: WordList,
    completer: Completer,
}

impl Setup {
    fn new() -> Self {
        Self {
            db: CompletionDb::builtin(),
            snippets: SnippetSet::defaults(),
            idx: ProjectIndex::default(),
            words: WordList::default(),
            completer: Completer::new(),
        }
    }

    fn with_index(idx: ProjectIndex) -> Self {
        Self { idx, ..Self::new() }
    }

    fn complete(&mut self, ctx: &Context, line: &str) -> Vec<Item> {
        let req = CompletionRequest::new(ctx, line);
        let mut out = vec![Item {
            label: "stale".into(),
            insert: Insert::Text(String::new()),
            kind: ItemKind::Word,
            detail: None,
            glyph: None,
            score: 0,
        }];
        self.completer.complete(
            &self.db,
            &self.snippets,
            &self.idx,
            &self.words,
            &req,
            &mut out,
        );
        assert!(out.len() <= MAX_ITEMS);
        assert!(
            out.windows(2).all(|w| w[0].score >= w[1].score),
            "not sorted by score: {:?}",
            labels(&out)
        );
        out
    }
}

fn labels(items: &[Item]) -> Vec<&str> {
    items.iter().map(|i| i.label.as_str()).collect()
}

fn snippet_text(chunks: &[Chunk]) -> String {
    chunks
        .iter()
        .map(|c| match c {
            Chunk::Text(t) => t.clone(),
            Chunk::Stop { n, .. } | Chunk::Mirror(n) => format!("${n}"),
        })
        .collect()
}

fn insert_text(item: &Item) -> String {
    match &item.insert {
        Insert::Text(t) => t.clone(),
        Insert::Snippet(chunks) => snippet_text(chunks),
    }
}

// ----- the acceptance table -----

#[test]
fn fr_in_math_offers_frac_first() {
    let mut s = Setup::new();
    let items = s.complete(&math(), r"$x = \fr");
    let frac = &items[0];
    assert_eq!(frac.label, r"\frac", "{:?}", labels(&items));
    assert_eq!(frac.kind, ItemKind::Command);
    assert_eq!(insert_text(frac), r"\frac{$1}{$2}$0");
    assert_eq!(frac.detail.as_deref(), Some("{num}{den} · LaTeX"));
    // With only the command-name context (mode unknown) \frac still leads.
    let items = s.complete(&Context::CommandName { start: 5 }, r"$x = \fr");
    assert_eq!(items[0].label, r"\frac");
}

#[test]
fn begin_al_offers_align_align_star_alignat() {
    let mut s = Setup::new();
    let line = r"\begin{al";
    let items = s.complete(&arg("begin", ArgKind::Env, line), line);
    assert_eq!(
        labels(&items)[..3],
        ["align", "align*", "alignat"],
        "{:?}",
        labels(&items)
    );
    assert!(items[..3].iter().all(|i| i.kind == ItemKind::Environment));
    assert_eq!(items[0].insert, Insert::Text("align".into()));
    assert_eq!(items[0].detail.as_deref(), Some("amsmath"));
}

#[test]
fn ref_offers_labels_with_aux_numbers() {
    let mut s = Setup::with_index(proj_index());
    let line = r"see~\ref{";
    let items = s.complete(&arg("ref", ArgKind::Ref, line), line);
    let get = |name: &str| {
        items
            .iter()
            .find(|i| i.label == name)
            .unwrap_or_else(|| panic!("{name} missing: {:?}", labels(&items)))
    };
    assert_eq!(get("sec:intro").detail.as_deref(), Some("1 · Introduction"));
    assert_eq!(
        get("thm:fpqc").detail.as_deref(),
        Some("3.2 · theorem · fpqc descent")
    );
    assert_eq!(
        get("lem:yoneda").detail.as_deref(),
        Some("2.1 · lemma · Yoneda")
    );
    assert_eq!(get("eq:cocycle").detail.as_deref(), Some("(1) · equation"));
    assert!(items.iter().all(|i| i.kind == ItemKind::Label));
    assert_eq!(get("cor:ff").insert, Insert::Text("cor:ff".into()));
    // Every label of both files and the .aux, once.
    assert_eq!(items.len(), 14, "{:?}", labels(&items));

    let line = r"see~\ref{thm";
    let items = s.complete(&arg("ref", ArgKind::Ref, line), line);
    assert_eq!(items[0].label, "thm:fpqc");
}

#[test]
fn cref_lists_complete_the_item_after_the_comma() {
    let mut s = Setup::with_index(proj_index());
    let line = r"\cref{sec:intro, lem";
    let items = s.complete(&arg("cref", ArgKind::Ref, line), line);
    assert_eq!(items[0].label, "lem:yoneda");
    let ctx = arg("cref", ArgKind::Ref, line);
    let req = CompletionRequest::new(&ctx, line);
    assert_eq!(&line[req.replace_start()..], "lem");
}

#[test]
fn cite_offers_bib_entries_with_author_and_year() {
    let mut s = Setup::with_index(proj_index());
    let line = r"\cite{";
    let items = s.complete(&arg("cite", ArgKind::Cite, line), line);
    assert_eq!(labels(&items).len(), 4, "{:?}", labels(&items));
    let get = |key: &str| items.iter().find(|i| i.label == key).expect(key);
    assert_eq!(
        get("SGA1").detail.as_deref(),
        Some("Grothendieck 1971 · Revêtements étales et groupe fondamental (SGA 1)")
    );
    assert_eq!(
        get("MacLane").detail.as_deref(),
        Some("Mac Lane 1998 · Categories for the Working Mathematician")
    );
    assert_eq!(
        get("Stacks").detail.as_deref(),
        Some("The Stacks project authors 2026 · The Stacks project")
    );
    assert!(items.iter().all(|i| i.kind == ItemKind::Citation));
    let line = r"\cite{Mac";
    let items = s.complete(&arg("cite", ArgKind::Cite, line), line);
    assert_eq!(items[0].label, "MacLane");
}

#[test]
fn user_macro_z_has_the_glyph_of_its_definition() {
    let mut s = Setup::with_index(index_of(
        "\\newcommand{\\Z}{\\mathbb{Z}}\n\\DeclareMathOperator{\\Hom}{Hom}\n",
    ));
    let items = s.complete(&math(), r"$x\in\Z");
    let z = &items[0];
    assert_eq!(z.label, r"\Z", "{:?}", labels(&items));
    assert_eq!(z.kind, ItemKind::UserMacro);
    assert_eq!(z.glyph, Some('ℤ'));
    assert_eq!(z.insert, Insert::Text(r"\Z".into()));
    assert_eq!(z.detail.as_deref(), Some(r"\mathbb{Z}"));

    // Undefined: no item `\Z` at all.
    let mut s = Setup::new();
    let items = s.complete(&math(), r"$x\in\Z");
    assert!(!labels(&items).contains(&r"\Z"), "{:?}", labels(&items));
}

#[test]
fn macros_of_a_local_package_but_not_its_internal_ones() {
    let mut idx = ProjectIndex::default();
    idx.update_file(
        PathBuf::from("/p/mymacros.sty"),
        Arc::new(index_file(
            "\\ProvidesPackage{mymacros}\n\\newcommand{\\my@helper}{x}\n\
             \\newcommand{\\mydef}{\\my@helper}\n\\def\\mysecond{y}\n",
        )),
    );
    let mut s = Setup::with_index(idx);
    let items = s.complete(&Context::Text, r"See \my");
    let found = labels(&items);
    assert!(found.contains(&r"\mydef"), "{found:?}");
    assert!(found.contains(&r"\mysecond"), "{found:?}");
    assert!(!found.iter().any(|l| l.contains('@')), "{found:?}");
}

#[test]
fn text_only_commands_are_hidden_in_math_and_math_only_in_text() {
    let mut s = Setup::new();
    let math_items = s.complete(&math(), r"$\sec");
    assert!(!labels(&math_items).contains(&r"\section"));
    assert!(labels(&math_items).contains(&r"\sec"));
    let text_items = s.complete(&Context::Text, r"\sec");
    assert!(labels(&text_items).contains(&r"\section"));
    assert!(!labels(&text_items).contains(&r"\sec"));

    assert!(!labels(&s.complete(&math(), r"$\ite")).contains(&r"\item"));
    assert!(labels(&s.complete(&Context::Text, r"\ite")).contains(&r"\item"));
    assert!(!labels(&s.complete(&math(), r"$\textbacks")).contains(&r"\textbackslash"));
    assert!(!labels(&s.complete(&math(), r"$\maket")).contains(&r"\maketitle"));

    let text_items = s.complete(&Context::Text, r"\alp");
    assert!(!labels(&text_items).contains(&r"\alpha"));
    let math_items = s.complete(&math(), r"$\alp");
    assert_eq!(math_items[0].label, r"\alpha");
    assert_eq!(math_items[0].glyph, Some('α'));

    // \text{…} inside math is text.
    let items = s.complete(&Context::TextInMath, r"$\text{\sec");
    assert!(labels(&items).contains(&r"\section"));
}

#[test]
fn exact_line_begin_trigger_shows_only_the_snippet() {
    let mut s = Setup::new();
    s.words = WordList::from_text(&["theorem\nthmlike\n"]);
    for line in ["thm", "  thm", "\tthm"] {
        let items = s.complete(&Context::Text, line);
        assert_eq!(labels(&items), ["thm"], "{line:?}");
        assert_eq!(items[0].kind, ItemKind::Snippet);
        assert_eq!(items[0].detail.as_deref(), Some("theorem"));
        assert_eq!(
            insert_text(&items[0]),
            "\\begin{theorem}$1\n  $0\n\\end{theorem}"
        );
    }
    // Not at the line's beginning: no line-begin snippet, the words remain.
    let items = s.complete(&Context::Text, "x thm");
    assert_eq!(labels(&items), ["thmlike"]);
    // A shorter prefix offers the snippets that start with it, and the words.
    let items = s.complete(&Context::Text, "th");
    assert_eq!(labels(&items), ["thm", "theorem", "thmlike"]);
}

// ----- more behaviour -----

#[test]
fn at_most_fifty_items_and_usual_commands_after_a_bare_backslash() {
    let mut s = Setup::new();
    let items = s.complete(&math(), r"$\");
    assert_eq!(items.len(), MAX_ITEMS);
    assert_eq!(items[0].label, r"\frac");
    assert!(items.iter().all(|i| i.kind == ItemKind::Command));
    let ctx = math();
    let req = CompletionRequest::new(&ctx, r"$\");
    assert_eq!(req.replace_start(), 1);
}

#[test]
fn begin_and_end_are_commands_too() {
    let mut s = Setup::new();
    let items = s.complete(&Context::Text, r"x \beg");
    assert_eq!(items[0].label, r"\begin");
    assert_eq!(insert_text(&items[0]), r"\begin{$1}$0");
    assert_eq!(items[0].detail.as_deref(), Some("{env} · LaTeX"));
    let items = s.complete(&math(), r"$\en");
    assert_eq!(items[0].label, r"\end");
}

#[test]
fn common_commands_win_ties() {
    let mut s = Setup::new();
    let items = s.complete(&math(), r"$\al");
    assert_eq!(items[0].label, r"\alpha");
    assert!(labels(&items).contains(&r"\aleph"));
    let items = s.complete(&math(), r"$\to");
    assert_eq!(items[0].label, r"\to");
    assert_eq!(items[0].glyph, Some('→'));
}

#[test]
fn recency_moves_accepted_items_up() {
    let mut s = Setup::new();
    assert_eq!(s.complete(&math(), r"$\fr")[0].label, r"\frac");
    for _ in 0..3 {
        s.completer.record_accepted(r"\frown");
    }
    assert_eq!(s.complete(&math(), r"$\fr")[0].label, r"\frown");
}

#[test]
fn dictionary_words_in_text_only_from_two_characters_at_most_twelve() {
    let mut s = Setup::new();
    let dict: String = (0..20).map(|i| format!("abelian{i:02}\n")).collect();
    s.words = WordList::from_text(&[&dict, "Grothendieck\nétale\ndual\n"]);
    let items = s.complete(&Context::Text, "ab");
    assert_eq!(items.len(), 12);
    assert!(items.iter().all(|i| i.kind == ItemKind::Word));
    assert_eq!(items[0].label, "abelian00");
    assert_eq!(items[11].label, "abelian11");
    let one = s.complete(&Context::Text, "a");
    assert!(
        one.iter().all(|i| i.kind != ItemKind::Word),
        "{:?}",
        labels(&one)
    );
    assert!(s.complete(&math(), "$ab").is_empty());
    // Case-insensitive prefix match only (blink-cmp-dictionary).
    assert_eq!(labels(&s.complete(&Context::Text, "gro")), ["Grothendieck"]);
    assert_eq!(labels(&s.complete(&Context::Text, "ét")), ["étale"]);
    assert!(s.complete(&Context::Text, "ual").is_empty());
    // The word being typed is not proposed back.
    assert!(s.complete(&Context::Text, "dual").is_empty());
    // Words are offered in \text{…} too.
    assert_eq!(
        s.complete(&Context::TextInMath, "$\\text{abelian0").len(),
        10
    );

    // A snippet and a word with the same prefix match: +20 against −8.
    s.snippets =
        SnippetSet::from_toml("[[snippet]]\ntrigger = 'abc'\nbody = 'x'\n").expect("valid");
    s.words = WordList::from_text(&["abcdef\n"]);
    let items = s.complete(&Context::Text, "x ab");
    assert_eq!(labels(&items), ["abc", "abcdef"]);
    assert_eq!(items[0].score - items[1].score, 28);
}

#[test]
fn project_words_and_used_commands() {
    let mut s = Setup::with_index(proj_index());
    s.words = WordList::from_project(&[&fixture("proj/main.tex")]);
    let items = s.complete(&Context::Text, "Groth");
    assert_eq!(labels(&items), ["Grothendieck"]);
    // \rightrightarrows is used in the project: +8.
    let used = s.complete(&math(), r"$\rightright")[0].clone();
    assert_eq!(used.label, r"\rightrightarrows");
    let unused = Setup::with_index(proj_index()).complete(&math(), r"$\rightright")[0].clone();
    assert_eq!(used.score - unused.score, 8);
    // User macros come with their definitions; math operators stay out of text.
    let items = s.complete(&math(), r"$\c");
    assert_eq!(&labels(&items)[..3], [r"\cC", r"\cF", r"\cO"]);
    assert_eq!(items[0].glyph, Some('𝒞'));
}

#[test]
fn project_environments_and_theorems() {
    let mut s = Setup::with_index(proj_index());
    let line = r"\begin{lem";
    let items = s.complete(&arg("begin", ArgKind::Env, line), line);
    assert_eq!(items[0].label, "lemma");
    assert_eq!(items[0].detail.as_deref(), Some("theorem: Lemma"));
    // tikzcd is used in the project although tikz-cd's tables are not loaded.
    let line = r"\begin{tikz";
    let items = s.complete(&arg("begin", ArgKind::Env, line), line);
    assert_eq!(items[0].label, "tikzcd");
    assert_eq!(items[0].detail.as_deref(), Some("used in the project"));
    s.db.load_packages(&s.idx.packages());
    let items = s.complete(&arg("begin", ArgKind::Env, line), line);
    assert_eq!(items[0].label, "tikzcd");
    assert_eq!(items[0].detail.as_deref(), Some("tikz-cd"));
}

#[test]
fn environments_follow_the_mode_when_it_is_known() {
    let mut s = Setup::new();
    assert_eq!(s.complete(&math(), r"$\begin{pm")[0].label, "pmatrix");
    let in_math = s.complete(&math(), r"$\begin{al");
    assert_eq!(labels(&in_math)[..2], ["aligned", "alignedat"]);
    assert!(!labels(&in_math).contains(&"align"));
    let in_text = s.complete(&Context::Text, r"\begin{al");
    assert_eq!(in_text[0].label, "align");
    assert!(!labels(&in_text).contains(&"aligned"));
    assert!(!labels(&s.complete(&Context::Text, r"\begin{pm")).contains(&"pmatrix"));
    assert!(!labels(&s.complete(&Context::Text, r"\begin{cas")).contains(&"cases"));
    assert!(labels(&s.complete(&math(), r"\[\begin{cas")).contains(&"cases"));
    // `\end{` closes what is open, whatever the mode: `align` inside `align` (math).
    let display = Context::Math {
        display: true,
        env: Some("align".into()),
    };
    assert_eq!(s.complete(&display, r"\end{alig")[0].label, "align");
    assert!(labels(&s.complete(&Context::Text, r"\end{pm")).contains(&"pmatrix"));
}

#[test]
fn eqref_prefers_equations() {
    let mut s = Setup::with_index(proj_index());
    let line = r"\eqref{";
    let items = s.complete(&arg("eqref", ArgKind::Ref, line), line);
    let all = labels(&items);
    // Referenced equations, then other referenced labels, then unreferenced equations.
    assert_eq!(all[..3], ["eq:cocycle", "eq:adj1", "eq:adj3"], "{all:?}");
    let pos = |l: &str| all.iter().position(|x| *x == l).expect(l);
    assert!(pos("eq:amitsur") < pos("def:descent-datum"));
    // Without \eqref unreferenced labels keep document order.
    let line = r"\ref{";
    let items = s.complete(&arg("ref", ArgKind::Ref, line), line);
    let all = labels(&items);
    let pos = |l: &str| all.iter().position(|x| *x == l).expect(l);
    assert!(pos("eq:amitsur") > pos("def:descent-datum"));
}

#[test]
fn commands_the_scanner_does_not_classify_use_the_word_list_classifiers() {
    let mut s = Setup::with_index(proj_index());
    s.db.load_packages(&["mathtools".into()]);
    // mathtools' \refeq is a reference command for the word list, not for the scanner.
    let items = s.complete(&Context::Text, r"see \refeq{eq:co");
    assert_eq!(items[0].label, "eq:cocycle");
    let req = CompletionRequest::new(&Context::Text, r"see \refeq{eq:co");
    assert_eq!(req.replace_start(), 11);
}

#[test]
fn manual_snippets_choose_the_if_undefined_body() {
    let set = SnippetSet::from_toml(
        "[[snippet]]\ntrigger = 'nat'\nbody = '\\N'\nif_undefined = '\\mathbb{N}'\n",
    )
    .expect("valid");
    let mut s = Setup::new();
    s.snippets = set.clone();
    let items = s.complete(&math(), "$nat");
    assert_eq!(insert_text(&items[0]), r"\mathbb{N}$0");
    let mut s = Setup::with_index(index_of("\\newcommand{\\N}{\\mathbb{N}}\n"));
    s.snippets = set;
    let items = s.complete(&math(), "$nat");
    assert_eq!(insert_text(&items[0]), r"\N$0");
}

#[test]
fn files_by_command() {
    let mut s = Setup::new();
    assert!(s.completer.files().is_empty());
    let files = [
        "main.tex",
        "chapters/yoneda.tex",
        "fig/plot.pdf",
        "refs.bib",
        "notes.txt",
    ]
    .map(String::from)
    .to_vec();
    s.completer.set_files(files.clone());
    assert_eq!(s.completer.files(), files);
    let line = r"\input{ch";
    let items = s.complete(&arg("input", ArgKind::File, line), line);
    assert_eq!(labels(&items), ["chapters/yoneda"]);
    assert_eq!(items[0].kind, ItemKind::File);
    let line = r"\includegraphics[width=3cm]{";
    let items = s.complete(&arg("includegraphics", ArgKind::File, line), line);
    assert_eq!(labels(&items), ["fig/plot.pdf"]);
    let line = r"\bibliography{";
    assert_eq!(
        labels(&s.complete(&arg("bibliography", ArgKind::File, line), line)),
        ["refs"]
    );
    let line = r"\addbibresource{";
    assert_eq!(
        labels(&s.complete(&arg("addbibresource", ArgKind::File, line), line)),
        ["refs.bib"]
    );
}

#[test]
fn files_default_to_the_index() {
    let mut s = Setup::with_index(proj_index());
    let line = r"\input{";
    let items = s.complete(&arg("input", ArgKind::File, line), line);
    assert_eq!(labels(&items), ["chapters/yoneda", "main"]);
    let line = r"\addbibresource{";
    let items = s.complete(&arg("addbibresource", ArgKind::File, line), line);
    assert_eq!(labels(&items), ["refs.bib"]);
}

#[test]
fn packages_classes_and_tikz_libraries() {
    let mut s = Setup::new();
    let line = r"\usepackage{amss";
    let items = s.complete(&arg("usepackage", ArgKind::Package, line), line);
    assert_eq!(items[0].label, "amssymb");
    assert_eq!(items[0].kind, ItemKind::Package);
    let line = r"\documentclass[11pt]{amsa";
    let items = s.complete(&arg("documentclass", ArgKind::Package, line), line);
    assert_eq!(items[0].label, "amsart");
    let line = r"\usetikzlibrary{arrows.m";
    let items = s.complete(&arg("usetikzlibrary", ArgKind::Package, line), line);
    assert_eq!(items[0].label, "arrows.meta");
}

#[test]
fn nothing_in_comments_verbatim_or_label_definitions() {
    let mut s = Setup::new();
    assert!(s.complete(&Context::Comment, r"% \fr").is_empty());
    assert!(s.complete(&Context::Verbatim, r"\fr").is_empty());
    let line = r"\label{eq:";
    assert!(
        s.complete(&arg("label", ArgKind::Label, line), line)
            .is_empty()
    );
}

#[test]
fn generated_tables_are_consistent() {
    let mut db = CompletionDb::builtin();
    db.load_packages(
        &CompletionDb::available_packages()
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        db.loaded_packages().len(),
        CompletionDb::available_packages().len()
    );
    let (commands, environments) = db.counts();
    assert!(commands > 2500, "{commands}");
    assert!(environments > 90, "{environments}");
    for c in db.commands() {
        assert!(c.snippet.starts_with(&format!("\\{}", c.name)), "{c:?}");
        parse_body(c.snippet).unwrap_or_else(|e| panic!("{c:?}: {e}"));
    }
    for e in db.environments() {
        let chunks = db.env_snippet(e.name);
        let text = snippet_text(&chunks);
        assert!(
            text.starts_with(&format!("\\begin{{{}}}", e.name))
                && text.ends_with(&format!("\\end{{{}}}", e.name)),
            "{e:?}: {text}"
        );
        assert_eq!(text.matches("$0").count(), 1, "{e:?}");
    }
    // Every builtin math symbol of the unicode-math table has its glyph.
    for name in [
        "alpha",
        "varepsilon",
        "infty",
        "otimes",
        "hookrightarrow",
        "mapsto",
        "Rightarrow",
    ] {
        assert!(
            db.command(name).is_some_and(|c| c.glyph.is_some()),
            "{name}"
        );
    }
    assert_eq!(glyph_for(r"\mathfrak{g}"), Some('𝔤'));
}

/// Commands and environments by context: in math, the ones that belong in formulas come first
/// and those of no particular mode that belong in text (`\frame`, `\footref`, `\figurename`,
/// `itemize`) far down or not at all; in text, math-only ones are not offered.
#[test]
fn math_and_text_entries_by_context() {
    let mut s = Setup::new();
    let math = math();
    let display = Context::Math {
        display: true,
        env: Some("align".into()),
    };
    let text = Context::Text;
    // (context, line, entries that must be offered, in this order; entries that may only come
    // after all of them, if at all)
    let cases: &[(&Context, &str, &[&str], &[&str])] = &[
        (
            &math,
            r"$x = \fr",
            &[r"\frac", r"\dfrac"],
            &[
                r"\frame",
                r"\framebox",
                r"\footref",
                r"\figurename",
                r"\fontseries",
            ],
        ),
        (
            &display,
            r"  a &= \fr",
            &[r"\frac"],
            &[r"\frame", r"\framebox", r"\footref", r"\figurename"],
        ),
        (&text, r"see \fr", &[r"\frame"], &[r"\frac", r"\cfrac"]),
        (&math, r"$\te", &[r"\text"], &[r"\textbf", r"\texttt"]),
        (
            &math,
            r"$\la",
            &[r"\label", r"\lambda"],
            &[r"\large", r"\LaTeX"],
        ),
        (&text, r"\la", &[r"\label"], &[r"\lambda", r"\langle"]),
        (&math, r"$\sm", &[r"\smash"], &[r"\small"]),
        (&math, r"$\hs", &[r"\hspace"], &[]),
        (&math, r"$\qu", &[r"\quad", r"\qquad"], &[]),
        (&math, r"$\en", &[r"\end"], &[r"\enlargethispage"]),
        (&math, r"$\tex", &[r"\text"], &[r"\textit"]),
        (&math, r"$\bf", &[], &[r"\bfseries"]),
    ];
    let mut failures = Vec::new();
    for (ctx, line, lead, late) in cases {
        let items = s.complete(ctx, line);
        let got = labels(&items);
        let at = |name: &str| got.iter().position(|l| *l == name);
        let positions: Vec<Option<usize>> = lead.iter().map(|l| at(l)).collect();
        if positions.iter().any(Option::is_none) || !positions.windows(2).all(|w| w[0] < w[1]) {
            failures.push(format!("{line:?}: {lead:?} not offered in order: {got:?}"));
            continue;
        }
        let last_lead = positions.iter().flatten().max().copied();
        for name in *late {
            if let Some(p) = at(name)
                && last_lead.is_some_and(|l| p < l)
            {
                failures.push(format!("{line:?}: {name} before {lead:?}: {got:?}"));
            }
        }
        // In math, every entry that belongs in text comes after every other one.
        if matches!(ctx, Context::Math { .. }) {
            for name in *late {
                if let (Some(p), Some(first_math)) =
                    (at(name), got.iter().position(|l| !late.contains(l)))
                    && p < first_math
                {
                    failures.push(format!("{line:?}: {name} leads: {got:?}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    // After `\fr` in math, the math commands with these letters all come before the first
    // command that belongs in text.
    let items = s.complete(&math, r"$\fr");
    let got = labels(&items);
    let first_text = got
        .iter()
        .position(|l| [r"\frame", r"\framebox", r"\footref", r"\figurename"].contains(l))
        .unwrap_or(got.len());
    for name in [
        r"\frac",
        r"\cfrac",
        r"\dfrac",
        r"\tfrac",
        r"\frown",
        r"\mathfrak",
    ] {
        let p = got.iter().position(|l| *l == name);
        assert!(
            p.is_some_and(|p| p < first_text),
            "{name} not before the text commands: {got:?}"
        );
    }
    // Environments: `itemize` belongs in text, `split` (math only) in formulas.
    let in_math = s.complete(&math, r"$\begin{it");
    let got = labels(&in_math);
    let (split, itemize) = (
        got.iter().position(|l| *l == "split"),
        got.iter().position(|l| *l == "itemize"),
    );
    assert!(
        split.is_some() && itemize.is_none_or(|i| split.is_some_and(|s| s < i)),
        "{got:?}"
    );
    assert_eq!(s.complete(&text, r"\begin{it")[0].label, "itemize");
}

/// User macros with optional arguments complete with their mandatory arguments only.
#[test]
fn user_macros_complete_their_mandatory_arguments() {
    let text = "\\newcommand{\\norm}[2][]{\\lVert #2\\rVert_{#1}}\n\
                \\newcommand{\\pair}[2]{(#1,#2)}\n\
                \\NewDocumentCommand{\\set}{s o m}{\\{#3\\}}\n";
    let mut s = Setup::with_index(index_of(text));
    for (line, label, expected) in [
        (r"$\nor", r"\norm", r"\norm{$1}$0"),
        (r"$\pai", r"\pair", r"\pair{$1}{$2}$0"),
        (r"$\se", r"\set", r"\set{$1}$0"),
    ] {
        let items = s.complete(&math(), line);
        let item = items
            .iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("no {label} in {:?}", labels(&items)));
        assert_eq!(insert_text(item), expected);
    }
}
