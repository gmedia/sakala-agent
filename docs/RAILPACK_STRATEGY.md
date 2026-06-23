# Railpack Builder Strategy

Railpack menjadi fallback otomatis bila repository tidak memiliki root Dockerfile. Dockerfile user tetap diprioritaskan karena merupakan build contract eksplisit.

## Create Project Preview

`sakala-api` mengorkestrasi workflow create-project dengan membuat command `InspectProject`. Agent mengeksekusi preview pada immutable checkout:

```bash
railpack info --format json --out railpack-info.json APP_DIR
```

Agent menggabungkan raw JSON Railpack dengan scanner ringan Sakala untuk Dockerfile, `.env.example`, Compose, manifest, dan package manager. Hasil stabil dikirim kembali melalui `POST .../{command}/complete` pada field `result`.

Preview sengaja berhenti setelah `railpack info`. Ia tidak menjalankan `railpack prepare`, BuildKit, container, health check, maupun route activation sehingga langkah review repository tetap ringan.

## Deployment

Agent menjalankan satu analysis final:

```bash
railpack prepare APP_DIR \
  --plan-out railpack-plan.json \
  --info-out railpack-info.json
```

Image kemudian dibangun dan dimuat ke daemon lokal:

```bash
docker buildx build \
  --load \
  --progress plain \
  --build-arg BUILDKIT_SYNTAX=ghcr.io/railwayapp/railpack-frontend:v0.23.0 \
  --file railpack-plan.json \
  --tag IMAGE_NAME \
  APP_DIR
```

`--load` wajib untuk builder lokal agar hasil build tersedia bagi `docker run`. CLI Railpack dan frontend harus memakai versi kompatibel; operator dapat mengganti pin melalui `SAKALA_RAILPACK_FRONTEND`.

Build secret Railpack belum termasuk Phase 8. Jangan mengubah runtime environment menjadi build argument. Ketika build secrets ditambahkan, gunakan nama secret pada `railpack prepare` dan BuildKit `--secret`, serta hash untuk cache invalidation sesuai dokumentasi Railpack.

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
