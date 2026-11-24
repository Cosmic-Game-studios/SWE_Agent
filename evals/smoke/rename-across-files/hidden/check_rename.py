import pathlib

for path in pathlib.Path(".").rglob("*.py"):
    if path.name == "check_rename.py":
        continue
    assert "calc_total" not in path.read_text(encoding="utf-8"), f"{path} still mentions calc_total"

import shop
from shop.cart import Cart
from shop.pricing import compute_total
from report import summary

assert compute_total([10, 20]) == 36.0
assert compute_total([100], discount=0.5) == 60.0
assert shop.compute_total([5]) == 6.0
cart = Cart()
cart.add(10)
assert cart.total() == 12.0
assert summary([10, 20]) == {"net": 30, "gross": 36.0, "cart": 36.0}
print("ok")
