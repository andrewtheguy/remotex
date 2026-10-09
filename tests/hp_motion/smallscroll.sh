#!/bin/zsh
# A short line four times a second, so the terminal scrolls gently: small movement for
# tests/hp_capture.sh to capture. Ctrl-C stops it.
while true; do date "+%H:%M:%S  small scroll"; sleep 0.25; done
