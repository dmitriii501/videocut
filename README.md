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
  - Нижние 50% — трекинг лица стримера / вебка или динамичный геймплей (Subway Surfers, Minecraft Parkour, GTA V, ASMR).
  - Караоке-субтитры с подсветкой текущего слова в такт речи (локальный `whisper-rs`).

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

# Telegram-бот (приём ссылок в ЛС)
./target/release/streamreels --bot
```

---

## 🎨 Брендбук

Полное описание визуального стиля, Tone of Voice и позиционирования доступно в [BRANDBOOK.md](./BRANDBOOK.md).

---

## 📂 Структура проекта

```
rust/
  src/
    web/              # Веб-платформа: Axum, SQLite (chik.db), очередь задач, UI
      db.rs           # База данных: пользователи, сессии, задачи, рилсы
      models.rs       # DTO и модели данных
      queue.rs        # Фоновый воркер задач (OOM-защита)
      routes/         # Эндпоинты (auth, jobs, reels, billing, ws)
      static/         # Встроенный фронтенд в темном стиле Volt Neon
      ui.rs           # Встраивание UI в бинарник
    bot.rs            # Telegram-бот
    chat.rs           # Twitch IRC чат-монитор
    cli.rs            # Парсер аргументов командной строки
    config.rs         # Конфигурация и переменные окружения
    describe.rs       # Генерация заголовков, описаний и тегов
    fetch.rs          # Загрузка через yt-dlp
    highlights.rs     # Алгоритм отбора лучших моментов
    live.rs           # Кольцевой буфер и аудио-детект хайпа
    live_pipeline.rs  # Оркестрация Live-режима Twitch
    main.rs           # Точка входа
    media.rs          # Аудио- и видеоутилиты
    moment.rs         # Пост-фильтр речи (проверка живого звука)
    pipeline.rs       # Основной конвейер нарезки VOD
    render.rs         # Рендеринг 9:16 через ffmpeg-next
    review.rs         # Разметка качества моментов
    subtitles.rs      # Субтитры whisper-rs с караоке ASS
    telegram.rs       # Отправка в Telegram
    translate.rs      # Перевод субтитров
    utils.rs          # Хелперы
    webcam.rs         # Автодетект вебка-панели
BRANDBOOK.md          # Брендбук ЧИК
```
