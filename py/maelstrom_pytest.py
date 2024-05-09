#!/usr/bin/env python
import os
import pytest
import subprocess
import sys
import threading

from typing import Sequence, List
from contextlib import redirect_stdout
from io import StringIO
from maelstrom_client import (
    AddLayerRequest,
    RunJobFuture,
    Client,
    Duration,
    EnvironmentSpec,
    GlobLayer,
    ImageSpec,
    JobDevice,
    JobMount,
    JobMountFsType,
    JobSpec,
    PrefixOptions,
    StubsLayer,
)


class Plugin:
    def __init__(self) -> None:
        self.items: List[pytest.Item] = []

    def pytest_collection_modifyitems(
        self, session: pytest.Session, config: pytest.Config, items: List[pytest.Item]
    ) -> None:
        self.items = items


def collect_pytest_tests() -> List[pytest.Item]:
    plugin = Plugin()
    with redirect_stdout(StringIO()):
        pytest.main(args=["-n", "1", "--co"], plugins=[plugin])
    return plugin.items


def create_venv(requirements: str, dest: str) -> None:
    if not os.path.exists(dest):
        os.mkdir(dest)
    subprocess.check_output(["python", "-m", "venv", dest])
    subprocess.check_output(
        [
            "/bin/bash",
            "-c",
            f"source {dest}/bin/activate && pip install -r {requirements}",
        ],
    )


def format_duration(dur: Duration) -> str:
    frac = dur.nano_seconds / 100_000_000
    return f"{dur.seconds}.{int(frac)}s"


def wait_for_job(name: str, job: RunJobFuture) -> None:
    result = job.result()
    if result.result.HasField("outcome"):
        if result.result.outcome.completed.exited == 0:
            dur = format_duration(result.result.outcome.completed.effects.duration)
            print(f"{name} completed success took {dur}")
        else:
            print(f"{name} completed failure")
            stdout = result.result.outcome.completed.effects.stdout.inline.decode()
            sys.stdout.write(stdout)
            stderr = result.result.outcome.completed.effects.stderr.inline.decode()
            sys.stderr.write(stderr)
    else:
        print("error:", str(result.result.error).strip())


def main() -> None:
    client = Client(slots=24)
    image = ImageSpec(
        name="python", tag="3.12-alpine3.19", use_layers=True, use_environment=True
    )

    work = os.path.abspath(".")
    venv_dir = "maelstrom_venv"

    print("creating venv")
    create_venv("test-requirements.txt", venv_dir)

    print("creating layers")
    layers = []
    layer = GlobLayer(
        glob=f"{venv_dir}/lib/python3.12/site-packages/**",
        prefix_options=PrefixOptions(
            canonicalize=False,
            follow_symlinks=False,
            strip_prefix=f"{venv_dir}/",
            prepend_prefix=f"/usr/local/",
        ),
    )
    layers.append(client.add_layer(layer))

    layers.append(
        client.add_layer(
            StubsLayer(stubs=["/dev/null", "/tmp/", f"{work}/.pytest_cache/"])
        )
    )

    opt = PrefixOptions(canonicalize=False, follow_symlinks=False, prepend_prefix=work)
    layers.append(client.add_layer(GlobLayer(glob="**.py", prefix_options=opt)))
    layers.append(client.add_layer(GlobLayer(glob="**.pyc", prefix_options=opt)))

    print("collecting tests")
    tests = collect_pytest_tests()

    print("enqueuing")
    job_threads = []
    for item in tests:
        (file, _, case_) = item.reportinfo()
        if not str(file).endswith(".py"):
            continue
        file = os.path.relpath(file, ".")

        case_ = case_.replace(".", "::")
        script = f"/usr/local/bin/python -m pytest -n 1 {file}::{case_}"

        spec = JobSpec(
            program="/bin/sh",
            arguments=["-c", script],
            image=image,
            layers=layers,
            user=0,
            group=0,
            # environment=[EnvironmentSpec(vars={'PYTHONDONTWRITEBYTECODE': '1'}, extend=True)],
            devices=[JobDevice.Null],
            mounts=[
                JobMount(fs_type=JobMountFsType.Tmp, mount_point="/tmp"),
                JobMount(
                    fs_type=JobMountFsType.Tmp, mount_point=f"{work}/.pytest_cache"
                ),
            ],
            working_directory=work,
            enable_writable_file_system=True,
        )
        job = client.run_job(spec)
        t = threading.Thread(target=wait_for_job, args=(f"{file}::{case_}", job))
        t.start()
        job_threads.append(t)
        break
    print(f"running {len(job_threads)} jobs")

    for t in job_threads:
        t.join()


if __name__ == "__main__":
    main()
