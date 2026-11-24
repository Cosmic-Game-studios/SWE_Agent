from pagination import page_count, page_slice

assert page_count(0, 10) == 0
assert page_count(10, 10) == 1
assert page_count(11, 10) == 2
assert page_count(99, 10) == 10
assert page_slice(list(range(25)), 3, 10) == [20, 21, 22, 23, 24]
try:
    page_count(5, 0)
except ValueError:
    pass
else:
    raise AssertionError("per_page of 0 must be rejected")
print("ok")
