#!/usr/bin/env python3
"""Copies every non-system library that Lemma.app's executables and modules load into
Contents/Resources/lib and points all of them at those copies.

Usage: bundle_dylibs.py APP [--data=PATH]... FILE...

FILE are Mach-O files already inside APP (the executables, gdk-pixbuf loaders, enchant
providers). Each dependency outside /usr/lib and /System is copied once, under the base name of
its install name, and every reference to it becomes @executable_path/../Resources/lib/NAME
(dlopen'ed modules resolve @executable_path against the main executable too). @rpath and
@loader_path references are resolved where the referencing file came from.

Libraries are kept in Contents/Resources/lib, a directory named "lib", because relocatable
libraries (enchant through gnulib) find their data relative to their own directory by replacing a
trailing "lib".

The licence files of every Homebrew formula a library was copied from, and of the formulae
that the --data paths belong to (data and modules bundle.sh copies itself), go to
Contents/Resources/licenses/FORMULA/, with the list of the formulae in
Contents/Resources/licenses/homebrew-formulae.txt.

Nothing here signs: run `codesign --force --sign -` over the files afterwards (bundle.sh does), as
install_name_tool invalidates signatures and arm64 macOS refuses unsigned code.

Exits non-zero if a dependency cannot be found or a file in the bundle still refers to a library
outside it.
"""

import os
import re
import shutil
import subprocess
import sys

INSTALL_PREFIX = "@executable_path/../Resources/lib/"
SYSTEM_PREFIXES = ("/usr/lib/", "/System/")
# A file at the top of a keg is a licence file when its name starts with one of these.
LICENSE_PREFIXES = ("license", "licence", "copying", "copyright", "notice", "unlicense")


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def dependencies(path):
    """The install names `path` loads (not its own id)."""
    lines = run("otool", "-L", path).splitlines()[1:]
    deps = [line.strip().split(" (compatibility")[0] for line in lines if line.strip()]
    own_id = run("otool", "-D", path).splitlines()[1:]
    return [d for d in deps if not own_id or d != own_id[0].strip()]


def rpaths(path):
    """The LC_RPATH entries of `path`."""
    out, found = [], False
    for line in run("otool", "-l", path).splitlines():
        line = line.strip()
        if line == "cmd LC_RPATH":
            found = True
        elif found and line.startswith("path "):
            out.append(line[len("path "):].split(" (offset")[0])
            found = False
    return out


def resolve(dep, origin, search):
    """The file `dep` (an install name in the file that lived at `origin`) refers to."""
    origin_dir = os.path.dirname(origin)
    candidates = []
    if dep.startswith("@rpath/"):
        rest = dep[len("@rpath/"):]
        for rp in rpaths(origin):
            rp = rp.replace("@loader_path", origin_dir).replace("@executable_path", origin_dir)
            candidates.append(os.path.join(rp, rest))
        candidates += [os.path.join(d, rest) for d in search]
    elif dep.startswith("@loader_path/"):
        candidates.append(os.path.join(origin_dir, dep[len("@loader_path/"):]))
    elif dep.startswith("@executable_path/"):
        candidates.append(os.path.join(origin_dir, dep[len("@executable_path/"):]))
    else:
        candidates.append(dep)
        candidates += [os.path.join(d, os.path.basename(dep)) for d in search]
    for c in candidates:
        if os.path.isfile(c):
            return os.path.realpath(c)
    return None


