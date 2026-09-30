#!/bin/bash
# Setup script for the claude.ai cloud environment used by Sirius Flash cloud sessions.
# Paste it into the environment's "Setup script" field; it runs as root on Ubuntu 24.04
# before a new session and is snapshotted when it finishes in about five minutes.
# Same system packages as CI: Tauri/webkit for the GUI crate, QEMU + nasm + dosfstools
# for the boot-test harness. Rust and cargo are pre-installed on the VM.
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev libxdo-dev \
  qemu-system-x86 nasm dosfstools || true
