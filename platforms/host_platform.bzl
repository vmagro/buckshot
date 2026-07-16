load("@prelude//:rules.bzl", "platform")

def host_platform(*, name: str):
    constraint_values = []
    hi = native.host_info()
    if hi.os.is_macos:
        constraint_values += ["prelude//os/constraints:macos"]
    elif hi.os.is_linux:
        constraint_values += ["prelude//os/constraints:linux"]
    elif hi.os.is_windows:
        constraint_values += ["prelude//os/constraints:windows"]
    else:
        fail("Unsupported host operating system: {}".format(hi.os))
    if hi.arch.is_aarch64:
        constraint_values += ["prelude//cpu/constraints:arm64"]
    elif hi.arch.is_x86_64:
        constraint_values += ["prelude//cpu/constraints:x86_64"]
    else:
        fail("Unsupported host architecture: {}".format(hi.arch))
    platform(
        name = name,
        constraint_values = constraint_values,
        visibility = ["PUBLIC"],
    )
