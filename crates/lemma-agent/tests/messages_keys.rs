//! `keys.rs` (WP5.1) against a fake `secret-tool` on a temporary PATH, and against a fake macOS
//! `security`. Never the real keyring.
#![cfg(unix)]

use lemma_agent::keys::{self, ApiKey, KeySource, Keyring};
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const KEY: &str = "sk-ant-test-KEYRING-00112233445566778899aabbccddeeff";

/// Writes an executable script. `cp` creates the file in its own process: had this process
/// written it, a child forked meanwhile by a test on another thread could still hold the file
/// open for writing when a test runs it ("Text file busy").
fn write_script(path: &Path, script: &str) {
    let source = path.with_extension("src");
    std::fs::write(&source, script).unwrap();
    let copied = std::process::Command::new("cp")
        .arg(&source)
        .arg(path)
        .status()
        .unwrap();
    assert!(copied.success());
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A `secret-tool` that keeps its one secret in a file next to it and logs its argv.
fn fake_secret_tool(dir: &Path) {
    let script = r#"#!/bin/sh
here="$(dirname "$0")"
printf '%s\n' "$*" >> "$here/argv.log"
case "$1" in
  lookup) [ -f "$here/secret" ] || exit 1; cat "$here/secret"; exit 0 ;;
  store) cat > "$here/secret"; exit 0 ;;
  clear) rm -f "$here/secret"; exit 0 ;;
  *) echo "unknown command" >&2; exit 2 ;;
esac
"#;
    write_script(&dir.join("secret-tool"), script);
}

#[test]
fn store_lookup_and_clear_through_secret_tool() {
    let dir = tempfile::tempdir().unwrap();
    fake_secret_tool(dir.path());
    let path = std::env::join_paths([Path::new("/nonexistent-lemma-bin"), dir.path()]).unwrap();
    let keyring = Keyring::on_path(&path).expect("found on the temporary PATH");
    assert!(keyring.available());
    assert_eq!(keyring.lookup().unwrap(), None);

    let key = ApiKey::new(&format!("  {KEY}\n")).unwrap();
    assert_eq!(key.expose(), KEY);
    keyring.store(&key).unwrap();
    assert_eq!(keyring.lookup().unwrap(), Some(key.clone()));
    assert_eq!(
        keys::lookup_with(None, &keyring),
        Some((key.clone(), KeySource::Keyring))
    );
    // The environment comes first.
    let env = ApiKey::new("sk-ant-from-env").unwrap();
    assert_eq!(
        keys::lookup_with(Some(OsStr::new("sk-ant-from-env")), &keyring),
        Some((env, KeySource::Env))
    );
    // An empty variable does not count.
    assert_eq!(
        keys::lookup_with(Some(OsStr::new("  ")), &keyring).map(|(_, s)| s),
        Some(KeySource::Keyring)
    );

    keyring.clear().unwrap();
    assert_eq!(keyring.lookup().unwrap(), None);
    assert_eq!(keys::lookup_with(None, &keyring), None);

    // The key went over stdin: never in argv.
    let argv = std::fs::read_to_string(dir.path().join("argv.log")).unwrap();
    assert!(!argv.contains(KEY), "{argv}");
    assert!(
        argv.lines()
            .any(|l| l
                == "store --label=Lemma Anthropic API key service lemma kind anthropic-api-key"),
        "{argv}"
    );
    assert!(
        argv.lines()
            .any(|l| l == "lookup service lemma kind anthropic-api-key")
    );
    assert!(
        argv.lines()
            .any(|l| l == "clear service lemma kind anthropic-api-key")
    );
}

#[test]
fn the_key_is_redacted_everywhere_it_could_be_printed() {
    let key = ApiKey::new(KEY).unwrap();
    assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
    assert_eq!(format!("{key}"), "<redacted>");
    assert_eq!(key.redact(&format!("x {KEY} y")), "x <redacted> y");
    let source = Some((key, KeySource::Keyring));
    assert!(!format!("{source:?}").contains(KEY));
    let expected = if cfg!(target_os = "macos") {
        "from macOS Keychain"
    } else {
        "from GNOME keyring"
    };
    assert_eq!(KeySource::Keyring.describe(), expected);
}

#[test]
fn keys_that_cannot_be_headers_are_refused() {
    assert_eq!(ApiKey::new(""), None);
    assert_eq!(ApiKey::new(" \n"), None);
    assert_eq!(ApiKey::new("sk-ant two"), None);
    assert_eq!(ApiKey::new("sk-ant\u{7}"), None);
    assert_eq!(ApiKey::new(&"k".repeat(5000)), None);
}

#[test]
fn a_missing_or_failing_secret_tool_means_no_key() {
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(Keyring::on_path(empty.path().as_os_str()), None);
    let missing = Keyring::with_program(empty.path().join("secret-tool"));
    assert!(!missing.available());
    assert!(missing.lookup().is_err());
    assert_eq!(keys::lookup_with(None, &missing), None);

    // A keyring error (stderr, non-zero exit) is an error, not "no key".
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret-tool");
    write_script(
        &path,
        "#!/bin/sh\necho 'Cannot autolaunch D-Bus' >&2\nexit 1\n",
    );
    let broken = Keyring::with_program(&path);
    let err = broken.lookup().unwrap_err();
    assert!(err.to_string().contains("D-Bus"), "{err}");
    let err = broken.store(&ApiKey::new(KEY).unwrap()).unwrap_err();
    assert!(!err.to_string().contains(KEY));
    assert_eq!(keys::lookup_with(None, &broken), None);
}

