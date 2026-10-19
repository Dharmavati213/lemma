import re, json, collections, subprocess
T='/usr/share/texmf-dist/tex/latex/unicode-math/unicode-math-table.tex'
rx=re.compile(r'^\\UnicodeMathSymbol\{"([0-9A-F]+)\}\{\\(\S+?)\s*\}\{\\(\w+)\}\{(.*)\}%\s*$')
rows=[];bad=[]
for l in open(T,encoding='utf-8'):
    if not l.startswith('\\UnicodeMathSymbol'): continue
    m=rx.match(l)
    if not m: bad.append(l); continue
    cp,name,cls,desc=m.groups()
    rows.append(dict(cp=int(cp,16),name=name,cls=cls,desc=desc))
print('rows',len(rows),'bad',len(bad))
names=collections.Counter(r['name'] for r in rows)
print('unique names',len(names),'dups',[n for n,c in names.items() if c>1][:10])
cps=collections.Counter(r['cp'] for r in rows); print('unique cps',len(cps))
# alphanumeric \mupalpha / \mbfA etc.
alpha=[r for r in rows if r['cls']=='mathalpha']
pref=collections.Counter((re.match(r'(m(?:up|bfit|bfscr|bffrak|bbit|bisans|bsans|itsans|it|bf|scr|frak|bb|sans|tt))[A-Za-z]',r['name']) or re.match('()','')).group(1) for r in alpha)
print(pref.most_common(20))
# names defined in pdflatex world
def scan(path, pats):
    s=open(subprocess.check_output(['kpsewhich',path]).decode().strip(),encoding='latin-1').read()
    out=set()
    for p in pats: out|=set(re.findall(p,s))
    return out
P_decl=[r'\\DeclareMath(?:Symbol|Delimiter|Accent|Radical)\s*\{?\\([A-Za-z]+)',
        r'\\(?:def|let|gdef|edef|chardef|mathchardef)\s*\\([A-Za-z]+)',
        r'\\(?:DeclareRobustCommand|newcommand|renewcommand|providecommand)\*?\s*\{?\\([A-Za-z]+)']
kernel=scan('fontmath.ltx',P_decl)|scan('latex.ltx',P_decl)
ams=scan('amssymb.sty',P_decl)|scan('amsfonts.sty',P_decl)
amsmath=scan('amsmath.sty',P_decl)
lsym=scan('latexsym.sty',P_decl)
stmary=scan('stmaryrd.sty',P_decl)
mtools=scan('mathtools.sty',P_decl)
def where(n):
    if n in kernel: return 'kernel'
    if n in amsmath: return 'amsmath'
    if n in ams: return 'amssymb'
    if n in lsym: return 'latexsym'
    if n in stmary: return 'stmaryrd'
    if n in mtools: return 'mathtools'
    return None
nonalpha=[r for r in rows if not (r['cls']=='mathalpha' and re.match(r'm(up|it|bf|scr|frak|bb|sans|tt|bfit|bfscr|bffrak|bbit|bsans|itsans|bisans)[A-Za-z]',r['name']))]
print('nonalpha (symbols, excluding styled alphanumerics)',len(nonalpha))
c=collections.Counter(where(r['name']) for r in nonalpha); print(c)
# aliases mentioned in description: e.g. "/neg /lnot"
alias=re.compile(r'/([A-Za-z]+)')
withalias=0; aliasmap={}
for r in rows:
    al=[a for a in alias.findall(r['desc'].split(':')[0] if ':' in r['desc'] else r['desc']) ]
    if al: withalias+=1
    for a in al:
        if a!=r['name'] and where(a): aliasmap.setdefault(r['name'],[]).append(a)
print('rows with /alias in desc',withalias,'umath names with pdflatex-known alias',len(aliasmap))
for k in ['varnothing','emptyset','leqslant','leq','le','Bbbk','mathbb','lneq','nleq','smallsetminus','setminus','mapsto','hookrightarrow','twoheadrightarrow','to','rightarrow','implies','iff','Longleftrightarrow','neq','ne','vartriangleleft','lhd','unlhd','trianglelefteq','dashv','vdash','otimes','boxtimes','coprod','amalg','varinjlim','injlim','lim','rightleftarrows','rightrightarrows','circlearrowright','curvearrowright','colon','mid','nmid','parallel','perp','top','bot','hbar','hslash','ell','wp','Im','Re','aleph','partial','nabla','infty','emptyset','diamond','lozenge','square','Box','blacksquare','checkmark','langle','rangle','lbrack','llbracket','lBrack']:
    rs=[r for r in rows if r['name']==k]
    print(k, [('U+%04X'%r['cp'],chr(r['cp']),r['cls'],r['desc']) for r in rs], 'pdflatex:',where(k))
# which commonly used pdflatex names are missing from unicode-math table?
known=kernel|ams|amsmath|lsym
umn=set(names)
miss=sorted(n for n in (ams|lsym) - umn if len(n)>1)
print('amssymb/latexsym names not in unicode-math table',len(miss), miss[:120])
json.dump([dict(r,glyph=chr(r['cp']),pdflatex=where(r['name'])) for r in rows],open('umath.json','w'),ensure_ascii=False)
