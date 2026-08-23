# Runtime Hardening v1

Phase 9 menambah guard untuk pilot terbatas tanpa menganggap Docker host sebagai sandbox multi-tenant yang sempurna.

## Ownership Policy

```txt
sakala-api
  plan dan entitlement
  max project per user/workspace
  allowed active deployment
  admin stop/suspend decision
  requested resource profile

sakala-agent
  enforce command resource profile
  hard maximum dan capacity guard node
  process/build deadline
  detect actual orphaned container
  report requested/applied resource dan failure code
```

Agent tidak mengetahui plan berbayar, membership, atau kuota workspace. Jika user memperoleh resource lebih besar, API menghitung profile baru dan mengirim command redeploy. Replacement container memakai profile baru; route hanya berpindah setelah health check berhasil.

## Timeout dan Cancellation

- `SAKALA_BUILD_TIMEOUT_SECONDS` membatasi keseluruhan builder operation.
- `SAKALA_COMMAND_TIMEOUT_SECONDS` membatasi lifecycle runtime setelah command diklaim.
- Setiap subprocess juga memakai deadline command sebagai fallback.
- Child dijalankan sebagai process-group leader pada Unix. Timeout, reporting error, task cancellation, atau shutdown menjatuhkan seluruh process group.
- Shutdown lebih dulu membatalkan token semua command aktif, memberi waktu `SAKALA_SHUTDOWN_GRACE_SECONDS` agar candidate/workspace dibersihkan dan failure kritis dikirim, lalu hanya meng-abort task yang masih tersisa.
- Cancellation dikirim sebagai `runtime_cancelled`; timeout tetap memakai `runtime_timeout`. Build, container, health, routing, capacity, dan filesystem memiliki code terpisah.

Build timeout harus lebih pendek daripada command timeout agar runtime sempat menjalankan cleanup dan mengirim failure status.

## Continuous Workload Health

Saat deploy, readiness kandidat ditentukan oleh `DockerHealthChecker`: container
harus berstatus `running` atau `healthy` sebelum route boleh diaktifkan. Setelah
deploy selesai, worker health lokal menjalankan snapshot batch setiap
`SAKALA_RUNTIME_HEALTH_INTERVAL_SECONDS` (default 30 detik) terhadap container
aktif dengan label `dev.sakala.managed=true` dan identity project/deployment
yang valid.

Snapshot membedakan `healthy/running`, `health: starting`, dan `unhealthy`.
State terakhir disimpan hanya di memori Agent; perubahan saja yang dicatat pada
log terstruktur beserta container, project, deployment, status, dan alasan
aman. Pengecekan mendapat jitter deterministik dari agent ID, satu query batch
per interval, dan tidak memeriksa container stopped karena `docker ps` tanpa
`--all` digunakan. Tidak ada environment atau secret container yang dibaca.

Worker ini **tidak** melakukan restart otomatis dan belum mengirim event health
baru ke control plane. Recovery policy dan payload pelaporan health tetap
membutuhkan kontrak lifecycle `sakala-api`; hingga itu ada, Agent hanya
mendeteksi dan memberi sinyal operator lokal.

## Runtime Log Lifecycle

Setelah candidate sehat, route aktif, dan startup log terkirim, runtime memulai follower `docker logs --follow --tail 0`. Follower berjalan sebagai task milik `DockerContainerEngine`, sehingga bukan task yang terlepas dari lifecycle agent. Saat shutdown, seluruh task follower dibatalkan dan process group subprocess dihentikan sebelum binary keluar.

Follower memakai reporter deployment yang sama dan seluruh baris tetap melewati redaction core. Kegagalan reporting menghentikan follower dengan warning operator; retry/cursor belum diterapkan sampai API memiliki kontrak resume yang eksplisit.

## Capacity Guard

`SAKALA_MAX_ACTIVE_CONTAINERS` adalah hard guard node. Deployment project baru ditolak ketika jumlah container managed yang aktif mencapai batas. Redeploy project yang sudah aktif tetap diizinkan karena candidate akan menggantikan container sebelumnya.

Guard ini bukan aggregate scheduler dan belum menghitung total memory/CPU reservation. Penempatan lintas node tetap pekerjaan control plane setelah MVP.

Perubahan resource user dilakukan melalui redeploy dengan payload resource baru. Agent tidak memakai `docker update` untuk mengubah container aktif secara diam-diam karena deployment harus tetap memiliki desired state, applied state, image, health check, dan route cutover yang dapat diaudit.

## Orphan Detection

Saat startup, Docker executor memindai container dengan label `dev.sakala.managed=true`. Container ditandai orphan bila:

- identity label project/deployment hilang atau invalid; atau
- status container `Created`, `Exited`, atau `Dead`.

Phase 9 hanya melaporkan warning. Auto-removal sengaja tidak dilakukan karena agent belum memiliki desired-state snapshot dari API; menghapus berdasarkan observasi host saja berisiko menghapus workload yang masih dibutuhkan.

Workspace checkout berbeda: saat startup, agent dapat menghapus direktori terbengkalai yang namanya tepat UUID command Sakala dan umurnya melewati `SAKALA_WORKSPACE_GC_MAX_AGE_SECONDS`. GC tidak mengikuti symlink, tidak menyentuh nama lain, dan tidak dijalankan saat command aktif diproses.

Sebelum checkout deployment baru, Agent juga membaca free space filesystem
workspace dengan `df -Pk`. Bila jumlahnya di bawah
`SAKALA_MIN_WORKSPACE_FREE_MB`, command gagal dengan
`runtime_disk_pressure` sebelum Git atau builder dimulai. Guard ini tidak
menjalankan `docker image prune -a` atau cleanup host generik; keputusan
retention dan pembersihan artifact lama tetap harus Sakala-owned.

## Isolation Decision

Docker rootful pada node khusus masih dapat dipakai untuk pilot dengan repository terkontrol. Untuk workload publik tidak tepercaya, target berikutnya adalah isolated/rootless builder terpisah dari runtime daemon. Rootless mode sendiri mengurangi privilege, tetapi tidak menggantikan tenant isolation, network policy, cache isolation, dan secret boundary.
