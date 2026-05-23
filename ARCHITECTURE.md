# Sakala Agent Architecture

Sakala Agent adalah data-plane executor untuk Sakala. Ia menjalankan operasi runtime pada node berdasarkan command yang dibuat oleh dashboard. Foundation ini menyediakan boundary dan perilaku dummy yang dapat diuji sebelum akses host benar-benar dibangun.

## Boundary Sistem

```txt
User / Browser
    |
Sakala Dashboard (control plane)
    | creates commands, stores state, authorizes users
    v
Agent API
    ^
    | outbound polling and reports
Sakala Agent (runtime executor)
    |
Future: Docker / Railpack / Caddy on runtime node
```

- `sakala-dashboard` memegang policy, user access, metadata, dan command records.
- `sakala-agent` memproses command privileged pada node runtime.
- `sakala-infra` menjadi referensi local network dan edge routing.
- Dashboard tidak mengakses Docker socket secara langsung.
- Agent tidak membuka HTTP server untuk menerima command dashboard.

## Workspace Crates

### `apps/sakala-agent`

Binary process. Ia memuat config dari environment/CLI, menginisialisasi tracing JSON, memilih mode, membuat executor noop, menjalankan worker, dan menangani graceful shutdown.

### `sakala-agent-protocol`

Kontrak serialisasi yang nantinya harus sinkron dengan API dashboard:

- command type dan status;
- heartbeat dan node info;
- deployment events;
- redacted deployment logs.

### `sakala-agent-core`

Orkestrasi application-independent:

- `DashboardClient` outbound dengan bearer token dan `X-Agent-Id`;
- heartbeat worker;
- polling worker dan lifecycle handler;
- redaksi log;
- helper ID/retry.

### `sakala-agent-runtime`

Boundary operasi host melalui `RuntimeExecutor`. `NoopRuntimeExecutor` saat ini hanya menghasilkan event/log sukses tanpa perubahan host. Modul Docker, Caddy, Railpack, health, dan log collector masih skeleton.

## Polling Model

```txt
Dashboard creates Pending command
Agent GET /api/agent/v1/commands
Agent POST .../{id}/claim
Agent runs RuntimeExecutor
Agent POST .../{id}/events and .../{id}/logs
Agent POST .../{id}/complete or .../{id}/fail
Agent POST /api/agent/v1/heartbeat periodically
```

Model outbound polling menjaga node tidak perlu membuka endpoint command publik pada MVP.

## Modes

`local` adalah default aman: worker aktif, tetapi tidak menghubungi dashboard. `connected` membuat request API dan mewajibkan token non-placeholder.

## Future Extraction Path

Implementasi berikutnya dapat menambahkan Docker executor di crate runtime dan contract test API di crate core. Crate baru tidak perlu dibuat kecuali boundary yang ada benar-benar tidak cukup. Integrasi Docker/Caddy/Railpack harus melalui review keamanan terlebih dahulu.
