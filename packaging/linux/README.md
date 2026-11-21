# Linux packages

`package.sh` turns a release build into the Linux files of a release. CI builds each one on its
own distribution and installs it in a fresh container (`.github/workflows/release.yml`).

| Command | File | For | Built on |
|---|---|---|---|
| `package.sh deb` | `lemma_<version>-1_amd64.deb` | Ubuntu 26.04+, Debian sid | Ubuntu 26.04 |
| `package.sh rpm` | `lemma-<version>-1.fc44.x86_64.rpm` | Fedora 44+ | Fedora 44 |
| `package.sh tarball` | `lemma-<label>-linux-x86_64.tar.gz` | other distributions with GNOME 50's libraries | Ubuntu 26.04 |
| `makepkg` in `packaging/` | `lemma-<version>-<rel>-x86_64.pkg.tar.zst` | Arch Linux | Arch |

```sh
LEMMA_SYNCTEX_BUNDLED=1 cargo build --release --locked -p lemma-app -p lemma-mcp
packaging/linux/package.sh deb          # or rpm, tarball; --build runs the cargo build itself
```

- All packages hold the same files as the PKGBUILD: `lemma` and `lemma-mcp` in `bin/` (Lemma
  finds `lemma-mcp` next to itself), the desktop entry, the icon, the AppStream metadata
  (`share/metainfo/`, what software centres show; `appstreamcli validate` checks it), the
  default snippets, prompts and templates in `share/lemma/` as examples for `~/.config/lemma/`,
  and the licence. They also have the README in `share/doc/lemma/`, which the PKGBUILD leaves out.
- The `.deb`, `.rpm` and tarball are built with the bundled SyncTeX parser
  (`LEMMA_SYNCTEX_BUNDLED=1`), so they need no libsynctex. The Arch package uses the system's.
  The parser's licence asks for its copyright and permission notice to go with the program:
  `../synctex-notice.sh` prints it from the vendored files themselves
  (`crates/lemma-synctex/vendor/synctex/`), and the `.rpm` and the tarball have it as
  `share/licenses/lemma/LICENSE.synctex` (the tarball's `install.sh` installs it there).
- The Rust crates compiled into the two programs are under MIT, Apache-2.0 and similar
  licences, which ask for their notices and texts to go with the program. `cargo run -p xtask
  -- licenses` prints them (the crates `cargo metadata` resolves for `lemma-app` and
  `lemma-mcp`, normal dependencies only, with the licence files of each crate's sources; a
  text several crates share is printed once). It is `LICENSE.crates` beside the licence
  (`share/doc/lemma/` in the `.deb`, `share/licenses/lemma/` elsewhere), also in the Arch
  package.
- Dependencies: `dpkg-shlibdeps` computes the `.deb`'s, `rpmbuild` the `.rpm`'s (from the
  libraries the binaries link). TeX, hunspell dictionaries, `git` and Node.js are Recommends and
  Suggests. The Debian `copyright` file is the GPL text followed by the SyncTeX parser's notice.
- `lemma.spec` packages the staged files only (`rpmbuild --define lemma_stage …`).
- `install.sh` goes into the tarball. It checks the libraries with `ldd` (and by running
  `lemma --version`, which also catches a C library that is too old) and names the packages for
  Arch, Fedora, Debian/Ubuntu and openSUSE when they are missing. It installs into `~/.local`, or
  `/usr/local` with `--system`, writes the desktop entry with the installed binary's path
  (quoted, so a folder with a space in its name works), and records what it installed for
  `--uninstall`.
- `smoke-test.sh LEMMA OUT NAME` starts an installed Lemma on a Broadway display and snapshots
  its window (CI keeps the snapshots as artifacts).
