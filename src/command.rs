use std::{
    env,
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    process::Command,
};

use portable_pty::CommandBuilder;

#[cfg(windows)]
const INTERNAL_LAUNCH_ARGUMENT: &str = "__crowded-launch";

#[cfg(windows)]
const LAUNCH_JOB: &str = "CROWDED_INTERNAL_LAUNCH_JOB";
#[cfg(windows)]
const LAUNCH_PROGRAM: &str = "CROWDED_INTERNAL_LAUNCH_PROGRAM";
#[cfg(windows)]
const LAUNCH_ARG_COUNT: &str = "CROWDED_INTERNAL_LAUNCH_ARG_COUNT";
#[cfg(windows)]
const LAUNCH_ARG_PREFIX: &str = "CROWDED_INTERNAL_LAUNCH_ARG_";

#[derive(Debug)]
pub(crate) struct ResolvedCommand {
    program: PathBuf,
    args: Vec<OsString>,
}

pub(crate) struct PortableCommand {
    command: CommandBuilder,
    tree: Option<ProcessTree>,
}

impl PortableCommand {
    pub(crate) fn into_parts(self) -> (CommandBuilder, Option<ProcessTree>) {
        (self.command, self.tree)
    }
}

impl ResolvedCommand {
    pub(crate) fn resolve(program: &OsStr, args: &[OsString], cwd: &Path) -> io::Result<Self> {
        Self::resolve_with(
            program,
            args,
            cwd,
            cfg!(windows),
            env::var_os("PATH"),
            env::var_os("PATHEXT"),
        )
    }

    pub(crate) fn resolve_with_environment(
        program: &OsStr,
        args: &[OsString],
        cwd: &Path,
        path: Option<OsString>,
        path_ext: Option<OsString>,
    ) -> io::Result<Self> {
        Self::resolve_with(program, args, cwd, cfg!(windows), path, path_ext)
    }

    fn resolve_with(
        program: &OsStr,
        args: &[OsString],
        cwd: &Path,
        windows: bool,
        path: Option<OsString>,
        path_ext: Option<OsString>,
    ) -> io::Result<Self> {
        let program = Path::new(program);
        let program = if windows {
            resolve_windows(program, cwd, path.as_deref(), path_ext.as_deref())?
        } else {
            program.to_path_buf()
        };
        Ok(Self {
            program,
            args: args.to_vec(),
        })
    }

    pub(crate) fn portable(&self) -> io::Result<PortableCommand> {
        #[cfg(windows)]
        {
            let tree = ProcessTree::new()?;
            let mut command = CommandBuilder::new(env::current_exe()?);
            command.arg(INTERNAL_LAUNCH_ARGUMENT);
            command.env(LAUNCH_JOB, tree.name());
            command.env(LAUNCH_PROGRAM, self.program.as_os_str());
            command.env(LAUNCH_ARG_COUNT, self.args.len().to_string());
            for (index, argument) in self.args.iter().enumerate() {
                command.env(format!("{LAUNCH_ARG_PREFIX}{index}"), argument);
            }
            Ok(PortableCommand {
                command,
                tree: Some(tree),
            })
        }
        #[cfg(not(windows))]
        {
            let mut command = CommandBuilder::new(&self.program);
            command.args(&self.args);
            Ok(PortableCommand {
                command,
                tree: None,
            })
        }
    }

    pub(crate) fn standard(&self) -> Command {
        // Rust's Windows process layer recognizes .cmd/.bat and applies its
        // hardened batch-script quoting. Native executables stay direct.
        let mut process = Command::new(&self.program);
        process.args(&self.args);
        process
    }
}

fn resolve_windows(
    program: &Path,
    cwd: &Path,
    path: Option<&OsStr>,
    path_ext: Option<&OsStr>,
) -> io::Result<PathBuf> {
    let bases = if program.components().count() > 1 {
        vec![if program.is_absolute() {
            program.to_path_buf()
        } else {
            cwd.join(program)
        }]
    } else {
        path.map(split_windows_paths)
            .unwrap_or_default()
            .into_iter()
            .map(|directory| directory.join(program))
            .collect()
    };
    let extensions = if program.extension().is_some() {
        vec![OsString::new()]
    } else {
        path_ext
            .map(split_windows_extensions)
            .filter(|extensions| !extensions.is_empty())
            .unwrap_or_else(|| {
                vec![
                    OsString::from(".COM"),
                    OsString::from(".EXE"),
                    OsString::from(".BAT"),
                    OsString::from(".CMD"),
                ]
            })
    };
    for base in bases {
        for extension in &extensions {
            let candidate = if extension.is_empty() {
                base.clone()
            } else {
                base.with_extension(extension.to_string_lossy().trim_start_matches('.'))
            };
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("command not found on PATH: {}", program.display()),
    ))
}

