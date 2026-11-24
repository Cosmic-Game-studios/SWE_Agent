TAX_RATE = 0.2


def calc_total(prices, discount=0.0):
    """Sum of `prices` after `discount` (0..1), with tax."""
    subtotal = sum(prices) * (1 - discount)
    return round(subtotal * (1 + TAX_RATE), 2)
