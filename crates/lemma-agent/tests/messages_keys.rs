//! `keys.rs` (WP5.1) against a fake `secret-tool` on a temporary PATH. Never the real keyring.

use lemma_agent::keys::{self, ApiKey, KeySource, Keyring};
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const KEY: &str = "sk-ant-test-KEYRING-00112233445566778899aabbccddeeff";

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
    let path = dir.join("secret-tool");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
    assert_eq!(KeySource::Keyring.describe(), "from GNOME keyring");
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
    std::fs::write(
        &path,
        "#!/bin/sh\necho 'Cannot autolaunch D-Bus' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let broken = Keyring::with_program(&path);
    let err = broken.lookup().unwrap_err();
    assert!(err.to_string().contains("D-Bus"), "{err}");
    let err = broken.store(&ApiKey::new(KEY).unwrap()).unwrap_err();
    assert!(!err.to_string().contains(KEY));
    assert_eq!(keys::lookup_with(None, &broken), None);
}
