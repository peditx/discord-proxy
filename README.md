# Discord Proxy

یک اپ ویندوزی (ساخته‌شده با **Tauri 2**) که همهٔ ترافیک دیسکورد را از پراکسیِ انتخابیِ شما رد می‌کند.

## چطور کار می‌کند

```
Discord ──► 127.0.0.1:17999 (رلهٔ محلی) ──► پراکسی انتخابی شما ──► discord.com
```

1. یک پراکسی اضافه و فعال می‌کنید (HTTP/SOCKS4/SOCKS5).
2. **رلهٔ محلی** را روشن می‌کنید؛ این یک پراکسی HTTP روی `127.0.0.1` است.
3. دیسکورد را از داخل برنامه اجرا می‌کنید — با فلگ‌های `--proxy-server` — یا دکمهٔ
   **Windows system proxy** را روشن می‌کنید تا دیسکوردی که خودتان از Taskbar باز
   می‌کنید هم از همین رله رد شود.

رله لازم است چون خود Chromium نمی‌تواند نام کاربری/رمز عبور پراکسی را به‌صورت خودکار
بفرستد و اتصال SOCKS5 احراز هویت‌دار را هم پشتیبانی نمی‌کند. رله این کارها را انجام
می‌دهد و همه‌چیز را از یک دروازهٔ واحد رد می‌کند.

### انواع پراکسی

| نوع | وضعیت |
| --- | --- |
| HTTP / HTTPS | ✅ با پشتیبانی از `CONNECT` و احراز هویت Basic |
| SOCKS5 | ✅ شامل username/password و IPv6 |
| SOCKS4 / SOCKS4a | ✅ |

### حالت Strict

این گزینه به‌صورت فلگ به دیسکوردی داده می‌شود که **از داخل برنامه اجرا می‌کنید**؛ با
فعال بودن آن، هر بسته‌ای که نمی‌تواند از پراکسی رد شود دور ریخته می‌شود (نشت نمی‌کند).
صدای دیسکورد روی UDP است و معمولاً با فعال بودن این گزینه کار نمی‌کند؛ اگر می‌خواهید
صدا برقرار بماند، آن را خاموش کنید.

### نکات رفتاری

- پراکسی سیستم ویندوز موقع **بستن برنامه** یا **توقف رله** به حالت قبلی برمی‌گردد تا
  سیستم با یک پراکسی مرده گیر نکند.
- اگر سیستم یک اسکریپت PAC داشته باشد (`AutoConfigURL`)، ویندوز تنظیمات پراکسی ساده را
  نادیده می‌گیرد؛ برنامه در این حالت خطا می‌دهد.
- تنظیمات در `%APPDATA%\com.peditx.discordproxy\store.json` ذخیره می‌شود.

## بیلد

بیلد **فقط روی GitHub** انجام می‌شود (اکشن [.github/workflows/build.yml](.github/workflows/build.yml)).

```bash
git init
git add -A
git commit -m "Discord proxy manager

Co-Authored-By: Claude <noreply@anthropic.com>"
git branch -M main
git remote add origin git@github.com:<USER>/<REPO>.git
git push -u origin main
```

بعد از push، تب **Actions** اینستالر NSIS را می‌سازد و در **Artifacts** می‌گذارد.
اگر تگ بگذارید (`v0.1.0`) همان فایل روی **Release** هم قرار می‌گیرد.

بیلد محلی (در ویندوز، اختیاری):

```bash
cargo tauri build
```

## توسعه

```bash
cargo tauri dev
```

بدون مرحلهٔ build فرانت‌اند — HTML/CSS/JS خام در [src/](src/) است و از طریق
`withGlobalTauri` با `window.__TAURI__.core.invoke` با بک‌اند Rust حرف می‌زند.

### ساختار

```
src/                  UI (بدون بیلدر)
src-tauri/src/
  lib.rs              state + دستورات Tauri
  relay.rs            رلهٔ محلی (پذیرش CONNECT و HTTP معمولی)
  dial.rs             اتصال به upstream: HTTP / SOCKS5 / SOCKS4
  sys.rs              registry ویندوز، پیدا/اجرا کردن دیسکورد
  store.rs            مدل‌ها + ذخیره‌سازی JSON
```
