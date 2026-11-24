def truncate(text, limit):
    """Cut `text` to at most `limit` characters, ending in an ellipsis when cut."""
    if len(text) <= limit:
        return text
    return text[: max(limit - 1, 0)] + "…"


def word_count(text):
    return len(text.split())
