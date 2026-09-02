use std::{process::Stdio, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    sync::{mpsc, watch},
};

use super::{
    command::Command,
    error::{ProcessError, ProcessOutput},
    event::{ProcessEvent, TerminatedPayload},
    handle::{Containment, Ctrl},
    pid_file::{
        EpochPidFile, EpochPidRecord, inspect_process_identity, publish_epoch_pid_file,
        reap_epoch_pid_file, remove_epoch_pid_file_if_matches,
    },
};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;
const TERMINAL_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct SpawnParts {
    pub(crate) pid: u32,
    pub(crate) containment: Containment,
    pub(crate) ctrl_tx: mpsc::Sender<Ctrl>,
    pub(crate) terminated_rx: watch::Receiver<Option<Result<TerminatedPayload, String>>>,
    pub(crate) events_rx: mpsc::Receiver<ProcessEvent>,
}

pub(crate) async fn spawn(command: Command) -> Result<SpawnParts, ProcessError> {
    let program = command.program.to_string_lossy().into_owned();
    let epoch_pid_file = command.epoch_pid_file.clone();
    if let Some(spec) = &epoch_pid_file {
        let runtime_dir = spec
            .path()
            .parent()
            .ok_or_else(|| ProcessError::Engine("epoch pid file has no parent".into()))?;
        reap_epoch_pid_file(spec.path(), runtime_dir).await?;
    }
    let mut child = tokio::process::Command::new(&command.program);
    child
        .args(&command.args)
        .envs(command.envs)
        .stdin(if command.pipe_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(current_dir) = command.current_dir {
        child.current_dir(current_dir);
    }
    #[cfg(windows)]
    if command.hide_window {
        child.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = child.spawn().map_err(|error| ProcessError::Spawn {
        program,
        message: error.to_string(),
    })?;
    let pid = child
        .id()
        .ok_or_else(|| ProcessError::Engine("spawned child has no pid".into()))?;
    let owned_pid_record = match epoch_pid_file {
        Some(spec) => match publish_spawned_pid_record(pid, &spec).await {
            Ok(record) => Some((spec.path().to_path_buf(), record)),
            Err(error) => {
                let _ = hard_kill_tree(pid).await;
                let _ = child.wait().await;
                return Err(error);
            }
        },
        None => None,
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProcessError::Engine("stdout pipe was not created".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProcessError::Engine("stderr pipe was not created".into()))?;
    let mut stdin = child.stdin.take();
    let (events_tx, events_rx) = mpsc::channel(command.event_channel_capacity);
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel(64);
    let (terminated_tx, terminated_rx) = watch::channel(None);
    let encoding = command.encoding;
    let mut stdout_task = tokio::spawn(pump_lines(stdout, encoding, true, events_tx.clone()));
    let mut stderr_task = tokio::spawn(pump_lines(stderr, encoding, false, events_tx.clone()));
    let timeout_at = command
        .timeout
        .map(|timeout| tokio::time::Instant::now() + timeout);
    let kill_grace = command.kill_grace;

    tokio::spawn(async move {
        let mut graceful_deadline = None;
        let mut timeout_at = timeout_at;
        loop {
            tokio::select! {
                status = child.wait() => {
                    let result = status
                        .map(|status| TerminatedPayload {
                            code: status.code(),
                            signal: exit_signal(&status),
                        })
                        .map_err(|error| error.to_string());
                    let drain = async {
                        let _ = (&mut stdout_task).await;
                        let _ = (&mut stderr_task).await;
                    };
                    if tokio::time::timeout(TERMINAL_DRAIN_TIMEOUT, drain).await.is_err() {
                        stdout_task.abort();
                        stderr_task.abort();
                    }
                    match &result {
                        Ok(payload) => {
                            let _ = tokio::time::timeout(
                                TERMINAL_DRAIN_TIMEOUT,
                                events_tx.send(ProcessEvent::Terminated(payload.clone())),
                            )
                            .await;
                        }
                        Err(error) => {
                            let _ = events_tx.send(ProcessEvent::Error(error.clone())).await;
                        }
                    }
                    if let Some((path, record)) = &owned_pid_record {
                        if let Err(error) = remove_epoch_pid_file_if_matches(path, record).await {
                            let _ = events_tx
                                .send(ProcessEvent::Error(format!(
                                    "failed to remove epoch pid record: {error}"
                                )))
                                .await;
                        }
                    }
                    let _ = terminated_tx.send(Some(result));
                    break;
                }
                Some(ctrl) = ctrl_rx.recv() => match ctrl {
                    Ctrl::GracefulKill(reply) => {
                        let result = kill_tree::tokio::kill_tree(pid)
                            .await
                            .map(|_| ())
                            .map_err(|error| ProcessError::Engine(error.to_string()));
                        if result.is_ok() {
                            graceful_deadline = Some(tokio::time::Instant::now() + kill_grace);
                        }
                        let _ = reply.send(result);
                    }
                    Ctrl::Kill(reply) => {
                        let result = hard_kill_tree(pid).await;
                        let _ = reply.send(result);
                    }
                    Ctrl::WriteStdin(data, reply) => {
                        let result = match stdin.as_mut() {
                            Some(stdin) => async {
                                stdin.write_all(&data).await?;
                                stdin.flush().await
                            }
                            .await
                            .map_err(ProcessError::Io),
                            None => Err(ProcessError::StdinUnavailable),
                        };
                        let _ = reply.send(result);
                    }
                },
                _ = wait_until(timeout_at), if timeout_at.is_some() => {
                    timeout_at = None;
                    let _ = events_tx.send(ProcessEvent::Error("process timed out".into())).await;
                    let _ = hard_kill_tree(pid).await;
                }
                _ = wait_until(graceful_deadline), if graceful_deadline.is_some() => {
                    graceful_deadline = None;
                    let _ = hard_kill_tree(pid).await;
                }
            }
        }
    });

    Ok(SpawnParts {
        pid,
        containment: Containment::ProcessTreeScan,
        ctrl_tx,
        terminated_rx,
        events_rx,
    })
}

async fn publish_spawned_pid_record(
    pid: u32,
    spec: &EpochPidFile,
) -> Result<EpochPidRecord, ProcessError> {
    let identity = inspect_process_identity(pid)
        .await?
        .ok_or_else(|| ProcessError::Engine(format!("spawned pid {pid} disappeared")))?;
    let record = EpochPidRecord {
        pid,
        epoch: spec.epoch(),
        executable: identity.executable,
        start_token: identity.start_token,
        runtime_config: spec.runtime_config().to_path_buf(),
    };
    publish_epoch_pid_file(spec.path(), &record).await?;
    Ok(record)
}

async fn wait_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn hard_kill_tree(pid: u32) -> Result<(), ProcessError> {
    #[cfg(unix)]
    {
        let config = kill_tree::Config {
            signal: "SIGKILL".into(),
            ..Default::default()
        };
        kill_tree::tokio::kill_tree_with_config(pid, &config)
            .await
            .map(|_| ())
            .map_err(|error| ProcessError::Engine(error.to_string()))
    }
    #[cfg(not(unix))]
    {
        kill_tree::tokio::kill_tree(pid)
            .await
            .map(|_| ())
            .map_err(|error| ProcessError::Engine(error.to_string()))
    }
}

async fn pump_lines<R>(
    reader: R,
    encoding: Option<&'static encoding_rs::Encoding>,
    stdout: bool,
    events: mpsc::Sender<ProcessEvent>,
) where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) => break,
            Ok(_) => {
                while matches!(buffer.last(), Some(b'\n' | b'\r')) {
                    buffer.pop();
                }
                let line = decode(&buffer, encoding);
                let event = if stdout {
                    ProcessEvent::Stdout(line)
                } else {
                    ProcessEvent::Stderr(line)
                };
                if events.send(event).await.is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = events.send(ProcessEvent::Error(error.to_string())).await;
                break;
            }
        }
    }
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

