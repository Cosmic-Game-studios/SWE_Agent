from textutil import slugify, truncate, word_count

assert slugify("Hello World") == "hello-world"
assert slugify("  Rust & Go: a comparison!  ") == "rust-go-a-comparison"
assert slugify("already-a-slug") == "already-a-slug"
assert slugify("Version 2.0 released") == "version-2-0-released"
assert slugify("--weird__input--") == "weird-input"
assert slugify("???") == ""
assert slugify("") == ""
assert truncate("abcdef", 4) == "abc…"
assert word_count("a b  c") == 3
print("ok")
