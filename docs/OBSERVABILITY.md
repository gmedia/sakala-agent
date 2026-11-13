# Operational Observability

## Model export

Agent v1 tidak membuka endpoint metrics HTTP. Telemetry operasional dikirim keluar melalui heartbeat terautentikasi ke control plane, command event/log, dan structured log lokal. Ini menjaga node tidak memiliki control atau metrics surface publik yang dapat disalahgunakan.

Nama metric/logical counter yang stabil untuk dashboard atau collector:

- `commands_total`, `commands_failed_total`, `command_duration`;
- `active_commands`, `active_builds`, `build_duration`, `build_failures`;
- `active_workloads`, `health_failures`, `reconciliation_failures`;
- `api_request_failures`, `runtime_cleanup_total`.

Agent mengirim snapshot yang tersedia pada heartbeat (resource node dan workload). Counter historis dapat dikumpulkan control plane dari lifecycle event; Agent tidak menyimpan time-series lokal tanpa retention policy.

Untuk idle sleep, Agent membaca counter `caddy_http_requests_total` milik Caddy runtime lewat admin endpoint lokal di dalam container Caddy (`127.0.0.1:2019`, melalui `docker exec`). Yang disimpan hanya total request dan waktu request terakhir per host di memori, lalu dikirim sebagai `metadata.workloads.traffic` pada heartbeat. Agent tidak mengekspor, menyimpan ke disk, atau meneruskan metrics Caddy, dan tidak membaca IP atau path pengunjung. Lihat [Caddy Integration](CADDY_INTEGRATION.md#route-tidur-dan-counter-request).

## Security boundary

- Tidak ada listener HTTP metrics atau command pada Agent.
- Tidak ada endpoint yang dapat digunakan untuk menjalankan command runtime.
- Structured log dan outbound reporting tidak memuat credential repository, environment secret, atau payload deployment mentah.
- Export tambahan (Prometheus scrape atau OTLP) harus opt-in dengan kontrak, autentikasi, bind address, dan retention yang ditinjau sebelum diaktifkan.
