# q7m4-lab

Research diagnostics and reproducible Rust build experiments.

The target product is a hosted Rust workbook tool layer. End users will not need
Windows or desktop Excel. The native Excel diagnostic is a compatibility oracle.
The separate `headless-research` workspace member tests portable candidates on
Linux and Windows without Office; it is research code, not a production backend.

The manual `Headless Rust research probes` workflow shares one resolved lockfile
between both operating systems. Its parser corpus and memory-only XLSX codec
test run on both hosts. The `umya-roundtrip-probe` executable separately tests a
marked synthetic fixture, writes a new candidate, and reports package loss even
when the requested cell edit succeeds. Do not use it on business workbooks.

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
