"""Real terminal smoke test, using only Python's POSIX standard library."""
import fcntl
import faulthandler
import json
import os
import pathlib
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


def main():
    faulthandler.dump_traceback_later(30)
    binary = str(pathlib.Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="dataexplorer-pty-") as directory:
        config = pathlib.Path(directory) / "config.toml"
        live_cluster = os.environ.get("DATAEXPLORER_TEST_CLUSTER")
        live_database = os.environ.get("DATAEXPLORER_TEST_DATABASE")
        lsp = os.environ.get("DATAEXPLORER_TEST_LSP")
        config.write_text(
            'version = 1\n[language_server]\ncommand = '
            + json.dumps(lsp or "dataexplorer-intentionally-missing-lsp")
            + '\nargs = ["--stdio"]\n'
        )
        argv = [binary, "--config", str(config)]
        if live_cluster:
            assert live_database, "explicit live database required"
            argv.extend(["-c", live_cluster, "-d", live_database])
            if os.environ.get("DATAEXPLORER_TEST_TENANT"):
                argv.extend(["--tenant", os.environ["DATAEXPLORER_TEST_TENANT"]])
        master, slave = pty.openpty()
        os.set_blocking(master, False)
        original = termios.tcgetattr(slave)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        process = subprocess.Popen(
            argv,
            stdin=slave, stdout=slave, stderr=slave,
            start_new_session=True,
            preexec_fn=lambda: fcntl.ioctl(slave, termios.TIOCSCTTY, 0),
            env={**os.environ, "TERM": "xterm-256color"},
        )
        captured = bytearray()

        def drain(seconds):
            until = time.monotonic() + seconds
            while time.monotonic() < until:
                if select.select([master], [], [], 0.05)[0]:
                    try:
                        captured.extend(os.read(master, 65536))
                    except BlockingIOError:
                        pass

        def send(data):
            os.write(master, data)
            drain(0.25)

        try:
            drain(0.7)
            assert process.poll() is None, "TUI exited unexpectedly"
            assert b"DataExplorer" in captured, "initial UI did not render"
            if not lsp:
                assert b"language service unavailable" in captured, "missing LSP not reported"
            query = b"print value='unicode \xf0\x9f\x98\x80'"
            if live_cluster:
                query = b"datatable(Label:string,Value:real)['negative',-1.25,'fraction',0.5] | render barchart with (xcolumn=Label,ycolumns=Value)"
            send(b"\x1b[200~" + query + b"\x1b[201~")
            if live_cluster:
                send(b"\x1b[17~")  # F6 metadata
                send(b"\x1b[15~")  # F5 run
                drain(3)
                drain(0.5)  # worker finishes local view
                send(b"\x1b[18~")  # F7 chart
                send(b"\x1b[18~")  # table
                send(b"\x10")  # Ctrl-P
                send(b"column 1 lt 0\r")
                drain(0.5)
                exported = pathlib.Path(directory) / "view.json"
                send(b"\x10")
                send(f'export json view "{exported}"\r'.encode())
                for _ in range(30):
                    drain(0.1)
                    if exported.exists():
                        break
                assert exported.exists(), re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b" ", captured).decode(errors="replace")
                result = json.loads(exported.read_text())
                assert result["rows"] == [["negative", -1.25]], result
                assert result["partial"] is False
                print("Live TUI query, metadata, chart toggle and filtered export passed", flush=True)
            send(b"\x1bOP")  # F1 help
            send(b"\x1b")  # close help
            send(b"\x1b[20~")  # F9 maximize
            send(b"\x1b[20~")  # restore
            send(b"\x1b[1;5C")  # Ctrl-Right divider resize
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 10, 40, 0, 0))
            os.kill(process.pid, signal.SIGWINCH)
            drain(0.2)
            plain = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b" ", captured)
            assert re.search(rb"Terminal\s+too\s+small", plain), "resize message not rendered"
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
            os.kill(process.pid, signal.SIGWINCH)
            drain(0.2)
            send(b"\x10")  # Ctrl-P command
            send(b"quit --force\r")
            process.wait(timeout=5)
            drain(0.1)
            assert process.returncode == 0, captured.decode(errors="replace")
            assert termios.tcgetattr(master) == original, "terminal modes were not restored"
            assert b"\x1b[?1049l" in captured, "alternate screen not restored"
            state = (pathlib.Path(directory) / "ui-state.toml").read_text()
            assert "cluster_width = 27" in state, state
            extra = ", live query/metadata/chart/filtered export" if live_cluster else ", offline editor"
            print("PTY smoke passed: paste, help, resize/maximize, atomic state, terminal cleanup" + extra)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            os.close(master)
            os.close(slave)
            faulthandler.cancel_dump_traceback_later()


if __name__ == "__main__":
    main()
