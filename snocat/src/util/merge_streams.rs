// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use futures::stream::{self, Stream, StreamExt};
use std::{boxed::Box, task::Poll};

pub fn merge_streams<'a, T: 'a>(
  source: impl futures::stream::Stream<Item = stream::BoxStream<'a, T>> + 'a + std::marker::Send,
) -> stream::BoxStream<'a, T> {
  let mut source_empty = false;
  let mut source = Box::pin(source);
  let mut items = Box::pin(futures::stream::SelectAll::new());
  futures::stream::poll_fn(move |ctx| -> Poll<Option<T>> {
    let source_ref = source.as_mut();
    if !source_empty {
      match Stream::poll_next(source_ref, ctx) {
        Poll::Ready(Some(new_stream)) => {
          items.push(new_stream);
          // Since we polled Ready, we need to poll again until Pending
          // or we won't be woken up for the next available result.
          //
          // The following call schedules us for immediate re-wake until we pend or complete.
          // See test merge_sequential_wake for verification
          ctx.waker().wake_by_ref();
        }
        Poll::Ready(None) => {
          source_empty = true;
          // Mark that we're at the end of the list of streams, so we know when to bail
        }
        Poll::Pending => {
          // Just poll the existing streams, do nothing here
        }
      };
    }

    let items_ref = items.as_mut();
    match Stream::poll_next(items_ref, ctx) {
      Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
      Poll::Ready(None) => {
        if source_empty {
          Poll::Ready(None)
        } else {
          Poll::Pending
        }
      }
      Poll::Pending => Poll::Pending,
    }
  })
  .fuse()
  .boxed()
}

#[cfg(test)]
mod tests {
  use crate::util::merge_streams::merge_streams;
  use tokio_util::sync::CancellationToken;

  #[tokio::test]
  async fn test_stream_merging() {
    use futures::{
      future::FutureExt,
      stream::{self, StreamExt},
    };

    let x = stream::unfold(1i32, async move |state| {
      if state <= 5 {
        Some((state, state + 1))
      } else {
        None
      }
    })
    .boxed();

    let y = stream::unfold(15, async move |state| {
      if state <= 17 {
        Some((state, state + 1))
      } else {
        None
      }
    })
    .boxed();

    let z = stream::unfold(-1i32, async move |state| {
      if state >= -10 {
        Some((state, state - 1))
      } else {
        None
      }
    })
    .boxed();

    let z_end = CancellationToken::new();
    let x_3 = CancellationToken::new();
    let first = stream::iter(vec![
      async {
        println!("x started");
        None
      }
      .into_stream()
      .boxed(),
      x.map(|x| Some(x))
        .inspect(|v| {
          if *v == Some(3i32) && !x_3.is_cancelled() {
            x_3.cancel()
          }
        })
        .boxed(),
      async {
        println!("x exhausted");
        None
      }
      .into_stream()
      .boxed(),
      {
        let z_end = z_end.clone();
        async move {
          z_end.cancelled().await;
          println!("y started");
          None
        }
      }
      .into_stream()
      .boxed(),
      y.map(|x| Some(x)).boxed(),
      async {
        println!("y exhausted");
        None
      }
      .into_stream()
      .boxed(),
    ])
    .flatten()
    .filter_map(async move |x| x)
    .boxed();
    let second = stream::iter(vec![
      {
        let x_3 = x_3.clone();
        async move {
          x_3.cancelled().await;
          println!("z started");
          None
        }
      }
      .into_stream()
      .boxed(),
      z.map(|x| Some(x)).boxed(),
      async {
        println!("z exhausted");
        z_end.cancel();
        None
      }
      .into_stream()
      .boxed(),
    ])
    .flatten()
    .filter_map(async move |x| x)
    .boxed();

    let stream_source: stream::BoxStream<'_, stream::BoxStream<'_, _>> =
      stream::iter(vec![first, second]).boxed();
    let out: stream::BoxStream<'_, i32> = super::merge_streams(stream_source);

    let mut items = Vec::new();
    out
      .fold(&mut items, async move |i, m| {
        println!("-> {:?}", &m);
        i.push(m.clone());
        i
      })
      .await;

    let pos_of = |x: i32| items.iter().position(|&v| v == x).unwrap();
    assert_eq!(
      pos_of(-1),
      pos_of(3) + 1,
      "Z must start directly after X reaches 3"
    );
    assert_eq!(pos_of(15), pos_of(-10) + 1, "Y must start after Z ends");
    assert_eq!(
      pos_of(-2),
      pos_of(4) + 1,
      "Z must start directly after X reaches 3"
    );
    assert_eq!(
      pos_of(5),
      pos_of(-2) + 1,
      "X must end just after Z reaches -2"
    );
  }

  // Ensures that new streams will be polled even if current streams produce no new events
  // This checks that the poll path on the outer stream schedules for wakes properly,
  // but does not test for fairness between loading new streams and reading events from
  // event streams that are already being observed.
  #[tokio::test]
  async fn merge_sequential_wake() {
    use futures::{
      future,
      stream::{self, StreamExt},
    };
    use tokio::time::timeout;

    // A set of streams with only end-of-stream events
    let empty_sources = stream::repeat_with(|| stream::iter(Vec::new()).boxed()).take(10);

    // The non-empty event stream has less items than the empty-sources stream to ensure we poll
    // for new streams more than we poll for new items
    let non_empty = stream::once(future::ready(stream::iter(vec![1u32, 2u32]).boxed()));

    let source = empty_sources.chain(non_empty);
    let results = timeout(
      std::time::Duration::from_secs(5),
      merge_streams(source).collect::<Vec<u32>>(),
    )
    .await
    .unwrap();

    assert_eq!(results.as_slice(), &[1u32, 2u32]);
  }
}
