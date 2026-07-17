#!/usr/bin/env bash
# Render scripts/social-card.html to site/social-preview.png (1200x630 og:image).
set -euo pipefail
cd "$(dirname "$0")/.."
browser=$(command -v chromium || command -v google-chrome-stable || command -v google-chrome)
"$browser" --headless=new --disable-gpu --no-sandbox \
  --window-size=1200,630 --screenshot=site/social-preview.png \
  "file://$PWD/scripts/social-card.html" 2>/dev/null
echo "site/social-preview.png: $(du -h site/social-preview.png | cut -f1)"
