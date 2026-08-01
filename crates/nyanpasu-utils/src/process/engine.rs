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
    let pid = child.id().ok_or_else(|| ProcessError::Engine("spawned child has no pid".into()))?;
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
    let mut stdout_task = tokio::spawn(pump_lines(
        stdout,
        encoding,
        true,
        events_tx.clone(),
    ));
    let mut stderr_task = tokio::spawn(pump_lines(
        stderr,
        encoding,
        false,
        events_tx.clone(),
    ));
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

