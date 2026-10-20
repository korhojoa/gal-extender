// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

use super::*;

fn chunks(data: &[u8], cap: usize) -> Vec<Vec<u8>> {
    fgets_chunks(data, cap).map(<[u8]>::to_vec).collect()
}

#[test]
fn fgets_chunks_split_at_newlines_the_cap_and_nul() {
    assert_eq!(
        chunks(b"ab\ncd\n\nlast", 512),
        [&b"ab\n"[..], b"cd\n", b"\n", b"last"]
    );
    // cap 5: four bytes per call, the newline goes with the next call.
    assert_eq!(chunks(b"abcdefg\nh", 5), [&b"abcd"[..], b"efg\n", b"h"]);
    assert_eq!(chunks(b"abcd\nx", 5), [&b"abcd"[..], b"\n", b"x"]);
    assert_eq!(chunks(b"ab\0cd\nef", 512), [&b"ab"[..], b"ef"]);
    assert_eq!(chunks(b"", 512).len(), 0);
}

#[test]
fn trim_removes_list_whitespace() {
    assert_eq!(trim(b" \t once: x \r\n"), b"once: x");
    assert_eq!(trim(b"\r\n"), b"");
    assert_eq!(trim(b"\n x"), b"\n x", "only blanks and tabs lead");
}

fn parse(text: &[u8]) -> (Vec<Entry>, Vec<Vec<u8>>) {
    let mut bad = Vec::new();
    let e = parse_list(text, |c| bad.push(c.to_vec()));
    (e, bad)
}

fn entry(cmd: &str, check: &str, is_once: bool) -> Entry {
    Entry {
        cmd: cmd.as_bytes().to_vec(),
        check: check.as_bytes().to_vec(),
        is_once,
        action: Action::None,
    }
}

#[test]
fn the_list_grammar() {
    let (e, bad) = parse(
        b"# comment\n\
          \n   \n\
          once: true\n\
          once:false\n\
          daemon:   check=btgamepadd  /fs/run.sh  a b \n\
          check=x.y-z_1 cmd\n\
          plain cmd\n\
          check=bad$name foo\n\
          check= foo\n\
          once:\n\
          check=onlyname\n\
          daemon: check=\tcmd2\n\
          ONCE: upper\n",
    );
    assert_eq!(
        e,
        [
            entry("true", "", true),
            entry("false", "", true),
            entry("/fs/run.sh  a b", "btgamepadd", false),
            entry("cmd", "x.y-z_1", false),
            entry("plain cmd", "", false),
            entry("ONCE: upper", "", false),
        ]
    );
    // Empty names are invalid too. a valid name with no command is dropped
    // silently.
    assert_eq!(bad, [&b"bad$name"[..], b"", b""]);
}

#[test]
fn a_long_check_name_is_cut_at_63_and_the_rest_is_the_command() {
    let name = "n".repeat(70);
    let (e, bad) = parse(format!("check={name} run\n").as_bytes());
    assert_eq!(bad.len(), 0);
    assert_eq!(e, [entry("nnnnnnn run", &"n".repeat(63), false)]);
}

#[test]
fn at_most_32_entries_and_long_lines_split() {
    let mut text = Vec::new();
    for i in 0..40 {
        text.extend_from_slice(format!("cmd{i}\n").as_bytes());
    }
    let (e, _) = parse(&text);
    assert_eq!(e.len(), MAX_ENTRIES);
    assert_eq!(e[31].cmd, b"cmd31");
    // Invalid names after the 32nd entry are never read, so not logged.
    let mut text = b"a\n".repeat(32);
    text.extend_from_slice(b"check=$ x\n");
    let (e, bad) = parse(&text);
    assert_eq!((e.len(), bad.len()), (32, 0));

    let long = format!("once: {}\n", "x".repeat(600));
    let (e, _) = parse(long.as_bytes());
    assert_eq!(e.len(), 2);
    assert_eq!(e[0].cmd.len(), 511 - 6);
    assert!(e[0].is_once);
    assert_eq!(e[1].cmd, "x".repeat(600 - 505).as_bytes());
    assert!(!e[1].is_once);
}

#[test]
fn launch_list_follows_the_conf() {
    assert_eq!(
        launch_list_path(b"/qnx/fdt/gal_loader.conf"),
        b"/qnx/fdt/gal_launch.conf"
    );
    assert_eq!(launch_list_path(b"/gal_loader.conf"), b"/gal_launch.conf");
    gal_feature_api::location::set_dir("/qnx/installed-loader");
    let default = b"/qnx/installed-loader/gal_launch.conf";
    assert_eq!(launch_list_path(b""), default);
    assert_eq!(launch_list_path(b"gal_loader.conf"), default);
    let deep = format!("/{}/gal_loader.conf", "d".repeat(240));
    assert_eq!(launch_list_path(deep.as_bytes()), default);
    let edge = format!("/{}/c", "d".repeat(233));
    assert_eq!(
        launch_list_path(edge.as_bytes()),
        format!("/{}/gal_launch.conf", "d".repeat(233)).as_bytes()
    );
}

