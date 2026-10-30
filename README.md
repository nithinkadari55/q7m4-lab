# q7m4-lab

Research diagnostics and reproducible Rust build experiments.

Only reviewed source code, build workflows and synthetic fixtures belong in this
public checkout. Detailed observations and local working material are retained
separately. Their exclusion from this repository does not remove them from the
research record.

Published experiments are scoped observations, not claims of production readiness.
Build outputs must be selected explicitly for upload; whole-workspace artifact
uploads are not permitted.

The Windows diagnostic uses a pinned Rust toolchain and dependency lockfile.
Run the manual `Native research probes` Actions workflow to compile it. CI builds
the executable without starting Excel; live observations are collected locally.

The executable creates its own hidden Excel instance and an unsaved synthetic
workbook, checks COM invocation and typed array round trips, then closes its owned
objects. It never attaches to an existing workbook. Do not run it against real
business files. Event delivery, recovery, broad feature coverage and production
readiness require separate research evidence.

Artifacts contain only the selected executable, dependency/identity manifests,
lockfile and dependency license notices. Bootstrap source is retained separately
to make diagnostic build failures reproducible.
