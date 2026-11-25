# ЧИК / CHIK ✂️
> **Автопилот вирусных нарезок для стримеров.** Из стрима (Twitch/YouTube) — в вертикальный клип 9:16 (Shorts, Reels, TikTok) за пару минут без монтажки.

Написано полностью на **Rust** (нативно, без Python-рантайма, с защитой от OOM, ffmpeg-next, whisper-rs и встроенным веб-сервером Axum).

---

## ⚡ Возможности

- **Веб-платформа Self-Service**:
  - Регистрация, личный кабинет стримера, баланс кредитов.
  - Вставка ссылки на стрим (Twitch Live или YouTube VOD).
  - Интерактивный 9:16 видеоплеер со стримингом (HTTP Range).
  - Автоматическая генерация заголовков, описаний и хештегов из транскрипта.
  - Разблокировка и скачивание чистого Full HD клипа.
  - Тарифные пакеты (Старт, Стример-Пак, Про).
- **Детектор хайпа (Звук + Чат)**:
  - Аудио-энергия: адаптивный порог громкости (MAD) для точного отлова смеха, крика и ярких эмоций.
  - Twitch IRC чат: взвешенный подсчет реакций (`KEKW`, `LUL`, `Pog`, `monkaS`, капс).
- **Умный видеопайплайн 9:16**:
  - Верхние 50% — стрим с фокусом и размытым фоном.
  - Нижние 50% — трекинг лица стримера / вебка **или** динамичная геймплей-подложка (Subway Surfers, Minecraft Parkour, GTA V, ASMR; можно задать свой клип или YouTube-поиск).
  - Караоке-субтитры с подсветкой текущего слова в такт речи (локальный `whisper-rs`).
  - Анимированный промо-баннер в середине клипа.

---

## 🚀 Быстрый запуск

### 1. Требования

- **Rust** 1.82+
- `ffmpeg` и `ffprobe` в системе

### 2. Сборка

```bash
cd rust
cargo build --release
```

### 3. Запуск веб-платформы

```bash
# Запуск веб-сервера на порту 8080 (или без параметров)
./target/release/streamreels --web --web-port 8080
```

Открой в браузере: `http://localhost:8080`

---

## 💻 Консольные команды (CLI)

Бинарник также поддерживает работу напрямую из терминала:

```bash
# Автонарезка 3 лучших моментов из VOD
./target/release/streamreels "https://www.youtube.com/watch?v=XXXX" --auto 3 -o out

# По конкретным таймкодам
./target/release/streamreels "https://www.youtube.com/watch?v=XXXX" -s 12:30-13:05 -s 45:10+30 -o out

# Live-режим (следить за прямым эфиром Twitch и резать клипы на лету)
./target/release/streamreels --live https://www.twitch.tv/<channel> --max-reels 3

# Геймплей-подложка в нижней половине: пресет, свой файл или YouTube-поиск
./target/release/streamreels "https://youtu.be/XXXX" --auto 3 --gameplay subway
./target/release/streamreels "https://youtu.be/XXXX" --auto 3 --gameplay /path/clip.mp4
./target/release/streamreels "https://youtu.be/XXXX" --auto 3 --gameplay-query "retro arcade gameplay"
./target/release/streamreels "https://youtu.be/XXXX" --auto 3 --gameplay none   # только вебка

# Telegram-бот (приём ссылок в ЛС)
./target/release/streamreels --bot
```

Пресеты геймплея: `subway`, `minecraft`, `gta`, `asmr`, `none`. Клип скачивается
один раз через `yt-dlp` и кешируется в `cache/gameplay/`; под нижнюю полосу
предрендеривается полоса нужного размера. Отключить геймплей целиком можно
переменной `STREAMREELS_GAMEPLAY=false`, а задать локальный клип для всех
задач — `STREAMREELS_GAMEPLAY_CLIP=/path/clip.mp4`.

---

## 🎨 Брендбук

Полное описание визуального стиля, Tone of Voice и позиционирования доступно в [BRANDBOOK.md](./BRANDBOOK.md).

---

## 📂 Структура проекта

Исходники сгруппированы по смыслу (глубокие модули, зависимости без циклов:
`livestream → vod → text → infra`).

```
rust/
  src/
    vod/              # Конвейер нарезки VOD
      pipeline.rs     # Оркестрация: источник → сегменты → рендер → сайдкар
      fetch.rs        # Загрузка источника через yt-dlp
      highlights.rs   # Алгоритм отбора лучших моментов
      render.rs       # Рендеринг 9:16 через ffmpeg-next
      describe.rs     # Заголовки, описания, хештеги
      gameplay.rs     # Геймплей-подложка: пресеты, кеш, предрендер полосы
      webcam.rs       # Автодетект лица/вебка-панели
    livestream/       # Live-режим Twitch
      live_pipeline.rs# Оркестрация live-нарезки
      live.rs         # Кольцевой буфер и аудио-детект хайпа
      chat.rs         # Twitch IRC чат-монитор
      review.rs       # Разметка качества моментов
    text/             # Текст и субтитры
      subtitles.rs    # Субтитры whisper-rs с караоке ASS
      translate.rs    # Перевод субтитров
      moment.rs       # Пост-фильтр речи (проверка живого звука)
    web/              # Веб-платформа: Axum, SQLite (chik.db), очередь, UI
      db.rs           # База данных: пользователи, сессии, задачи, рилсы
      models.rs       # DTO и модели данных
      queue.rs        # Фоновый воркер задач (OOM-защита)
      tbank.rs        # Приём платежей T-Bank (API v2)
      routes/         # Эндпоинты (auth, jobs, reels, billing, gameplay, ws)
      static/         # Встроенный фронтенд в темном стиле Volt Neon
      ui.rs           # Встраивание UI в бинарник
    bot.rs            # Telegram-бот
    cli.rs            # Парсер аргументов командной строки
    config.rs         # Конфигурация и переменные окружения
    http.rs           # HTTP-хелперы (JSON POST)
    main.rs           # Точка входа
    media.rs          # Аудио- и видеоутилиты
    telegram.rs       # Отправка в Telegram
    utils.rs          # Хелперы (процессы, пути, логи)
BRANDBOOK.md          # Брендбук ЧИК
HANDOFF.md            # Состояние проекта: что сделано / что осталось
```
