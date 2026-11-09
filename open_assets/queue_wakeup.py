"""Linux queue notifications are hints; durable records remain the authority."""

import ctypes
import os
import select
import sys


class QueueWakeup:
    def __init__(self, directory):
        self.fd = -1
        if sys.platform != "linux":
            return
        try:
            library = ctypes.CDLL(None, use_errno=True)
            fd = library.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
            if fd < 0:
                return
            # Atomic enqueue uses MOVED_TO. Other writes and watch loss also
            # wake the scan; queue overflow cannot lose a durable publication.
            if library.inotify_add_watch(fd, os.fsencode(directory), 0x188) < 0:
                os.close(fd)
                return
            self.fd = fd
        except (OSError, AttributeError):
            pass

    def drain(self):
        if self.fd >= 0:
            for _ in range(8):
                try:
                    if not os.read(self.fd, 65536):
                        break
                except BlockingIOError:
                    break

    def wait(self, seconds):
        # Periodic scans recover missed events, watch loss and process restarts.
        select.select([self.fd] if self.fd >= 0 else [], [], [], seconds)

    def close(self):
        if self.fd >= 0:
            os.close(self.fd)
            self.fd = -1
