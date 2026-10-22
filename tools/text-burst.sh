#!/bin/sh
# Image quality test scene: run in a terminal inside the session. Still for 8 s (time for a
# client to connect), then prints lines of text for SECONDS (default 2), then stays still.
# Comparing the client's last frame with the server's capture (GLIDEDESK_DEBUG_FRAME) shows
# the quality a still image is left with after motion.
#
#   tilix --maximize -e "sh tools/text-burst.sh 2"
sleep 8
end=$(($(date +%s) + ${1:-2})); i=0
while [ "$(date +%s)" -lt "$end" ]; do
  i=$((i + 1))
  printf '%05d lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod %08x\n' \
    "$i" $((i * 2654435761 % 4294967296))
  sleep 0.02
done
exec sleep 1000000
