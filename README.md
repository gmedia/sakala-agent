# Sakala Agent

**Sakala Agent** adalah runtime executor untuk platform deployment **Sakala** dari **PT Media Sarana Data / GMEDIA**. Agent dirancang berjalan pada compute/runtime node, mengambil command dari dashboard, menjalankan operasi runtime, lalu melaporkan status, event, log, dan heartbeat.

Foundation ini sengaja aman dan minimal: executor aktif adalah `NoopRuntimeExecutor`. Belum ada akses Docker, Railpack, Caddy, atau Docker socket.

## Posisi Repository

```txt
sakala-dashboard  Laravel control plane dan agent API
sakala-agent      Rust runtime executor pada node
sakala-infra      Local runtime playground dengan Caddy/network referensi
```

Dashboard membuat command. Agent melakukan polling outbound ke dashboard. Agent tidak menyediakan command server inbound.

## Workspace Layout

```txt
apps/sakala-agent/              Binary, config, telemetry, shutdown
crates/sakala-agent-protocol/   Tipe payload dashboard-agent
crates/sakala-agent-core/       HTTP client, workers, lifecycle, redaction
crates/sakala-agent-runtime/    Trait executor dan executor noop
```

## Quickstart

```bash
cargo run -p sakala-agent
```

Default mode adalah `local`. Agent menulis heartbeat tick dan polling tick ke log JSON tanpa membuat request dashboard. Hentikan dengan `Ctrl+C`.

`.env.example` merupakan referensi variable. Binary membaca environment atau CLI flag dan sengaja tidak menambahkan loader file dotenv pada foundation ini.

## Mode

### Local

```dotenv
SAKALA_AGENT_MODE=local
SAKALA_AGENT_ID=local-agent-01
SAKALA_RUNTIME_NETWORK=sakala-runtime
```

Mode ini digunakan untuk pengembangan foundation dan aman dijalankan tanpa token nyata.

### Connected

```dotenv
SAKALA_AGENT_MODE=connected
SAKALA_AGENT_ID=node-01
SAKALA_AGENT_TOKEN=replace-with-real-agent-token
SAKALA_DASHBOARD_URL=http://localhost:8000
```

Connected mode mengaktifkan `DashboardClient`. Token placeholder `change-me` ditolak. Dashboard endpoint harus tersedia sebelum mode ini digunakan.

## Command Types

Protocol awal mendukung:

```txt
DeployProject RestartProject StopProject SleepProject
WakeProject HealthCheck RefreshRoute
```

Status awal:

```txt
Pending Claimed Running Succeeded Failed Cancelled Expired
```

## Configuration

Variable lengkap didokumentasikan di [docs/CONFIGURATION.md](docs/CONFIGURATION.md). Contoh payload command tersedia di `examples/commands/`.

## Development

```bash
make fmt
make lint
make test
make build
```

Lihat [ARCHITECTURE.md](ARCHITECTURE.md), [SECURITY.md](SECURITY.md), dan [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) sebelum menambah runtime implementation.

## License

Sakala Agent dilisensikan berdasarkan Apache License 2.0. Lihat [LICENSE](LICENSE) dan [NOTICE](NOTICE).
