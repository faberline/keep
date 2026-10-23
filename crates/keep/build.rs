//! Build script: stamp `KEEP_GIT_SHA`, `KEEP_BUILT_AT`, and `KEEP_TARGET` into
//! the binary so the standard CLI ops (`upgrade` picks the matching release
//! asset; `issue` reports provenance) work without a server. The stamping
//! logic lives in the shared `core/build-stamp` crate; this file only supplies
//! keep's `KEEP` env-var prefix.

fn main() {
    build_stamp::stamp("KEEP");
}
