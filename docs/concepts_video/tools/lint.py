"""Compiles the compositions and runs the lint of HyperFrames on them.

It needs the files of the repository and the pinned tools only: no voice
model, no Chrome and no footage.
"""

from tools.project import compile_and_lint_without_media

if __name__ == "__main__":
    compile_and_lint_without_media()
    print("The lint of the compositions found no fault.")
