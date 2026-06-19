#!/bin/bash
mkdir -p frames

for file in ./*; do
  case "$file" in
    *.mp4|*.mkv|*.mov|*.avi|*.webm)
      name=$(basename "$file")
      name="${name%.*}"
      ffmpeg -i "$file" -frames:v 1 "frames/${name}.png"
      ;;
  esac
done
