"""
user_lookup/python_buggy.py
===========================

The same task ("look up a display name from a registry") in Python.
This version *looks* correct — the type annotations are sound, the
control flow is reasonable, default mypy/pyright pass.

Two natural Python idioms, both of which fail at runtime when the
key is missing:

    A. users[user_id].upper()
       Subscript access. Returns str when the key is present;
       raises KeyError when not. mypy is happy because the dict
       value type IS str — the lookup itself is total in the type
       system, even though it isn't total at runtime.

    B. users.get(user_id).upper()
       .get() returns Optional[str]. Calling .upper() on None
       raises AttributeError. mypy under --strict catches this; mypy
       under default settings does not.

A small but instructive observation: Python's defaults punt the
totality check to runtime. The "natural" idioms are the ones that
fail; the safe shape (users.get(user_id, "Unknown")) requires the
author to *remember* the default. Forgetting it is a one-character
deletion that drops you back into nullable-territory with no
checker complaint.

Kāra inverts that: the unsafe shape (assume present, return
directly) does not compile; the author has to write a handler at
the lookup site, choose what "missing" means, and only then proceed.

Run with:

    python3 python_buggy.py
    # raises KeyError: 99
"""


def get_display_name(users: dict[int, str], user_id: int) -> str:
    # Idiom A: subscript access. Type-correct, runtime-fragile.
    return users[user_id].upper()


def get_display_name_alt(users: dict[int, str], user_id: int) -> str:
    # Idiom B: .get() then call. Type-correct in default mypy,
    # AttributeError on None at runtime.
    return users.get(user_id).upper()  # type: ignore[union-attr]


if __name__ == "__main__":
    users = {1: "Alice", 2: "Bob"}
    print(get_display_name(users, 1))    # ALICE — fine
    print(get_display_name(users, 99))   # KeyError: 99