pub(crate) async fn run_capture(command: Command) -> Result<ProcessOutput, ProcessError> {
    let program = command.program.to_string_lossy().into_owned();
    let mut child = tokio::process::Command::new(&command.program);
    child
        .args(&command.args)
        .envs(command.envs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(current_dir) = command.current_dir {
        child.current_dir(current_dir);
    }
    #[cfg(windows)]
    if command.hide_window {
        child.creation_flags(CREATE_NO_WINDOW);
    }

    let output = match command.timeout {
        Some(after) => tokio::time::timeout(after, child.output())
            .await
            .map_err(|_| ProcessError::Timeout { after })?
            .map_err(|error| ProcessError::Spawn {
                program,
                message: error.to_string(),
            })?,
        None => child.output().await.map_err(|error| ProcessError::Spawn {
            program,
            message: error.to_string(),
        })?,
    };

    Ok(ProcessOutput {
        code: output.status.code(),
        stdout: decode(&output.stdout, command.encoding),
        stderr: decode(&output.stderr, command.encoding),
    })
}

fn decode(bytes: &[u8], encoding: Option<&'static encoding_rs::Encoding>) -> String {
    match encoding {
        Some(encoding) => encoding.decode(bytes).0.into_owned(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_a_successful_command() {
        let output = Command::new("rustc")
            .arg("--version")
            .output()
            .await
            .unwrap();
        assert!(output.success());
        assert!(output.stdout.contains("rustc"));
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    async fn spawn_failure_preserves_the_program_name() {
        let error = Command::new("definitely-not-a-real-program")
            .output()
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ProcessError::Spawn { ref program, .. }
                if program == "definitely-not-a-real-program"
        ));
    }

    #[tokio::test]
    async fn spawn_streams_output_before_the_terminal_event() {
        let (handle, mut events) = Command::new("rustc")
            .arg("--version")
            .spawn()
            .await
            .unwrap();
        let mut saw_version = false;
        let mut saw_terminal = false;
        while let Some(event) = events.recv().await {
            match event {
                ProcessEvent::Stdout(line) => saw_version |= line.contains("rustc"),
                ProcessEvent::Terminated(payload) => {
                    assert_eq!(payload.code, Some(0));
                    saw_terminal = true;
                }
                _ => {}
            }
        }
        assert!(saw_version);
        assert!(saw_terminal);
        assert_eq!(handle.wait().await.unwrap().code, Some(0));
    }

    #[cfg(windows)]
    fn long_running_command() -> Command {
        Command::new("powershell.exe").args([
            "-NoProfile",
            "-Command",
            "[Console]::Out.WriteLine('ready'); [Console]::Out.Flush(); Start-Sleep -Seconds 30",
        ])
    }

    #[cfg(unix)]
    fn long_running_command() -> Command {
        Command::new("sh").args(["-c", "echo ready; sleep 30"])
    }

    #[tokio::test]
    async fn epoch_pid_record_tracks_the_live_child_and_is_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config-7.yaml");
        let pid_file = dir.path().join("core-7.pid");
        tokio::fs::write(&config, "port: 0\n").await.unwrap();

        let (handle, _events) = long_running_command()
            .epoch_pid_file(EpochPidFile::new(&pid_file, 7, &config))
            .spawn()
            .await
            .unwrap();
        let record = crate::process::read_epoch_pid_file(&pid_file)
            .await
            .unwrap()
            .expect("spawn must publish its pid record");
        assert_eq!(record.pid, handle.pid());
        assert_eq!(record.epoch, 7);
        assert_eq!(record.runtime_config, config);

        handle.kill().await.unwrap();
        handle.wait().await.unwrap();
        assert!(
            crate::process::read_epoch_pid_file(&pid_file)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn hard_kill_completes_wait() {
        let (handle, mut events) = long_running_command().spawn().await.unwrap();
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                if matches!(event, ProcessEvent::Stdout(ref line) if line == "ready") {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(ready);
        handle.kill().await.unwrap();
        let terminated = tokio::time::timeout(Duration::from_secs(5), handle.wait())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(terminated.code, Some(0));
    }

    #[cfg(windows)]
    fn stdin_echo_command() -> Command {
        Command::new("powershell.exe").args([
            "-NoProfile",
            "-Command",
            "$line = [Console]::In.ReadLine(); [Console]::Out.WriteLine($line)",
        ])
    }

    #[cfg(unix)]
    fn stdin_echo_command() -> Command {
        Command::new("sh").args(["-c", "read line; printf '%s\\n' \"$line\""])
    }

    #[tokio::test]
    async fn piped_stdin_is_written_and_flushed() {
        let (handle, mut events) = stdin_echo_command().pipe_stdin(true).spawn().await.unwrap();
        handle.write_stdin(b"hello from stdin\n").await.unwrap();
        let echoed = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                if let ProcessEvent::Stdout(line) = event {
                    return line;
                }
            }
            String::new()
        })
        .await
        .unwrap();
        assert_eq!(echoed, "hello from stdin");
        assert_eq!(handle.wait().await.unwrap().code, Some(0));
    }

    #[tokio::test]
    async fn one_shot_timeout_returns_a_typed_error() {
        let timeout = Duration::from_millis(100);
        let error = long_running_command()
            .timeout(timeout)
            .output()
            .await
            .unwrap_err();
        assert!(matches!(error, ProcessError::Timeout { after } if after == timeout));
    }
}
