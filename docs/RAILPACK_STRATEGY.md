# Railpack Builder Strategy

Railpack menjadi fallback otomatis bila repository tidak memiliki root Dockerfile. Dockerfile user tetap diprioritaskan karena merupakan build contract eksplisit.

## Create Project Preview

`sakala-api` mengorkestrasi workflow create-project dengan membuat command `InspectProject`. Agent mengeksekusi preview pada immutable checkout:

```bash
railpack info --format json --out railpack-info.json APP_DIR
```

Agent menggabungkan raw JSON Railpack dengan scanner ringan Sakala untuk Dockerfile, `.env.example`, Compose, manifest, dan package manager. Hasil stabil dikirim kembali melalui `POST .../{command}/complete` pada field `result`.

Preview sengaja berhenti setelah `railpack info`. Ia tidak menjalankan `railpack prepare`, BuildKit, container, health check, maupun route activation sehingga langkah review repository tetap ringan.

## Sandbox

Railpack tidak pernah berjalan di host. Menganalisis repository berarti menjalankan kode repository itu: Railpack v0.23.0 menandai direktori aplikasi sebagai trusted bagi mise, dan mise merender versi tool dengan Tera `exec`, sehingga `node = "{{ exec(command='...') }}"` pada `mise.toml` langsung tereksekusi. Railpack juga mengikuti symlink saat membaca file versi, dan pesan error mise bisa memuat isinya. Inspection berjalan saat project dibuat, jadi serangan ini tidak butuh deploy.

Karena itu `railpack info` dan `railpack prepare` berjalan di container sekali pakai:

| Batas | Nilai |
| --- | --- |
| User | `65534:65534`, `--cap-drop ALL`, `no-new-privileges` |
| Filesystem | `--read-only`, tmpfs `/tmp` 256 MiB dengan `noexec`, dan satu mount host: source read-only di `/app` |
| Limit | 512 MiB tanpa swap, 1 vCPU, 256 PID, deadline 300 detik di dalam container |
| Secret | Tidak ada socket Docker, tidak ada token, dan nilai variabel project diganti placeholder kecuali `RAILPACK_*` |
| Network | Network sendiri tanpa komunikasi antar-container, terpisah dari `sakala-runtime` |

Sandbox tidak punya path host yang bisa ditulis, sehingga repository tidak bisa mengisi disk node atau meninggalkan file yang tidak bisa dihapus Agent. Hasilnya keluar lewat stdout dalam base64, sedangkan output Railpack lewat stderr dan masuk log. Agent hanya mengambil 1 MiB dan memperlakukan hasilnya sebagai data tidak tepercaya: JSON yang tidak terbaca ditolak, dan plan yang menyebut image `sakala/project-*` ditolak setelah di-parse, supaya escape JSON tidak bisa menyembunyikan namanya. Pemeriksaan ini perlu karena frontend Railpack mengabaikan `--pull`. Plan yang lolos ditulis Agent ke direktorinya sendiri sebelum build.

### Yang masih bisa dijangkau

Railpack butuh network untuk me-resolve versi. Dengan `--network none` ia menggantung sampai deadline. Smoke test `the_sandbox_cannot_reach_other_containers_but_can_reach_out` menguji jangkauan ini dengan perintah sandbox yang sebenarnya:

| Tujuan | Hasil | Keterangan |
| --- | --- | --- |
| Workload di `sakala-runtime` | Tertutup | Bridge berbeda. |
| Sandbox lain di network yang sama | Tertutup | `enable_icc=false`. |
| Internet | Terbuka | Dibutuhkan Railpack, dan bisa dipakai kode repository selama 300 detik. |
| Port host yang terbuka lewat gateway | Terbuka | Termasuk service host yang mendengarkan di `0.0.0.0`. |
| Alamat metadata cloud `169.254.169.254` | Belum terbukti | Laptop pengujian tidak punya layanan ini. Anggap terbuka di VM cloud. |
| Control plane | Terbuka bila terjangkau dari node | Tidak ada kredensial di dalam sandbox yang bisa dipakai untuk itu. |

Agent tidak bisa menutup jalur keluar tanpa firewall host. Operator yang ingin menutup metadata, control plane, atau jaringan internal perlu aturan di host, pada subnet network `sakala-railpack`. Pada Docker rootless, aturan itu berlaku di dalam network namespace RootlessKit, jadi perlu diuji di node.

### Asal image

