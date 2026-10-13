REM Windows launcher for the sibling `buckshot` dotslash file (dotslash
REM files don't execute directly on Windows: no shebangs). The alias passes
REM the dotslash file as the first arg, followed by the user's args.
@dotslash.exe %*
