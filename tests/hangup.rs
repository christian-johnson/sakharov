//! The run loop must not survive its terminal.
//!
//! When the master side of a pty closes, an editor sitting on the slave side
//! is orphaned: reads return instant EOF forever.  Because the slave is not
//! the process's *controlling* terminal in this situation (the classic case
//! is a process reparented to init when its shell died), the kernel sends no
//! `SIGHUP` — nothing tells the editor to stop.  A run loop that polls such
//! an fd without recognising the hang-up spins at 100% CPU indefinitely.
//!
//! This test reproduces that exact arrangement and asserts two things, both
//! of which matter: the process **exits**, and it exits **without having
//! burned CPU getting there**.  Exiting alone is not enough — a process that
//! spins for ten seconds and then dies has still flattened a battery.

#![cfg(unix)]

use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What an orphaned editor did after its terminal went away.
struct Outcome {
    /// Did the process exit on its own, before the timeout?
    exited: bool,
    /// CPU seconds (user + system) it consumed over its whole life.
    cpu: f64,
    /// Wall time from closing the master to the process exiting.
    elapsed: Duration,
}

/// Spawn `sv` on the slave side of a fresh pty, let it settle into its idle
/// loop, then close the master and watch what happens.
fn orphan_the_editor(settle: Duration, timeout: Duration) -> Outcome {
    let dir = std::env::temp_dir().join(format!("sv-hangup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("t.txt");
    std::fs::write(&file, "hello\nworld\n").expect("write fixture");

    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    // A real window size: a 0x0 terminal is a different failure mode and
    // would let this test pass for the wrong reason.
    let mut win = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut win as *mut libc::winsize,
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    // The child must NOT inherit the master. An inherited master keeps the pty
    // alive from the inside, so closing our copy hangs up nothing and the
    // editor sits in a perfectly healthy poll — the test then "reproduces" a
    // hang that has no relation to the bug.
    unsafe { libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC) };

    // Each stdio slot needs its own descriptor: `Stdio::from_raw_fd` takes
    // ownership and closes its fd in the parent once the child is spawned.
    let dup = |fd: libc::c_int| unsafe { Stdio::from_raw_fd(libc::dup(fd)) };
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sv"));
    cmd.arg(&file)
        .stdin(dup(slave))
        .stdout(dup(slave))
        .stderr(dup(slave));
    // The pty must become the child's *controlling* terminal, which means its
    // own session. This is not incidental to the bug — it is the bug. Without
    // it the orphaned process merely blocks forever on a dead fd; with it, the
    // hung-up terminal reports itself endlessly readable and the poll spins at
    // 100% CPU. Reproducing the cheap variant would test the wrong thing.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // Deliberately NOT TIOCSCTTY. The pty is the child's terminal but
            // not its *controlling* terminal, so the kernel sends no SIGHUP
            // when the master closes — nothing at all tells the editor its
            // terminal is gone. That is the state a process is left in when it
            // is reparented away from the session that owned it, and it is the
            // state both runaway processes were found in.
            Ok(())
        });
    }
    let child = cmd.spawn().expect("spawn sv");
    // `Command` owns the `Stdio` values, and with them live dups of the slave.
    // While the parent holds any slave descriptor the pty is not fully
    // abandoned; the child must be its only user, exactly as it is when a
    // shell dies and leaves an editor behind.
    drop(cmd);
    let pid = child.id() as libc::pid_t;
    // The parent's own copy of the slave: hold nothing open, or the hang-up
    // never happens.
    unsafe { libc::close(slave) };

    // How long the editor gets before its terminal is taken away decides
    // *which* read is in progress when it happens — see the two tests below.
    std::thread::sleep(settle);

    // The terminal goes away.
    unsafe { libc::close(master) };
    let start = Instant::now();

    let mut status: libc::c_int = 0;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let mut exited = false;
    while start.elapsed() < timeout {
        // wait4 gives us this child's own resource usage, rather than the
        // test process's cumulative total across parallel tests.
        let r = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut usage) };
        if r == pid {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let elapsed = start.elapsed();

    if !exited {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::wait4(pid, &mut status, 0, &mut usage);
        }
    }
    // We reaped it ourselves; keep std from trying to as well.
    std::mem::forget(child);
    let _ = std::fs::remove_dir_all(&dir);

    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
    Outcome {
        exited,
        cpu: secs(usage.ru_utime) + secs(usage.ru_stime),
        elapsed,
    }
}

/// Assert an orphaned editor went quietly.
fn assert_went_quietly(out: Outcome, when: &str) {
    assert!(
        out.exited,
        "editor never exited after its terminal hung up {when} (burned {:.1}s of CPU while we waited)",
        out.cpu
    );
    // Startup — tree-sitter grammars, config, the first frame — is the bulk of
    // a legitimate run's CPU. Spinning shows up as seconds, not milliseconds,
    // so a 2s ceiling separates the two without being flaky on a loaded box.
    assert!(
        out.cpu < 2.0,
        "editor exited after {:?} {when} but burned {:.1}s of CPU doing it — it spun before noticing",
        out.elapsed,
        out.cpu
    );
}

/// The terminal dies while the editor is still starting up, so the read in
/// progress is one of the terminal capability queries — `read_all_stdin`'s
/// OSC colour probe, which sits in its own poll/read loop long before the run
/// loop exists.
#[test]
fn an_editor_orphaned_during_startup_exits_instead_of_spinning() {
    let out = orphan_the_editor(Duration::from_millis(1500), Duration::from_secs(10));
    assert_went_quietly(out, "during startup");
}

/// The terminal dies once the editor is idle in its run loop — the case both
/// runaway processes were found in, where the wedged read is inside
/// `crossterm::event::poll`.
#[test]
fn an_idle_editor_orphaned_in_its_run_loop_exits_instead_of_spinning() {
    let out = orphan_the_editor(Duration::from_secs(6), Duration::from_secs(10));
    assert_went_quietly(out, "while idle");
}
