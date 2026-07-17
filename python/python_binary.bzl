def python_binary(
    *,
    name: str,
    main_function: str,
    **kwargs
):
    native.python_library(
        name = name + "-library",
        **kwargs
    )
    native.python_binary(
        name = name,
        main_function = main_function,
        deps = [":{}-library".format(name)]
    )