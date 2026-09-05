# Chimera Proxy Server

> Pure Rust Xray-compatible proxy server with VLESS, REALITY, VMess, Trojan, XHTTP and Vision support.

Chimera Proxy Server is the Rust-native server core of the Chimera proxy ecosystem. It is designed to pair with [Chimera Client](https://github.com/MFSGA/Chimera_Client) for VLESS + REALITY deployments and to integrate with [Chimera](https://github.com/MFSGA/Chimera), the cross-platform desktop client.

## Chimera Ecosystem

```text
Chimera Desktop
       ↕
Chimera Client (Rust client core)
       ↕ VLESS / REALITY
Chimera Proxy Server (Rust server core)
       ↕
AChimera · Proxy Wiki
```

- [Chimera](https://github.com/MFSGA/Chimera) — desktop application
- [Chimera Client](https://github.com/MFSGA/Chimera_Client) — Rust client core with Clash / Mihomo compatibility
- [Chimera Proxy Server](https://github.com/MFSGA/Chimera_Service) — Rust server core
- [AChimera](https://github.com/MFSGA/AChimera) — Android client
- [Proxy Wiki](https://mfsga.github.io/Proxy_WIKI/) — documentation and deployment notes

## Protocol compatibility

| Capability | Status |
| --- | --- |
| VLESS | Stable |
| REALITY | Stable |
| VMess | Stable |
| Trojan | Stable |
| XHTTP | Stable |
| Vision | Stable |

## Development

This workspace provides the Windows service and IPC integration used by the Chimera desktop application. It is based on the service architecture of [`nyanpasu-service`](https://github.com/libnyanpasu/nyanpasu-service).

```bash
cargo check --workspace
cargo test --workspace
cargo fmt --all -- --check
```
