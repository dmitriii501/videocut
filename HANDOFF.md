# HANDOFF — передача проекта ЧИК / CHIK другому агенту

Документ для агента, который продолжит доработку. Описывает **как собрать,
запустить, проверить, пушить**, где что лежит и что осталось. Пользовательские
фичи — в [README.md](./README.md), визуальный стиль — в [BRANDBOOK.md](./BRANDBOOK.md).

Проект целиком на **Rust** (нативно: ffmpeg-next, whisper-rs, Axum, SQLite).
Корень репозитория — папка `streamreels/`, весь код в `rust/`.

---

## 1. Требования окружения

| Что | Зачем | Проверка |
|-----|-------|----------|
| Rust 1.82+ (edition 2021) | сборка | `cargo --version` |
| `ffmpeg`, `ffprobe` | декод/рендер/пробы | `ffmpeg -version` |
| `yt-dlp` | скачивание VOD/лайва | `yt-dlp --version` |
| Модель whisper (ggml) | субтитры | лежит в `cache/models/` |
| `deslop-please` | архитектурный линтер | `~/.local/bin/deslop-please` |

Модель `cache/models/ggml-large-v3-turbo.bin` (~1.6 ГБ) в git **не** входит —
качается отдельно (см. `config::whisper_model`). ONNX-детектор лица
`assets/face_detection_yunet_2023mar.onnx` тоже игнорируется git'ом
(`assets/*.onnx`), но на этой машине присутствует.

`deslop-please` не в PATH по умолчанию; собран из исходников в
`~/.local/bin/deslop-please` (crates.io через TLS тут флапает, поэтому ставили
`git clone` + `cargo build --release`).

---

## 2. Сборка

```bash
cd rust
cargo build --release
```

Бинарник: `rust/target/release/streamreels`.

Полная проверка перед коммитом:

```bash
cd rust
cargo build --release
cargo clippy --release -- -D warnings     # должно быть чисто
cargo test --release                        # 39 тестов, все зелёные
~/.local/bin/deslop-please                  # должно печатать "ok"
```

> ⚠️ TLS к crates.io в этом окружении нестабилен (`SSL connect error`).
> Если `cargo` падает на загрузке индекса — просто повтори команду; для новых
> крейтов помогает `git clone` и сборка из исходников.

---

## 3. Запуск

### Веб-платформа (основной режим)

```bash
cd rust
./target/release/streamreels --web --web-port 8080
# или в фоне:
nohup ./target/release/streamreels --web >> ../cache/logs/web.log 2>&1 &
```

Открыть `http://localhost:8080`. Слушает `0.0.0.0:8080` (`--web-bind`).

> В dev фронтенд `src/web/static/index.html` читается **с диска** при каждом
> запросе (см. `web/ui.rs`), поэтому правки HTML видны без пересборки. Правки
> Rust — требуют `cargo build`.

### CLI

```bash
# автонарезка 3 моментов из VOD
./target/release/streamreels "https://www.youtube.com/watch?v=XXXX" --auto 3 -o out
# по таймкодам
./target/release/streamreels "https://youtu.be/XXXX" -s 12:30-13:05 -s 45:10+30 -o out
# live-режим Twitch
./target/release/streamreels --live https://www.twitch.tv/<channel> --max-reels 3
# telegram-бот
./target/release/streamreels --bot
```

### Docker (весь бинарник)

Образ `streamreels:local` — не веб-обёртка: entrypoint это `streamreels`,
внутри ffmpeg, ffprobe, yt-dlp и шрифты. Режимы те же, что у хостового
бинарника (`--web`, URL/`--auto`, `--live`, `--bot`).

```bash
docker compose build
docker compose up                              # веб
docker compose run --rm streamreels --help     # любой другой режим — свои аргументы
```

Тома: `./cache` → `/data/cache` (модель, `chik.db`, геймплей), `./out` →
`/data/out`. `STREAMREELS_CACHE` / `STREAMREELS_OUTPUT` в контейнере указывают
сюда и перебивают `.env`, потому что `config::root()` запекается из
`CARGO_MANIFEST_DIR` сборки (`/src`) и на рантайме бесполезен. HTML
`rust/src/web/static` смонтирован: правки фронта без пересборки.

