//! Two renders of one [`crate::model::Response`]: the `-i`/`-I` text form
//! and the `--json` structured object docs/curl.md's "Output, and the
//! `--json` collision" section specifies.
//!
//! Empty stub. The render rules — when headers print above the body, what
//! `--fail` does to the body, what `-o`/`-O` write instead of rendering —
//! are HTTP-surface work and wait on the review docs/curl.md's "Status"
//! section calls for. No functions are declared here yet rather than
//! guessing a signature the review might reshape; "no dual
//! representations" (one [`crate::model::Response`], two renders of it) is
//! the one rule already settled and worth stating up front.
