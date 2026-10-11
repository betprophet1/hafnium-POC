#!/usr/bin/env python3
"""Cut a running venue's journal: stop the core, embed a snapshot, archive what the cut removes, cut, start the core.

  journal_cut.py --to s3://BUCKET --environment NAME [--statefulset venue-core] [--data /var/lib/venue]
  journal_cut.py --to s3://BUCKET --environment NAME --dry-run     everything that can refuse, and nothing that stops
  journal_cut.py --self-test                                       check the orchestration against fakes

STATE.md §2 says a cut is three commands run by hand, and that it stays a deliberate act because a journal has no
lock: `embed-snapshot` and `cut-journal` write the file a serving node is appending to. This is those commands run
in the one order that is safe, by something that cannot forget the last step.

**What it guarantees, and what it does not.**

- Every check that can refuse runs *before* the core is stopped, so a refusal leaves the venue exactly as it was:
  a previous published pair to carry on from, and room on the disk for what `export-cut` stages.
- Once the core is stopped, the core is started again whatever happens — a failed step, an exception, a SIGTERM
  from a deleted Job. That is the reason for being one process and not a pod of init containers: init containers
  that fail leave the venue down with nothing left running to bring it back.
- It does not make the cut atomic. A step that fails after `embed-snapshot` leaves one extra snapshot in the
  journal, which is harmless; `cut-journal` itself refuses unless the segment is shipped and read back
  (`--require-shipped`), so a failure never discards history that is not archived.

**Where it cuts.** At the sequence of the latest *published pair*, not at the journal's end. After a cut the
journal starts at the snapshot, and the next scheduled pair must carry on from a previous view that is at least
that far on (`pair::take_after` refuses otherwise: the units in between would be in the archive, not the journal).
Cutting at the published pair's own sequence keeps that chain unbroken without anything having to publish first.
What follows the pair — at most a day of traffic — stays in the journal.

**Which `venue-node`.** The one on this image's PATH, which must write the snapshot encoding the *core* reads. The
journal's snapshot encoding changed without a version bump (see the migration notes), so this image is built on the
pinned old-format venue image and the core's own image must read the same format.
"""
import argparse
import json
import os
import shutil
import signal
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

SERVICEACCOUNT = Path("/var/run/secrets/kubernetes.io/serviceaccount")
GIB = 1 << 30


class Refused(Exception):
    """A check failed while the core was still running. The venue has not been touched."""


