//! A thread with a Go-sized stack for the recursive ports.
//!
//! Go's goroutine stacks grow to 1 GiB, so Go's `html/template` executor, goldmark's renderer and
//! x/net/html's parser recurse as deep as their input asks. Their ports recurse the same way, and
//! on a tokio worker's 2 MiB stack — in a debug build, whose frames are several times larger — a
//! notification e-mail's `messages_notification` body overflowed it and took the process down
//! (measured 2026-09-24). Each call is short and CPU-bound, so it runs to completion on a scoped
//! thread with a large stack while the caller waits, as Go runs it inline.

/// The stack the recursive ports get: 256 MiB of address space, committed only as touched.
const STACK_SIZE: usize = 256 * 1024 * 1024;

/// Run `f` on a thread with [`STACK_SIZE`] of stack and return its result, or the error of a
/// thread that could not be spawned. A panic inside `f` is resumed on the caller's thread.
pub fn run<T: Send, F: FnOnce() -> T + Send>(f: F) -> Result<T, std::io::Error> {
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("mm-deep-stack".to_owned())
            .stack_size(STACK_SIZE)
            .spawn_scoped(scope, f)?;
        Ok(handle
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
    })
}