На этой машине plugin Compose может отсутствовать (`docker compose` → unknown
command). Тогда `docker build -t streamreels:local .` и `docker run` с теми же
флагами, томами и `-e STREAMREELS_CACHE=/data/cache -e STREAMREELS_OUTPUT=/data/out`.
Если хостовый `streamreels --web` уже занял 8080, задай `STREAMREELS_PORT`,
чтобы порт контейнера не столкнулся с ним.

Сборка образа тяжёлая (whisper.cpp + libav). В Dockerfile намеренно
`CARGO_BUILD_JOBS=1` и LTO выключен: на ~10 ГиБ RAM с занятой сессией thin LTO
уходит в OOM. TLS к crates.io тут флапает — при обрыве просто повтори build.

### Остановка / перезапуск сервера

**Только по PID**, никогда `pkill -f` (снесёт лишнее):

```bash
pgrep -f 'target/release/streamreels --web'      # найти PID
kill <PID>                                        # остановить
cd rust && nohup ./target/release/streamreels --web >> ../cache/logs/web.log 2>&1 &
```

---

## 4. Публичный доступ (Cloudflare Tunnel)

Пользователь выбрал **Cloudflare Tunnel** (без домена). Сейчас работает
**quick tunnel** (URL меняется при рестарте):

```bash
~/.local/bin/cloudflared tunnel --url http://localhost:8080 --no-autoupdate --protocol http2
```

- Лог: `cache/logs/cloudflared.log`. Текущий URL — в последней строке лога
  (`https://<random>.trycloudflare.com`).
- `--protocol http2` обязателен: QUIC на этой сети нестабилен.
- **Важно:** локальный резолвер (`192.168.31.1`) не резолвит `*.trycloudflare.com`
  (NXDOMAIN). Снаружи всё работает. Для локальной проверки обходить DNS:
  `curl --resolve <host>:443:<cloudflare-ip> https://<host>/`.
- Named tunnel (стабильный URL) требует **домен на Cloudflare** — пользователь
  пока не решил. Quick tunnel автостарта нет; при перезагрузке URL сменится.

---

## 5. Оплата (T-Bank / Тинькофф Касса)

Интеграция готова (`rust/src/web/tbank.rs`, `routes/billing.rs`). Пока
**креденшелы не заданы → демо-режим**: пополнение начисляет кредиты бесплатно.

Чтобы включить реальные платежи, задать env и перезапустить сервер:

```bash
export TBANK_TERMINAL_KEY=...      # TerminalKey магазина
export TBANK_PASSWORD=...          # пароль терминала (для подписи Token)
export STREAMREELS_PUBLIC_URL=https://<публичный-URL-туннеля>
# опционально: TBANK_API_URL (по умолчанию https://securepay.tinkoff.ru/v2)
```

Поток: `POST /api/billing/topup` → создаётся заказ → редирект на `payment_url` →
T-Bank шлёт webhook `POST /api/billing/tbank/notification` → подпись проверяется →
статус подтверждается через `GetState` → кредиты начисляются **только** в webhook
(идемпотентно). `GET /api/billing/order?order=...` статус читает, но не начисляет.
SPA опрашивает статус заказа при возврате (`?order=...`).

---

## 6. Переменные окружения

Все читаются в `rust/src/config.rs`. Часто нужные:

