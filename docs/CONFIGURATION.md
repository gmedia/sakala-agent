# Configuration

Agent memuat konfigurasi dari environment variable atau CLI flag yang sepadan. CLI flag mengoverride environment value.

| Variable | Default | Keterangan |
| --- | --- | --- |
| `SAKALA_AGENT_MODE` | `local` | `local` atau `connected`. |
| `SAKALA_AGENT_ID` | `local-agent-01` | Identitas node yang dikirim sebagai `X-Agent-Id`. |
| `SAKALA_AGENT_TOKEN` | tidak ada | Bearer token; wajib dan bukan `change-me` pada connected mode. |
| `SAKALA_API_URL` | `http://localhost:8000` | Base URL `sakala-api` control plane. |
| `SAKALA_POLL_INTERVAL_SECONDS` | `3` | Interval polling command, harus lebih besar dari nol. |
| `SAKALA_HEARTBEAT_INTERVAL_SECONDS` | `10` | Interval heartbeat, harus lebih besar dari nol. |
| `SAKALA_RUNTIME_NETWORK` | `sakala-runtime` | Nama network referensi dari `sakala-infra`. |
| `SAKALA_RUNTIME_DRIVER` | `noop` | `noop` atau executor opt-in `docker`. |
| `SAKALA_RUNTIME_WORKSPACE` | `/var/lib/sakala/builds` | Root workspace checkout/build sementara. |
| `SAKALA_CADDY_SITES_DIR` | `/var/lib/sakala/caddy/sites` | Folder route host yang dimount ke Caddy. |
| `SAKALA_CADDY_CONTAINER` | `sakala-caddy` | Container Caddy yang divalidasi dan direload. |
| `SAKALA_RAILPACK_FRONTEND` | `ghcr.io/railwayapp/railpack-frontend:v0.23.0` | BuildKit frontend; pin sesuai Railpack CLI. |
| `SAKALA_DEFAULT_CONTAINER_MEMORY_MB` | `256` | Fallback memory bila command tidak menentukan nilai. |
| `SAKALA_MAX_CONTAINER_MEMORY_MB` | `512` | Hard ceiling memory yang diizinkan node. |
| `SAKALA_DEFAULT_CONTAINER_CPU_MILLIS` | `500` | Fallback CPU; `500` berarti `0.5` vCPU. |
| `SAKALA_MAX_CONTAINER_CPU_MILLIS` | `1000` | Hard ceiling CPU; `1000` berarti `1` vCPU. |
| `SAKALA_DEFAULT_CONTAINER_PIDS_LIMIT` | `128` | Fallback jumlah process container. |
| `SAKALA_MAX_CONTAINER_PIDS_LIMIT` | `256` | Hard ceiling jumlah process pada node. |
| `SAKALA_LOG_LEVEL` | `info` | Filter `tracing-subscriber`, misalnya `debug` atau `sakala_agent=debug`. |

## Local Mode

```bash
cp examples/env/local.env.example .env
cargo run -p sakala-agent
```

File `.env` tidak dimuat otomatis oleh binary. Export variable menggunakan shell/environment runner yang dipilih, atau jalankan dengan default local mode tanpa file. Tidak ada control-plane request pada mode ini.

## Connected Mode

```bash
SAKALA_AGENT_MODE=connected \
SAKALA_AGENT_ID=node-01 \
SAKALA_AGENT_TOKEN='<issued-agent-token>' \
SAKALA_API_URL=http://localhost:8000 \
cargo run -p sakala-agent
```

Jangan menyimpan token issued pada repository atau command history yang dibagikan.

Executor Docker harus diaktifkan eksplisit dan hanya pada runtime node yang telah memiliki Git, Docker Buildx, Railpack, network `sakala-runtime`, dan Caddy contract dari `sakala-infra`. Jangan mengaktifkannya pada laptop contributor tanpa memahami operasi container yang akan dijalankan.

## Resource Policy Boundary

`sakala-api` menentukan policy resource berdasarkan project, workspace, atau plan lalu mengirim `resources` pada command deployment. Agent tidak memiliki logic plan. Konfigurasi `DEFAULT` hanya dipakai ketika field command kosong, sedangkan konfigurasi `MAX` melindungi kapasitas node. Request nol atau melebihi maximum ditolak secara eksplisit dan tidak di-clamp diam-diam.

`SAKALA_RUNTIME_NETWORK` tetap konfigurasi node karena merupakan detail topologi host, bukan product-level resource policy.
