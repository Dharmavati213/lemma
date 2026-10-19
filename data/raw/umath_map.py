import json, re, collections
rows = json.load(open('umath.json'))
GREEK = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega".split()
GREEK += ["var"+g for g in "epsilon theta kappa pi rho sigma phi".split()]
CAPS = "Gamma Delta Theta Lambda Xi Pi Sigma Upsilon Phi Psi Omega".split()
# unicode-math legacy aliases (from unicode-math-luatex.sty \protected\def lines)
ALIAS = {'vysmwhtcircle':'circ','smblkcircle':'bullet','smwhtdiamond':'diamond','mdlgwhtcircle':'bigcirc','rightarrow':'to','leq':'le','geq':'ge','ne':'neq','varnothing':'emptyset','hslash':'hbar','wedge':'land','vee':'lor','ni':'owns','leftarrow':'gets','neg':'lnot'}
STYLE = {'Bbb':('amssymb','\\mathbb{%s}'),'mfrak':('amssymb','\\mathfrak{%s}'),'mscr':('mathrsfs','\\mathscr{%s}'),'mbf':('kernel','\\mathbf{%s}'),'msans':('kernel','\\mathsf{%s}'),'mtt':('kernel','\\mathtt{%s}'),'mbfit':('amsmath','\\boldsymbol{%s}')}
out=[]; stat=collections.Counter()
for r in rows:
    n=r['name']; cmd=None; pkg=None
    if r['pdflatex']: cmd='\\'+n; pkg=r['pdflatex']
    m=re.fullmatch(r'm(up|it)([A-Za-z]+)',n)
    if not cmd and m and (m.group(2) in GREEK or m.group(2) in CAPS):
        # traditional TeX: lowercase greek italic, capitals upright
        if (m.group(1)=='it' and m.group(2)[0].islower()) or (m.group(1)=='up' and m.group(2) in CAPS):
            cmd='\\'+m.group(2); pkg='kernel'
        elif m.group(1)=='up' and m.group(2)[0].islower():
            cmd='\\'+m.group(2); pkg='kernel (upright glyph; italic in output)'
    m=re.fullmatch(r'(Bbb|mfrak|mscr|mbf|msans|mtt|mbfit)([A-Za-z])',n)
    if not cmd and m: pkg,fmt=STYLE[m.group(1)]; cmd=fmt%m.group(2)
    if not cmd and n in ALIAS: cmd='\\'+ALIAS[n]; pkg='kernel'
    stat['typable' if cmd else 'unicode-math-only']+=1
    out.append((r['cp'],r['glyph'],n,r['cls'],cmd or '',pkg or 'unicode-math only'))
print(stat)
with open('umath_map.tsv','w') as f:
    f.write('codepoint\tglyph\tunicode_math_name\tclass\tpdflatex_command\tpackage\n')
    for o in out: f.write('U+%04X\t%s\t%s\t%s\t%s\t%s\n'%o)
by_cp=collections.defaultdict(list)
for o in out:
    if o[4]: by_cp[o[0]].append(o[4])
print('codepoints with a pdflatex command:',len(by_cp))
for g in 'α ε ϵ φ ϕ ≤ → ∅ ℏ ⊗ ↪ ↠ 𝔸 𝒜 ℤ ⩽ ⟨ ∘ ∐ ⨿ ≅ ≃ ∼'.split():
    print(g, [o[2:] for o in out if o[1]==g])
