#!/bin/sh
# Runs inside the container as the non-root user "agent".
# Test 1: the bwrap line from PLAN §11 / the spike brief.
# (Tests 2-3 use the --proc line if it works, else the 1b line.)
# Test 2: the per-agent file overlay (PLAN §8): bind a shared "home" dir rw,
#         then bind a per-agent file over a file inside it.
# Test 3: the same overlay when the target file does NOT exist yet.
echo "uid=$(id -u) bwrap=$(bwrap --version)"

BW="bwrap --ro-bind /usr /usr --ro-bind /lib /lib --ro-bind /lib64 /lib64 --ro-bind /bin /bin --proc /proc --dev /dev --unshare-all --share-net"
# Test 1b: the same, but bind the container's /proc instead of mounting a new
# procfs (--proc fails when Docker masks paths under /proc).
BWB="bwrap --ro-bind /usr /usr --ro-bind /lib /lib --ro-bind /lib64 /lib64 --ro-bind /bin /bin --ro-bind /proc /proc --dev /dev --unshare-all --share-net"
if $BWB true 2>/tmp/err; then echo "TEST1b ro-bind /proc: OK"; OKB=1; else echo "TEST1b ro-bind /proc: FAIL: $(cat /tmp/err)"; fi
if $BW true 2>/tmp/err; then echo "TEST1 basic: OK"; else echo "TEST1 basic: FAIL: $(cat /tmp/err)"; [ -n "$OKB" ] || exit 1; BW=$BWB; fi

mkdir -p /tmp/shared/.copilot
echo "shared prompt" > /tmp/shared/.copilot/SYSTEM.md
echo "agent A prompt" > /tmp/promptA.md
echo "agent B prompt" > /tmp/promptB.md
A=$($BW --bind /tmp/shared /home/agent --ro-bind /tmp/promptA.md /home/agent/.copilot/SYSTEM.md \
      cat /home/agent/.copilot/SYSTEM.md 2>&1)
B=$($BW --bind /tmp/shared /home/agent --ro-bind /tmp/promptB.md /home/agent/.copilot/SYSTEM.md \
      cat /home/agent/.copilot/SYSTEM.md 2>&1)
W=$($BW --bind /tmp/shared /home/agent --ro-bind /tmp/promptA.md /home/agent/.copilot/SYSTEM.md \
      sh -c 'echo x > /home/agent/.copilot/SYSTEM.md' 2>&1; echo "rc=$?")
S=$(cat /tmp/shared/.copilot/SYSTEM.md)
echo "TEST2 overlay: A sees '$A'; B sees '$B'; write attempt: $W; shared file after: '$S'"

rm -f /tmp/shared/.copilot/APPEND.md
N=$($BW --bind /tmp/shared /home/agent --ro-bind /tmp/promptA.md /home/agent/.copilot/APPEND.md \
      cat /home/agent/.copilot/APPEND.md 2>&1; echo "rc=$?")
if [ -e /tmp/shared/.copilot/APPEND.md ]; then L="left a $(stat -c %s /tmp/shared/.copilot/APPEND.md)-byte placeholder in the shared dir"; else L="left nothing in the shared dir"; fi
echo "TEST3 overlay on missing file: '$N'; $L"
