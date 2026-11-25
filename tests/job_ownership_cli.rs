#![cfg(windows)]
//! End-to-end proof that the production binary joins the launcher job:
//! drives `crowded __crowded-launch` with a real job object and a real
//! .cmd wrapper, checks membership from outside the job, terminates the
//! job, and checks every member is dead by an independent liveness probe.
//!
//! The `CROWDED_INTERNAL_LAUNCH_*` names and the `__crowded-launch`
//! argument mirror the internal launcher protocol in `src/command.rs`;
//! a mismatch fails loudly below (no join, no pid file), so drift cannot
//! go unnoticed. `tempfile` comes from the crate's dependencies, as in
//! the other CLI tests.

use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, STILL_ACTIVE},
    System::{
        JobObjects::{
            CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

/// A job that kills every member when its last handle closes.
fn create_job(name: &std::ffi::OsString) -> HANDLE {
    let wide: Vec<u16> = name.encode_wide().chain(std::iter::once(0)).collect();
    let job = unsafe { CreateJobObjectW(std::ptr::null(), wide.as_ptr()) };
    assert!(!job.is_null(), "CreateJobObjectW failed");
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    assert_ne!(
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            )
        },
        0,
        "SetInformationJobObject failed"
    );
    job
}

fn is_member(job: HANDLE, pid: u32) -> bool {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let mut code = 0;
    let alive =
        unsafe { GetExitCodeProcess(process, &mut code) } != 0 && code == STILL_ACTIVE as u32;
    let mut member = 0;
    let inside = alive && unsafe { IsProcessInJob(process, job, &mut member) } != 0 && member != 0;
    unsafe { CloseHandle(process) };
    inside
}

fn is_alive(pid: u32) -> bool {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let mut code = 0;
    let alive =
        unsafe { GetExitCodeProcess(process, &mut code) } != 0 && code == STILL_ACTIVE as u32;
    unsafe { CloseHandle(process) };
    alive
}

fn wait_pid_file(path: &std::path::Path) -> u32 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server grandchild never reported its pid"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// The real binary joins the job before running the wrapper; the wrapper's
/// powershell grandchild inherits the job. Termination reaps the tree, and
/// independent liveness probes (not membership checks) prove it.
#[test]
fn launcher_wrapper_and_server_share_one_job_until_terminate() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("server.pid");
    std::fs::write(
        directory.path().join("server.cmd"),
        "@echo off\r\npowershell.exe -NoProfile -NonInteractive -Command \"$PID | Set-Content -NoNewline -Path $env:CROWDED_TEST_PID_FILE; Start-Sleep -Seconds 60\"\r\n",
    )
    .unwrap();

    let name = std::ffi::OsString::from(format!(
        "Local\\CrowdedCliTest-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let job = create_job(&name);

    let mut launcher = std::process::Command::new(env!("CARGO_BIN_EXE_crowded"))
        .arg("__crowded-launch")
        .env("CROWDED_INTERNAL_LAUNCH_JOB", &name)
        .env(
            "CROWDED_INTERNAL_LAUNCH_PROGRAM",
            directory.path().join("server.cmd"),
        )
        .env("CROWDED_INTERNAL_LAUNCH_ARG_COUNT", "0")
        .env("CROWDED_TEST_PID_FILE", &pid_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let launcher_pid = launcher.id();

    let server_pid = wait_pid_file(&pid_file);
    assert_ne!(server_pid, launcher_pid);
    assert!(
        is_member(job, server_pid),
        "server grandchild is not a job member"
    );
    assert!(is_member(job, launcher_pid), "launcher is not a job member");
    assert!(
        !is_member(job, std::process::id()),
        "test runner must stay outside the job"
    );

    let mut quick = std::process::Command::new("cmd")
        .args(["/c", "exit", "0"])
        .spawn()
        .unwrap();
    let quick_pid = quick.id();
    quick.wait().unwrap();
    assert!(
        !is_member(job, quick_pid),
        "an exited pid must fail the membership check"
    );

    assert_ne!(
        unsafe { TerminateJobObject(job, 1) },
        0,
        "TerminateJobObject failed"
    );
    launcher.wait().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while (is_alive(server_pid) || is_alive(launcher_pid)) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        !is_alive(server_pid),
        "server grandchild survived the job terminate"
    );
    assert!(
        !is_alive(launcher_pid),
        "launcher survived the job terminate"
    );

    unsafe { CloseHandle(job) };
}
