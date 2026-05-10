"""
welcome_emails/python_buggy.py
==============================

The same task ("send a welcome message to a list of user IDs and print
how many were sent") in Python. This version *looks* correct — mypy
passes, pyright passes, runs without exceptions, single-threaded
behaviour matches expectations.

Under load it under-counts because `self.sent += 1` is not atomic
across threads. Read-modify-write on a plain int is two bytecodes
(`LOAD_ATTR`, `STORE_ATTR` after `BINARY_OP`); a thread switch between
them drops the increment.

This is the kind of bug a static checker for Python cannot catch:
- the types are correct
- the control flow is correct
- the function signatures are reasonable
- the bug is a *concurrency* property of shared mutable state

Kāra catches it because the effect system makes the shared resource
(`writes(SentCount)`) visible to the auto-par analyzer, which refuses
to parallelize two operations that share a write resource. The loop
either runs sequentially (safe), or — if the user asked for parallelism
explicitly — the compiler points at the conflicting writes before the
program ever runs.

Run with:

    python3 python_buggy.py            # often prints "sent 982 of 1000"
    python3 python_buggy.py            # next run: "sent 951 of 1000"
"""

from concurrent.futures import ThreadPoolExecutor
import time


def send_email(user_id: int) -> bool:
    # Pretend to call a real email service.
    time.sleep(0.0005)
    return True


class WelcomeBatch:
    def __init__(self) -> None:
        self.sent: int = 0

    def send_one(self, user_id: int) -> None:
        if send_email(user_id):
            # Race window: read self.sent, add 1, store. A thread
            # switch between read and store drops the update.
            self.sent += 1

    def run(self, user_ids: list[int]) -> None:
        with ThreadPoolExecutor(max_workers=32) as pool:
            list(pool.map(self.send_one, user_ids))


if __name__ == "__main__":
    batch = WelcomeBatch()
    user_ids = list(range(1, 1001))
    batch.run(user_ids)
    print(f"sent {batch.sent} of {len(user_ids)}")
