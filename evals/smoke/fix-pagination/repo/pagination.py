def page_count(total_items, per_page):
    """Number of pages needed to show `total_items`, `per_page` at a time."""
    if per_page <= 0:
        raise ValueError("per_page must be positive")
    return total_items // per_page


def page_slice(items, page, per_page):
    """The items on the 1-based `page`."""
    start = (page - 1) * per_page
    return items[start : start + per_page]
