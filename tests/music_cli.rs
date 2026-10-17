//! `gowl music` argument handling (no audio server or device needed: usage errors
//! are reported before libpulse is loaded or anything is sent).

use std::process::Command;

fn music(args: &[&str]) -> (i32, String) {
    let tmp = std::env::temp_dir().join(format!("gowl-music-cli-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gowl"))
        .arg("music")
        .args(args)
        .env("XDG_CONFIG_HOME", &tmp)
        .env("XDG_STATE_HOME", &tmp)
        .env("XDG_RUNTIME_DIR", &tmp)
        .env("HOME", &tmp)
        .env("GOWL_DEVICE", "127.0.0.1")
        .env_remove("GOWL_CONFIG")
        .output()
        .expect("run gowl");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn usage_errors_exit_2() {
    let (code, err) = music(&["--reaction", "nope"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("center_burst"), "{err}");
    let (code, err) = music(&["--palette", "album_art"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("not supported"), "{err}");
    for bad in [&["--fps", "0"][..], &["--fps"], &["--bogus"]] {
        let (code, err) = music(bad);
        assert_eq!(code, 2, "{bad:?}: {err}");
    }
}