fn split_windows_paths(path: &OsStr) -> Vec<PathBuf> {
    path.to_string_lossy()
        .split(';')
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn split_windows_extensions(path_ext: &OsStr) -> Vec<OsString> {
    path_ext
        .to_string_lossy()
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(OsString::from)
        .collect()
}

fn resolve_unix(program: &Path, path: Option<&OsStr>) -> io::Result<PathBuf> {
    let bases = path
        .map(split_unix_paths)
        .unwrap_or_default()
        .into_iter()
        .map(|directory| directory.join(program))
        .collect::<Vec<_>>();
    for base in bases {
        if base.is_file() {
            return Ok(base);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("command not found on PATH: {}", program.display()),
    ))
}

fn split_unix_paths(path: &OsStr) -> Vec<PathBuf> {
    path.to_string_lossy()
        .split(':')
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Whether the `headroom` wrapper binary is installed on the given PATH.
/// Mirrors the launch-time lookup (`resolve_windows` for Windows, the new
/// `resolve_unix` scan for Unix) without spawning a subprocess.
pub(crate) fn headroom_on_path(path: Option<&OsStr>, path_ext: Option<&OsStr>) -> bool {
    let program = Path::new("headroom");
    if cfg!(windows) {
        resolve_windows(program, Path::new("."), path, path_ext).is_ok()
    } else {
        resolve_unix(program, path).is_ok()
    }
}

#[cfg(windows)]
fn required_environment(name: &str) -> io::Result<OsString> {
    env::var_os(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("internal launcher is missing {name}"),
        )
    })
}

#[cfg(windows)]
pub(crate) fn run_internal_launcher() -> io::Result<i32> {
    let job = required_environment(LAUNCH_JOB)?;
    let program = required_environment(LAUNCH_PROGRAM)?;
    let count: usize = required_environment(LAUNCH_ARG_COUNT)?
        .to_string_lossy()
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid launcher arg count"))?;
    let mut args = Vec::with_capacity(count);
    for index in 0..count {
        args.push(required_environment(&format!(
            "{LAUNCH_ARG_PREFIX}{index}"
        ))?);
    }

    ProcessTree::join_current(&job)?;
    let mut command = Command::new(program);
    command.args(&args);
    command
        .env_remove(LAUNCH_JOB)
        .env_remove(LAUNCH_PROGRAM)
        .env_remove(LAUNCH_ARG_COUNT);
    for index in 0..count {
        command.env_remove(format!("{LAUNCH_ARG_PREFIX}{index}"));
    }
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(windows)]
pub(crate) fn internal_launcher_requested() -> bool {
    env::args_os().nth(1).as_deref() == Some(OsStr::new(INTERNAL_LAUNCH_ARGUMENT))
}

pub(crate) struct ProcessTree {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(windows)]
    name: OsString,
}

#[cfg(windows)]
impl ProcessTree {
    fn new() -> io::Result<Self> {
        use std::{
            os::windows::ffi::OsStrExt,
            sync::atomic::{AtomicU64, Ordering},
        };
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::JobObjects::{
                CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };

        static NEXT_JOB: AtomicU64 = AtomicU64::new(1);
        let name = OsString::from(format!(
            "Local\\CrowdedRoom-{}-{}",
            std::process::id(),
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        ));
        let wide: Vec<u16> = name.encode_wide().chain(std::iter::once(0)).collect();
        let job = unsafe { CreateJobObjectW(std::ptr::null(), wide.as_ptr()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            unsafe { CloseHandle(job) };
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle: job, name })
    }

    fn name(&self) -> &OsStr {
        &self.name
    }

