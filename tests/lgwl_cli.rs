//! End-to-end checks for the launcher's user-facing output.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture {
    bin: PathBuf,
    fake: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lgwl-cli-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state = dir.join("state");
        fs::create_dir_all(&state).unwrap();
        let fake = dir.join("fake-lgw");
        fs::write(
            &fake,
            r#"#!/usr/bin/env bash
printf '%s' '{"url":"http://127.0.0.1:18787/"}' > "$LGW_READY_FILE"
echo 'INFO light-ai-gateway listening listen=127.0.0.1:18787 console=http://127.0.0.1:18787/'
trap 'exit 0' TERM
while true; do sleep 0.2; done
"#,
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        Fixture {
            bin: PathBuf::from(env!("CARGO_BIN_EXE_lgwl")),
            fake,
            state,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(&self.bin)
            .args(args)
            .env("LGW_BIN", &self.fake)
            .env("LGWL_STATE_DIR", &self.state)
            .output()
            .expect("running lgwl")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new(&self.bin)
            .arg("exit")
            .env("LGW_BIN", &self.fake)
            .env("LGWL_STATE_DIR", &self.state)
            .status();
        let _ = fs::remove_dir_all(self.state.parent().unwrap_or(Path::new(".")));
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn launch_status_and_a_second_launch_all_name_the_url() {
    let fixture = Fixture::new();
    let started = fixture.run(&["launch"]);
    assert!(started.status.success(), "{started:?}");
    assert!(
        stdout(&started).contains("http://127.0.0.1:18787/v1"),
        "launch did not name the API base: {}",
        stdout(&started)
    );

    let status = fixture.run(&["status"]);
    assert!(status.status.success(), "{status:?}");
    let status_text = stdout(&status);
    assert!(status_text.contains("running (pid "), "{status_text}");
    assert!(
        status_text.contains("http://127.0.0.1:18787/"),
        "status did not name the console URL: {status_text}"
    );

    let again = fixture.run(&["launch"]);
    assert!(!again.status.success());
    assert!(
        stdout(&again).contains("http://127.0.0.1:18787/v1"),
        "a refused launch did not name the running URL: {}",
        stdout(&again)
    );
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("already running"),
        "the refusal must stay explicit"
    );
}
