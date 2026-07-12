# Real-server validation environment

Record of the local Minecraft 1.21.4 test environment used to validate
MineRider against a real server. All server artifacts live under
`.test-servers/` (git-ignored); only this document, the scripts and
sanitized fixtures are committed.

## Environment

| Item | Value |
|---|---|
| OS | Windows (Git Bash + Windows PowerShell 5.1) |
| Rust | 1.97.0 (2026-07-07) |
| System Java | 1.8.0_491 — **too old for MC 1.21.4 (needs 21)** |
| Test JDK | Eclipse Temurin 21 JRE, downloaded from api.adoptium.net into `.test-servers/jdk-21/` (local only, no global install) |
| Server | Paper 1.21.4 (build recorded in `.test-servers/paper-1.21.4/server-metadata.json`) |
| Minecraft version | 1.21.4 |
| Protocol version | 769 |
| Validation date | 2026-07-12 |

## EULA

Running a Minecraft server requires accepting the Minecraft EULA
(<https://aka.ms/MinecraftEULA>). `scripts/test-server/setup-paper-1.21.4.ps1`
writes `eula.txt` with `eula=true` for the **local, offline-mode,
localhost-only** test server. Re-run setup if you do not accept the EULA
— the server will refuse to start without it.

## Network safety

- The server binds to `127.0.0.1` only (`server-ip=127.0.0.1`).
- RCON (used to drive the teleport scenario) also binds localhost only;
  its password lives in the git-ignored `.test-servers/` tree.
- No firewall changes, no port forwarding, offline mode, no real accounts.

## Layout

```
.test-servers/
  jdk-21/                 downloaded Temurin JRE (not committed)
  paper-1.21.4/           Paper server dir (not committed)
    server-metadata.json  build + sha256 + download timestamp
  traces/                 captured MineRider traces (not committed)
scripts/test-server/      management scripts (committed)
```

## Reproduction

```powershell
scripts\test-server\setup-paper-1.21.4.ps1
scripts\test-server\start-paper-1.21.4.ps1
scripts\test-server\validate-server-ready.ps1
scripts\test-server\run-minerider-validation.ps1 -DurationSeconds 60
scripts\test-server\stop-paper-1.21.4.ps1
```
