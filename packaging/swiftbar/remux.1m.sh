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

BIN=$(command -v remux || echo /opt/homebrew/bin/remux)
STATUS=$("$BIN" service status --short 2>/dev/null || echo "unknown unknown")
RUNNING=${STATUS%% *}
ENABLED=${STATUS##* }

# Menu-bar glyph: template SF Symbol, tinted only while the daemon runs.
if [ "$RUNNING" = running ]; then
  echo "| sfimage=terminal.fill sfcolor=#8b5cf6"
else
  echo "| sfimage=terminal"
fi

echo "---"
echo "remux: ${RUNNING}, login: ${ENABLED} | sfimage=info.circle"
echo "---"
if [ "$RUNNING" = running ]; then
  echo "Stop | bash='$BIN' param1=service param2=stop terminal=false refresh=true sfimage=stop.circle"
else
  echo "Start | bash='$BIN' param1=service param2=start terminal=false refresh=true sfimage=play.circle"
fi
if [ "$ENABLED" = enabled ]; then
  echo "Turn off (also at login) | bash='$BIN' param1=service param2=off terminal=false refresh=true sfimage=power"
else
  echo "Turn on (also at login) | bash='$BIN' param1=service param2=on terminal=false refresh=true sfimage=power"
fi
echo "---"
# Pairing prints a QR — that needs a real terminal window.
echo "Pair a device… | bash='$BIN' param1=pair terminal=true sfimage=qrcode"

URL=$(sed -n 's/^url = "\(.*\)"/\1/p' "${XDG_CONFIG_HOME:-$HOME/.config}/remux/config.toml" 2>/dev/null)
if [ -n "$URL" ]; then
  echo "Open in browser | href=$URL sfimage=safari"
fi
echo "---"
echo "Refresh | refresh=true sfimage=arrow.clockwise"
