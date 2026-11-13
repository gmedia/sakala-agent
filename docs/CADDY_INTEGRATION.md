# Caddy Integration

Agent menulis satu file `<project-id>.Caddyfile` pada `SAKALA_CADDY_SITES_DIR`. Domain hanya diterima bila berupa satu label di bawah suffix pada `SAKALA_PROJECT_DOMAIN_SUFFIXES` (default `run.sakala.localhost`, `run.staging.sakala.dev`, dan `run.sakala.dev`; lihat [Domain project](CONFIGURATION.md#domain-project)); upstream selalu candidate container terkelola dan port tervalidasi.

```txt
write temporary file
-> atomic rename
-> docker exec caddy validate
-> docker exec caddy reload
-> restore route lama bila reload gagal
```

Contract `sakala-infra` memasang folder route read-only ke `/etc/caddy/sites`, mengimport `*.Caddyfile`, dan membatasi Admin API ke loopback container. Port admin tidak dipublish. Docker socket tidak pernah diberikan ke Caddy.

## Topologi yang Didukung Saat Ini

Adapter MVP hanya mendukung Caddy container dari `sakala-infra`. Caddy dan container aplikasi bergabung pada network `sakala-runtime`, sehingga upstream seperti `sakala-app-<project8>-<deployment-id>:3000` dapat ditemukan melalui Docker DNS.

Nama container sekaligus menjadi hostname upstream, sehingga harus tetap berada di bawah batas 63 oktet satu label DNS. Karena itu nama memakai delapan karakter pertama project id ditambah deployment id penuh; dua UUID penuh diterima Docker sebagai nama container tetapi tidak dapat dijawab oleh DNS internalnya, dan route akan gagal resolve.

Instalasi Caddy host pada `/usr/bin/caddy` tidak otomatis kompatibel dengan route tersebut. Proses host tidak menggunakan DNS internal Docker dan tidak dapat mengandalkan nama container sebagai upstream. Karena itu, jangan hanya mengganti reload `docker exec` dengan `caddy reload`.

Untuk pilot yang memakai implementasi sekarang:

1. jalankan `sakala-caddy` dari `sakala-infra`;
2. mount `SAKALA_CADDY_SITES_DIR` ke `/etc/caddy/sites` secara read-only;
3. hubungkan Caddy dan workload ke `sakala-runtime`;
4. nonaktifkan Caddy host atau pastikan tidak bind pada port edge yang sama;
5. pakai image Caddy yang menyediakan `wget`, seperti `caddy:*-alpine`, karena probe readiness berjalan di dalam container ini. Lihat [Docker Runtime](DOCKER_RUNTIME.md#readiness).
6. untuk idle sleep, aktifkan counter request per host di blok global Caddyfile runtime. Lihat [Route tidur dan counter request](#route-tidur-dan-counter-request).

Caddy host juga dapat dipertahankan sebagai outer edge statis:

```caddyfile
http://*.run.sakala.dev {
	reverse_proxy 127.0.0.1:8080
}
```

Dalam pola ini, `sakala-caddy` tetap router dinamis yang dikelola agent dan tetap terhubung ke `sakala-runtime`. Bind port `8080` milik container Caddy ke loopback host. Caddy mempertahankan header `Host` secara default, sehingga router internal tetap dapat memilih file route project. Contoh HTTP di atas tidak menetapkan strategi TLS production; sertifikat dan hardening outer edge harus diputuskan operator secara terpisah.

## Route tidur dan counter request

Saat `SleepProject`, Agent tidak menghapus file route project, tetapi menggantinya dengan route tidur untuk domain yang sama. Baris pertamanya memakai penanda kepemilikan sendiri:

```caddyfile
# Managed by sakala-agent for project <project-id> deployment <deployment-id> (sleeping).
portfolio.run.sakala.dev:80 {
	header {
		Content-Type "text/html; charset=utf-8"
		Cache-Control "no-store"
		Retry-After "5"
		X-Robots-Tag "noindex"
	}
	respond <<SAKALA_SLEEPING_PAGE
		<!doctype html>
		...
		SAKALA_SLEEPING_PAGE 503
}
```

Halaman itu menjawab HTTP 503 dan berisi teks "Proyek sedang dinyalakan". Script kecil di dalamnya mengirim `POST <SAKALA_API_URL>/api/v1/public/projects/<slug>/wake` sekali per pemuatan dengan `mode: 'no-cors'`, tanpa membaca jawabannya, lalu memuat ulang halaman setelah 3 detik. Tanpa JavaScript, `<noscript>` memuat ulang setiap 5 detik tetapi tidak membangunkan project. `<slug>` adalah label domain sebelum suffix project, yang di `sakala-api` sama dengan slug project. Karena browser pengunjung yang memanggil endpoint itu, `SAKALA_API_URL` harus URL publik. Pada instalasi yang memakai URL internal, halaman tetap tampil tetapi tidak bisa membangunkan project.

Domain dan URL wake dibatasi ke karakter yang tidak punya arti di Caddyfile, heredoc, placeholder Caddy (`{...}`), HTML, maupun string JavaScript. Bila halaman tidak bisa dibangun, misalnya `SAKALA_API_URL` tidak valid, atau Caddy menolak route tidur, Agent menghapus route seperti sebelum fitur ini ada dan mencatat alasannya di log operator. Sleep tetap sukses.

Route tidur ikut aturan kepemilikan yang sama dengan route biasa:

- `WakeProject` menulis route biasa di atas route tidur lewat rename atomik yang sama, lalu validate dan reload. Bila Caddy menolaknya, route tidur dipulihkan.
- `StopProject` dan redeploy menghapus atau menimpa route tidur. Command untuk deployment lain tidak menyentuhnya.
- Discovery route basi menganggap route tidur masih dipakai selama container deployment-nya masih ada dalam status apa pun, biasanya `exited`. Route tidur yang containernya sudah hilang dianggap basi. Route biasa tetap butuh container yang berjalan.

Untuk mengukur kapan project terakhir dikunjungi, Caddyfile runtime perlu counter per host:

```caddyfile
{
	admin localhost:2019
	auto_https off
	metrics {
		per_host
	}
}
```

Setiap heartbeat, Agent menjalankan `docker exec <SAKALA_CADDY_CONTAINER> sh -c 'wget -qO- http://127.0.0.1:2019/metrics | grep ^caddy_http_requests_total'` (bentuk persisnya ada di `routing/docker_exec.rs`). Alamatnya `127.0.0.1`, bukan `localhost`, karena BusyBox `wget` mencoba `::1` lebih dulu, sedangkan admin Caddy hanya listen di IPv4 loopback. Hanya host yang punya blok sendiri yang mendapat label `host`; host lain masuk `_other`, termasuk header `Host` yang membawa port. Agent tidak pernah membuka atau meneruskan metrics ini. Detail pembacaannya ada di [Sakala Agent API](AGENT_API.md#traffic-workload).

## Arah Host Caddy

Host Caddy dapat ditambahkan sebagai adapter berbeda tanpa mengubah `RuntimeExecutor`, tetapi membutuhkan lebih dari transport reload. Container perlu mem-publish port unik ke loopback host, route harus mengarah ke loopback tersebut, dan agent perlu mengelola alokasi serta cleanup port secara persisten.

```txt
application container :3000
-> publish 127.0.0.1:31001
-> host Caddy reverse_proxy 127.0.0.1:31001
```

Reload host harus memakai permission terbatas. Agent tidak boleh memperoleh akses `sudo` arbitrer atau mengekspos Docker socket kepada Caddy.

TLS production, host-Caddy adapter, concurrent per-project lease, dan distributed route storage belum termasuk MVP.
