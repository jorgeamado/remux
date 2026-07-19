#!/usr/bin/env bash
# remux menu-bar control for SwiftBar (or xbar).
#
# Install: brew install swiftbar, then copy this file into your SwiftBar
# plugin folder (SwiftBar asks for one on first launch). It refreshes every
# minute and after every action.
#
# <xbar.title>remux</xbar.title>
# <xbar.version>v1</xbar.version>
# <xbar.author>remux</xbar.author>
# <xbar.desc>Start/stop the remux daemon and pair devices from the menu bar.</xbar.desc>
# <xbar.dependencies>remux</xbar.dependencies>
# <swiftbar.hideAbout>true</swiftbar.hideAbout>
# <swiftbar.hideRunInTerminal>true</swiftbar.hideRunInTerminal>

# SwiftBar runs with a minimal GUI PATH — check the standard prefixes too.
BIN=$(command -v remux 2>/dev/null || true)
[ -z "$BIN" ] && [ -x /opt/homebrew/bin/remux ] && BIN=/opt/homebrew/bin/remux
[ -z "$BIN" ] && [ -x /usr/local/bin/remux ] && BIN=/usr/local/bin/remux
if [ -z "$BIN" ]; then
  echo "| sfimage=exclamationmark.triangle"
  echo "---"
  echo "remux binary not found"
  exit 0
fi
STATUS=$("$BIN" service status --short 2>/dev/null || echo "unknown unknown")
RUNNING=${STATUS%% *}
ENABLED=${STATUS##* }

# Menu-bar glyph: the remux "r" (packaging/icons/bar.svg, 44px retina PNG).
# Idle: template image (macOS tints it for light/dark). Running: the violet
# variant, shown as-is.
GLYPH_IDLE="iVBORw0KGgoAAAANSUhEUgAAACwAAAAsCAYAAAAehFoBAAAAAXNSR0IArs4c6QAAADhlWElmTU0AKgAAAAgAAYdpAAQAAAABAAAAGgAAAAAAAqACAAQAAAABAAAALKADAAQAAAABAAAALAAAAADfpsWZAAAAtUlEQVRYCe3SvQnAIBQE4JefVQQ7wakcI0u5hyO4g5WN+YFUV1/AwL3umuP4dGmtnTlnc85ZjNFmv63WevTebYxhIYTZ99qeUjLvvZVSph/7DFzO+36x9B25/mnss1WDv34xCUsYBPQlAIQeJUwnhUIJAwg9SphOCoUSBhB6lDCdFAolDCD0KGE6KRRKGEDoUcJ0UiiUMIDQo4TppFAoYQChRwnTSaFQwgBCjxKmk0KhhAGEHi/0ghkE44V6aAAAAABJRU5ErkJggg=="
GLYPH_ACTIVE="iVBORw0KGgoAAAANSUhEUgAAACwAAAAsCAYAAAAehFoBAAAAAXNSR0IArs4c6QAAADhlWElmTU0AKgAAAAgAAYdpAAQAAAABAAAAGgAAAAAAAqACAAQAAAABAAAALKADAAQAAAABAAAALAAAAADfpsWZAAAAu0lEQVRYCe3SsQ3CMBRF0R+TBUIDdNmAITJnFmGKTEABJfQoiogTIapbv8KWnrvbPH0duZk/a75P3+jOKU59itJfuo1LvB453s+19Ft/97XX4RDHS9qPruPgJu+vCtr/keV/Wmj6YIDI08JyUgxaGCDytLCcFIMWBog8LSwnxaCFASJPC8tJMWhhgMjTwnJSDFoYIPK0sJwUgxYGiDwtLCfFoIUBIk8Ly0kxaGGAyNPCclIMWhgg8qxOeAN1VxapBTbHXwAAAABJRU5ErkJggg=="
if [ "$RUNNING" = running ]; then
  echo "| image=$GLYPH_ACTIVE"
else
  echo "| templateImage=$GLYPH_IDLE"
fi

echo "---"
echo "remux: ${RUNNING}, login: ${ENABLED} | sfimage=info.circle"
echo "---"
if [ "$RUNNING" = running ]; then
  echo "Stop | bash=\"$BIN\" param1=service param2=stop terminal=false refresh=true sfimage=stop.circle"
else
  echo "Start | bash=\"$BIN\" param1=service param2=start terminal=false refresh=true sfimage=play.circle"
fi
if [ "$ENABLED" = enabled ]; then
  echo "Turn off (also at login) | bash=\"$BIN\" param1=service param2=off terminal=false refresh=true sfimage=power"
else
  echo "Turn on (also at login) | bash=\"$BIN\" param1=service param2=on terminal=false refresh=true sfimage=power"
fi
echo "---"
# Pairing prints a QR — that needs a real terminal window.
echo "Pair a device… | bash=\"$BIN\" param1=pair terminal=true sfimage=qrcode"

# The config URL feeds a menu action; SwiftBar splits parameters on spaces,
# so only accept a strictly-shaped https?://host[:port] value — anything
# else could smuggle extra parameters (e.g. a bash=) into the line.
URL=$(sed -n 's/^url = "\(.*\)"/\1/p' "${XDG_CONFIG_HOME:-$HOME/.config}/remux/config.toml" 2>/dev/null | head -1)
if printf '%s' "$URL" | grep -Eq '^https?://([A-Za-z0-9.-]+|\[[0-9A-Fa-f:]+\])(:[0-9]+)?/?$'; then
  echo "Open in browser | href=$URL sfimage=safari"
fi
echo "---"
echo "Refresh | refresh=true sfimage=arrow.clockwise"
