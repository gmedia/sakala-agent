# Sakala Agent Architecture

Sakala Agent adalah data-plane executor untuk Sakala. Ia menjalankan operasi runtime pada node berdasarkan command yang dibuat oleh `sakala-api`. Foundation ini menyediakan boundary dan perilaku dummy yang dapat diuji sebelum akses host benar-benar dibangun.

## Boundary Sistem

```txt
User / Browser
    |
Sakala Console (presentation layer)
    |
    v
Sakala API (control plane / Agent API)
    | creates commands, stores state, authorizes requests
    ^
    | outbound polling and reports
Sakala Agent (runtime executor)
    |
Future: Docker / Railpack / Caddy on runtime node
```

- `sakala-console` menampilkan state dan mengirim intent user melalui API.
- `sakala-api` memegang policy, user access, metadata, dan command records.
- `sakala-agent` memproses command privileged pada node runtime.
- `sakala-infra` menjadi referensi local network dan edge routing.
- API dan console tidak mengakses Docker socket secara langsung.
- Agent tidak membuka HTTP server untuk menerima command dari control plane.

## Workspace Crates

### `apps/sakala-agent`

Binary process. Ia memuat config dari environment/CLI, menginisialisasi tracing JSON, memilih mode, membuat executor noop, menjalankan worker, dan menangani graceful shutdown.

### `sakala-agent-protocol`

Kontrak serialisasi yang harus sinkron dengan agent API pada `sakala-api`:

- command type dan status;
- heartbeat dan node info;
- deployment events;
- redacted deployment logs.

### `sakala-agent-core`

Orkestrasi application-independent:

- `ApiClient` outbound dengan bearer token dan `X-Agent-Id`;
- heartbeat worker;
- polling worker dan lifecycle handler;
- redaksi log;
- helper ID/retry.

### `sakala-agent-runtime`

Boundary operasi host melalui `RuntimeExecutor`. `NoopRuntimeExecutor` saat ini hanya menghasilkan event/log sukses tanpa perubahan host. Modul Docker, Caddy, Railpack, health, dan log collector masih skeleton.

## Polling Model

```txt
Sakala API creates Pending command
Agent GET /api/agent/v1/commands
Agent POST .../{id}/claim
Agent runs RuntimeExecutor
Agent POST .../{id}/events and .../{id}/logs
Agent POST .../{id}/complete or .../{id}/fail
Agent POST /api/agent/v1/heartbeat periodically
```

Model outbound polling menjaga node tidak perlu membuka endpoint command publik pada MVP.

## Modes

`local` adalah default aman: worker aktif, tetapi tidak menghubungi control plane. `connected` membuat request ke `sakala-api` dan mewajibkan token non-placeholder.

## Future Extraction Path

Implementasi berikutnya dapat menambahkan Docker executor di crate runtime dan contract test API di crate core. Crate baru tidak perlu dibuat kecuali boundary yang ada benar-benar tidak cukup. Integrasi Docker/Caddy/Railpack harus melalui review keamanan terlebih dahulu.
