#!/usr/bin/env python3
"""Copies every non-system library that Lemma.app's executables and modules load into
Contents/Resources/lib and points all of them at those copies.

Usage: bundle_dylibs.py APP FILE...

FILE are Mach-O files already inside APP (the executables, gdk-pixbuf loaders, enchant
providers). Each dependency outside /usr/lib and /System is copied once, under the base name of
its install name, and every reference to it becomes @executable_path/../Resources/lib/NAME
(dlopen'ed modules resolve @executable_path against the main executable too). @rpath and
@loader_path references are resolved where the referencing file came from.

Libraries are kept in Contents/Resources/lib, a directory named "lib", because relocatable
libraries (enchant through gnulib) find their data relative to their own directory by replacing a
trailing "lib".

Nothing here signs: run `codesign --force --sign -` over the files afterwards (bundle.sh does), as
install_name_tool invalidates signatures and arm64 macOS refuses unsigned code.

Exits non-zero if a dependency cannot be found or a file in the bundle still refers to a library
outside it.
"""

import os
import shutil
import subprocess
import sys

INSTALL_PREFIX = "@executable_path/../Resources/lib/"
SYSTEM_PREFIXES = ("/usr/lib/", "/System/")


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
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    app = os.path.abspath(sys.argv[1])
    lib_dir = os.path.join(app, "Contents", "Resources", "lib")
    os.makedirs(lib_dir, exist_ok=True)
    brew = run("brew", "--prefix").strip()
    search = [os.path.join(brew, "lib")]

    # (file in the bundle, where it came from: for @loader_path and @rpath)
    queue = []
    for f in sys.argv[2:]:
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


def is_macho(path):
    with open(path, "rb") as f:
        magic = f.read(4)
    return magic in (b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe", b"\xca\xfe\xba\xbe")


if __name__ == "__main__":
    main()