| Переменная | Смысл |
|------------|-------|
| `STREAMREELS_CACHE` / `STREAMREELS_OUTPUT` | куда класть cache / результаты |
| `STREAMREELS_PUBLIC_URL` | публичный URL (для T-Bank success/fail/notify) |
| `TBANK_TERMINAL_KEY`, `TBANK_PASSWORD`, `TBANK_API_URL` | эквайринг |
| `TYPESAFE_API_KEY` | авто-детект вебка-панели (face detection) |
| `WHISPER_MODEL`, `WHISPER_LANG`, `WHISPER_THREADS` | субтитры |
| `STREAMREELS_LLM_*`, `OPENAI_API_KEY` | генерация заголовков/описаний |
| `STREAMREELS_TG_TOKEN`, `STREAMREELS_TG_CHAT_ID`, `STREAMREELS_TG_ALLOWED` | Telegram |
| `REEL_W`, `REEL_H`, `REEL_FPS`, `REEL_CRF`, `REEL_PRESET` | параметры рендера (по умолчанию 1080×1920, 30 fps) |
| `MAX_SOURCE_HEIGHT` | потолок скачивания VOD, геймплея и эфира (по умолчанию 1080) |
| `STREAMREELS_BANNER*` | промо-баннер |
| `STREAMREELS_GAMEPLAY` | вкл/выкл геймплей-подложку (по умолчанию `true`) |
| `STREAMREELS_GAMEPLAY_CLIP` | локальный клип для всех задач (перебивает пресеты) |
| `STREAMREELS_GAMEPLAY_MIN_SECONDS` | мин. длина кеш-клипа (по умолчанию 15 с) |

Полный список — `grep -oE '"[A-Z_][A-Z0-9_]+"' rust/src/config.rs | sort -u`.

---

## 7. Структура кода (актуальная)

После рефактора под `deslop-please` код разложен по слоям/домену. Зависимости
ацикличны: **livestream → vod → text → infra**.

```
rust/src/
  main.rs            # mod-дерево + точка входа
  cli.rs             # разбор аргументов, диспетчеризация режимов
  config.rs          # все env-переменные и пути
  utils.rs           # хелперы (log, run, ffprobe, timecode, slugify…)
  http.rs            # HTTP-клиент (post_json/get_json/get_text)
  media.rs           # аудио-декод в 16 kHz mono f32 (ffmpeg-next)
  telegram.rs        # Telegram API
  bot.rs             # Telegram-бот
  text/              # текст-стадии (общие для обоих пайплайнов)
    subtitles.rs     #   whisper-rs + караоке ASS
    translate.rs     #   перевод субтитров
    moment.rs        #   судья качества момента
  vod/               # пайплайн записи (VOD)
    pipeline.rs      #   оркестрация нарезки (run_job)
    fetch.rs         #   загрузка через yt-dlp
    highlights.rs    #   выбор лучших моментов
    render.rs        #   рендер 9:16 (ffmpeg-next)
    describe.rs      #   заголовки/описания/хештеги
    gameplay.rs      #   геймплей-подложка: пресеты, кеш-клип, предрендер полосы
    webcam.rs        #   детект вебка-панели по лицу (YuNet + TypeSafe)
  livestream/        # пайплайн прямого эфира
    live_pipeline.rs #   оркестрация live (run_live)
    live.rs          #   кольцевой буфер + аудио-детект хайпа
    chat.rs          #   Twitch IRC чат-монитор
    review.rs        #   разметка качества (index.csv)
  web/               # веб-платформа (Axum + SQLite)
    mod.rs           #   запуск, AppState, вызывает routes::router()
    db.rs            #   SQLite: users, sessions, jobs, reels, orders
    models.rs        #   DTO
    queue.rs         #   фоновый воркер задач
    tbank.rs         #   эквайринг: подпись, Init, GetState, webhook
    ui.rs            #   отдача встроенного фронтенда
    routes/          #   эндпоинты; снаружи виден только router()
      mod.rs         #     сборка таблицы маршрутов
      auth.rs jobs.rs reels.rs billing.rs gameplay.rs ws.rs
    static/index.html#   SPA (один файл, читается с диска в dev)
```

`text`, `vod`, `livestream` — «глубокие» модули: наружу торчит только вход
(`run_job`, `run_live`, `Word`), внутренности приватны. Не расширяй их публичную
поверхность без нужды — `deslop-please` следит за этим.

---

## 8. Git: коммит и пуш

Репозиторий: `git@github.com:dmitriii501/videocut.git`, ветка `main`.

Стиль сообщений — Conventional Commits (`feat(scope): …`, `fix(scope): …`),
краткий заголовок + тело по пунктам. Пример из истории:

```
feat(billing): T-Bank acquiring, and restructure src into deep modules
```

Порядок:

```bash
cd /home/dmitro/Projects/streamreels
git status                       # посмотреть, что меняется
git diff                         # проверить дифф
git add -A                       # или точечно: git add <файлы>
git commit -m "feat(scope): короткое описание"
git push origin main
```

