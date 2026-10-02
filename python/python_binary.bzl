def python_binary(*, name: str, main_function: str, **kwargs):
    native.python_library(name = name + "-library", **kwargs)
    native.python_binary(name = name, deps = [":{}-library".format(name)], main_function = main_function)
