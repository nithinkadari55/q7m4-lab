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
The separate `aspose-roundtrip-probe` compares a pinned FOSS editor on the same
marked fixtures. It records requested strict/repair flags without treating them
as enforced policy, and distinguishes supplied/stale caches from calculation.
The read-only `calamine-read-probe` keeps streaming shared-formula metadata and
typed caches separate, then compares bounded dense APIs only after diagnostic
geometry/formula gates. These gates are not a production hostile-upload boundary.

The manual `Linux external calculation research` workflow separately checks a
distribution build of LibreOffice against authored synthetic scalar cases,
including stale caches, names, LET, LAMBDA and a date boundary. Its Python driver
is a disposable research harness, not a production backend. It does not establish
dynamic-array, advanced-object or round-trip fidelity. Workbooks and detailed
runtime observations are not uploaded; the log contains reviewed case identifiers,
statuses and provenance.
The current manual calculation dispatch compares five client/observation setups
with fail-fast disabled: a distribution pair, mixed old-client/new-server and
matched vendor pairs, with export-only controls for the latter two. The vendor
client is checked against the inspected archive's Python/UNO module hashes and
Python version; no silent fallback to the distribution client is accepted.
Export-only controls explicitly report that memory values were not observed.
Conversion-only CLI controls remain available in the script and earlier cohorts.
Offline fault controls run before engine setup and under the selected interpreter; the runtime
retains bounded partial receipts and reports independent case/API/cleanup failures
together. Probe failures and observed compatibility mismatches have separate exit
codes. Runtime summaries contain reviewed identifiers and statuses, not cell values.

The separate `Expanded Linux semantics research` workflow compares the original
16-case control and a37-case native-qualified synthetic corpus under matching
distribution/vendor runtimes. New cases cover named/recursive LAMBDA, LET scoping,
coercion, date boundaries and array-capable functions. All37 expectations matched
native Excel after explicit calculation; the input passed independent schema
validation. Scalar results here do not establish spill geometry. The four jobs
collect all failures and upload no runtime workbooks or detailed observations.
The original corpus and runtime comparison remain unchanged.

Only reviewed source code, build workflows and synthetic fixtures belong in this
public checkout. Detailed observations and local working material are retained
separately. Their exclusion from this repository does not remove them from the
research record.

Published experiments are scoped observations, not claims of production readiness.
Build outputs must be selected explicitly for upload; whole-workspace artifact
uploads are not permitted.

The standalone `crates/eval-core-research` diagnostic pins Formualizer's evaluator,
parser and common types to one Git commit. Its manual `Pinned Rust evaluator
semantics research` workflow resolves one shared lock, builds on Linux and Windows,
and attempts53 independent synthetic cases. Expected values enter comparison only.
This core-registration route does not exercise XLSX loading, saving, cache
invalidation or general dependency discovery. Dates retain their native variant
and a separately labeled, unqualified calendar-to-serial candidate comparison.
Logical evaluator budgets and direct-child timeout controls do not establish a
hard memory or process-tree sandbox. Runtime files stay private; public logs contain
only reviewed case identifiers, statuses, classes and hashes. Selected executable,
lock, exact formatted source and dependency notices have separate build receipts.
Calculation differences produce a failing comparison stage after build artifacts
have been retained; a successful build is not a compatibility claim.

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
