"""Hermetic python toolchain backed by `python-build-standalone` archives.
"""


def _astral_python_impl(ctx):
    archive = ctx.attrs.archive[DefaultInfo].default_outputs[0]
    interpreter = archive.project("bin/python3")
    return [
        DefaultInfo(sub_targets = {
            "interpreter": [
                DefaultInfo(),
                RunInfo(cmd_args(interpreter)),
            ]
        })
    ]


astral_python = rule(
    impl=_astral_python_impl,
    attrs={
        "archive": attrs.exec_dep(
            providers=[DefaultInfo],
        ),
    },
)