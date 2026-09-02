//! Small Mihomo-compatible controller simulator used by integration tests.

use std::{
    env,
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_yaml_ng::{Mapping, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

struct Behavior {
    ready_delay_ms: u64,
    never_ready: bool,
    reject_patch: bool,
    patch_delay_ms: u64,
    patch_no_effect: bool,
    reject_put: bool,
}

struct Context {
    ready: AtomicBool,
    behavior: Behavior,
    runtime: tokio::sync::Mutex<Mapping>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if has_arg(&args, "-v") {
        println!("Mihomo Meta v1.18.9 test");
        return;
    }

    let config_path = value_after(&args, "-f").expect("fake core needs -f <config>");
    let raw = std::fs::read_to_string(&config_path).expect("read fake core config");
    let document = parse_mapping(&raw);
    let behavior = behavior(&document);

    if has_arg(&args, "-t") {
        if let Some(path) = config_string(&document, "check-started-file")
            .or_else(|| behavior_string(&document, "check-started-file"))
        {
            std::fs::write(path, "started").expect("write check marker");
        }
        let delay = config_u64(&document, "check-delay-ms")
            .or_else(|| behavior_u64(&document, "check-delay-ms"))
            .unwrap_or_default();
        std::thread::sleep(Duration::from_millis(delay));
        if raw.contains("reject: true")
            || behavior_string(&document, "check-fail").is_some()
        {
            eprintln!("fake core rejected config");
            std::process::exit(1);
        }
        return;
    }

    for line in configured_lines(&document, "stdout-log", "stdout-lines") {
        println!("{line}");
    }
    for line in configured_lines(&document, "stderr-log", "stderr-lines") {
        eprintln!("{line}");
    }
    if raw.contains("finish: true") {
        return;
    }
    println!("fake core started");

    let ctx = Arc::new(Context {
        ready: AtomicBool::new(false),
        behavior,
        runtime: tokio::sync::Mutex::new(document.clone()),
    });
    if !ctx.behavior.never_ready {
        let ready = ctx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ready.behavior.ready_delay_ms)).await;
            ready.ready.store(true, Ordering::SeqCst);
        });
    }

    let mut served = false;
    if let Some(address) = config_string(&document, "external-controller") {
        serve_tcp(address, ctx.clone()).await;
        served = true;
    }
    served |= serve_local(&document, ctx.clone());
    if !served {
        eprintln!("fake-core: no controller configured");
    }

    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

async fn serve_tcp(address: String, ctx: Arc<Context>) {
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .expect("bind fake HTTP controller");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let ctx = ctx.clone();
            tokio::spawn(async move { serve_conn(stream, ctx).await });
        }
    });
}

fn serve_local(document: &Mapping, ctx: Arc<Context>) -> bool {
    let mut served = false;
    #[cfg(windows)]
    if let Some(path) = config_string(document, "external-controller-pipe") {
        served = true;
        let pipe_ctx = ctx.clone();
        tokio::spawn(async move {
            use tokio::net::windows::named_pipe::ServerOptions;
            let mut server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&path)
                .expect("create fake controller pipe");
            loop {
                if server.connect().await.is_err() {
                    continue;
                }
                let connection = server;
                server = ServerOptions::new()
                    .create(&path)
                    .expect("recreate fake controller pipe");
                let connection_ctx = pipe_ctx.clone();
                tokio::spawn(async move { serve_conn(connection, connection_ctx).await });
            }
        });
    }
    #[cfg(unix)]
    if let Some(path) = config_string(document, "external-controller-unix") {
        served = true;
        let unix_ctx = ctx.clone();
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind fake unix controller");
        listener.set_nonblocking(true).expect("set nonblocking");
        let listener = tokio::net::UnixListener::from_std(listener).expect("tokio unix listener");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let connection_ctx = unix_ctx.clone();
                tokio::spawn(async move { serve_conn(stream, connection_ctx).await });
            }
        });
    }
    let _ = document;
    served
}