def main():
    data = [a[len("--data="):] for a in sys.argv[2:] if a.startswith("--data=")]
    files = [a for a in sys.argv[2:] if not a.startswith("--data=")]
    if len(sys.argv) < 3 or not files:
        sys.exit(__doc__)
    app = os.path.abspath(sys.argv[1])
    lib_dir = os.path.join(app, "Contents", "Resources", "lib")
    os.makedirs(lib_dir, exist_ok=True)
    brew = run("brew", "--prefix").strip()
    search = [os.path.join(brew, "lib")]

    # (file in the bundle, where it came from: for @loader_path and @rpath)
    queue = []
    for f in files:
        f = os.path.abspath(f)
        queue.append((f, f))
    copied = {}  # base name -> real source path
    missing = []
    while queue:
        path, origin = queue.pop()
        for dep in dependencies(path):
            if dep.startswith(SYSTEM_PREFIXES):
                continue
            name = os.path.basename(dep)
            if dep.startswith(INSTALL_PREFIX):
                continue
            source = resolve(dep, origin, search)
            if source is None:
                missing.append(f"{dep} (needed by {path})")
                continue
            if name not in copied:
                target = os.path.join(lib_dir, name)
                shutil.copyfile(source, target)
                os.chmod(target, 0o755)
                run("install_name_tool", "-id", INSTALL_PREFIX + name, target)
                copied[name] = source
                queue.append((target, source))
            elif copied[name] != source:
                print(f"warning: two libraries named {name}: {copied[name]} and {source}")
            run("install_name_tool", "-change", dep, INSTALL_PREFIX + name, path)
    if missing:
        sys.exit("cannot find:\n  " + "\n  ".join(missing))

    # Nothing in the bundle may still point outside it.
    leaks = []
    for root, _, files in os.walk(app):
        for f in files:
            p = os.path.join(root, f)
            if os.path.islink(p) or not is_macho(p):
                continue
            for dep in dependencies(p):
                if not dep.startswith(SYSTEM_PREFIXES) and not dep.startswith(INSTALL_PREFIX):
                    leaks.append(f"{p}: {dep}")
    if leaks:
        sys.exit("libraries outside the bundle:\n  " + "\n  ".join(leaks))
    print(f"bundled {len(copied)} libraries into {lib_dir}")

    cellar = run("brew", "--cellar").strip()
    kegs = {keg_of(p, cellar) for p in copied.values()}
    for path in data:
        kegs |= kegs_under(path, cellar)
    kegs.discard(None)
    licenses = os.path.join(app, "Contents", "Resources", "licenses")
    lines = copy_licenses(sorted(kegs), licenses)
    without = [line for line in lines if line.endswith("no licence file in its keg")]
    print(f"licences of {len(lines)} Homebrew formulae in {licenses}")
    for line in without:
        print(f"warning: {line}")


def keg_of(path, cellar):
    """The keg (CELLAR/FORMULA/VERSION) that PATH lies in once links are followed, or None."""
    real = os.path.realpath(path)
    prefix = os.path.join(os.path.realpath(cellar), "")
    if not real.startswith(prefix):
        return None
    parts = real[len(prefix):].split(os.sep)
    if len(parts) < 2 or not parts[0] or not parts[1]:
        return None
    return os.path.join(prefix, parts[0], parts[1])


def kegs_under(path, cellar, depth=2):
    """The kegs PATH belongs to: its own, or, for a directory of Homebrew's prefix that links
    into several kegs (lib/enchant-2, share/icons/Adwaita), those of its entries, DEPTH levels
    down at most."""
    keg = keg_of(path, cellar)
    if keg is not None:
        return {keg}
    kegs = set()
    if depth > 0 and os.path.isdir(path):
        for name in os.listdir(path):
            kegs |= kegs_under(os.path.join(path, name), cellar, depth - 1)
    return kegs


def formula_license(keg):
    """The licence the keg's formula states (`license "MIT"` in .brew/FORMULA.rb), or ""."""
    formula = os.path.join(keg, ".brew", os.path.basename(os.path.dirname(keg)) + ".rb")
    try:
        with open(formula, encoding="utf-8", errors="replace") as f:
            for line in f:
                m = re.match(r"\s*license\s+(.+?)\s*$", line)
                if m:
                    return m.group(1)
    except OSError:
        pass
    return ""


def copy_licenses(kegs, dest):
    """Copies the licence files at the top of each keg to DEST/FORMULA/ and writes
    DEST/homebrew-formulae.txt, one line per keg: the formula, its version, the licence its
    formula states and the files copied. Returns those lines."""
    os.makedirs(dest, exist_ok=True)
    lines = []
    for keg in kegs:
        formula = os.path.basename(os.path.dirname(keg))
        version = os.path.basename(keg)
        names = sorted(
            n
            for n in os.listdir(keg)
            if n.lower().startswith(LICENSE_PREFIXES) and os.path.isfile(os.path.join(keg, n))
        )
        if names:
            # Two versions of one formula would share the directory: keep both apart.
            target = os.path.join(dest, formula)
            if os.path.exists(target):
                target = os.path.join(dest, f"{formula}-{version}")
            os.makedirs(target)
            for n in names:
                shutil.copyfile(os.path.join(keg, n), os.path.join(target, n))
        stated = formula_license(keg)
        line = f"{formula} {version}"
        if stated:
            line += f" ({stated})"
        line += ": " + (", ".join(names) if names else "no licence file in its keg")
        lines.append(line)
    with open(os.path.join(dest, "homebrew-formulae.txt"), "w", encoding="utf-8") as f:
        f.write(
            "Lemma.app contains libraries and data of these Homebrew formulae. The licence\n"
            "files of each are in the folder of its name beside this file.\n\n"
        )
        f.write("".join(line + "\n" for line in lines))
    return lines


def is_macho(path):
    with open(path, "rb") as f:
        magic = f.read(4)
    return magic in (b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe", b"\xca\xfe\xba\xbe")


if __name__ == "__main__":
    main()
