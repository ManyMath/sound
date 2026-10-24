# Soundboard vendor reconciliation

Compared the Soundboard vendored native source against canonical Sound e3880ab.
The canonical decoder, networking, stream transport and lifecycle code is newer;
the vendor tree must not replace it wholesale.

Carried over bounded AudioQueue sender regressions and ALSA short-write tests,
then retained the canonical 2-slot queue and progress-driven drain. Callback
stall deadlines now scale with low sample rates, bounded to 1–5 seconds; the
final hardware-buffer delay is separately capped to 2020ms. This bounds these
waits, not arbitrary platform teardown calls.

Windows name matching keeps exact-match precedence and ambiguous-prefix
rejection. Reverse-prefix matching is allowed only when WinMM's device name
occupies its full 31 UTF-16-unit field; tests cover surrogate pairs. The vendor
threaded volatile-header test was deliberately not restored: volatile accesses
do not synchronize a Rust data race.

Preserved canonical CoreAudio output-stream filtering and five-argument
CFStringCreate ABI, WinMM checked arithmetic/raw header access/write mutex and
smaller buffers, decoder panic containment and memory limits, bounded downloads,
redirect credential handling, and pause/seek/stop lifecycle handling. The root
PCM timing fix at e3880ab supersedes the vendored timer loop with bounded elapsed
time catch-up. Hardware-specific behavior still requires fleet/device checks;
host helper tests are not proof of CoreAudio or WinMM playback.

Reviewed all eight differing Dart files as well. Keep the canonical decode-once
buffer/finalizer, off-isolate large decodes, generation guards, prompt voice
cleanup and preserved final position. Keep WebAudio input/error handling,
loop-position/replay behavior, volume-aware fade and explicit backend selection
with initialization fallback. Vendor code adds no missing behavior in those
files beyond PCM timing already superseded by e3880ab.
