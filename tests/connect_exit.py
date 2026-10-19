"""Exercise connect's terminal handoff on a private PTY and scratch board."""

import errno
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time


def check_exit(binary, key, trailing_fragment=False):
    ready, start = os.pipe()
    pid, master = pty.fork()
    if pid == 0:
        os.close(start)
        os.read(ready, 1)
        os.close(ready)
        os.execvp("bash", [
            "bash", "--noprofile", "--norc", "-c",
            '"$1" connect || exit; IFS= read -r reply; '
            'printf "SHELL_RECEIVED=<%s>\\n" "$reply"',
            "bash", binary,
        ])

    os.close(ready)
    try:
        original = termios.tcgetattr(master)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        os.write(start, b"1")
        transcript = bytearray()

        def read_until(marker):
            deadline = time.monotonic() + 8
            start = len(transcript)
            while marker not in transcript[start:]:
                remaining = deadline - time.monotonic()
                assert remaining > 0, (marker, bytes(transcript[-1000:]))
                if not select.select([master], [], [], remaining)[0]:
                    continue
                try:
                    chunk = os.read(master, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    chunk = b""
                assert chunk, (marker, bytes(transcript[-1000:]))
                transcript.extend(chunk)

        read_until(b"press any key")
        os.write(master, b" ")
        read_until(b"Main menu")
        os.write(master, key)
        if trailing_fragment:
            # Give a read-ahead thread a chance to start parsing an incomplete
            # sequence. Joining that reader used to hang despite poll's timeout.
            time.sleep(0.0001)
            os.write(master, b"\x1b[")
        # Type as soon as the shell screen returns, while BBS may still be
        # exiting. Its reader must already be stopped, not racing Bash for input.
        read_until(b"\x1b[?1049l")
        assert termios.tcgetattr(master) == original, "terminal modes not restored"
        os.write(master, b"first-character\n")
        # The deliberately late fragment may belong to Bash already. It must
        # not prevent exit or consume the line typed after terminal restoration.
        read_until(b"first-character>" if trailing_fragment else b"SHELL_RECEIVED=<first-character>")
        assert b"\x1b[?2004l" in transcript, "bracketed paste not disabled"
    finally:
        # Also cleans up a stuck BBS child on a failed regression.
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)
        os.close(master)
        os.close(start)


for exit_key in (b"\x1b", b"q"):
    check_exit(sys.argv[1], exit_key)
check_exit(sys.argv[1], b"q", trailing_fragment=True)
