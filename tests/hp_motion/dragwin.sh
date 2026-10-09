#!/bin/zsh
# Slides iTerm2's front window 120 points right and back, about 30 steps a second: a window dragged,
# for tests/hp_capture.sh to capture. Ctrl-C stops it.
osascript <<"EOF"
tell application "iTerm2"
  set {l, t, r, b} to bounds of current window
  repeat
    repeat with d in {2, -2}
      repeat 60 times
        set l to l + d
        set r to r + d
        set bounds of current window to {l, t, r, b}
        delay 0.03
      end repeat
    end repeat
  end repeat
end tell
EOF
