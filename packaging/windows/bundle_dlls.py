#!/usr/bin/env python3
"""Copies the DLLs that Windows programs and libraries need into a bundle.

    bundle_dlls.py DEST_BIN SEARCH_DIR FILE...

Reads the import tables (normal and delay-loaded) of each FILE (EXE or DLL) and copies every DLL
they name that SEARCH_DIR (MSYS2's `clang64/bin`) holds into DEST_BIN, then does the same for the
copied DLLs, until nothing is missing. DLLs that SEARCH_DIR does not hold are Windows' own
(`KERNEL32.dll`, `api-ms-win-*`, …) and are left out. Prints the name of every DLL it copies.

Runs on any system: it only parses the files (PE32+), it never loads them.
"""

import os
import shutil
import struct
import sys

IMPORT_DIRECTORY = 1
DELAY_IMPORT_DIRECTORY = 13


def imported_dlls(path):
    """The DLL names in the import and delay-import tables of the PE32+ file at `path`."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:2] != b"MZ":
        raise ValueError(f"{path} is not a PE file")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe : pe + 4] != b"PE\0\0":
        raise ValueError(f"{path} has no PE header")
    coff = pe + 4
    sections, optional_size = struct.unpack_from("<H12xH", data, coff + 2)
    optional = coff + 20
    if struct.unpack_from("<H", data, optional)[0] != 0x20B:
        raise ValueError(f"{path} is not a 64-bit (PE32+) file")
    directories = optional + 112
    table = optional + optional_size
    section_list = [
        struct.unpack_from("<8xIIII", data, table + 40 * i) for i in range(sections)
    ]  # (virtual size, virtual address, raw size, raw offset)

    def offset(rva):
        for vsize, vaddr, rsize, raw in section_list:
            if vaddr <= rva < vaddr + max(vsize, rsize):
                return raw + rva - vaddr
        raise ValueError(f"{path}: RVA {rva:#x} is in no section")

    def name_at(rva):
        start = offset(rva)
        return data[start : data.index(b"\0", start)].decode("ascii")

    names = []
    for directory, size, name_field in (
        (IMPORT_DIRECTORY, 20, 12),
        (DELAY_IMPORT_DIRECTORY, 32, 4),
    ):
        rva, length = struct.unpack_from("<II", data, directories + 8 * directory)
        if rva == 0 or length == 0:
            continue
        at = offset(rva)
        while True:
            entry = data[at : at + size]
            if len(entry) < size or entry == b"\0" * size:
                break
            names.append(name_at(struct.unpack_from("<I", entry, name_field)[0]))
            at += size
    return names


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    dest, search, roots = sys.argv[1], sys.argv[2], sys.argv[3:]
    available = {name.lower(): name for name in os.listdir(search) if name.lower().endswith(".dll")}
    os.makedirs(dest, exist_ok=True)
    present = {name.lower() for name in os.listdir(dest)}
    todo = list(roots)
    while todo:
        for dll in imported_dlls(todo.pop()):
            key = dll.lower()
            if key in present or key not in available:
                continue
            present.add(key)
            target = os.path.join(dest, available[key])
            shutil.copy2(os.path.join(search, available[key]), target)
            print(available[key])
            todo.append(target)


if __name__ == "__main__":
    main()