Tidak ada image publik yang bisa ditarik. Resepnya ada di `docker/railpack-sandbox`, dan operator membangunnya sendiri di node atau di registry miliknya:

```bash
docker build --provenance=false -t sakala-railpack-sandbox docker/railpack-sandbox
docker image inspect --format '{{.Id}}' sakala-railpack-sandbox
# SAKALA_RAILPACK_IMAGE=sha256:<hasil perintah di atas>
```

`--provenance=false` membuat ID tetap sama saat dibangun ulang dari cache. Tanpa flag itu, Docker dengan containerd image store (29.9.2 di mesin pengujian) memberi ID baru pada setiap build dan menghapus ID lama, sehingga pin yang sudah ada di konfigurasi Agent tidak lagi menunjuk ke image apa pun.

Bila image di-push ke registry, pakai bentuk `registry/nama@sha256:<digest>`. Base image Alpine dipin dengan digest. Railpack dan mise dipin dengan versi dan SHA-256 yang cocok dengan `checksums.txt` dan `SHASUMS256.txt` rilis upstream. Paket `ca-certificates` dari `apk` tidak dipin, jadi build ulang bisa menghasilkan digest berbeda. Image ini hanya x86_64. Versi mise harus sama dengan `core/mise/version.txt` rilis Railpack, dan versi Railpack harus sama dengan `SAKALA_RAILPACK_FRONTEND`. CLI Railpack tidak perlu terpasang di host.

## Deployment

Agent menjalankan satu analysis final:

```bash
railpack prepare /app --plan-out /tmp/sakala-result.json
```

Image kemudian dibangun dan dimuat ke daemon lokal:

```bash
docker buildx build \
  --load \
  --progress plain \
  --no-cache \
  --pull \
  --build-arg cache-key=PROJECT_ID \
  --build-arg BUILDKIT_SYNTAX=ghcr.io/railwayapp/railpack-frontend:v0.23.0 \
  --file railpack-plan.json \
  --tag IMAGE_NAME \
  APP_DIR
```

`--load` wajib untuk builder lokal agar hasil build tersedia bagi `docker run`. Railpack di image sandbox dan frontend harus memakai versi kompatibel; operator dapat mengganti pin melalui `SAKALA_RAILPACK_FRONTEND`.

`root_directory` menjadi direktori `railpack prepare` sekaligus konteks build. `build_command` dan `start_command` diteruskan sebagai `RAILPACK_BUILD_CMD` dan `RAILPACK_START_CMD` lewat `--env NAME`, sehingga teksnya ada di environment process dan bukan di argumen. Keduanya menang atas variabel project dengan nama yang sama, dan keduanya berjalan di dalam build atau image, bukan di host.

Hasil `railpack info` adalah `BuildResult` Railpack, disimpan API apa adanya. Menurut source v0.23.0, field `detectedProviders` dan `metadata` memang ada, dan provider Node mengisi `nodeRuntime`, `nodeSPAFramework`, `nodePackageManager`, `nodeIsSPA`, serta `nodeUsesCorepack`. Next.js tidak termasuk `nodeSPAFramework`. Field `detectedProviders` sudah terlihat pada output sungguhan untuk `heroku/node-js-getting-started`, sedangkan field `metadata` lainnya baru dibaca dari source.

Variabel project sampai ke build sebagai secret: namanya lewat `railpack prepare --env`, nilainya lewat BuildKit `--secret`, dan `secrets-hash` membatalkan cache saat nilainya berubah. Jangan mengubah runtime environment menjadi build argument. Detailnya ada di [Docker Runtime](DOCKER_RUNTIME.md#variabel-saat-build). Smoke test `railpack_projects_keep_their_own_secrets` membuktikan jalur ini dengan build Railpack sungguhan.

## Builder Order

```txt
1. Root Dockerfile
2. Railpack auto builder
3. Manual build/start command (future)
```

Pipeline normal tidak menjalankan `info -> plan -> prepare`; `prepare` sudah menghasilkan info dan plan final untuk deployment.

## Product Flow

```txt
1. Pilih repository
   -> sakala-api membuat InspectProject
   -> agent checkout + railpack info

2. Lihat preview
   -> console menampilkan metadata hasil inspection

3. Deploy
   -> sakala-api membuat DeployProject
   -> agent checkout + Dockerfile build atau railpack prepare + BuildKit
```

Inspection adalah snapshot untuk UX, bukan build plan final. Deployment selalu checkout commit target dan melakukan analisis final agar hasil build sesuai source immutable yang benar-benar dijalankan.
