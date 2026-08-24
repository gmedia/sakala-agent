# Operational Observability

## Model export

Agent v1 tidak membuka endpoint metrics HTTP. Telemetry operasional dikirim keluar melalui heartbeat terautentikasi ke control plane, command event/log, dan structured log lokal. Ini menjaga node tidak memiliki control atau metrics surface publik yang dapat disalahgunakan.

Nama metric/logical counter yang stabil untuk dashboard atau collector:

- `commands_total`, `commands_failed_total`, `command_duration`;
- `active_commands`, `active_builds`, `build_duration`, `build_failures`;
- `active_workloads`, `health_failures`, `reconciliation_failures`;
- `api_request_failures`, `runtime_cleanup_total`.

Agent mengirim snapshot yang tersedia pada heartbeat (resource node dan workload). Counter historis dapat dikumpulkan control plane dari lifecycle event; Agent tidak menyimpan time-series lokal tanpa retention policy.

## Security boundary

- Tidak ada listener HTTP metrics atau command pada Agent.
- Tidak ada endpoint yang dapat digunakan untuk menjalankan command runtime.
- Structured log dan outbound reporting tidak memuat credential repository, environment secret, atau payload deployment mentah.
- Export tambahan (Prometheus scrape atau OTLP) harus opt-in dengan kontrak, autentikasi, bind address, dan retention yang ditinjau sebelum diaktifkan.
