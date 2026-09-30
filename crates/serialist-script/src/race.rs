//! A two-way `select` without pulling in a macro crate.

use std::future::{Future, poll_fn};
use std::pin::pin;
use std::task::Poll;

pub(crate) enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Waits for whichever of `a` and `b` finishes first and drops the other. `a` is polled
/// first, so it wins a tie.
pub(crate) async fn race<A: Future, B: Future>(a: A, b: B) -> Either<A::Output, B::Output> {
    let mut a = pin!(a);
    let mut b = pin!(b);
    poll_fn(|cx| {
        if let Poll::Ready(value) = a.as_mut().poll(cx) {
            return Poll::Ready(Either::Left(value));
        }
        if let Poll::Ready(value) = b.as_mut().poll(cx) {
            return Poll::Ready(Either::Right(value));
        }
        Poll::Pending
    })
    .await
}
