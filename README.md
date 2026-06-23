# Sakala Agent

**Sakala Agent** adalah runtime executor untuk **Sakala**, project deployment open-source yang didukung **PT Media Sarana Data / GMEDIA** sebagai founding sponsor dan infrastructure supporter. Agent dirancang berjalan pada compute/runtime node, mengambil command dari `sakala-api`, menjalankan operasi runtime, lalu melaporkan status, event, log, dan heartbeat kembali ke API.

GMEDIA menyediakan dukungan awal berupa domain, infrastruktur, ruang eksperimen, dan dukungan teknis. Dukungan ini tidak mengubah prinsip Sakala sebagai project open-source dengan roadmap, dokumentasi, issue, dan kontribusi yang dikembangkan secara terbuka.

Default tetap aman: executor aktif adalah `NoopRuntimeExecutor`. Phase 8 menambahkan executor Docker opt-in untuk deployment repository GitHub publik melalui Dockerfile atau fallback Railpack, lalu mendaftarkan route Caddy. Executor nyata tidak aktif tanpa `SAKALA_RUNTIME_DRIVER=docker`.

## Posisi Repository

```txt
sakala-console  SvelteKit frontend pada app.sakala.dev
sakala-api      Laravel control plane dan agent API
sakala-agent    Rust runtime executor pada node
sakala-infra    Local runtime playground dengan Caddy/network referensi
```

Sakala API membuat command. Agent melakukan polling outbound ke API. Agent tidak menyediakan command server inbound.

## Workspace Layout

```txt
apps/sakala-agent/              Binary, config, telemetry, shutdown
crates/sakala-agent-protocol/   Tipe payload control-plane-agent
crates/sakala-agent-core/       HTTP client, workers, lifecycle, ports, redaction
crates/sakala-agent-runtime/    Implementasi noop, inspection, Docker, Railpack, Caddy
```

## Quickstart

```bash
cargo run -p sakala-agent
```

Default mode adalah `local`. Agent menulis heartbeat tick dan polling tick ke log JSON tanpa membuat request control plane. Hentikan dengan `Ctrl+C`.

`.env.example` merupakan referensi variable. Binary membaca environment atau CLI flag dan sengaja tidak menambahkan loader file dotenv pada foundation ini.

## Mode

Mode koneksi dan runtime driver merupakan dua konfigurasi terpisah. Ringkasan kombinasi `local/connected` dengan `noop/docker` tersedia di [docs/OPERATING_MODES.md](docs/OPERATING_MODES.md).

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
SAKALA_API_URL=http://localhost:8000
```

Connected mode mengaktifkan `ApiClient`. Token placeholder `change-me` ditolak. Endpoint `sakala-api` harus tersedia sebelum mode ini digunakan.

### Docker Runtime (Opt-in)

```dotenv
SAKALA_AGENT_MODE=connected
SAKALA_RUNTIME_DRIVER=docker
SAKALA_RUNTIME_NETWORK=sakala-runtime
SAKALA_RUNTIME_WORKSPACE=/var/lib/sakala/builds
SAKALA_CADDY_SITES_DIR=/absolute/path/to/sakala-infra/caddy/sites
SAKALA_CADDY_CONTAINER=sakala-caddy
SAKALA_RAILPACK_FRONTEND=ghcr.io/railwayapp/railpack-frontend:v0.23.0
```

Host harus menyediakan Git, Docker Buildx, Railpack CLI dengan versi yang sesuai frontend, Caddy container dari `sakala-infra`, serta permission runtime yang telah direview. Caddy binary yang terpasang langsung pada host belum digunakan oleh adapter MVP. Lihat [operating modes](docs/OPERATING_MODES.md), [Docker runtime](docs/DOCKER_RUNTIME.md), [runtime hardening](docs/RUNTIME_HARDENING.md), dan [strategi Railpack](docs/RAILPACK_STRATEGY.md).

Create-project preview memakai command `InspectProject`: agent menjalankan scanner ringan dan `railpack info`, kemudian mengembalikan metadata tanpa menjalankan `railpack prepare`, build image, container, atau routing. Deployment baru berjalan setelah command `DeployProject` dibuat.

## Command Types

Protocol awal mendukung:

```txt
InspectProject DeployProject RestartProject StopProject SleepProject
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
