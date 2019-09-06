//! Asynchronous values.

use core::marker::Unpin;
use core::ops::{Drop, Generator, GeneratorState};
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};
use core::task::{Context, Poll};

#[doc(inline)]
pub use core::future::*;

/// Wrap a generator in a future.
///
/// This function returns a `GenFuture` underneath, but hides it in `impl Trait` to give
/// better error messages (`impl Future` rather than `GenFuture<[closure.....]>`).
#[doc(hidden)]
pub fn from_generator<T: Generator<Yield = ()>>(x: T) -> impl Future<Output = T::Return> {
    GenFuture(x)
}

/// A wrapper around generators used to implement `Future` for `async`/`await` code.
#[doc(hidden)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct GenFuture<T: Generator<Yield = ()>>(T);

// We rely on the fact that async/await futures are immovable in order to create
// self-referential borrows in the underlying generator.
impl<T: Generator<Yield = ()>> !Unpin for GenFuture<T> {}

#[doc(hidden)]
impl<T: Generator<Yield = ()>> Future for GenFuture<T> {
    type Output = T::Return;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Safe because we're !Unpin + !Drop mapping to a ?Unpin value
        let gen = unsafe { Pin::map_unchecked_mut(self, |s| &mut s.0) };
        set_task_context(cx, || match gen.resume() {
            GeneratorState::Yielded(()) => Poll::Pending,
            GeneratorState::Complete(x) => Poll::Ready(x),
        })
    }
}

static FAKE_TLS_CX: AtomicPtr<Context<'static>> = AtomicPtr::new(ptr::null_mut());

struct SetOnDrop(*mut Context<'static>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        FAKE_TLS_CX.store(self.0, Ordering::Relaxed);
    }
}

#[doc(hidden)]
/// Sets the thread-local task context used by async/await futures.
pub fn set_task_context<F, R>(cx: &mut Context<'_>, f: F) -> R
where
    F: FnOnce() -> R
{
    // transmute the context's lifetime to 'static so we can store it.
    let cx = unsafe {
        core::mem::transmute::<&mut Context<'_>, &mut Context<'static>>(cx)
    };
    let old_cx = FAKE_TLS_CX.swap(cx, Ordering::Relaxed);
    let _reset = SetOnDrop(old_cx);
    f()
}

#[doc(hidden)]
/// Retrieves the thread-local task context used by async/await futures.
///
/// This function acquires exclusive access to the task context.
///
/// Panics if no context has been set or if the context has already been
/// retrieved by a surrounding call to get_task_context.
pub fn get_task_context<F, R>(f: F) -> R
where
    F: FnOnce(&mut Context<'_>) -> R
{
    // Clear the entry so that nested `get_task_waker` calls
    // will fail or set their own value.
    let cx_ptr = FAKE_TLS_CX.swap(ptr::null_mut(), Ordering::Relaxed);
    let _reset = SetOnDrop(cx_ptr);

    if cx_ptr.is_null() {
        panic!("TLS Context not set. This is a rustc bug. \
               Please file an issue on https://github.com/rust-lang/rust.");
    }

    // Safety: we've ensured exclusive access to the context by
    // removing the pointer from TLS, only to be replaced once
    // we're done with it.
    //
    // The pointer that was inserted came from an `&mut Context<'_>`,
    // so it is safe to treat as mutable.
    unsafe { f(&mut *cx_ptr) }
}

#[doc(hidden)]
/// Polls a future in the current thread-local task waker.
pub fn poll_with_tls_context<F>(f: Pin<&mut F>) -> Poll<F::Output>
where
    F: Future
{
    get_task_context(|cx| F::poll(f, cx))
}