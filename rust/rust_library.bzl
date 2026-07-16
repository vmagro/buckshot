def rust_library(
    *,
    name: str,
    unittests: bool = True,
    **kwargs
):
    native.rust_library(
        name = name,
        **kwargs
    )
    if unittests:
        native.rust_test(
            name = name + "-unittest",
            **kwargs
        )