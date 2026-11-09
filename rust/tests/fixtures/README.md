# Compatibility fixtures

`python-json.json` and `python-reports.json` contain synthetic inputs and expected
outputs from the Python implementation at application revision
`4c3607a474982d94da27d0fde087dbd725af4af8` (Python 3.13). They contain no business
data or credentials. Report variants exercise invalid types, identities, phases,
flags, counts, times and completion consistency. Three cases explicitly record
Rust's stricter rejection of a boolean schema version.

`../../src/publisher/mime_cases.json` contains 663 MIME decisions from Python 3.13,
using the extension-only maps in `../../src/python_mime_types.json`. That bundled
map freezes the previous runtime's behavior; it contains only public extension
and media-type names, not production domains, paths, credentials or asset names.

These files are immutable test oracles. Tests execute in Rust and do not require
a Python interpreter. Each JSON array entry occupies one line for reviewability.
