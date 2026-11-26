# ЧИК / streamreels — полный рантайм: веб, CLI-нарезка, live и Telegram-бот.
# Связка с libav собирается внутри образа. Бинарник с хоста сюда не копировать:
# на Arch он линкуется с ffmpeg 8 (libavcodec.so.63), в образе — ffmpeg Debian.
# Dockerfile без BuildKit: на этой машине нет buildx, `docker build` — legacy.

FROM rust:1-bookworm AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        pkg-config \
        clang \
        libclang-dev \
        cmake \
        nasm \
        libavcodec-dev \
        libavdevice-dev \
        libavfilter-dev \
        libavformat-dev \
        libavutil-dev \
        libswresample-dev \
        libswscale-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src/rust

# Один поток и без LTO: сборка whisper.cpp + rustc на машине с ~2 ГиБ свободной
# памяти иначе уходит в OOM. На рантайм это не влияет функционально.
ENV CARGO_BUILD_JOBS=1 \
    CMAKE_BUILD_PARALLEL_LEVEL=1 \
    CARGO_NET_RETRY=10 \
    CARGO_HTTP_TIMEOUT=120 \
    CARGO_PROFILE_RELEASE_LTO=off \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16

COPY rust/Cargo.toml rust/Cargo.lock ./
RUN mkdir -p src /out && printf 'fn main() {}\n' > src/main.rs \
    && cargo build --release --locked

COPY rust/src ./src

RUN touch src/main.rs \
    && cargo build --release --locked \
    && cp target/release/streamreels /out/streamreels

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        ffmpeg \
        python3 \
        fontconfig \
        fonts-dejavu-core \
        fonts-noto-core \
        libgomp1 \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /usr/share/fonts/noto \
    && bold="$(find /usr/share/fonts -name 'NotoSans-Bold.ttf' -print -quit)" \
    && reg="$(find /usr/share/fonts -name 'NotoSans-Regular.ttf' -print -quit)" \
    && if [ -n "$bold" ]; then ln -sf "$bold" /usr/share/fonts/noto/NotoSans-Bold.ttf; fi \
    && if [ -n "$reg" ]; then ln -sf "$reg" /usr/share/fonts/noto/NotoSans-Regular.ttf; fi \
    && fc-cache -f

ADD https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp /usr/local/bin/yt-dlp

RUN chmod 755 /usr/local/bin/yt-dlp \
    && mkdir -p /data/cache /data/out /app/rust/src/web/static \
    && chmod 1777 /data/cache /data/out

COPY --from=build /out/streamreels /usr/local/bin/streamreels
COPY rust/src/web/static/index.html /app/rust/src/web/static/index.html

WORKDIR /app

# config::root() запекается из CARGO_MANIFEST_DIR на этапе сборки (/src).
# Кэш и выхлоп всегда задаём явно, чтобы данные жили в томах, а не в /src.
ENV STREAMREELS_CACHE=/data/cache \
    STREAMREELS_OUTPUT=/data/out

EXPOSE 8080
VOLUME ["/data/cache", "/data/out"]

ENTRYPOINT ["streamreels"]
CMD ["--web", "--web-bind", "0.0.0.0", "--web-port", "8080"]
