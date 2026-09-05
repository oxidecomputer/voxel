// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Exercise the actual CLI with fake OS commands, without root, systemd, or
//! BIRD. Only Bash is real, so script failure and read-only inputs are tested
//! on macOS too. Actual package/daemon integration needs a Debian guest.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Guest(PathBuf);

impl Guest {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "voxel-bird-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("bin")).unwrap();
        let mock = dir.join("mock");
        fs::write(
            &mock,
            r#"#!/bin/sh
cmd=${0##*/}
printf '%s\n' "$cmd $*" >> "$COMMAND_LOG"
if [ "$cmd $*" = "$FAIL_COMMAND" ] || [ "$cmd" = "$FAIL_COMMAND" ]; then
    exit 1
fi
if [ "$cmd" = birdc ] && [ -f "$TEST_READY.socket-pending" ]; then
    /bin/rm "$TEST_READY.socket-pending"
    exit 1
fi
case "$cmd" in
    rm) /bin/rm -f "$TEST_READY" ;;
    touch) : > "$TEST_READY" ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&mock, fs::Permissions::from_mode(0o755)).unwrap();
        for cmd in ["rm", "touch", "bird", "birdc", "install", "systemctl"] {
            symlink(&mock, dir.join("bin").join(cmd)).unwrap();
        }
        symlink("/bin/bash", dir.join("bin/bash")).unwrap();
        fs::write(dir.join("bird.conf"), "router id 192.0.2.1;\n").unwrap();
        fs::set_permissions(
            dir.join("bird.conf"),
            fs::Permissions::from_mode(0o400),
        )
        .unwrap();
        fs::write(dir.join("ready"), "stale").unwrap();
        Self(dir)
    }

    fn run(&self, script: Option<&str>, fail: &str) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_voxel-init"));
        cmd.arg("bird").arg("--config").arg(self.0.join("bird.conf"));
        if let Some(script) = script {
            let path = self.0.join("init script.sh");
            if path.exists() {
                fs::remove_file(&path).unwrap();
            }
            fs::write(&path, script).unwrap();
            // Intentionally not executable, as on the read-only cargo-bay.
            fs::set_permissions(&path, fs::Permissions::from_mode(0o400))
                .unwrap();
            cmd.arg("--init-script").arg(path);
        }
        cmd.env("PATH", self.0.join("bin"))
            .env("COMMAND_LOG", self.0.join("commands"))
            .env("TEST_READY", self.0.join("ready"))
            .env("FAIL_COMMAND", fail)
            .output()
            .unwrap()
    }

    fn commands(&self) -> String {
        fs::read_to_string(self.0.join("commands")).unwrap()
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn config_is_applied_offline_with_or_without_a_read_only_init_script() {
    for script in [None, Some("echo init >> \"$COMMAND_LOG\"\n")] {
        let guest = Guest::new();
        let out = guest.run(script, "");
        assert!(out.status.success(), "{out:?}");
        let config = guest.0.join("bird.conf");
        let init = if script.is_some() { "init\n" } else { "" };
        assert_eq!(
            guest.commands(),
            format!(
                "rm -f /run/voxel-bird-ready\n\
                 {init}bird -p -c {}\n\
                 install -o bird -g bird -m 0640 {} /etc/bird/bird.conf\n\
                 systemctl restart bird\nbirdc show status\n\
                 systemctl enable bird\ntouch /run/voxel-bird-ready\n",
                config.display(),
                config.display(),
            )
        );
        assert_eq!(fs::read(guest.0.join("ready")).unwrap(), b"");
        // A repeated application also succeeds, without package installation.
        assert!(guest.run(script, "").status.success());
    }
}

#[test]
fn failed_steps_do_not_report_ready_or_continue() {
    for fail in [
        "bird",
        "install",
        "systemctl restart bird",
        "birdc",
        "systemctl enable bird",
        "touch",
    ] {
        let guest = Guest::new();
        let out = guest.run(None, fail);
        assert!(!out.status.success(), "{fail}: {out:?}");
        assert!(!guest.0.join("ready").exists(), "{fail}");
        let commands = guest.commands();
        assert!(
            commands.lines().last().unwrap().starts_with(fail),
            "{commands}"
        );
    }
}

#[test]
fn waits_for_the_control_socket_after_service_start() {
    let guest = Guest::new();
    fs::write(guest.0.join("ready.socket-pending"), "").unwrap();
    let out = guest.run(None, "");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(guest.commands().matches("birdc show status").count(), 2);
    assert!(guest.0.join("ready").exists());
}

#[test]
fn script_failure_stops_before_config_install_and_clears_stale_readiness() {
    let guest = Guest::new();
    let out =
        guest.run(Some("false\necho should-not-run >> \"$COMMAND_LOG\""), "");
    assert!(!out.status.success());
    assert_eq!(guest.commands(), "rm -f /run/voxel-bird-ready\n");
    assert!(!guest.0.join("ready").exists());
}

#[test]
fn missing_config_clears_stale_readiness_and_fails_before_running_commands() {
    let guest = Guest::new();
    fs::remove_file(guest.0.join("bird.conf")).unwrap();
    let out = guest.run(None, "");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("config not found"));
    assert_eq!(guest.commands(), "rm -f /run/voxel-bird-ready\n");
    assert!(!guest.0.join("ready").exists());
}
