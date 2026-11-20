# Build environment for the openrigcue AppImage, targeting Ubuntu 22.04's glibc 2.35.
#
#   podman build -t openrigcue-build -f packaging/build.Containerfile packaging
#   podman run --rm -v "$PWD":/src:Z -w /src openrigcue-build packaging/appimage/build-appimage.sh
#
# The only AppImage output is target/appimage/openrigcue-x86_64.AppImage.
FROM docker.io/library/ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl file build-essential pkg-config \
      libasound2-dev \
      libsdl2-2.0-0 \
      gcc-mingw-w64-x86-64 \
      desktop-file-utils squashfs-tools binutils perl avrdude unzip \
 && rm -rf /var/lib/apt/lists/*

# Rust toolchain with the Windows target for the bridge.
ARG RUST_VERSION=1.89.0
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain "$RUST_VERSION" \
      --target x86_64-pc-windows-gnu --component clippy \
 && chmod -R a+rwX /opt/cargo /opt/rustup

# appimagetool, unpacked so it runs without FUSE inside the container. Invoke its binary directly so the
# distro mksquashfs supplies xz support (the tool bundle may contain a zstd-only mksquashfs).
ARG APPIMAGETOOL_URL=https://github.com/AppImage/appimagetool/releases/download/1.9.0/appimagetool-x86_64.AppImage
RUN curl -sSfL -o /tmp/appimagetool "$APPIMAGETOOL_URL" \
 && chmod +x /tmp/appimagetool \
 && cd /opt && /tmp/appimagetool --appimage-extract >/dev/null \
 && mv /opt/squashfs-root /opt/appimagetool \
 && printf '#!/bin/sh\nexec /opt/appimagetool/usr/bin/appimagetool "$@"\n' > /usr/local/bin/appimagetool \
 && chmod +x /usr/local/bin/appimagetool \
 && rm /tmp/appimagetool
