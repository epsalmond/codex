from pkg import primary, secondary


def run() -> tuple[str, str, str, str]:
    first = secondary.render("first")
    repeated = primary.render("a") + primary.render("b") + primary.render("c")
    astral = ("🛰️", primary.render("unicode"))[1]
    other = secondary.render("other")
    return first, repeated, astral, other


# no_symbol_here
