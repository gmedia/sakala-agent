# Caddy Integration

Agent menulis satu file `<project-id>.Caddyfile` pada `SAKALA_CADDY_SITES_DIR`. Domain hanya diterima bila cocok dengan `*.run.sakala.localhost`, `*.run.staging.sakala.dev`, atau `*.run.sakala.dev`; upstream selalu candidate container terkelola dan port tervalidasi.

```txt
write temporary file
-> atomic rename
-> docker exec caddy validate
-> docker exec caddy reload
-> restore route lama bila reload gagal
```

Contract `sakala-infra` memasang folder route read-only ke `/etc/caddy/sites`, mengimport `*.Caddyfile`, dan membatasi Admin API ke loopback container. Port admin tidak dipublish. Docker socket tidak pernah diberikan ke Caddy.

## Topologi yang Didukung Saat Ini

Adapter MVP hanya mendukung Caddy container dari `sakala-infra`. Caddy dan container aplikasi bergabung pada network `sakala-runtime`, sehingga upstream seperti `sakala-project-...:3000` dapat ditemukan melalui Docker DNS.

Instalasi Caddy host pada `/usr/bin/caddy` tidak otomatis kompatibel dengan route tersebut. Proses host tidak menggunakan DNS internal Docker dan tidak dapat mengandalkan nama container sebagai upstream. Karena itu, jangan hanya mengganti reload `docker exec` dengan `caddy reload`.

Untuk pilot yang memakai implementasi sekarang:

1. jalankan `sakala-caddy` dari `sakala-infra`;
2. mount `SAKALA_CADDY_SITES_DIR` ke `/etc/caddy/sites` secara read-only;
3. hubungkan Caddy dan workload ke `sakala-runtime`;
4. nonaktifkan Caddy host atau pastikan tidak bind pada port edge yang sama.

Caddy host juga dapat dipertahankan sebagai outer edge statis:

```caddyfile
http://*.run.sakala.dev {
	reverse_proxy 127.0.0.1:8080
}
```

Dalam pola ini, `sakala-caddy` tetap router dinamis yang dikelola agent dan tetap terhubung ke `sakala-runtime`. Bind port `8080` milik container Caddy ke loopback host. Caddy mempertahankan header `Host` secara default, sehingga router internal tetap dapat memilih file route project. Contoh HTTP di atas tidak menetapkan strategi TLS production; sertifikat dan hardening outer edge harus diputuskan operator secara terpisah.

## Arah Host Caddy

Host Caddy dapat ditambahkan sebagai adapter berbeda tanpa mengubah `RuntimeExecutor`, tetapi membutuhkan lebih dari transport reload. Container perlu mem-publish port unik ke loopback host, route harus mengarah ke loopback tersebut, dan agent perlu mengelola alokasi serta cleanup port secara persisten.

```txt
application container :3000
-> publish 127.0.0.1:31001
-> host Caddy reverse_proxy 127.0.0.1:31001
```

Reload host harus memakai permission terbatas. Agent tidak boleh memperoleh akses `sudo` arbitrer atau mengekspos Docker socket kepada Caddy.

TLS production, host-Caddy adapter, concurrent per-project lease, dan distributed route storage belum termasuk MVP.
