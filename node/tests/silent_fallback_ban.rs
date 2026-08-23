//! Silent-fallback ban (test class 5): no consensus input may silently degrade to a default value on
//! an error path (the difficulty-0 escape: `unwrap_or((ssi, 0))` fed difficulty 0 into the slot state
//! during failure windows). Each banned site either propagates the error (proven here) or is pinned
//! correct with a comment at the site.

use dg_xch_node::header::HeaderSink;
use dg_xch_node::sync::drain_header_sink;

// The window sink (VDF proofs + header signatures): a poisoned sink (a staging thread panicked
// mid-window) must FAIL the window, never yield an empty queue -- the old unwrap_or_default()
// confirmed every staged block with its deferred verification silently skipped.
#[test]
fn poisoned_window_sink_fails_the_window_instead_of_skipping_verification() {
    let sink = HeaderSink::default();
    let (vdf, sig) = drain_header_sink(sink).expect("a clean sink drains");
    assert!(vdf.is_empty() && sig.is_empty());

    let sink = HeaderSink::default();
    // Poison the VDF queue: a thread panics while holding the lock (exactly a mid-stage panic).
    let poisoner = std::thread::spawn({
        let sink = &sink as *const HeaderSink as usize;
        move || {
            // SAFETY: the parent joins before the sink is dropped; the raw pointer only bridges the
            // non-'static thread boundary for this scoped-by-join poison.
            let sink = unsafe { &*(sink as *const HeaderSink) };
            let _guard = sink.vdf.lock().unwrap();
            panic!("staged thread dies holding the sink");
        }
    });
    assert!(poisoner.join().is_err(), "the poisoner must have panicked");
    let out = drain_header_sink(sink);
    assert!(
        out.is_err(),
        "a poisoned sink must fail closed, not drain to an empty (verification-skipping) queue"
    );

    // Symmetric guard on the signature queue: a panic holding the sig lock must fail the window too.
    let sink = HeaderSink::default();
    let poisoner = std::thread::spawn({
        let sink = &sink as *const HeaderSink as usize;
        move || {
            let sink = unsafe { &*(sink as *const HeaderSink) };
            let _guard = sink.sig.lock().unwrap();
            panic!("staged thread dies holding the sig sink");
        }
    });
    assert!(poisoner.join().is_err(), "the poisoner must have panicked");
    let out = drain_header_sink(sink);
    assert!(
        out.is_err(),
        "a poisoned sig sink must fail closed, not drain to an empty (verification-skipping) queue"
    );
}
