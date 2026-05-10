"""
order_status/python_buggy.py
============================

The same task ("describe an OrderStatus enum") in Python. This version
*looks* correct — mypy and pyright accept it, the type annotations are
sound, and three of the four cases produce the expected output.

The bug is the missing `Cancelled` branch. With the `else` fallback,
calling `describe(OrderStatus.CANCELLED)` returns the string
`"unknown status"` — a *plausible-looking* output that doesn't crash,
doesn't raise, and doesn't show up in tests that don't exercise the
cancellation path. It propagates downstream as a display bug
(customers see "unknown status" on their cancelled orders), and the
incident is diagnosed by reading the rendered UI, not by reading a
stack trace.

Static type checkers cannot catch this because:

- The `Literal[...]` form *can* be exhaustively checked under
  `assert_never`, but the moment an `else` branch (or a default
  return) exists, the checker has nothing to flag.
- The `enum.Enum` form (used here, the more common shape in
  production Python) is not narrowed exhaustively by mypy/pyright by
  default; the if/elif chain is just opaque control flow.

The Kāra version cannot reach this state: a `match` over an enum is
required to be exhaustive, and the compiler names the missing variant
by name. The bug is structurally impossible to write — the build
fails before the function is callable.

Run with:

    python3 python_buggy.py
    # prints:
    #   pending: awaiting payment
    #   shipped: in transit
    #   delivered: received by customer
    #   cancelled: unknown status   <-- the bug
"""

from enum import Enum


class OrderStatus(Enum):
    PENDING = "pending"
    SHIPPED = "shipped"
    DELIVERED = "delivered"
    CANCELLED = "cancelled"


def describe(status: OrderStatus) -> str:
    if status == OrderStatus.PENDING:
        return "awaiting payment"
    elif status == OrderStatus.SHIPPED:
        return "in transit"
    elif status == OrderStatus.DELIVERED:
        return "received by customer"
    else:
        # The author intended to handle CANCELLED here and forgot.
        # The else clause silently swallows it.
        return "unknown status"


if __name__ == "__main__":
    for status in OrderStatus:
        print(f"{status.value}: {describe(status)}")
