"""Run from repository root: python3 .../reset-policy.py IMAGE. Uses CID 210."""
import pathlib
import socket
import subprocess
import sys
import threading

root = pathlib.Path.cwd()
for mode in ("deny", "allow"):
    path = root / "reset-host.sock"
    path.unlink(missing_ok=True)
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(str(path))
    listener.listen()
    listener.settimeout(0.2)
    accepted = []
    done = threading.Event()

    def peer():
        while not done.is_set():
            try:
                connection, _ = listener.accept()
                accepted.append(1)
                connection.close()
            except socket.timeout:
                pass

    thread = threading.Thread(target=peer)
    thread.start()
    log = root / ("reset-" + mode + ".log")
    try:
        with log.open("wb") as output:
            result = subprocess.run(
                ["sudo", str(root / "target/release/examples/nitro_reset"),
                 sys.argv[1], str(path), mode], stdout=output,
                stderr=subprocess.STDOUT, timeout=20)
    finally:
        done.set()
        thread.join(timeout=2)
        listener.close()
        path.unlink(missing_ok=True)
    text = log.read_bytes()
    assert result.returncode == 0, (mode, result.returncode)
    assert b"RESET_FIXTURE_FAIL" not in text, mode
    assert b"CONTROLS_OK" in text and b"EXIT_CALLBACKS_OK" in text, mode
    if mode == "deny":
        assert b"PRE_RESET_DENIED" in text and b"POST_RESET_DENIED" in text
        assert not accepted, accepted
    else:
        assert b"PRE_RESET_UNEXPECTED_ALLOW" in text and b"POST_RESET_ALLOWED" in text
        assert len(accepted) == 2, accepted
    print("RESET_POLICY_OK mode=" + mode + " accepted=" + str(len(accepted)))