#[test]
fn names_and_numbers() {
    assert!(valid_process_name(b"btgamepadd"));
    assert!(valid_process_name(b"A.b-c_9"));
    assert!(!valid_process_name(b""));
    assert!(!valid_process_name(b"a b"));
    assert!(!valid_process_name(b"x'"));
    assert_eq!(scan_int(b"  1234\n"), Some(1234));
    assert_eq!(scan_int(b"-5"), Some(-5));
    assert_eq!(scan_int(b"\n"), None);
    assert_eq!(atoi(b"3)"), 3);
    assert_eq!(atoi(b"x"), 0);
    assert!(exited_zero(0));
    assert!(!exited_zero(1 << 8));
    assert!(!exited_zero(-1));
    assert!(!exited_zero(9), "killed by a signal");
}

#[test]
fn runner_script_text() {
    let s = runner_script(b"echo 'a b'", Some("/s/x.done"));
    assert_eq!(
        String::from_utf8(s).unwrap(),
        "echo 'a b' >/dev/null 2>&1\n\
         rc=$?\n\
         [ $rc -eq 0 ] && : > /s/x.done\n"
    );
    let s = runner_script(b"d", None);
    assert!(!String::from_utf8(s).unwrap().contains(".done"));
}

/// A fresh directory for one test: under `GAL_TEST_DIR` when set, else the
/// temporary directory. On QNX use the current directory because
/// /dev/shmem does not support subdirectories.
fn test_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::var_os("GAL_TEST_DIR").map_or_else(
        || {
            if cfg!(target_os = "nto") {
                std::env::current_dir().unwrap_or_else(|_| "/".into())
            } else {
                std::env::temp_dir()
            }
        },
        Into::into,
    );
    base.join(format!("gal-launcher-{tag}-{}", std::process::id()))
}

/// The whole flow with real shells: launch and completion. then a second
/// gal start that skips what is done or running.
#[test]
fn launch_once_and_keep_running_daemon() {
    struct OwnedDaemon(i32);
    impl Drop for OwnedDaemon {
        fn drop(&mut self) {
            if self.0 > 0 {
                // SAFETY: only the PID returned for this test's daemon.
                unsafe { kill(self.0, 15) };
            }
        }
    }

    let dir = test_dir("flow");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap().to_string();
    let list = format!("{d}/gal_launch.conf");
    std::fs::write(
        &list,
        format!(
            "once: true\n\
         once: sh -c 'exit 3'\n\
         exec sleep 30\n\
         daemon: check=no-such-proc-x sh -c 'echo hi > {d}/observed'\n"
        ),
    )
    .unwrap();
    let shm = Shm { dir: &d };
    run(&shm, list.as_bytes());
    let mut daemon = OwnedDaemon(shm.read_pid(2));
    assert!(daemon.0 > 0);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !(shm.done_exists(0)
        && shm.read_pid(2) > 0
        && std::fs::read_to_string(format!("{d}/observed")).unwrap_or_default() == "hi\n")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "owned launch did not complete"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(shm.done_exists(0));
    assert!(!shm.done_exists(1));

    // The second start: #0 done, #2 alive by pid. #1 and #3 run again.
    run(&shm, list.as_bytes());
    assert!(shm.done_exists(0));
    assert!(!shm.done_exists(1));
    assert!(shm.read_pid(2) > 0);

    // Stop the daemon: its runner shell exec'd the sleep, so the recorded
    // pid is the sleep.
    let pid = shm.read_pid(2);
    assert!(pid > 0);
    // SAFETY: SIGTERM to the process this test started.
    assert_eq!(unsafe { kill(pid, 15) }, 0);
    daemon.0 = 0;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_list_starts_nothing() {
    let dir = test_dir("none");
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap().to_string();
    let shm = Shm { dir: &d };
    run(&shm, format!("{d}/absent.conf").as_bytes());
    std::fs::write(format!("{d}/empty.conf"), "# nothing\n").unwrap();
    run(&shm, format!("{d}/empty.conf").as_bytes());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nonzero_shell_status_is_a_launch_failure() {
    for result in [-1, 1, 256] {
        assert_eq!(
            launch_result(0, b"test-command", false, result),
            Action::Failed
        );
    }
    assert_eq!(
        launch_result(0, b"test-command", false, 0),
        Action::Launched
    );
}
