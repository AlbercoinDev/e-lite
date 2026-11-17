//! Tests of main.rs: the panic policy, error classification and formatting.
//! Every other module keeps its tests next to its code.

use super::*;
use crate::pipeline::tests::{info, tip};

#[test]
fn uptime_is_human_readable() {
    assert_eq!(format_uptime(Duration::from_millis(500)), "0.5s");
    assert_eq!(format_uptime(Duration::from_secs(59)), "59.0s");
    assert_eq!(format_uptime(Duration::from_secs(3661)), "1h 01m 01s");
}

/// AUD-173: the panic hook keeps the node up when a wallet-facing thread
/// panics and aborts the process for any other thread. Abort cannot be
/// observed in-process, so the test runs itself again as a child.
#[test]
fn panic_hook_contains_wallet_threads_and_aborts_the_rest() {
    use std::os::unix::process::ExitStatusExt;
    const CHILD: &str = "E_LITE_PANIC_HOOK_CHILD";
    if let Ok(which) = std::env::var(CHILD) {
        crate::install_panic_hook();
        let name = if which == "contained" {
            el_model::sync::THREAD_CLIENT
        } else {
            "el-prune"
        };
        let _ = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(|| panic!("injected"))
            .unwrap()
            .join();
        std::process::exit(0);
    }
    let exe = std::env::current_exe().unwrap();
    for (which, survives) in [("contained", true), ("critical", false)] {
        let out = std::process::Command::new(&exe)
            .args([
                "--exact",
                "tests::panic_hook_contains_wallet_threads_and_aborts_the_rest",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, which)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if survives {
            assert!(out.status.success(), "{which}: {:?}\n{stderr}", out.status);
            assert!(stderr.contains("the node keeps running"), "{stderr}");
            assert!(
                !stderr.contains("injected"),
                "wallet panic message leaked: {stderr}"
            );
        } else {
            assert_eq!(
                out.status.signal(),
                Some(libc::SIGABRT),
                "{which}: {stderr}"
            );
            assert!(
                stderr.contains("FATAL: internal error in el-prune"),
                "{stderr}"
            );
        }
    }
}

/// AUD-175: the class of an indexing error comes from its type, built where
/// it is detected, never from the words in the message.
#[test]
fn indexing_errors_are_classified_by_type_not_by_text() {
    let class = |r: R<()>| classify_error(r.unwrap_err().as_ref());
    // Producers of the failures that used to be matched by text.
    assert_eq!(
        class(check_pruned(&info(true, 100), None, None)),
        ErrorClass::Permanent
    );
    assert_eq!(
        class(check_pruned(&info(true, 100), Some(&tip(50)), None)),
        ErrorClass::Permanent
    );
    let regtest = NetworkIdentity::new(Chain::Regtest, Chain::Regtest.known_genesis().unwrap());
    let other = NetworkIdentity::new(Chain::Regtest, Hash32([7; 32]));
    assert_eq!(
        class(verify_identity(&regtest, &other)),
        ErrorClass::Permanent
    );
    let mut signet = regtest.clone();
    signet.signet_challenge = Some(Hash32([1; 32]));
    assert_eq!(
        class(verify_identity(&regtest, &signet)),
        ErrorClass::Permanent
    );
    // A database of another network.
    let dir = std::env::temp_dir().join(format!("e-lite-classify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let store = Store::open(&dir).unwrap();
        store.init_identity(&regtest).unwrap();
        let wrong = stored_identity(&store, Chain::Signet).map(|_| ());
        assert_eq!(class(wrong), ErrorClass::Permanent);
    }
    let _ = std::fs::remove_dir_all(&dir);

    // The words alone decide nothing any more: an RPC hiccup that mentions
    // "schema" or "txids.dat" is retried, and a fatal error reworded stays
    // fatal.
    assert_eq!(
        class(fail("rpc timed out reading the schema")),
        ErrorClass::Transient
    );
    assert_eq!(
        class(fail("txids.dat: Resource temporarily unavailable")),
        ErrorClass::Transient
    );
    assert_eq!(
        class(fatal("something new and unrecoverable")),
        ErrorClass::Permanent
    );

    // A wrapped Fatal is still fatal.
    #[derive(Debug)]
    struct Wrapper(Fatal);
    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "context: {}", self.0)
        }
    }
    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }
    let wrapped: E = Box::new(Wrapper(Fatal("deep".into())));
    assert_eq!(classify_error(wrapped.as_ref()), ErrorClass::Permanent);

    // Core still starting is transient, and recognised for the log line.
    assert!(el_rpc::is_core_loading(
        "getblockchaininfo failed: \"Loading block index…\" (code -28)"
    ));
    assert!(!el_rpc::is_core_loading("connection refused"));
}
