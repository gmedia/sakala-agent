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
| `SAKALA_COMMAND_TIMEOUT_SECONDS` | `900` | Hard maximum node untuk deadline seluruh lifecycle command setelah claim; payload deployment boleh meminta nilai lebih pendek. |
| `SAKALA_MAX_CONCURRENT_COMMANDS` | `4` | Batas global command aktif. Command pada project yang sama tetap diproses satu per satu; command yang belum mendapat slot tetap pending di control plane. |
| `SAKALA_SHUTDOWN_GRACE_SECONDS` | `30` | Waktu maksimum untuk command aktif menyelesaikan cancellation dan cleanup sebelum agent membatalkan task yang tersisa. |
| `SAKALA_MAX_CONCURRENT_BUILDS` | `1` | Batas build image Docker/Railpack aktif pada node agar build tidak saling menghabiskan CPU, memori, dan I/O. |
| `SAKALA_RUNTIME_NETWORK` | `sakala-runtime` | Nama network referensi dari `sakala-infra`. |
| `SAKALA_RUNTIME_DRIVER` | `noop` | `noop` atau executor opt-in `docker`. |
| `SAKALA_RUNTIME_WORKSPACE` | `/var/lib/sakala/builds` | Root workspace checkout/build sementara. |
| `SAKALA_WORKSPACE_GC_MAX_AGE_SECONDS` | `86400` | Umur minimum workspace UUID terbengkalai sebelum GC startup boleh menghapusnya. Nama non-UUID dan symlink tidak disentuh. |
| `SAKALA_RUNTIME_HEALTH_INTERVAL_SECONDS` | `30` | Interval pemeriksaan batch workload aktif berlabel Sakala. Agent mencatat perubahan state lokal saja sampai kontrak pelaporan health ke API disepakati. |
| `SAKALA_CADDY_SITES_DIR` | `/var/lib/sakala/caddy/sites` | Folder route host yang dimount ke Caddy. |
| `SAKALA_CADDY_CONTAINER` | `sakala-caddy` | Container Caddy yang divalidasi dan direload. |
| `SAKALA_RAILPACK_FRONTEND` | `ghcr.io/railwayapp/railpack-frontend:v0.23.0` | BuildKit frontend; pin sesuai Railpack CLI. |
| `SAKALA_BUILD_TIMEOUT_SECONDS` | `600` | Hard maximum node untuk fase build image; payload deployment boleh meminta nilai lebih pendek. |
| `SAKALA_START_TIMEOUT_SECONDS` | `120` | Hard maximum node untuk start container dan health readiness; harus lebih pendek dari command timeout. |
| `SAKALA_MAX_ACTIVE_CONTAINERS` | `20` | Guard kapasitas workload aktif pada node, bukan kuota plan/user. Redeploy project aktif tetap memperoleh replacement slot. |
| `SAKALA_DEFAULT_CONTAINER_MEMORY_MB` | `256` | Fallback memory bila command tidak menentukan nilai. |
| `SAKALA_MAX_CONTAINER_MEMORY_MB` | `512` | Hard ceiling memory yang diizinkan node. |
| `SAKALA_DEFAULT_CONTAINER_CPU_MILLIS` | `500` | Fallback CPU; `500` berarti `0.5` vCPU. |
| `SAKALA_MAX_CONTAINER_CPU_MILLIS` | `1000` | Hard ceiling CPU; `1000` berarti `1` vCPU. |
| `SAKALA_DEFAULT_CONTAINER_PIDS_LIMIT` | `128` | Fallback jumlah process container. |
| `SAKALA_MAX_CONTAINER_PIDS_LIMIT` | `256` | Hard ceiling jumlah process pada node. |
| `SAKALA_LOG_LEVEL` | `info` | Filter `tracing-subscriber`, misalnya `debug` atau `sakala_agent=debug`. |

`SAKALA_AGENT_MODE` dan `SAKALA_RUNTIME_DRIVER` tidak mewakili hal yang sama. Mode menentukan koneksi control plane, sedangkan driver menentukan efek command pada runtime node. Lihat [Operating Modes](OPERATING_MODES.md) untuk matriks lengkap.

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

Implementasi MVP mengharapkan Caddy berjalan sebagai container bernama `SAKALA_CADDY_CONTAINER` dan bergabung ke network `sakala-runtime`. Instalasi `/usr/bin/caddy` pada host tidak dipakai oleh adapter ini. Jangan menjalankan Caddy host dan Caddy container pada host port yang sama.

## Resource Policy Boundary

`sakala-api` menentukan policy resource berdasarkan project, workspace, atau plan lalu mengirim `resources` pada command deployment. Agent tidak memiliki logic plan. Konfigurasi `DEFAULT` hanya dipakai ketika field command kosong, sedangkan konfigurasi `MAX` melindungi kapasitas node. Request nol atau melebihi maximum ditolak secara eksplisit dan tidak di-clamp diam-diam.

Kuota jumlah project per user/workspace dan jumlah deployment yang diizinkan plan tetap milik `sakala-api`. `SAKALA_MAX_ACTIVE_CONTAINERS` hanya mencegah runtime node menerima workload baru setelah kapasitas lokal tercapai. Nilai ini tidak boleh dipakai untuk menyimpulkan entitlement user.

Perubahan memory/CPU/PID dilakukan dengan command redeploy yang membawa resource profile baru. Agent membuat candidate container baru, memverifikasinya, mengalihkan route, lalu membersihkan container sebelumnya. Phase 9 sengaja tidak memakai `docker update` agar requested state, applied state, image, dan deployment history tetap konsisten.

`SAKALA_RUNTIME_NETWORK` tetap konfigurasi node karena merupakan detail topologi host, bukan product-level resource policy.

## Runtime Timeout and Log Policy Boundary

`sakala-api` mengirim `timeouts` dan `log_bounds` yang telah di-resolve dari policy produk. Untuk `DeployProject`, agent memakai deadline payload pada fase build, start/health, dan lifecycle command. Konfigurasi `SAKALA_*_TIMEOUT_SECONDS` tetap merupakan hard maximum node: nilai nol atau nilai payload di atas maximum ditolak, bukan di-clamp diam-diam.

Log deployment selalu melewati redaksi sebelum dikirim. Agent kemudian memotong setiap message pada `log_bounds.max_line_length` dan berhenti mengirim setelah `log_bounds.max_total_bytes`; endpoint log agent saat ini mengirim satu baris per request, sehingga setiap request secara inheren berada di bawah `max_batch_lines` selama policy nilainya minimal satu. API tetap harus memvalidasi ulang seluruh batas ini sebagai trust boundary terakhir.
