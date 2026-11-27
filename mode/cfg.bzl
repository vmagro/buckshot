"""Root-project setup for the debug/release build-mode modifiers.

A project enables `-m` modifiers by calling `set_cfg_constructor` from
the `PACKAGE` file of the project root -- which buck2 enforces: the call
aborts the build from any other PACKAGE file. So this helper registers
the constructor only when the calling PACKAGE belongs to the expected
root cell, letting one file serve both this repo (called with
`"buckshot"` from buckshot//PACKAGE) and downstream projects that
consume buckshot as a cell (called with their own root cell name, e.g.
`"root"`, from their root PACKAGE, with aliases pointing back at
buckshot//mode).
"""

load("@prelude//:native.bzl", "native")
load(
    "@prelude//cfg/modifier:cfg_constructor.bzl",
    "cfg_constructor_post_constraint_analysis",
    "cfg_constructor_pre_constraint_analysis",
)
load("@prelude//cfg/modifier:common.bzl", "MODIFIER_METADATA_KEY")

def setup_build_mode(expected_root):
    """Register the debug/release cfg constructor if this is the root.

    Args:
        expected_root: name of the project-root cell whose PACKAGE file is
            calling (e.g. `"buckshot"` here, `"root"` downstream). When
            this runs anywhere else -- notably the same file evaluated as
            a cell in a downstream project, where get_cell_name() still
            returns the file's own cell -- this silently does nothing: a
            mismatch is the normal cell-consumption case, not an error.
            (Comparing against get_cell_name() cannot work for exactly
            that reason; the expected root must be matched against the
            actual project root.)
    """
    if expected_root != get_root_cell_name():
        return
    native.set_cfg_constructor(
        stage0 = cfg_constructor_pre_constraint_analysis,
        stage1 = cfg_constructor_post_constraint_analysis,
        key = MODIFIER_METADATA_KEY,
        aliases = struct(
            debug = "buckshot//mode:mode[debug]",
            release = "buckshot//mode:mode[release]",
        ),
        extra_data = struct(),
    )