Перед коммитом:

1. Прогнать сборку/clippy/тесты/deslop (раздел 2).
2. Убедиться, что не коммитятся секреты — `.gitignore` уже исключает `.env*`,
   `*.pem`, `*.key`, `cache/`, `out*/`, `assets/*.onnx`, `target/`.
3. Не коммитить `Cargo.lock`-бэкапы и логи.

Правила для агента (из `~/.config/opencode/AGENTS.md`):

- **Никогда `pkill -f`** — только `kill <PID>`.
- `ffprobe` — с `-of csv=p=0`.
- Читать большие файлы точечно (grep/диапазоны), а не целиком.
- Массовые однотипные правки — одноразовым скриптом.
- Ошибки `deslop-please` чинить от первых принципов (слои, узкие интерфейсы),
  а не механическими переносами.

---

## 9. Что сделано (журнал)

Краткая хронология по последним итерациям — чтобы понимать, на чём стоит код.

- **Face-based вебка-детект** (`vod/webcam.rs`): TypeSafe спрашивает лицо
  стримера (VTuber → аватар), бокс расширяется до «голова+плечи»
  (`expand_face_box`), это и есть нижняя панель.
- **Ужесточение веб-доступа**: студия за авторизацией, эндпоинты рилсов/задач
  проверяют владельца, видео отдаётся по `?token=`; нормальные пароли
  (соль + 100k SHA-256), `POST /api/auth/logout`.
- **Оплата T-Bank (API v2)** (`web/tbank.rs`, `web/db.rs` `orders`): подпись,
  Init/GetState, идемпотентное зачисление по webhook, опрос статуса заказа в
  SPA. Без креденшелов — демо-режим (бесплатные кредиты).
- **Рефактор под `deslop-please`**: `src/` разложен на `vod/`, `livestream/`,
  `text/`, `web/`; глубокие модули, `routes::router()` как единственный вход.
- **Геймплей-подложка** (`vod/gameplay.rs`): пресеты/свой клип/`query:`,
  кеш клипов, предрендер полосы, вставка через `movie=`+`trim`; UI-селектор,
  `GET /api/gameplay`, поле `gameplay_preset`, CLI `--gameplay`.
- **Документация**: README и этот HANDOFF приведены к текущей структуре.
- **Docker**: `Dockerfile` + `docker-compose.yml` — образ всего бинарника
  (веб, CLI, live, бот), кэш и `out` томами с хоста.

---

## 10. Что осталось / на что обратить внимание

- **Named tunnel vs quick tunnel.** Quick tunnel меняет URL при рестарте и
  ломает `STREAMREELS_PUBLIC_URL` для T-Bank. Для продакшена нужен домен на
  Cloudflare + named tunnel (решение за пользователем).
- **Реальные платежи не включены** — нужны креденшелы T-Bank (раздел 5).
- **Вебка-детект** сейчас по лицу стримера (VTuber → лицо аватара). Для
  некоторых раскладок может требовать подстройки `expand_face_box` в
  `vod/webcam.rs`.
- **Геймплей-подложка** реализована (`vod/gameplay.rs`): пресеты `subway`,
  `minecraft`, `gta`, `asmr`, свой файл или `query:<поиск>`. Клип качается через
  `yt-dlp` и кешируется в `cache/gameplay/`; под нижнюю полосу предрендерится
  strip (от 60 с, по умолчанию 1080×960) и вставляется через `movie=`+`trim`. Выбор — в UI
  (`select#job-gameplay`, список из `GET /api/gameplay`), в CLI `--gameplay`,
  в API — поле `gameplay_preset`. **Авто-подбора** «под настроение стрима» нет —
  пользователь выбирает пресет вручную.
- **`deslop-please`** держим на нуле ошибок. Если добавляешь модуль — следи за
  `wide-directory` (>15 членов), `shallow-module` (>7 показанных функций при
  скрытых ≤ показанных) и `field-leak` (не отдавай поля наружу — делай методы).
- **README** обновлён под актуальную структуру `src/` и геймплей-подложку.