async fn serve_conn<S>(mut stream: S, ctx: Arc<Context>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some((method, path, body)) = read_request(&mut stream).await else {
        return;
    };
    match (method.as_str(), path.as_str()) {
        ("GET", "/version") => {
            if ctx.ready.load(Ordering::SeqCst) {
                respond(&mut stream, 200, r#"{"meta":true,"version":"1.18.9"}"#).await;
            } else {
                respond(&mut stream, 503, r#"{"message":"starting"}"#).await;
            }
        }
        ("GET", "/configs") => {
            let runtime = ctx.runtime.lock().await;
            let body = runtime_config_json(&runtime);
            respond(&mut stream, 200, &body).await;
        }
        ("PATCH", "/configs") => {
            if ctx.behavior.reject_patch {
                respond(&mut stream, 500, r#"{"message":"patch rejected"}"#).await;
                return;
            }
            if ctx.behavior.patch_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(ctx.behavior.patch_delay_ms)).await;
            }
            if !ctx.behavior.patch_no_effect {
                let patch: serde_json::Value = serde_json::from_str(&body).expect("PATCH json");
                let Value::Mapping(patch) = serde_yaml_ng::to_value(patch).expect("PATCH yaml") else {
                    panic!("PATCH must be a mapping");
                };
                let mut runtime = ctx.runtime.lock().await;
                merge_mapping(&mut *runtime, &patch);
            }
            respond(&mut stream, 204, "").await;
        }
        ("PUT", "/configs") => {
            if ctx.behavior.reject_put {
                respond(&mut stream, 500, r#"{"message":"reload rejected"}"#).await;
                return;
            }
            let request: serde_json::Value = serde_json::from_str(&body).expect("PUT json");
            let desired = if let Some(path) = request.get("path").and_then(serde_json::Value::as_str)
                && !path.is_empty()
            {
                std::fs::read_to_string(path).expect("read PUT config path")
            } else {
                request
                    .get("payload")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            *ctx.runtime.lock().await = parse_mapping(&desired);
            respond(&mut stream, 204, "").await;
        }
        _ => respond(&mut stream, 404, r#"{"message":"not found"}"#).await,
    }
}

async fn read_request<S>(stream: &mut S) -> Option<(String, String, String)>
where
    S: AsyncRead + Unpin,
{
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 1024];
        let size = stream.read(&mut chunk).await.ok()?;
        if size == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..size]);
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = head.split("\r\n");
    let mut request = lines.next()?.split_whitespace();
    let method = request.next()?.to_owned();
    let path = request.next()?.split('?').next()?.to_owned();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or_default();
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0_u8; 1024];
        let size = stream.read(&mut chunk).await.ok()?;
        if size == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..size]);
    }
    Some((method, path, String::from_utf8_lossy(&body).into_owned()))
}

async fn respond<S>(stream: &mut S, status: u16, body: &str)
where
    S: AsyncWrite + Unpin,
{
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Not Found",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn runtime_config_json(document: &Mapping) -> String {
    let value = |key: &str| {
        document
            .get(Value::String(key.into()))
            .and_then(|value| serde_json::to_value(value).ok())
    };
    let integer = |key: &str| value(key).and_then(|value| value.as_i64()).unwrap_or_default();
    let boolean = |key: &str| value(key).and_then(|value| value.as_bool()).unwrap_or(false);
    let string = |key: &str, default: &str| {
        value(key)
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| default.to_owned())
    };
    serde_json::to_string(&serde_json::json!({
        "port": integer("port"),
        "socks-port": integer("socks-port"),
        "redir-port": integer("redir-port"),
        "tproxy-port": integer("tproxy-port"),
        "mixed-port": integer("mixed-port"),
        "tun": value("tun").unwrap_or_else(|| serde_json::json!({})),
        "tuic-server": value("tuic-server").unwrap_or_else(|| serde_json::json!({})),
        "ss-config": string("ss-config", ""),
        "vmess-config": string("vmess-config", ""),
        "tcptun-config": value("tcptun-config").unwrap_or(serde_json::Value::Null),
        "udptun-config": value("udptun-config").unwrap_or(serde_json::Value::Null),
        "authentication": serde_json::Value::Null,
        "skip-auth-prefixes": value("skip-auth-prefixes").unwrap_or(serde_json::Value::Null),
        "lan-allowed-ips": value("lan-allowed-ips").unwrap_or(serde_json::Value::Null),
        "lan-disallowed-ips": value("lan-disallowed-ips").unwrap_or(serde_json::Value::Null),
        "allow-lan": boolean("allow-lan"),
        "bind-address": string("bind-address", "*"),
        "inbound-tfo": false,
        "inbound-mptcp": false,
        "mode": string("mode", "rule"),
        "unified-delay": false,
        "log-level": string("log-level", "info"),
        "ipv6": boolean("ipv6"),
        "interface-name": string("interface-name", ""),
        "routing-mark": 0,
        "geox-url": {},
        "geo-auto-update": false,
        "geo-update-interval": 0,
        "geodata-mode": false,
        "geodata-loader": "",
        "geosite-matcher": "",
        "tcp-concurrent": boolean("tcp-concurrent"),
        "find-process-mode": string("find-process-mode", "off"),
        "sniffing": boolean("sniffing"),
        "global-ua": "",
        "etag-support": false,
        "keep-alive-idle": 0,
        "keep-alive-interval": 0,
        "disable-keep-alive": false
    }))
    .expect("serialize runtime config")
}

