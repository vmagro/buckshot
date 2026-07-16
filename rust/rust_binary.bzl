def rust_binary(
    *,
    name: str,
    unittests: bool = True,
    **kwargs
):
    native.rust_binary(
        name = name,
        **kwargs
    )
    if unittests:
        native.rust_test(
            name = name + "-unittest",
            **kwargs
        )