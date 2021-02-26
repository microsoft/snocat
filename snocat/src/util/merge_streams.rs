// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use anyhow::{Error as AnyErr, Result};
use async_std::net::TcpStream;
use futures::future::*;
use futures::io::{AsyncRead, AsyncWrite};
use futures::stream::{self, SelectAll, Stream, StreamExt};
use futures::AsyncReadExt;
use quinn::{
  Certificate, CertificateChain, ClientConfig, ClientConfigBuilder, Endpoint, Incoming, PrivateKey,
  ServerConfig, ServerConfigBuilder, TransportConfig,
};
use std::boxed::Box;
use std::path::Path;
use std::task::{Context, Poll};
use std::{error::Error, net::SocketAddr, sync::Arc};

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
  #[tokio::test]
  async fn test_stream_merging() {
    use futures::{
      future::FutureExt,
      stream::{self, Stream, StreamExt},
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

    let (trigger_z_end, listener_z_end) = triggered::trigger();
    let (trigger_x_3, listener_x_3) = triggered::trigger();
    let first = stream::iter(vec![
      async {
        println!("x started");
        None
      }
      .into_stream()
      .boxed(),
      x.map(|x| Some(x))
        .inspect(|v| {
          if *v == Some(3i32) && !trigger_x_3.is_triggered() {
            trigger_x_3.trigger()
          }
        })
        .boxed(),
      async {
        println!("x exhausted");
        None
      }
      .into_stream()
      .boxed(),
      async {
        listener_z_end.await;
        println!("y started");
        None
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
      async {
        listener_x_3.await;
        println!("z started");
        None
      }
      .into_stream()
      .boxed(),
      z.map(|x| Some(x)).boxed(),
      async {
        println!("z exhausted");
        trigger_z_end.trigger();
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
}
