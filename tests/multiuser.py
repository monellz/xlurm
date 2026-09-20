#!/usr/bin/env python3
"""Root-only integration test; no account creation and no accelerator use.

Build first: cargo build --bins
Run: sudo python3 tests/multiuser.py
"""
import json
import os
from pathlib import Path
import pwd
import shutil
import socket
import subprocess
import tempfile
import time


def main():
    if os.geteuid() != 0:
        raise SystemExit("Requires root: sudo python3 tests/multiuser.py")
    source = Path(__file__).resolve().parents[1] / "target" / "debug"
    users = [pwd.getpwnam(name) for name in ("nobody", "daemon")]
    assert users[0].pw_uid != users[1].pw_uid and all(u.pw_uid != 0 for u in users)
    with tempfile.TemporaryDirectory(prefix="xlurm-multiuser-") as directory:
        root = Path(directory)
        root.chmod(0o755)
        binaries = root / "bin"
        binaries.mkdir(mode=0o755)
        for name in ("xlurm", "xrun", "xbatch", "xqueue", "xcancel", "xinfo"):
            shutil.copyfile(source / name, binaries / name)
            (binaries / name).chmod(0o755)
        workdirs = {}
        for user in users:
            work = root / user.pw_name
            work.mkdir(mode=0o700)
            os.chown(work, user.pw_uid, user.pw_gid)
            workdirs[user.pw_name] = work
        env = {"PATH": "/usr/bin:/bin", "XLURM_HOME": str(root / "state"), "HOME": "/root"}

        def command(binary, args, user=None, check=True, **kwargs):
            def demote():
                os.initgroups(user.pw_name, user.pw_gid)
                os.setgid(user.pw_gid)
                os.setuid(user.pw_uid)

            return subprocess.run(
                [str(binaries / binary), *args],
                cwd=workdirs[user.pw_name] if user else root,
                env=dict(env, HOME=user.pw_dir, USER=user.pw_name) if user else env,
                preexec_fn=demote if user else None,
                capture_output=True, text=True, timeout=15, check=check, **kwargs,
            )

        def wait_job(job_id, state):
            deadline = time.monotonic() + 12
            while time.monotonic() < deadline:
                job = json.loads(command("xqueue", [str(job_id), "--json"]).stdout)
                if job["state"] == state:
                    return job
                time.sleep(0.05)
            raise AssertionError(f"job did not reach {state}: {job}")

        def submit(user, script):
            return int(command("xbatch", ["-g", "0", "--wrap", script], user).stdout)

        log = open(root / "daemon.log", "w")
        daemon = subprocess.Popen(
            [str(binaries / "xlurm"), "daemon", "--backend", "none", "--max-running", "1"],
            cwd=root, env=env, stdout=log, stderr=log,
        )
        active = []
        try:
            deadline = time.monotonic() + 10
            while command("xinfo", ["--json"], users[0], check=False).returncode:
                assert daemon.poll() is None, (root / "daemon.log").read_text()
                assert time.monotonic() < deadline, "daemon failed to start"
                time.sleep(0.05)
            alice, bob = users
            # Real/effective/saved IDs, supplementary groups, and output ownership.
            for user in users:
                result = command("xrun", ["-g", "0", "python3", "-c",
                    "import json,os,pathlib; pathlib.Path('owned').write_text('ok'); "
                    "print(json.dumps([os.getresuid(),os.getresgid(),sorted(os.getgroups())]))"], user)
                uid, gid, groups = json.loads(result.stdout)
                assert uid == [user.pw_uid] * 3 and gid == [user.pw_gid] * 3
                assert groups == sorted(set(os.getgrouplist(user.pw_name, user.pw_gid)))
                assert (workdirs[user.pw_name] / "owned").stat().st_uid == user.pw_uid

            first = submit(alice, "echo private-log; sleep 60")
            active.append(first)
            wait_job(first, "RUNNING")
            second = submit(bob, "echo bob-ran")
            active.append(second)
            assert wait_job(second, "PENDING")["owner"]["uid"] == bob.pw_uid
            summary = json.loads(command("xqueue", ["--json"], bob).stdout)
            assert {row["owner"]["uid"] for row in summary} == {alice.pw_uid, bob.pw_uid}
            assert all("spec" not in row and "env" not in row for row in summary)
            for args in ([str(first), "--json"], [str(first), "--log"], ["--cancel", str(first)]):
                denied = command("xqueue", args, bob, check=False)
                assert denied.returncode != 0 and "permission denied" in denied.stderr, denied
            denied = command("xcancel", [str(first)], bob, check=False)
            assert denied.returncode != 0 and "permission denied" in denied.stderr, denied
            assert command("xlurm", ["stop"], bob, check=False).returncode != 0
            command("xinfo", [], alice)  # A denied stop must not stop the daemon.
            # Private state is inaccessible even outside the CLI.
            for path in (root / "state" / "state.json", root / "state" / "jobs" / f"{first}.log"):
                def demote_bob():
                    os.initgroups(bob.pw_name, bob.pw_gid)
                    os.setgid(bob.pw_gid)
                    os.setuid(bob.pw_uid)
                result = subprocess.run(["/bin/cat", str(path)], preexec_fn=demote_bob,
                    capture_output=True, timeout=5)
                assert result.returncode != 0
            command("xcancel", [str(first)], alice)
            wait_job(first, "CANCELLED")
            wait_job(second, "COMPLETED")
            assert command("xqueue", [str(second), "--log"], bob).stdout == "bob-ran\n"
            # Root can manage every user's work.
            third = submit(bob, "sleep 60")
            active.append(third)
            wait_job(third, "RUNNING")
            command("xcancel", [str(third)])
            wait_job(third, "CANCELLED")
            # A raw client cannot inject a root owner into a submission.
            message = {"Submit": {"command": ["id"], "cwd": "/", "env": {}, "name": "forged",
                "count": 0, "kind": None, "time_limit": None, "script": None,
                "owner": {"uid": 0, "gid": 0, "name": "root"}}}
            with socket.socket(socket.AF_UNIX) as peer:
                peer.connect(str(root / "state" / "xlurm.sock"))
                peer.sendall(json.dumps(message).encode() + b"\n")
                assert "Error" in json.loads(peer.makefile("rb").readline())
            # Log cleanup is offline and restricted to the state owner.
            command("xlurm", ["stop"])
            daemon.wait(timeout=5)
            bob_log = root / "state" / "jobs" / f"{second}.log"
            assert bob_log.exists()
            assert command("xlurm", ["clean"], bob, check=False).returncode != 0
            assert bob_log.exists()
            command("xlurm", ["clean"])
            assert not bob_log.exists()
            print("PASS: real UID/GID/groups, shared queue, private spool/logs, owner cancellation, admin control and cleanup")
        finally:
            for job_id in active:
                command("xqueue", ["--cancel", str(job_id)], check=False)
            # Allow independent workers to finish before deleting their spool.
            deadline = time.monotonic() + 4
            while time.monotonic() < deadline:
                result = command("xqueue", ["--json"], check=False)
                if result.returncode or not json.loads(result.stdout):
                    break
                time.sleep(0.1)
            command("xlurm", ["stop"], check=False)
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()
            log.close()


if __name__ == "__main__":
    main()
