# Lemma supplement to latex-document.cwl (merged into the same package).
# Written for Lemma (GPL-3.0-or-later) in TeXstudio's .cwl syntax: kernel commands that
# TeXstudio keeps in tex.cwl and latex-mathsymbols.cwl, which data/raw/cwl does not have.
# Math symbols such as \alpha come from data/raw/umath_map.tsv; glyphs for the commands
# below are in data/completion/glyphs.tsv.

# \begin and \end themselves (the word list only describes environments)
\begin{%<env%>}
\end{%<env%>}

# Roots, delimiters and sizes
\sqrt{arg}#m
\middle#m
\big#m
\Big#m
\bigl#m
\bigr#m
\Bigl#m
\Bigr#m
\biggl#m
\biggr#m
\Biggl#m
\Biggr#m
\bigm#*m
\Bigm#*m

# Log-like functions
\arccos#m
\arcsin#m
\arctan#m
\arg#m
\cos#m
\cosh#m
\cot#m
\coth#m
\csc#m
\deg#m
\det#m
\dim#m
\exp#m
\gcd#m
\hom#m
\inf#m
\ker#m
\lg#m
\lim#m
\liminf#m
\limsup#m
\ln#m
\log#m
\max#m
\min#m
\Pr#m
\sec#m
\sin#m
\sinh#m
\sup#m
\tan#m
\tanh#m

# Dots, decorations, styles
\dots
\cdots#m
\ddots#m
\overline{arg}#m
\underline{text}
\displaystyle#m
\textstyle#m
\scriptstyle#m
\scriptscriptstyle#m
\limits#m
\nolimits#m
\mathop{arg}#m
\mathrel{arg}#m
\mathbin{arg}#m
\mathord{arg}#m
\mathopen{arg}#*m
\mathclose{arg}#*m
\phantom{arg}
\hphantom{arg}
\vphantom{arg}
\smash{arg}

# Relations and operators known by their short or legacy names
\to#m
\gets#m
\ne#m
\neq#m
\le#m
\ge#m
\iff#m
\land#m
\lor#m
\lnot#m
\owns#m
\emptyset#m
\notin#m
\not#m
\colon#m
\hbar#m
\pmod{arg}#m
\bmod#m

# Symbols valid in text and math
\S
\P
\dag
\ddag
\copyright
\pounds
\TeX
\LaTeX

# Boxes (the word list only has the picture forms of \makebox in this package)
\makebox{text}
\framebox{text}

# Spacing and layout
\quad
\qquad
\centering
\raggedright
\raggedleft
\noindent
\indent
\par
\smallskip
\medskip
\bigskip
\hfill
\vfill
\hfil#*
\vfil#*
\leavevmode#*
\centerline{text}#*
