"""AOTInductor packages (.pt2) of the mokuro-bunko recognizers for the libtorch backend.

Dev/CI-only (never on users' machines). docs/rust-port/TORCH-BACKEND.md, section
"Compiled model packages", describes the layout, the targets and the graph I/O.
"""

TOOL_VERSION = "0.1.0"
# Graph I/O contract version written into every package (metadata ``bunko.io``).
IO_VERSION = 2
