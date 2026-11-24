import shop
from shop.cart import Cart


def summary(prices):
    cart = Cart()
    for price in prices:
        cart.add(price)
    # calc_total applies tax; the report shows both figures.
    return {"net": sum(prices), "gross": shop.calc_total(prices), "cart": cart.total()}
