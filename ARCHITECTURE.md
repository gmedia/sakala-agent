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
Docker / Railpack / Caddy on runtime node (opt-in)
```

- `sakala-console` menampilkan state dan mengirim intent user melalui API.
- `sakala-api` memegang policy, user access, metadata, dan command records.
- `sakala-agent` memproses command privileged dan menegakkan resource request pada node runtime.
- `sakala-infra` menjadi referensi local network dan edge routing.
- API dan console tidak mengakses Docker socket secara langsung.
- Agent tidak membuka HTTP server untuk menerima command dari control plane.

## Workspace Crates

### `apps/sakala-agent`

Composition root dan binary process. Ia memuat `AppConfig` dari environment/CLI, memisahkan konfigurasi agent dari konfigurasi adapter Docker, menginisialisasi tracing JSON, memilih executor noop atau Docker, menjalankan worker, dan menangani graceful shutdown.

### `sakala-agent-protocol`

Kontrak serialisasi yang harus sinkron dengan agent API pada `sakala-api`:

- command type dan status;
- heartbeat dan node info;
- project inspection result;
- deployment events;
- redacted deployment logs.

Crate ini hanya berisi DTO/enum serde. Ia tidak mengetahui HTTP client, scheduler, Docker, Caddy, Railpack, filesystem, atau process host.

### `sakala-agent-core`

Orkestrasi application-independent:

- `ApiClient` outbound dengan bearer token dan `X-Agent-Id`;
- heartbeat worker;
- polling worker, command processor, dan dispatcher;
- `RuntimeExecutor` serta `RuntimeReporter` sebagai port antara orchestration dan adapter;
- redaksi log;
- helper ID/retry.

### `sakala-agent-runtime`

Implementasi adapter operasi host untuk port `RuntimeExecutor` milik core. `NoopRuntimeExecutor` tetap default. `DockerRuntimeExecutor` adalah implementasi opt-in dan hanya mengorkestrasi port runtime berikut:

```txt
WorkspaceManager -> checkout dan cleanup source workspace
ImageBuilder     -> Dockerfile/Railpack image build
ContainerEngine  -> container lifecycle dan startup logs
HealthChecker    -> readiness container
RouteManager     -> route activation dan rollback
```

Implementasi adapter disusun berdasarkan responsibility:

```txt
executor/   runtime use-case orchestration dan noop implementation
workspace/  Git checkout serta temporary workspace
builders/   Dockerfile, Railpack, dan BuildKit commands
inspections/ Railpack info dan scanner metadata Sakala
containers/ Docker container lifecycle, environment, limits, cleanup
routing/    Caddy file transaction dan reload transport
health/     Docker readiness inspection
logs/       process output reporting adapter
process/    structured subprocess runner
```

Core hanya memanggil port `RuntimeExecutor`; ia tidak memanggil Docker, Railpack, atau Caddy secara langsung. Runtime bergantung satu arah pada core untuk mengimplementasikan port tersebut. Binary menggabungkan keduanya. Caddy juga tidak diasumsikan selalu berjalan di Docker: implementasi saat ini memakai `CaddyFileRouteManager` dengan `DockerExecCaddyReloader`, sehingga transport reload dapat diganti tanpa mengubah executor atau route transaction.

Policy memory, CPU, dan process berasal dari `sakala-api`. `ContainerEngine` menyelesaikan request itu terhadap fallback dan hard maximum node sebelum checkout/build dimulai. Nilai berlebih ditolak, bukan di-clamp, dan completion result melaporkan requested serta applied resources. Network runtime tetap konfigurasi lokal agent karena merupakan detail topologi node.

```txt
protocol <- core <- runtime
              ^       ^
              |       |
          apps/sakala-agent
```

## Runtime Build Strategy

```txt
InspectProject -> checkout -> Sakala scanner + railpack info -> preview result

Dockerfile ditemukan/explicit -> docker buildx build --load
Dockerfile tidak ada          -> railpack prepare -> BuildKit frontend --load
```

Inspection dan deployment memakai full commit SHA. Preview tidak menjalankan `railpack prepare`, BuildKit, container, atau routing. Dockerfile user selalu diprioritaskan pada mode deploy `auto`; Railpack bukan pengganti konfigurasi eksplisit user. Manual build/start command belum termasuk Phase 8.

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

Mode tersebut independen dari runtime driver. `connected + noop` menguji kontrak API dan lifecycle command tanpa perubahan host, sedangkan `connected + docker` menjalankan runtime nyata. Lihat [docs/OPERATING_MODES.md](docs/OPERATING_MODES.md).

## Future Extraction Path

Implementasi baru ditambahkan di belakang port yang relevan, misalnya host-process Caddy reloader, remote image builder, atau container engine lain. Crate baru tidak perlu dibuat sebelum satu module memiliki lifecycle, dependency, atau ownership release yang benar-benar independen.

Ekstraksi berikutnya berfokus pada continuous log following, timeout/cancellation, stronger isolation, dan remote builder.
