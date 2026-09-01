---
name: ping-network-diagnostics
description: "Ping hosts and diagnose network connectivity on Windows"
metadata: {"nanobot":{"always":false}}
---

# Ping & Network Diagnostics (Windows)

Quick connectivity checks on Windows hosts.

## Ping a Host

```bash
ping -n 4 <IP_OR_HOST>
```

Example: `ping -n 4 10.0.0.254`

**Key flags:**
- `-n 4` — send 4 packets (Windows default, unlike Linux `-c`)
- `-t` — continuous ping (Ctrl+C to stop)

**Output reading:**
- `Reply from X.X.X.X: bytes=32 time=6ms TTL=64` — host is up
- `Request timed out` — host unreachable or firewall blocking ICMP
- 0% loss = all good; anything above 0% = packet loss

## PowerShell Alternative

```powershell
Test-Connection -ComputerName <IP> -Count 4
```

## Quick Port Check (if ping fails)

```bash
powershell -Command "Test-NetConnection -ComputerName <IP> -Port <PORT>"
```

## Common Gotchas

- Windows `ping` uses `-n`, Linux uses `-c` for count
- Some hosts block ICMP (ping) — use `Test-NetConnection` for TCP check instead
- TTL=64 typically means Linux; TTL=128 Windows; TTL=255 network device
