"""Apply inherited per-process limits before exec; do not fork with preexec_fn."""

import os
import resource
import sys


def main():
    memory, file_bytes = map(int, sys.argv[1:3])
    if sys.platform.startswith("linux"):
        resource.setrlimit(resource.RLIMIT_AS, (memory, memory))
    resource.setrlimit(resource.RLIMIT_FSIZE, (file_bytes, file_bytes))
    os.execvp("git", ["git", *sys.argv[3:]])


if __name__ == "__main__":
    main()
