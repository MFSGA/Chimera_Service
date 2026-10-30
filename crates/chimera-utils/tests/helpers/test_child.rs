//! Test helper child for Chimera utility process integration tests.
//! Not a production binary. Modes documented in the implementation plan.

use std::{io::Write, time::Duration};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    #[cfg(windows)]
    if let Some(result) = chimera_utils::process::windows_shutdown_helper_entry() {
        if let Err(error) = result {
            eprintln!("shutdown helper failed: {error}");
            std::process::exit(2);
        }
        return;
    }

    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    match mode.as_str() {
        "exit-with" => {
            let code: i32 = args.next().expect("code").parse().expect("i32");
            std::process::exit(code);
        }
        "echo-lines" => {
            for a in args {
                println!("{a}");
            }
            eprintln!("stderr-marker");
        }
        "spam-stdout" => {
            let n: usize = args.next().expect("n").parse().expect("usize");
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            for i in 0..n {
                writeln!(lock, "line-{i}").expect("write");
            }
        }
        "sleep-forever" => {
            println!("ready");
            sleep_forever().await;
        }
        "sleep-then-exit" => {
            let delay_ms: u64 = args.next().expect("milliseconds").parse().expect("u64");
            let code: i32 = args.next().expect("code").parse().expect("i32");
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            std::process::exit(code);
        }
        "trap-term" => {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
                println!("ready");
                term.recv().await;
                println!("got-term");
                std::process::exit(0);
            }
            #[cfg(not(unix))]
            {
                println!("ready");
                sleep_forever().await;
            }
        }
        "wait-for-ctrl-break" => {
            let marker = args.next().expect("marker path");
            #[cfg(windows)]
            wait_for_ctrl_break(marker).await;
            #[cfg(not(windows))]
            {
                let _ = marker;
                panic!("wait-for-ctrl-break is only available on Windows");
            }
        }
        "spawn-grandchild" => {
            let exe = std::env::current_exe().expect("current_exe");
            #[expect(
                clippy::zombie_processes,
                reason = "the helper intentionally leaves its grandchild running"
            )]
            let child = std::process::Command::new(exe)
                .arg("sleep-forever")
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("spawn grandchild");
            println!("grandchild-pid:{}", child.id());
            sleep_forever().await;
        }
        "gbk-stdout" => {
            // "中文" encoded as GBK, plus newline
            let bytes = [0xD6u8, 0xD0, 0xCE, 0xC4, b'\n'];
            std::io::stdout().write_all(&bytes).expect("write gbk");
        }
        "echo-stdin" => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).expect("read line");
            print!("echo:{line}");
        }
        other => {
            eprintln!("unknown mode: {other}");
            std::process::exit(2);
        }
    }
}

#[cfg(windows)]
async fn wait_for_ctrl_break(marker: String) {
    let mut signal = tokio::signal::windows::ctrl_break().expect("register Ctrl+Break handler");
    println!("ready");
    std::io::stdout().flush().expect("flush ready marker");
    signal.recv().await;
    std::fs::write(marker, b"ctrl-break-received").expect("write signal marker");
}

async fn sleep_forever() -> ! {
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}
