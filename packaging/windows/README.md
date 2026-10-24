# Lemma for Windows

`packaging/windows/build.sh` builds Lemma for Windows 10 (1903) and 11 on x86_64 and packages
it as:

- `dist/Lemma/`, the app folder, which runs from wherever it is (`bin\lemma.exe`);
- `dist/Lemma-<version>-windows-x86_64.zip`, that folder zipped, for a portable install;
- `dist/Lemma-<version>-windows-x86_64-setup.exe`, an installer made with
  [Inno Setup](https://jrsoftware.org/isinfo.php).

Nothing of this runs on GitHub: the script builds everything on your own machine, either on
Windows itself or on Linux as a cross build.

```sh
packaging/windows/build.sh            # build, the folder, the zip and the installer
packaging/windows/build.sh --test     # also run the tests of the crates without GTK windows
packaging/windows/build.sh --smoke    # also start the packaged app and snapshot its window
packaging/windows/build.sh --help     # every option
```

## On Linux (cross build)

Needs `rustup`, `curl`, `python3`, `tar` with zstd, `pkg-config` and `glib-compile-schemas`
(Arch: `pacman -S rustup curl python tar zstd pkgconf glib2`), and for `--test` the Liberation
fonts (`ttf-liberation`), which stand in for Arial and Times in Wine. The first run downloads,
into `target/windows-sdk/` (about 3.5 GB with the Wine prefix; `LEMMA_WINDOWS_SDK` moves it):

| What | Why | Checked against |
|---|---|---|
| [llvm-mingw](https://github.com/mstorsjo/llvm-mingw) 20261020 | clang, lld and the MinGW headers and libraries for the `x86_64-pc-windows-gnullvm` target | pinned SHA-256 |
| MSYS2's CLANG64 packages: GTK 4, libadwaita, GtkSourceView, libspelling, poppler and what they need | the libraries to link against and to bundle | the repository database's SHA-256 ([`msys2.py`](msys2.py)) |
| a portable [Wine](https://github.com/Kron4ek/Wine-Builds) 11.20 | runs the Windows tools the packaging needs, and the tests and the app for `--test` and `--smoke` | pinned SHA-256 |
| Inno Setup 6.7.3 | the installer's compiler (installed into the Wine prefix) | pinned SHA-256 |

Rust's `x86_64-pc-windows-gnullvm` target is added with rustup. Later runs only download MSYS2
packages that changed. The build itself is a normal `cargo build --release` for that target,
with pkg-config pointed at the MSYS2 packages.

## On Windows

Install [MSYS2](https://www.msys2.org), open its **CLANG64** shell, and install the packages:

```sh
pacman -S --needed git mingw-w64-clang-x86_64-{toolchain,rust,pkgconf,python,gtk4,libadwaita,gtksourceview5,libspelling,poppler,librsvg,adwaita-icon-theme,hunspell-en,enchant,fontconfig}
git clone https://github.com/dharmavati213/lemma.git
cd lemma
packaging/windows/build.sh
```

For the installer, install Inno Setup 6 (or pass `--no-installer`).

## What is in the folder

| Path | Contents |
|---|---|
| `bin/` | `lemma.exe` (a GUI program, with the icon, the version and an application manifest embedded by `crates/lemma-app/build.rs`), `lemma-mcp.exe`, every DLL they need ([`bundle_dlls.py`](bundle_dlls.py) follows the import tables), GLib's `gspawn-win64-helper*.exe` |
| `lib/gdk-pixbuf-2.0/` | image loaders, and `loaders.cache` with paths relative to the folder (MSYS2's gdk-pixbuf is relocatable) |
| `lib/enchant-2/` | spell checking with Hunspell, and with Windows' own spell checker (`winspell`) |
| `share/` | compiled GSettings schemas, the Adwaita and hicolor icon themes, GtkSourceView's languages and styles, Hunspell's English dictionaries, Lemma's default snippets, prompts and templates |
| `etc/fonts/` | fontconfig's configuration (cairo uses it) |
| `share/licenses/` | Lemma's licence, the licences of the bundled MSYS2 packages, and `msys2-packages.txt` with their versions |

GLib, GTK and the rest find `share`, `lib` and `etc` next to `bin` by themselves, so the folder
needs no environment variables. `lemma.exe` attaches to the console of a terminal that starts
it (for `--help` and the log) and adds the usual TeX, Node.js and Git folders to PATH when they
are missing (`crates/lemma-app/src/windows.rs`).

## The installer

[`lemma.iss`](lemma.iss): Lemma for the current user (no administrator rights) or for everyone,
a Start menu entry, and optionally a desktop icon, Lemma under "Open with" for `.tex` and `.bib`
files, and `bin` on PATH. Uninstalling removes all of it; the settings in `%APPDATA%\lemma` stay.

The installer and `lemma.exe` are not code-signed, so Windows SmartScreen warns the first time
("Windows protected your PC"): **More info → Run anyway**.

## A release

Build on a clean checkout of the release's tag and attach the zip and the installer to the
GitHub release:

```sh
packaging/windows/build.sh --test --smoke
gh release upload <tag> dist/Lemma-*-windows-x86_64.zip dist/Lemma-*-windows-x86_64-setup.exe
```

`LEMMA_WINDOWS_NAME` changes the file names (for example `Lemma-<tag>-windows-x86_64`).

## Notes

- Paths inside Lemma are canonical without Windows' `\\?\` prefix (`lemma_core::canonicalize`),
  so TeX, GTK and comparisons see `C:\…`.
- latexmk, the agent and the other programs run without console windows, each in a job object
  that ends everything they started when Lemma stops them (or when Lemma ends).
- The agent's MCP bridge reaches Lemma through a loopback TCP port named in a file where the
  unix socket would be, with the same token check (`crates/lemma-agent/src/transport.rs`).
- The API key is a generic credential (`lemma/anthropic-api-key`) in the Windows Credential
  Manager.
- Under Wine (`--smoke`) the window's text looks rough: Wine lacks Windows' fonts (Segoe UI,
  Consolas). On Windows it does not.