/// A macOS `security` that keeps its one password in a file next to it, logs its argv and, in
/// interactive mode (`-i`), the commands it reads from stdin. Like the real one, `-i` exits 0
/// whatever its commands do, and a missing item is exit status 44.
fn fake_security(dir: &Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
here="$(dirname "$0")"
printf '%s\n' "$*" >> "$here/argv.log"
missing() { echo 'security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.' >&2; exit 44; }
case "$*" in
  'find-generic-password -s lemma -a anthropic-api-key -w')
    [ -f "$here/secret" ] || missing; cat "$here/secret"; echo; exit 0 ;;
  'delete-generic-password -s lemma -a anthropic-api-key')
    [ -f "$here/secret" ] || missing; rm -f "$here/secret"; echo 'password has been deleted.'; exit 0 ;;
  '-i')
    printf 'security> '
    while IFS= read -r line; do
      printf '%s\n' "$line" >> "$here/stdin.log"
      case "$line" in
        'add-generic-password -U -s lemma -a anthropic-api-key -l "Lemma Anthropic API key" -w "'*'"')
          if [ -f "$here/locked" ]; then
            echo "security: SecKeychainItemCreateFromContent: User interaction is not allowed." >&2
          else
            key="${line##*-w \"}"; printf '%s' "${key%\"}" > "$here/secret"
          fi ;;
        *) echo "unknown command: $line" >&2 ;;
      esac
      printf 'security> '
    done
    exit 0 ;;
  *) echo "unexpected arguments: $*" >&2; exit 2 ;;
esac
"#;
    let path = dir.join("security");
    write_script(&path, script);
    path
}

#[test]
fn store_lookup_and_clear_through_the_macos_keychain() {
    let dir = tempfile::tempdir().unwrap();
    let keyring = Keyring::macos_keychain(fake_security(dir.path()));
    assert!(keyring.available());
    assert_eq!(keyring.lookup().unwrap(), None);
    // Nothing stored is not an error.
    keyring.clear().unwrap();

    let key = ApiKey::new(KEY).unwrap();
    keyring.store(&key).unwrap();
    assert_eq!(keyring.lookup().unwrap(), Some(key.clone()));
    assert_eq!(
        keys::lookup_with(None, &keyring),
        Some((key.clone(), KeySource::Keyring))
    );
    // Storing again replaces the item (`-U`).
    let newer = ApiKey::new("sk-ant-test-NEWER-0123456789").unwrap();
    keyring.store(&newer).unwrap();
    assert_eq!(keyring.lookup().unwrap(), Some(newer));

    keyring.clear().unwrap();
    assert_eq!(keyring.lookup().unwrap(), None);

    // The key went over stdin: never in argv.
    let argv = std::fs::read_to_string(dir.path().join("argv.log")).unwrap();
    assert!(!argv.contains(KEY), "{argv}");
    assert!(argv.lines().any(|l| l == "-i"), "{argv}");
    let stdin = std::fs::read_to_string(dir.path().join("stdin.log")).unwrap();
    assert!(
        stdin.lines().any(|l| l
            == format!(
                "add-generic-password -U -s lemma -a anthropic-api-key \
                 -l \"Lemma Anthropic API key\" -w \"{KEY}\""
            )),
        "{stdin}"
    );
}

#[test]
fn a_failed_macos_keychain_store_is_an_error_without_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let keyring = Keyring::macos_keychain(fake_security(dir.path()));
    // `security -i` exits 0 although the keychain refused: reading back tells.
    std::fs::write(dir.path().join("locked"), "").unwrap();
    let err = keyring.store(&ApiKey::new(KEY).unwrap()).unwrap_err();
    assert!(err.to_string().contains("User interaction"), "{err}");
    assert!(!err.to_string().contains(KEY), "{err}");
    assert_eq!(keyring.lookup().unwrap(), None);

    // A key that would break `security`'s quoting never reaches it.
    let quoted = ApiKey::new("sk-ant-\";delete-keychain").unwrap();
    assert!(keyring.store(&quoted).is_err());
    let stdin = std::fs::read_to_string(dir.path().join("stdin.log")).unwrap();
    assert!(!stdin.contains("delete-keychain"), "{stdin}");

    // A broken keychain (another exit status) is an error, not "no key".
    let broken = dir.path().join("broken-security");
    write_script(&broken, "#!/bin/sh\necho 'keychain damaged' >&2\nexit 51\n");
    let broken = Keyring::macos_keychain(&broken);
    let err = broken.lookup().unwrap_err();
    assert!(err.to_string().contains("keychain damaged"), "{err}");
    assert!(broken.clear().is_err());
    assert_eq!(keys::lookup_with(None, &broken), None);
}
