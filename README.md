# ethercrab-etg8200-gateway

EtherCAT Mailbox Gateway (ETG.8200) for EtherCrab.
Enables TwinSAFE Loader/User to configure safety devices.

Requirements

1. Slaves must be in SAFE-OP or OP state for TwinSAFE operations
2. Do NOT use CoE/SDO in parallel with TwinSAFE tools (causes timeouts)
3. Open firewall for UDP port 34980
4. Default timeout is 10 seconds (TwinSAFE Loader default)
5. Maximum packet size: 1498 bytes (Ethernet MTU minus EtherCAT header)

Features

- UDP port 0x88A4 (34980)
- Single-worker serialization (prevents mailbox interference)
- Logs EtherCAT address mapping at startup
- Configurable bind address for safety
- Supports networks up to 512 slaves (increase at build-time if needed)

Error Behavior

Gateway returns NO REPLY (client sees timeout) on:
- Malformed packets
- Unknown station addresses  
- Timeouts
- Packets exceeding 1498 bytes
- Any mailbox communication error

This silent-drop behavior matches standard ETG.8200 implementations.

Usage

```bash
# Basic (binds to 0.0.0.0:34980)
ethercrab-mbg eth0

# Recommended: bind to specific interface for safety
GW_BIND_ADDR=192.168.1.100 ethercrab-mbg eth0

# Custom timeout and bind address
GW_TIMEOUT_MS=5000 GW_BIND_ADDR=192.168.1.100 ethercrab-mbg eth0
```

Security Note: By default the gateway binds to 0.0.0.0 (all interfaces). For production deployments, set GW_BIND_ADDR to your field-side network interface to prevent exposure to untrusted networks.

TwinSAFE Example

```powershell
# List devices
TwinSAFE_Loader.exe --gw <PLC_IP> --list safetyterminals.csv

# Program device
TwinSAFE_Loader.exe --gw <PLC_IP> --slave 1001 --rdpara params.xml
```

Build

```bash
cargo build --release
```

systemd

```ini
[Unit]
Description=EtherCAT Mailbox Gateway
After=network-online.target

[Service]
Environment=RUST_LOG=info
Environment=GW_TIMEOUT_MS=10000
Environment=GW_BIND_ADDR=0.0.0.0
ExecStart=/usr/local/bin/ethercrab-mbg eth0
Restart=on-failure
RestartSec=5
AmbientCapabilities=CAP_NET_RAW

[Install]
WantedBy=multi-user.target
```

Recovery

The gateway restarts automatically on failure (systemd Restart=on-failure).

Important: The station address mapping is built once at startup. Any bus topology changes (adding/removing/replacing slaves) require a gateway restart:

```bash
sudo systemctl restart ethercrab-mbg
```

Firewall

```bash
sudo ufw allow 34980/udp
```

Operational Gotchas

CRITICAL: Do NOT access CoE data and TwinSAFE Loader in parallel. Commands may collide and be aborted (Beckhoff docs, chapter 9).
- Do NOT run other mailbox traffic (CoE/SDO, FoE, etc.) during TwinSAFE operations - causes timeouts
- Slaves must be in SAFE-OP or OP state for TwinSAFE tools to work
- Single gateway instance only - multiple gateways on same bus will interfere
- TwinSAFE tools expect reachable slaves - gateway just provides UDP transport on port 34980
- Topology changes require restart - station address mapping is cached at startup
- One outstanding transaction at a time (serialized across clients) - UDP-only

License

MIT OR Apache-2.0