// Owner: WP1.9 structure
//! `.aux` parsing for label numbers.

use crate::structure::args::{at, brace, is_letter, skip_space, strip_outer_braces};
use std::collections::BTreeMap;

/// Label numbers from the last build.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuxInfo {
    /// label -> (number, page), from `\newlabel`.
    pub labels: BTreeMap<String, (String, String)>,
}

/// Reads the `\newlabel` lines of an `.aux` file.
///
/// Handles the plain form `\newlabel{name}{{number}{page}}` and hyperref's five-argument form
/// `{{number}{page}{title}{anchor}{}}`. cleveref's `name@cref` entries and non-label uses such as
/// amsart's `\newlabel{tocindent0}{13.99pt}` are skipped. A later definition replaces an earlier one,
/// as in LaTeX. Labels defined in `\include`d files live in their own `.aux`; see [`aux_inputs`].
pub fn parse_aux(text: &str) -> AuxInfo {
    let b = text.as_bytes();
    let mut labels = BTreeMap::new();
    for p in memchr::memmem::find_iter(b, b"\\newlabel") {
        let after = p + "\\newlabel".len();
        if is_letter(at(b, after)) {
            continue;
        }
        let Some((name, end)) = brace(b, skip_space(b, after)) else {
            continue;
        };
        let name = text[name].trim();
        if name.is_empty() || name.ends_with("@cref") {
            continue;
        }
        let Some((data, _)) = brace(b, skip_space(b, end)) else {
            continue;
        };
        let Some((number, end)) = brace(b, skip_space(b, data.start)) else {
            continue;
        };
        let page = brace(b, skip_space(b, end))
            .filter(|(r, _)| r.end < data.end)
            .map_or("", |(r, _)| text[r].trim());
        let number = strip_outer_braces(text[number].trim()).trim();
        labels.insert(name.to_owned(), (number.to_owned(), page.to_owned()));
    }
    AuxInfo { labels }
}

/// The `.aux` files an `.aux` file pulls in with `\@input{…}` (one per `\include`d file), as
/// written (relative to the build directory).
pub fn aux_inputs(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    memchr::memmem::find_iter(b, b"\\@input")
        .filter_map(|p| brace(b, skip_space(b, p + "\\@input".len())))
        .map(|(r, _)| text[r].trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}
