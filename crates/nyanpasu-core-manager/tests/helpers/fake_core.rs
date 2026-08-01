use std::{
    env,
    ffi::OsString,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
};

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if has_arg(&args, "-v") {
        println!("Mihomo Meta v1.18.9 test");
        return;
    }
    if has_arg(&args, "-t") {
        let config_path = value_after(&args, "-f").expect("config check needs -f <config>");
        let raw = std::fs::read_to_string(config_path).expect("read config check input");
        if raw.contains("reject: true") {
            eprintln!("fake core rejected config");
            std::process::exit(1);
        }
        return;
    }

    let config_path = value_after(&args, "-f").expect("fake core needs -f <config>");
    let raw = std::fs::read_to_string(config_path).expect("read fake core config");
    if raw.contains("finish: true") {
        return;
    }
    println!("fake core started");
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&raw).expect("parse fake core config");
    let document = value.as_mapping().expect("top-level config mapping");
    let address = document
        .get(serde_yaml_ng::Value::String("external-controller".into()))
        .and_then(serde_yaml_ng::Value::as_str)
        .expect("fake core requires external-controller");

    let listener = TcpListener::bind(address).expect("bind fake controller");
    for stream in listener.incoming() {
        let mut stream = stream.expect("accept fake controller connection");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);
        let body = r#"{"meta":true,"version":"1.18.9"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write fake controller response");
    }
}

fn has_arg(args: &[OsString], expected: &str) -> bool {
    args.iter().any(|arg| arg.to_string_lossy() == expected)
}

fn value_after(args: &[OsString], flag: &str) -> Option<PathBuf> {
    args.windows(2)
        .find(|pair| pair[0].to_string_lossy() == flag)
        .map(|pair| PathBuf::from(&pair[1]))
}
