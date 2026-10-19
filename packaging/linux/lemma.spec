# Fedora package of Lemma, built from files staged by packaging/linux/package.sh rpm
# (rpmbuild --define "lemma_version …" --define "lemma_stage …"). rpmbuild adds the library
# dependencies itself.

# The binaries are stripped already (Cargo's release profile).
%global debug_package %{nil}

Name:           lemma
Version:        %{lemma_version}
Release:        1%{?dist}
Summary:        LaTeX editor that shows an AI agent's edits inline for review
License:        GPL-3.0-or-later
URL:            https://github.com/Dharmavati213/lemma

Recommends:     texlive-latexmk
Recommends:     texlive-biber
Recommends:     texlive-biblatex
Recommends:     texlive-collection-latexrecommended
Recommends:     hunspell-en-US
Suggests:       texlive-chktex
Suggests:       texlive-texcount
Suggests:       texlive-latexindent
Suggests:       texlive-mylatexformat
Suggests:       git
Suggests:       nodejs
Suggests:       npm

%description
Lemma is a GTK 4 LaTeX editor built around reviewing an AI agent's edits: every
change the agent makes shows up inline as a hunk that you accept or reject on its
own. It is also a complete LaTeX editor without the agent: continuous PDF preview
with SyncTeX, build diagnostics in plain English, completion, snippets and a
project outline.

%install
cp -a %{lemma_stage}/. %{buildroot}/

%files
%{_bindir}/lemma
%{_bindir}/lemma-mcp
%{_datadir}/applications/io.github.lemma_editor.Lemma.desktop
%{_datadir}/icons/hicolor/scalable/apps/io.github.lemma_editor.Lemma.svg
%{_datadir}/lemma/
%{_datadir}/licenses/lemma/
%{_docdir}/lemma/
