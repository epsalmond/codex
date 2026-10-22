from pkg import primary, secondary


def run() -> tuple[str, str, str, str]:
    first = primary.render("first")
    repeated = primary.render("left") + primary.render("right")
    astral = ("🛰️", primary.render("unicode"))[1]
    other = secondary.render("other")
    return first, repeated, astral, other


# no_symbol_here