class Kube:
    """The three things this needs from the Kubernetes API, and nothing else: scale a StatefulSet, and ask whether a
    pod exists and whether it is ready. In-cluster only; the Job's service account is the credential."""

    def __init__(self, namespace):
        self.namespace = namespace
        self.base = f"https://{os.environ['KUBERNETES_SERVICE_HOST']}:{os.environ.get('KUBERNETES_SERVICE_PORT', '443')}"
        self.token = (SERVICEACCOUNT / "token").read_text().strip()
        self.context = ssl.create_default_context(cafile=str(SERVICEACCOUNT / "ca.crt"))

    def _call(self, method, path, body=None):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method)
        request.add_header("Authorization", f"Bearer {self.token}")
        if data is not None:
            request.add_header("Content-Type", "application/merge-patch+json")
        try:
            with urllib.request.urlopen(request, context=self.context, timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            raise

    def scale(self, statefulset, replicas):
        path = f"/apis/apps/v1/namespaces/{self.namespace}/statefulsets/{statefulset}/scale"
        self._call("PATCH", path, {"spec": {"replicas": replicas}})

    def pod(self, name):
        return self._call("GET", f"/api/v1/namespaces/{self.namespace}/pods/{name}")

    def pod_gone(self, name):
        return self.pod(name) is None

    def pod_ready(self, name):
        pod = self.pod(name)
        if not pod:
            return False
        return any(c.get("type") == "Ready" and c.get("status") == "True" for c in pod.get("status", {}).get("conditions", []))


def run(cmd):
    """Runs a command, showing it, and raises if it fails. Output goes straight to the Job's log."""
    print("+ " + " ".join(str(c) for c in cmd), flush=True)
    subprocess.run([str(c) for c in cmd], check=True)


def wait_for(condition, what, timeout, interval=2.0, sleep=time.sleep, clock=time.monotonic):
    deadline = clock() + timeout
    while True:
        # Checked before the deadline is, so a condition that is already true is never reported as a timeout.
        if condition():
            return
        if clock() >= deadline:
            raise TimeoutError(f"{what}: not true after {timeout:.0f}s")
        sleep(interval)


def sequence_of_pair(pair_dir):
    """The sequence a fetched pair was taken at: what its view half says it reached."""
    view = Path(pair_dir) / "view.json"
    if not view.is_file():
        return None
    return int(json.loads(view.read_text())["sequence"])


def preflight(config, runner, work):
    """Everything that can say no while the venue is still serving. Returns the sequence to cut at."""
    data = Path(config.data)
    journal, results = data / "journal.sbe", data / "results.sbe"
    for path in (journal, results):
        if not path.is_file():
            raise Refused(f"{path} does not exist: is the journal volume mounted at {data}?")

    previous = work / "previous"
    runner(["python3", config.tools / "snapshot_fetch.py", "--from", config.to, "--environment", config.environment,
            "--pair", previous])
    at = sequence_of_pair(previous)
    if at is None:
        raise Refused(
            "no pair has been published for this environment, so there is nothing to cut at without breaking the "
            "chain the scheduled snapshot job carries on from. Publish one first (venue-snapshot, STATE.md §4)"
        )

    # `export-cut` stages what the cut removes — nearly the whole journal and result log on a venue never cut. A
    # disk that fills here takes the node's other pods with it (kubelet evicts below 10% free), so the room is
    # checked now, with a margin, rather than found out half way.
    #
    # And once more for the biggest half: shipping verifies an upload by downloading it back into the temp
    # directory, so while the last file is verified the segment *and* a second copy of it are on the disk. The first
    # real run was evicted there because the temp directory was a small volume; counting it here is what keeps
    # that from being learned from a stopped venue again.
    sizes = journal.stat().st_size, results.stat().st_size
    need = sum(sizes) + max(sizes) + config.margin_gib * GIB
    free = shutil.disk_usage(work).free
    if free < need:
        raise Refused(
            f"not enough room to stage the segment: {free / GIB:.1f} GiB free, {need / GIB:.1f} GiB needed "
            f"(journal + results + a read-back copy of the larger + {config.margin_gib} GiB margin). Free some "
            "space, or grow the volume"
        )
    return at


def cut(config, kube, runner=run, sleep=time.sleep, clock=time.monotonic):
    """The whole procedure. Returns 0 on success; raises Refused before the venue is touched, and any other
    exception after it has been stopped *and started again*."""
    work = Path(config.work)
    work.mkdir(parents=True, exist_ok=True)
    at = preflight(config, runner, work)
    print(f"cutting at sequence {at}, the latest published pair", flush=True)
    if config.dry_run:
        print("--dry-run: every check passed; the core was not stopped", flush=True)
        return 0

    pod = f"{config.statefulset}-0"
    data = Path(config.data)
    journal, results, segment = data / "journal.sbe", data / "results.sbe", work / "segment"

    def stop_requested(signum, frame):
        # A deleted Job sends SIGTERM. Raising unwinds through `finally`, which is the whole point.
        raise SystemExit(f"terminated by signal {signum}")

    previous_handler = signal.signal(signal.SIGTERM, stop_requested)
    stopped = False
    try:
        print(f"stopping {config.statefulset}", flush=True)
        stopped = True  # set first: if the request half-succeeds, `finally` still starts it again
        kube.scale(config.statefulset, 0)
        wait_for(lambda: kube.pod_gone(pod), f"{pod} to stop", config.stop_timeout, sleep=sleep, clock=clock)

        runner(["venue-node", "embed-snapshot", journal, "--at", at])
        runner(["venue-node", "export-cut", journal, results, segment])
        runner(["python3", config.tools / "journal_ship.py", segment, "--to", config.to, "--environment",
                config.environment])
        runner(["venue-node", "cut-journal", journal, results, "--require-shipped", segment])
        print("cut", flush=True)
    finally:
        signal.signal(signal.SIGTERM, previous_handler)
        if stopped:
            print(f"starting {config.statefulset}", flush=True)
            kube.scale(config.statefulset, 1)
            try:
                wait_for(lambda: kube.pod_ready(pod), f"{pod} to be ready", config.start_timeout, sleep=sleep,
                         clock=clock)
                print(f"{pod} is ready", flush=True)
            except TimeoutError as e:
                # Do not hide the original failure behind this one, and do not call the venue fine.
                print(f"WARNING: {e}. The core has been asked to start; check it.", file=sys.stderr, flush=True)
    return 0


class Config:
    def __init__(self, **kw):
        self.__dict__.update(kw)


def self_test():
    failures = []

    class FakeKube:
        def __init__(self):
            self.calls = []
            self.up = True
            self.ready_after_start = True

        def scale(self, name, replicas):
            self.calls.append(("scale", replicas))
            self.up = replicas > 0

        def pod_gone(self, name):
            return not self.up

        def pod_ready(self, name):
            return self.up and self.ready_after_start

    def config(directory, **over):
        data = Path(directory) / "data"
        data.mkdir(exist_ok=True)
        (data / "journal.sbe").write_bytes(b"j" * 100)
        (data / "results.sbe").write_bytes(b"r" * 100)
        base = dict(data=str(data), work=str(Path(directory) / "work"), to="s3://bucket", environment="prod",
                    statefulset="venue-core", margin_gib=0, stop_timeout=1, start_timeout=1, dry_run=False,
                    tools=Path("/opt/venue"))
        base.update(over)
        return Config(**base)

    def fake_runner(log, fail_on=None, pair_sequence=62):
        def runner(cmd):
            cmd = [str(c) for c in cmd]
            log.append(cmd)
            if cmd[1].endswith("snapshot_fetch.py"):
                if pair_sequence is not None:
                    pair = Path(cmd[cmd.index("--pair") + 1])
                    pair.mkdir(parents=True, exist_ok=True)
                    (pair / "view.json").write_text(json.dumps({"sequence": pair_sequence}))
                return
            if fail_on and fail_on in cmd:
                raise subprocess.CalledProcessError(1, cmd)
        return runner

    no_sleep = dict(sleep=lambda s: None)

    with tempfile.TemporaryDirectory() as d:
        # The happy path: the commands in the order STATE.md gives, cut at the pair's sequence, core stopped
        # before the first of them and started after the last.
        kube, log = FakeKube(), []
        cut(config(d), kube, fake_runner(log), **no_sleep)
        verbs = [c[1] for c in log if c[0] == "venue-node"] + ["ship" for c in log if c[1].endswith("journal_ship.py")]
        if [c[1] for c in log if c[0] == "venue-node"] != ["embed-snapshot", "export-cut", "cut-journal"]:
            failures.append(f"the venue-node steps ran out of order or not at all: {verbs}")
        embed = next(c for c in log if c[:2] == ["venue-node", "embed-snapshot"])
        if embed[-2:] != ["--at", "62"]:
            failures.append(f"it did not cut at the published pair's sequence: {embed}")
        if kube.calls != [("scale", 0), ("scale", 1)]:
            failures.append(f"the core was not stopped once and started once: {kube.calls}")
        order = [c[1] for c in log if len(c) > 1]
        if order.index("embed-snapshot") > order.index("export-cut") or not any(
                c[1].endswith("journal_ship.py") for c in log):
            failures.append("a step ran out of order, or shipping never happened")
        ship_at = next(i for i, c in enumerate(log) if c[1].endswith("journal_ship.py"))
        cut_at = next(i for i, c in enumerate(log) if c[:2] == ["venue-node", "cut-journal"])
        if ship_at > cut_at:
            failures.append("the segment was cut before it was shipped")

    for step in ("embed-snapshot", "export-cut", "cut-journal"):
        with tempfile.TemporaryDirectory() as d:
            kube, log = FakeKube(), []
            try:
                cut(config(d), kube, fake_runner(log, fail_on=step), **no_sleep)
                failures.append(f"a failing {step} did not fail the run")
            except subprocess.CalledProcessError:
                pass
            if kube.calls != [("scale", 0), ("scale", 1)] or not kube.up:
                failures.append(f"a failing {step} left the core down: {kube.calls}")

    with tempfile.TemporaryDirectory() as d:  # a failing ship is the one step that touches the network
        kube, log = FakeKube(), []
        try:
            cut(config(d), kube, fake_runner(log, fail_on="--require-shipped"), **no_sleep)
        except subprocess.CalledProcessError:
            pass
        if not kube.up:
            failures.append("a refused cut left the core down")

    with tempfile.TemporaryDirectory() as d:  # no published pair: refuse, and do not touch the venue
        kube, log = FakeKube(), []
        try:
            cut(config(d), kube, fake_runner(log, pair_sequence=None), **no_sleep)
            failures.append("it cut with no published pair to carry on from")
        except Refused:
            pass
        if kube.calls:
            failures.append(f"a refusal before the stop still touched the core: {kube.calls}")

    with tempfile.TemporaryDirectory() as d:  # not enough room: refuse before stopping
        kube, log = FakeKube(), []
        try:
            cut(config(d, margin_gib=10 ** 6), kube, fake_runner(log), **no_sleep)
            failures.append("it cut with no room to stage the segment")
        except Refused as e:
            if "not enough room" not in str(e):
                failures.append(f"the disk refusal did not say why: {e}")
        if kube.calls:
            failures.append(f"a disk refusal still touched the core: {kube.calls}")

    # The read-back copy: files of 100 bytes each need 100 + 100 + 100 (a second copy of the larger), so 250 free
    # would have passed when only the segment was counted and must now be refused; 300 must pass.
    real_usage = shutil.disk_usage
    try:
        for free, should_refuse in ((250, True), (300, False)):
            shutil.disk_usage = lambda path, free=free: type("U", (), {"free": free})()
            with tempfile.TemporaryDirectory() as d:
                kube, log = FakeKube(), []
                try:
                    cut(config(d, dry_run=True), kube, fake_runner(log), **no_sleep)
                    refused = False
                except Refused:
                    refused = True
                if refused != should_refuse:
                    failures.append(f"with {free} bytes free the read-back copy was "
                                    f"{'not counted' if should_refuse else 'over-counted'}")
                if kube.calls:
                    failures.append(f"the room check touched the core: {kube.calls}")
    finally:
        shutil.disk_usage = real_usage

    with tempfile.TemporaryDirectory() as d:  # dry run: every check, no stop
        kube, log = FakeKube(), []
        cut(config(d, dry_run=True), kube, fake_runner(log), **no_sleep)
        if kube.calls or any(c[0] == "venue-node" for c in log):
            failures.append(f"--dry-run changed something: {kube.calls} {log}")

    with tempfile.TemporaryDirectory() as d:  # a core that does not come back is reported, not hidden
        kube, log = FakeKube(), []
        kube.ready_after_start = False
        ticks = iter(range(0, 10 ** 6, 10))
        cut(config(d, start_timeout=1), kube, fake_runner(log), sleep=lambda s: None, clock=lambda: next(ticks))
        if kube.calls != [("scale", 0), ("scale", 1)]:
            failures.append(f"a core that stayed unready changed what was asked of it: {kube.calls}")

    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    if not failures:
        print("self-test passed")
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--to", help="s3://BUCKET, or a directory, as snapshot_publish.py and journal_ship.py use")
    parser.add_argument("--environment", help="which venue this is: prod, pr-4. Names part of every key.")
    parser.add_argument("--namespace", default=os.environ.get("POD_NAMESPACE"), help="defaults to the pod's own")
    parser.add_argument("--statefulset", default="venue-core", help="the StatefulSet whose single pod writes the journal")
    parser.add_argument("--data", default="/var/lib/venue", help="where the journal volume is mounted")
    parser.add_argument("--work", default="/work", help="scratch for the segment; must be on a disk with room")
    parser.add_argument("--margin-gib", type=int, default=6, help="free space to leave beyond the segment (kubelet "
                        "evicts below 10%%)")
    parser.add_argument("--stop-timeout", type=int, default=180, help="seconds to wait for the core to stop")
    parser.add_argument("--start-timeout", type=int, default=900, help="seconds to wait for it to be ready again")
    parser.add_argument("--dry-run", action="store_true", help="run every check that can refuse and stop there")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not (args.to and args.environment and args.namespace):
        parser.error("--to, --environment and a namespace (--namespace or POD_NAMESPACE) are required")
    args.tools = Path(__file__).resolve().parent
    try:
        return cut(args, Kube(args.namespace))
    except Refused as e:
        print(f"refused, the venue was not touched: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
