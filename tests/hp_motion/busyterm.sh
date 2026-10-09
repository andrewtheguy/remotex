#!/bin/zsh
# Lines of random text the terminal's width, each in a colour of its own, as fast as they print: a busy
# picture for tests/hp_capture.sh to capture. Ctrl-C stops it.
while true; do
  cols=$(tput cols)
  color=$((RANDOM%256))
  printf '\033[38;5;%sm' "$color"
  LC_ALL=C tr -dc 'A-Za-z0-9@#$%&*+=:;!?' </dev/urandom | head -c "$((cols-2))"
  printf '\033[0m\n'
done