    fn join_current(name: &OsStr) -> io::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::{
                JobObjects::{AssignProcessToJobObject, OpenJobObjectW},
                SystemServices::JOB_OBJECT_ASSIGN_PROCESS,
                Threading::GetCurrentProcess,
            },
        };

        let wide: Vec<u16> = name.encode_wide().chain(std::iter::once(0)).collect();
        let job = unsafe { OpenJobObjectW(JOB_OBJECT_ASSIGN_PROCESS, 0, wide.as_ptr()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == 0 {
            let error = io::Error::last_os_error();
            unsafe { CloseHandle(job) };
            return Err(error);
        }
        unsafe { CloseHandle(job) };
        Ok(())
    }

    pub(crate) fn terminate(&self) {
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.handle, 1) };
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    fn test_directory() -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "crowded-command-test-{}-{nonce}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        directory
    }

    #[test]
    fn windows_resolves_executables_and_batch_shims_from_path() {
        let directory = test_directory();
        fs::write(directory.join("uv.exe"), "").unwrap();
        fs::write(directory.join("agent.cmd"), "").unwrap();
        let path = directory.clone().into_os_string();
        let uv = ResolvedCommand::resolve_with(
            OsStr::new("uv"),
            &[],
            Path::new("."),
            true,
            Some(path.clone()),
            Some(OsString::from(".exe;.cmd")),
        )
        .unwrap();
        assert_eq!(uv.program.file_name(), Some(OsStr::new("uv.exe")));
        let agent = ResolvedCommand::resolve_with(
            OsStr::new("agent"),
            &[],
            Path::new("."),
            true,
            Some(path),
            Some(OsString::from(".exe;.cmd")),
        )
        .unwrap();
        assert_eq!(agent.program.file_name(), Some(OsStr::new("agent.cmd")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn non_windows_commands_stay_direct() {
        let command = ResolvedCommand::resolve_with(
            OsStr::new("claude"),
            &[OsString::from("model name")],
            Path::new("."),
            false,
            None,
            None,
        )
        .unwrap();
        let mut direct = CommandBuilder::new(&command.program);
        direct.args(&command.args);
        assert_eq!(
            direct.get_argv().as_slice(),
            &[OsString::from("claude"), OsString::from("model name")]
        );
    }

    #[test]
    fn headroom_detection_finds_and_misses_the_binary_on_path() {
        let directory = test_directory();
        let ext = if cfg!(windows) {
            fs::write(directory.join("headroom.exe"), "").unwrap();
            OsString::from(".exe;.cmd")
        } else {
            fs::write(directory.join("headroom"), "").unwrap();
            OsString::new()
        };
        let path = directory.as_os_str().to_os_string();

        assert!(headroom_on_path(Some(&path), Some(&ext)));
        assert!(!headroom_on_path(None, None));

        let empty = test_directory();
        assert!(!headroom_on_path(Some(empty.as_os_str()), Some(&ext)));
        fs::remove_dir_all(empty).unwrap();
        fs::remove_dir_all(&directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn standard_batch_launch_preserves_common_metacharacters() {
        let directory = test_directory();
        let shim = directory.join("echo-args.cmd");
        let script = directory.join("echo-args.vbs");
        fs::write(
            &shim,
            "@echo off\r\ncscript.exe //nologo \"%~dp0echo-args.vbs\" %*\r\n",
        )
        .unwrap();
        fs::write(&script, "WScript.StdOut.Write WScript.Arguments(0)\r\n").unwrap();
        // ponytail: literal embedded quotes are a cmd.exe limitation; Crowded
        // sends prompts through the PTY, so startup arguments do not need them.
        let argument = OsString::from("model name & | < > ^ % !");
        let output = ResolvedCommand {
            program: shim,
            args: vec![argument.clone()],
        }
        .standard()
        .output()
        .unwrap();
        assert!(output.status.success(), "{:?}", output.status);
        assert_eq!(
            OsString::from(String::from_utf8(output.stdout).unwrap()),
            argument
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_launcher_test_entry() {
        if env::var_os("CROWDED_TEST_LAUNCHER").is_some() {
            std::process::exit(run_internal_launcher().unwrap());
        }
    }

    #[cfg(windows)]
    fn process_is_running(pid: u32) -> bool {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, STILL_ACTIVE},
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };

        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return false;
        }
        let mut code = 0;
        let running =
            unsafe { GetExitCodeProcess(process, &mut code) } != 0 && code == STILL_ACTIVE as u32;
        unsafe { CloseHandle(process) };
        running
    }

    #[cfg(windows)]
    #[test]
    fn process_tree_termination_reaps_a_batch_descendant() {
        use std::{io::Read, thread, time::Duration};

        use portable_pty::{PtySize, native_pty_system};

        let directory = test_directory();
        let pid_file = directory.join("child.pid");
        let shim = directory.join("long-child.cmd");
        fs::write(
            &shim,
            "@echo off\r\npowershell.exe -NoProfile -NonInteractive -Command \"$PID | Set-Content -NoNewline -Path $env:CROWDED_TEST_PID_FILE; Start-Sleep -Seconds 30\"\r\n",
        )
        .unwrap();

        let launch = ResolvedCommand {
            program: shim,
            args: Vec::new(),
        }
        .portable()
        .unwrap();
        let (mut command, tree) = launch.into_parts();
        let tree = tree.unwrap();
        let argv = command.get_argv_mut();
        argv.truncate(1);
        argv.extend([
            OsString::from("--exact"),
            OsString::from("command::tests::windows_launcher_test_entry"),
            OsString::from("--nocapture"),
        ]);
        command.env("CROWDED_TEST_LAUNCHER", "1");
        command.env("CROWDED_TEST_PID_FILE", &pid_file);

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut output = pty.master.try_clone_reader().unwrap();
        let mut input = pty.master.take_writer().unwrap();
        thread::spawn(move || {
            let mut bytes = [0; 4096];
            let mut tail = Vec::new();
            while let Ok(count) = output.read(&mut bytes) {
                if count == 0
                    || crate::pane::respond_to_terminal_queries(
                        &mut *input,
                        &mut tail,
                        &bytes[..count],
                    )
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);

        for _ in 0..50 {
            if pid_file.is_file() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let pid: u32 = fs::read_to_string(&pid_file)
            .unwrap_or_else(|error| {
                panic!(
                    "batch descendant did not report its PID ({error}); launcher status: {:?}",
                    child.try_wait()
                )
            })
            .parse()
            .unwrap();
        assert!(process_is_running(pid));

        tree.terminate();
        child.wait().unwrap();
        for _ in 0..20 {
            if !process_is_running(pid) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !process_is_running(pid),
            "batch descendant survived cleanup"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