fn merge_mapping(target: &mut Mapping, patch: &Mapping) {
    for (key, value) in patch {
        if let (Some(target), Some(patch)) = (
            target.get_mut(key).and_then(Value::as_mapping_mut),
            value.as_mapping(),
        ) {
            merge_mapping(target, patch);
        } else {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn parse_mapping(raw: &str) -> Mapping {
    let value: Value = serde_yaml_ng::from_str(raw).expect("parse fake core config");
    value.as_mapping().cloned().expect("top-level config mapping")
}

fn behavior(document: &Mapping) -> Behavior {
    Behavior {
        ready_delay_ms: behavior_u64(document, "ready-delay-ms").unwrap_or_default(),
        never_ready: behavior_bool(document, "never-ready"),
        reject_patch: behavior_bool(document, "reject-patch"),
        patch_delay_ms: behavior_u64(document, "patch-delay-ms").unwrap_or_default(),
        patch_no_effect: behavior_bool(document, "patch-no-effect"),
        reject_put: behavior_bool(document, "reject-put"),
    }
}

fn behavior_mapping(document: &Mapping) -> Option<&Mapping> {
    document
        .get(Value::String("x-fake-core".into()))
        .and_then(Value::as_mapping)
}

fn behavior_string(document: &Mapping, key: &str) -> Option<String> {
    behavior_mapping(document)
        .and_then(|mapping| config_string(mapping, key))
}

fn behavior_u64(document: &Mapping, key: &str) -> Option<u64> {
    behavior_mapping(document)
        .and_then(|mapping| config_u64(mapping, key))
}

fn behavior_bool(document: &Mapping, key: &str) -> bool {
    behavior_mapping(document)
        .and_then(|mapping| mapping.get(Value::String(key.into())))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn configured_lines(document: &Mapping, single: &str, multiple: &str) -> Vec<String> {
    let mut lines = config_string(document, single).into_iter().collect::<Vec<_>>();
    if let Some(values) = behavior_mapping(document)
        .and_then(|mapping| mapping.get(Value::String(multiple.into())))
        .and_then(Value::as_sequence)
    {
        lines.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    lines
}

fn config_string(document: &Mapping, key: &str) -> Option<String> {
    document
        .get(Value::String(key.into()))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn config_u64(document: &Mapping, key: &str) -> Option<u64> {
    document
        .get(Value::String(key.into()))
        .and_then(Value::as_u64)
}

fn has_arg(args: &[OsString], expected: &str) -> bool {
    args.iter().any(|arg| arg.to_string_lossy() == expected)
}

fn value_after(args: &[OsString], flag: &str) -> Option<PathBuf> {
    args.windows(2)
        .find(|pair| pair[0].to_string_lossy() == flag)
        .map(|pair| PathBuf::from(&pair[1]))
}
