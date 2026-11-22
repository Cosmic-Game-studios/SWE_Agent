#!/usr/bin/env bash
# Create the small example project the demo recording works on.
# Usage: setup-workspace.sh <target-dir>
set -euo pipefail

target="${1:?usage: setup-workspace.sh <target-dir>}"
rm -rf "$target"
mkdir -p "$target/src" "$target/tests"
cd "$target"

cat > src/store.py <<'PY'
"""In-memory note storage."""


class NoteStore:
    def __init__(self):
        self._notes = {}
        self._next_id = 1

    def add(self, title, body=""):
        note = {"id": self._next_id, "title": title, "body": body}
        self._notes[note["id"]] = note
        self._next_id += 1
        return note

    def get(self, note_id):
        return self._notes[note_id]

    def all(self):
        return list(self._notes.values())
PY

cat > tests/test_store.py <<'PY'
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent.parent / "src"))
from store import NoteStore


class StoreTest(unittest.TestCase):
    def test_add_and_get(self):
        store = NoteStore()
        note = store.add("Buy milk")
        self.assertEqual(store.get(note["id"])["title"], "Buy milk")


if __name__ == "__main__":
    unittest.main()
PY

printf '# notes-api\n\nA tiny notes service.\n\nRun the tests with `python3 -m unittest discover -s tests`.\n' > README.md
printf '__pycache__/\n' > .gitignore

git init -q -b main
git config user.email "demo@example.com"
git config user.name "Demo"
git config commit.gpgsign false
git add -A
git commit -q -m "Initial notes store"
